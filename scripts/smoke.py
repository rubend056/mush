#!/usr/bin/env python3
"""End-to-end smoke tests for mush.

These drive the *real* binary through a pseudo-terminal: they send keystrokes,
wait for the configured model to call tools, and assert on files on disk. That
makes them the only test that covers the whole path — keys, agent loop, tool
execution, atomic writes, and session persistence.

Usage:
    python3 scripts/smoke.py [BINARY] [WORKDIR] [--agent|--resize|--cancel|--sigterm|--lock]

The resize, cancel, sigterm and lock scenarios need no model endpoint; the
others do.

Defaults to ./target/debug/mush and a fresh directory under /tmp.
Requires a reachable model endpoint (see the MUSH_URL / MUSH_MODEL variables).
"""

import argparse
import fcntl
import json
import os
import pathlib
import pty
import select
import signal
import socket
import struct
import subprocess
import sys
import termios
import threading
import time


class Child:
    """The parts of `subprocess.Popen` the harness uses, for a forked child."""

    def __init__(self, pid: int):
        self.pid = pid
        self.returncode = None

    def wait(self, timeout: float = None) -> int:
        deadline = None if timeout is None else time.time() + timeout
        while True:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.returncode = os.waitstatus_to_exitcode(status)
                return self.returncode
            if deadline is not None and time.time() > deadline:
                raise subprocess.TimeoutExpired(self.pid, timeout)
            time.sleep(0.05)

    def kill(self) -> None:
        os.kill(self.pid, signal.SIGKILL)
        _, status = os.waitpid(self.pid, 0)
        self.returncode = os.waitstatus_to_exitcode(status)


class Tui:
    """A mush process attached to a pseudo-terminal."""

    def __init__(
        self,
        binary: str,
        workspace: pathlib.Path,
        rows: int = 34,
        cols: int = 110,
        env_extra: dict = None,
    ):
        self.workspace = workspace
        self.workspace.mkdir(parents=True, exist_ok=True)
        self.captured = bytearray()

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.master = master
        env = dict(os.environ, **(env_extra or {}))

        pid = os.fork()
        if pid == 0:
            # Child: become a session leader owning this pty, exactly as a
            # shell does for the program it runs. Without that, the app's
            # /dev/tty — where crossterm reads the window size — would be the
            # terminal these tests run in, not this pty, so resizing here
            # would be invisible to it (and the geometry would not be ours).
            try:
                os.close(master)
                os.setsid()
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
                os.dup2(slave, 0)
                os.dup2(slave, 1)
                os.dup2(slave, 2)
                if slave > 2:
                    os.close(slave)
                os.execve(binary, [binary, str(self.workspace)], env)
            except BaseException:
                os._exit(127)
        os.close(slave)
        self.proc = Child(pid)

    def pump(self, seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.master], [], [], 0.1)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.captured.extend(chunk)

    def send(self, text: str, settle: float = 0.3) -> None:
        os.write(self.master, text.encode())
        self.pump(settle)

    def wait_for(self, path: pathlib.Path, predicate, timeout: float = 120) -> str:
        deadline = time.time() + timeout
        text = ""
        while time.time() < deadline:
            self.pump(1.0)
            if path.exists():
                text = path.read_text()
                if predicate(text):
                    return text
        return text

    def close(self) -> int:
        # `Ctrl-Q` twice. Quitting over live work says what it would kill first
        # and quits on the second press (finding H9), and a scenario can end
        # while an agent is still running — the agent scenario does, by
        # construction, since it stops the moment the file appears. With
        # nothing live the first press is the quit and the second is written to
        # a pty nobody is reading, which is ignored.
        for _ in range(2):
            try:
                self.send("\x11", 0.5)
            except OSError:
                break
        self.pump(1.0)
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        return self.proc.returncode

    def tail(self, lines: int = 6) -> str:
        return self.captured.decode(errors="replace")[-2500:]


def check(name: str, ok: bool, detail: str = "") -> bool:
    print(f"{'PASS' if ok else 'FAIL'}  {name}{(' — ' + detail) if detail else ''}")
    return ok


def scenario_agent(binary: str, root: pathlib.Path) -> bool:
    """The agent creates a file from scratch; persistence must be in place."""
    print(f"\n== agent == {root}")
    tui = Tui(binary, root)
    tui.pump(2.0)
    tui.send("create a file hello.txt containing exactly: hi from mush\r")
    text = tui.wait_for(root / "hello.txt", lambda t: t.strip() != "")
    exit_code = tui.close()

    results = [
        check("agent created hello.txt", (root / "hello.txt").exists()),
        check("file has the requested content", text.strip() == "hi from mush", repr(text)),
        check(".mush/session.json written", (root / ".mush" / "session.json").exists()),
        check(
            ".mush/.gitignore ignores itself",
            (root / ".mush" / ".gitignore").read_text() == "*\n"
            if (root / ".mush" / ".gitignore").exists()
            else False,
        ),
        check("clean exit", exit_code == 0, f"exit {exit_code}"),
    ]
    if not all(results):
        print(tui.tail())
    return all(results)


def scenario_resize(binary: str, root: pathlib.Path) -> bool:
    """A terminal resize must schedule its own redraw — no keypress needed.

    This one does not need a model: the app is pointed at an unreachable
    endpoint and never asked to chat. Resizing the pty alone (window-size
    ioctl + SIGWINCH, which is exactly what a foreground TUI receives) has to
    make the screen repaint; before the fix the redraw awaited any other input.
    """
    print(f"\n== resize == {root}")
    env = {"MUSH_URL": "http://127.0.0.1:1", "MUSH_PROVIDER": "custom", "MUSH_MODEL": "probe"}
    tui = Tui(binary, root, rows=34, cols=110, env_extra=env)
    tui.pump(1.5)
    before = len(tui.captured)

    fcntl.ioctl(tui.master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 150, 0, 0))
    os.kill(tui.proc.pid, signal.SIGWINCH)
    tui.pump(1.0)
    redraw_bytes = len(tui.captured) - before

    # The app must still accept keys and exit cleanly afterwards.
    before_keys = len(tui.captured)
    tui.send("\t", 0.5)  # cycle focus
    responds = len(tui.captured) > before_keys
    exit_code = tui.close()

    results = [
        check("resize alone triggered a redraw", redraw_bytes > 200, f"{redraw_bytes} bytes"),
        check("app still responds to keys", responds),
        check("clean exit", exit_code == 0, f"exit {exit_code}"),
    ]
    if not all(results):
        print(tui.tail())
    return all(results)


def scenario_lock(binary: str, root: pathlib.Path) -> bool:
    """One mush per workspace: the second start says no, and lets the first work.

    No model needed — the refusal happens before any request, and before the
    session is read. The three things that matter are all here: the second
    process exits non-zero *with the holder's pid in the sentence* (so the
    human knows who to quit), a subcommand still reaches the running mush (the
    refusal tells them to do exactly that), and the lock dies with its holder,
    so the workspace is usable again after a `kill -9` with nothing to clean up.
    """
    print(f"\n== lock == {root}")
    env = {"MUSH_URL": "http://127.0.0.1:1", "MUSH_PROVIDER": "custom", "MUSH_MODEL": "probe"}
    tui = Tui(binary, root, rows=34, cols=110, env_extra=env)
    tui.pump(2.0)

    second = subprocess.run([binary, str(root)], capture_output=True, text=True, timeout=30)
    refusal = second.stderr.strip()

    # `mush agents` is the escape hatch the refusal names: it drives the
    # running process over its socket, so it must not be refused itself.
    subcommand = subprocess.run([binary, "agents", str(root)], capture_output=True, text=True, timeout=30)

    exit_code = tui.close()

    # The holder is gone now; the same workspace must open again. This is the
    # part a pid file would get wrong (a stale file, and a refused start after
    # a crash), which is why the lock is `flock(2)` and never unlinked.
    reopened = Tui(binary, root, rows=34, cols=110, env_extra=env)
    reopened.pump(1.5)
    reopened_code = reopened.close()

    results = [
        check("the second start is refused", second.returncode == 1, f"exit {second.returncode}"),
        check(
            "the refusal names the holder",
            str(tui.proc.pid) in refusal and "already running in this workspace" in refusal,
            refusal or "(nothing on stderr)",
        ),
        check(
            "the refusal points at `mush agents`",
            "mush agents" in refusal,
            refusal,
        ),
        check(
            "a subcommand still reaches the running mush",
            subcommand.returncode == 0 and "root" in subcommand.stdout,
            f"exit {subcommand.returncode}: {subcommand.stdout.strip()[:200]}",
        ),
        check("the first mush exited cleanly", exit_code == 0, f"exit {exit_code}"),
        check("the lock went with it", reopened_code == 0, f"exit {reopened_code}"),
    ]
    if not all(results):
        print(tui.tail())
    return all(results)


def scenario_cancel(binary: str, root: pathlib.Path) -> bool:
    """Ctrl-C must stop a model call that has not answered yet.

    The endpoint accepts the first chat request and then says nothing for thirty
    seconds — the shape of a reasoning model thinking, only ruder. The proof is
    behavioural: after Ctrl-C the agent must be free to send the *next* request
    while the first socket is still being held. Without cancellation reaching the
    reader, the actor would sit in that read for thirty seconds and no second
    request could exist. The model list is answered, or startup itself would
    block on this socket before the TUI enters raw mode.
    """
    print(f"\n== cancel == {root}")
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(4)
    port = listener.getsockname()[1]

    second_request = threading.Event()
    state = {"chats": 0}

    env = {
        "MUSH_URL": f"http://127.0.0.1:{port}",
        "MUSH_PROVIDER": "custom",
        "MUSH_MODEL": "probe",
    }
    # Fork the TUI *before* any thread exists: forking a process that already
    # has threads is a known CPython deadlock hazard, and this was the one
    # scenario that did it.
    tui = Tui(binary, root, rows=24, cols=100, env_extra=env)

    def reply(connection, payload: bytes):
        connection.sendall(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            b"Content-Length: " + str(len(payload)).encode() + b"\r\n\r\n" + payload
        )
        connection.close()

    def handle(connection):
        request = connection.recv(4096)
        if b"/models" in request:
            reply(connection, b'{"object":"list","data":[{"id":"probe"}]}')
            return
        state["chats"] += 1
        if state["chats"] == 1:
            time.sleep(30)  # a model that never answers
            connection.close()
        else:
            second_request.set()
            reply(
                connection,
                b'{"choices":[{"message":{"role":"assistant",'
                b'"content":"answered the second request"},"finish_reason":"stop"}]}',
            )

    def endpoint():
        # One thread per connection: the first chat is held for thirty seconds,
        # and the accept loop has to stay free to receive the second one.
        while True:
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            threading.Thread(target=handle, args=(connection,), daemon=True).start()

    threading.Thread(target=endpoint, daemon=True).start()
    tui.pump(1.5)

    tui.send("a question that will not be answered\r", settle=0.6)
    started = time.time()
    tui.send("\x03", 0.5)  # Ctrl-C, once
    tui.send("a question that will\r", settle=0.3)

    landed = second_request.wait(timeout=6.0)
    took = time.time() - started

    exit_code = tui.close()
    results = [
        check("one Ctrl-C frees the agent to work again", landed, f"{took:.2f}s"),
        check("it lands in a moment, not at the deadline", landed and took < 3, f"{took:.2f}s"),
        check("the endpoint saw exactly two chats", state["chats"] == 2, str(state["chats"])),
        check("clean exit", exit_code == 0, f"exit {exit_code}"),
    ]
    if not all(results):
        print(tui.tail())
    return all(results)


def scenario_sigterm(binary: str, root: pathlib.Path) -> bool:
    """SIGTERM must take the same road as a confirmed Ctrl-Q.

    A killed mush used to run no destructor at all: the last messages of the
    session were lost with the debounce, every process group it started stayed
    alive — the build holding `target/`, the server holding a port — and
    `.mush/mush.sock`, which is removed by exactly one `Drop`, survived as the
    proof that *nothing* was cleaned up (finding E1).

    The job is a real detached `sleep 600; touch marker`; the model is a fake
    endpoint that asks for it and then answers with a sentence the debounce (a
    minute) has not written yet. `kill -TERM` on the mush pid, and all four
    facts are asserted: the job's group is gone within a bounded wait, the
    marker was never created, the sentence is in `session.json`, and the socket
    is gone.
    """
    print(f"\n== sigterm == {root}")
    marker = root / "marker"
    message = "the answer the debounce had not written"
    command = f"sleep 600; touch {marker}"

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(8)
    port = listener.getsockname()[1]
    state = {"chats": 0}

    env = {
        "MUSH_URL": f"http://127.0.0.1:{port}",
        "MUSH_PROVIDER": "custom",
        "MUSH_MODEL": "probe",
    }
    # Fork the TUI before any thread exists, as the cancel scenario explains.
    tui = Tui(binary, root, rows=30, cols=110, env_extra=env)

    def reply(connection, payload: bytes):
        connection.sendall(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            b"Content-Length: " + str(len(payload)).encode() + b"\r\n\r\n" + payload
        )
        connection.close()

    def handle(connection):
        request = connection.recv(65536)
        if b"/models" in request:
            reply(connection, b'{"object":"list","data":[{"id":"probe"}]}')
            return
        state["chats"] += 1
        if state["chats"] == 1:
            arguments = json.dumps({"command": command, "detach": True})
            reply(
                connection,
                b'{"choices":[{"message":{"role":"assistant","content":null,'
                b'"tool_calls":[{"id":"call_1","type":"function","function":'
                b'{"name":"run_command","arguments":'
                + json.dumps(arguments).encode()
                + b"}}]},\"finish_reason\":\"tool_calls\"}]}",
            )
        else:
            reply(
                connection,
                b'{"choices":[{"message":{"role":"assistant","content":'
                + json.dumps(message).encode()
                + b'},"finish_reason":"stop"}]}',
            )

    def endpoint():
        while True:
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            threading.Thread(target=handle, args=(connection,), daemon=True).start()

    threading.Thread(target=endpoint, daemon=True).start()
    tui.pump(1.5)
    tui.send("start the long job\r", settle=0.5)

    # The job exists once `sh -c <command>` is running; it is the group leader,
    # so its pid is the pgid whose end the signal must bring.
    job_pgid = None
    deadline = time.time() + 20
    while time.time() < deadline and job_pgid is None:
        tui.pump(0.3)
        job_pgid = find_job_group(command)

    # The second chat means the tool result is in the transcript and the fake
    # model is answering; the sentence on screen means it is in the conversation.
    deadline = time.time() + 20
    while state["chats"] < 2 and time.time() < deadline:
        tui.pump(0.2)
    deadline = time.time() + 20
    while message.encode() not in bytes(tui.captured) and time.time() < deadline:
        tui.pump(0.2)

    session = root / ".mush" / "session.json"
    socket_path = root / ".mush" / "mush.sock"
    stored_before = session.read_text() if session.exists() else ""
    socket_before = socket_path.exists()

    os.kill(tui.proc.pid, signal.SIGTERM)
    try:
        exit_code = tui.proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        tui.proc.kill()
        exit_code = None

    # A bounded wait for the group: the kill of every job happens in `Drop`,
    # which is before the process is gone, so this is a formality — and the
    # thing the finding was about.
    deadline = time.time() + 5
    while time.time() < deadline and group_alive(job_pgid):
        time.sleep(0.1)

    stored = session.read_text() if session.exists() else ""
    # The killed mush's own scratch pair: whether its exit removed them or not,
    # the next start must not leave a dead mush's pair behind (finding E5 — the
    # name carries the pid, so a start can reap the dead and never a live one).
    leftovers = sorted(pathlib.Path("/tmp").glob(f"mush-cmd-{tui.proc.pid}-*"))
    # No more requests will be answered, and the endpoint thread must be gone
    # before the next fork: forking a process that already has threads is a
    # known CPython deadlock hazard (the cancel scenario's comment).
    listener.close()
    time.sleep(0.1)
    reopened = Tui(binary, root, rows=24, cols=100, env_extra=env)
    reopened.pump(1.5)
    reopened_code = reopened.close()
    still_there = [path for path in leftovers if path.exists()]
    results = [
        check(
            "the debounce had not written the answer yet",
            message not in stored_before,
            f"{len(stored_before)} bytes in session.json before the signal",
        ),
        check("the socket was there to be cleaned up", socket_before),
        check("the signal still exits cleanly", exit_code == 0, f"exit {exit_code}"),
        check(
            "the job's process group is gone",
            not group_alive(job_pgid),
            f"pgid {job_pgid} still holds {group_members(job_pgid)}",
        ),
        check("the marker was never created", not marker.exists()),
        check("the exit flush wrote the answer", message in stored),
        check("the attach socket is gone", not socket_path.exists()),
        check(
            "the next start reaps the killed mush's scratch pair",
            not still_there and reopened_code == 0,
            f"{len(leftovers)} file(s), {len(still_there)} left after a start",
        ),
    ]
    if not all(results):
        print(tui.tail())
    # Reap anything the assertions found alive, so a failure does not leak the
    # very ghost the finding is about.
    if job_pgid is not None and group_alive(job_pgid):
        try:
            os.killpg(job_pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        listener.close()
    except OSError:
        pass
    return all(results)


def find_job_group(command: str) -> int:
    """The pgid of a live `sh -c <command>`, or None while it is not up yet."""
    wanted = "sh -c " + command
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as handle:
                argv = handle.read().replace(b"\0", b" ").decode(errors="replace").strip()
        except OSError:
            continue
        if argv == wanted:
            return int(entry)
    return None


def group_members(pgid: int) -> list:
    """Every live process in one group, as (pid, command)."""
    members = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "rb") as handle:
                fields = handle.read().rsplit(b")", 1)[1].split()
            if int(fields[2]) == pgid:
                with open(f"/proc/{entry}/cmdline", "rb") as handle:
                    members.append(handle.read().replace(b"\0", b" ").decode(errors="replace").strip())
        except (OSError, IndexError, ValueError):
            continue
    return members


def group_alive(pgid: int) -> bool:
    return pgid is not None and bool(group_members(pgid))


def main() -> int:
    parser = argparse.ArgumentParser(description="mush end-to-end smoke tests")
    parser.add_argument("binary", nargs="?", default="target/debug/mush")
    parser.add_argument("workdir", nargs="?", default="/tmp/mush-smoke")
    parser.add_argument("--agent", action="store_true", help="run only the agent scenario")
    parser.add_argument("--resize", action="store_true", help="run only the resize scenario")
    parser.add_argument("--cancel", action="store_true", help="run only the cancel scenario")
    parser.add_argument("--sigterm", action="store_true", help="run only the sigterm scenario")
    parser.add_argument("--lock", action="store_true", help="run only the lock scenario")
    args = parser.parse_args()

    binary = str(pathlib.Path(args.binary).resolve())
    if not pathlib.Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    chosen = [args.agent, args.resize, args.cancel, args.sigterm, args.lock]
    both = not any(chosen)
    base = pathlib.Path(args.workdir)
    passed = True
    if both or args.agent:
        passed &= scenario_agent(binary, base / "agent")
    if both or args.resize:
        passed &= scenario_resize(binary, base / "resize")
    if both or args.cancel:
        passed &= scenario_cancel(binary, base / "cancel")
    if both or args.sigterm:
        passed &= scenario_sigterm(binary, base / "sigterm")
    # Last, and not only because it is cheap: it needs the workspace to itself.
    if both or args.lock:
        passed &= scenario_lock(binary, base / "lock")

    print("\n" + ("all scenarios passed" if passed else "scenario failures"))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())