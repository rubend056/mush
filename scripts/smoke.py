#!/usr/bin/env python3
"""End-to-end smoke tests for mush.

These drive the *real* binary through a pseudo-terminal: they send keystrokes,
wait for the configured model to call tools, and assert on files on disk. That
makes them the only test that covers the whole path — keys, agent loop, tool
round-trip through the UI thread, atomic writes, and session persistence.

Usage:
    python3 scripts/smoke.py [BINARY] [WORKDIR] [--agent|--editor|--resize]

The resize scenario needs no model endpoint; the others do.

Defaults to ./target/debug/mush and a fresh directory under /tmp.
Requires a reachable model endpoint (see the MUSH_URL / MUSH_MODEL variables).
"""

import argparse
import fcntl
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
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
        self.send("\x11", 0.4)  # Ctrl-Q
        self.send("\x11", 0.4)  # again, in case a buffer is dirty
        self.pump(1.5)
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


def scenario_editor(binary: str, root: pathlib.Path) -> bool:
    """The human edits and saves; then the agent edits the same live buffer."""
    print(f"\n== editor == {root}")
    (root).mkdir(parents=True, exist_ok=True)
    (root / "notes.txt").write_text("hello\n")

    tui = Tui(binary, root)
    tui.pump(2.0)

    # Open notes.txt via the /open command (the file explorer is gone), type a
    # line above, save.
    tui.send("/open notes.txt\r", 0.8)
    tui.pump(0.5)
    tui.send("i")                  # insert mode
    tui.send("new line")
    tui.send("\r")                 # newline
    tui.send("\x1b", 0.5)          # normal mode
    tui.send("\x13", 0.8)          # Ctrl-S save

    after_human = (root / "notes.txt").read_text()

    # Editor -> Chat, ask the agent to edit the open buffer.
    tui.send("\t", 0.4)
    tui.send("in notes.txt replace the word hello with world and change nothing else\r")
    final = tui.wait_for(root / "notes.txt", lambda t: "world" in t)
    exit_code = tui.close()

    results = [
        check("human edit saved", after_human == "new line\nhello\n", repr(after_human)),
        check("agent edited the live buffer", "world" in final, repr(final)),
        check("agent preserved the human's edit", "new line" in final, repr(final)),
        check("clean exit", exit_code == 0, f"exit {exit_code}"),
    ]
    if not all(results):
        print(tui.tail())
    return all(results)


def main() -> int:
    parser = argparse.ArgumentParser(description="mush end-to-end smoke tests")
    parser.add_argument("binary", nargs="?", default="target/debug/mush")
    parser.add_argument("workdir", nargs="?", default="/tmp/mush-smoke")
    parser.add_argument("--agent", action="store_true", help="run only the agent scenario")
    parser.add_argument("--editor", action="store_true", help="run only the editor scenario")
    parser.add_argument("--resize", action="store_true", help="run only the resize scenario")
    args = parser.parse_args()

    binary = str(pathlib.Path(args.binary).resolve())
    if not pathlib.Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    both = not (args.agent or args.editor or args.resize)
    base = pathlib.Path(args.workdir)
    passed = True
    if both or args.agent:
        passed &= scenario_agent(binary, base / "agent")
    if both or args.editor:
        passed &= scenario_editor(binary, base / "editor")
    if both or args.resize:
        passed &= scenario_resize(binary, base / "resize")

    print("\n" + ("all scenarios passed" if passed else "scenario failures"))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())