#!/usr/bin/env python3
"""End-to-end smoke tests for mush.

These drive the *real* binary through a pseudo-terminal: they send keystrokes,
wait for the configured model to call tools, and assert on files on disk. That
makes them the only test that covers the whole path — keys, agent loop, tool
round-trip through the UI thread, atomic writes, and session persistence.

Usage:
    python3 scripts/smoke.py [BINARY] [WORKDIR] [--agent|--editor]

Defaults to ./target/debug/mush and a fresh directory under /tmp.
Requires a reachable model endpoint (see the MUSH_URL / MUSH_MODEL variables).
"""

import argparse
import fcntl
import os
import pathlib
import pty
import select
import struct
import subprocess
import sys
import termios
import time


class Tui:
    """A mush process attached to a pseudo-terminal."""

    def __init__(self, binary: str, workspace: pathlib.Path, rows: int = 34, cols: int = 110):
        self.workspace = workspace
        self.workspace.mkdir(parents=True, exist_ok=True)
        self.captured = bytearray()

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.master = master
        self.proc = subprocess.Popen(
            [binary, str(self.workspace)],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=dict(os.environ),
            close_fds=True,
        )
        os.close(slave)

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
    args = parser.parse_args()

    binary = str(pathlib.Path(args.binary).resolve())
    if not pathlib.Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    both = not (args.agent or args.editor)
    base = pathlib.Path(args.workdir)
    passed = True
    if both or args.agent:
        passed &= scenario_agent(binary, base / "agent")
    if both or args.editor:
        passed &= scenario_editor(binary, base / "editor")

    print("\n" + ("all scenarios passed" if passed else "scenario failures"))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())