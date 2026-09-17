#!/usr/bin/env python3
"""Print what mush looks like, as plain text, at a range of terminal sizes.

The UI audit (docs/mush.md §4.5) is easy to redo and easy to skip; this makes it
one command. It drives the real binary over a pty — the pty is the controlling
terminal, exactly as a shell would give it — and renders the screen mush painted
as text, so it can be read (or pasted into an issue) without a screenshot.

Usage:
    python3 scripts/screen.py [BINARY] [WORKDIR]
        [--sizes 200x50,120x32,80x24,60x17,40x10,30x8]
        [--keys STR] [--ask STR] [--settle SECONDS]
        [--url URL] [--model NAME]

`--keys` sends keystrokes after the first paint (`\\t` is Tab, `\\x1b` Escape).
`--ask` types a message and waits, which needs a reachable model; without it the
screens need no endpoint at all (mush is pointed at a closed port).
"""

import argparse
import codecs
import fcntl
import os
import pathlib
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

CSI = re.compile(r"\x1b\[([0-9;?]*)([@-~])", re.S)


class Screen:
    """Just enough terminal to render ratatui: a grid, a cursor, and the
    control sequences it actually emits (no colours — this review is about
    what is on the screen, not what it looks like)."""

    def __init__(self, cols: int, rows: int):
        self.resize(cols, rows)
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def resize(self, cols: int, rows: int):
        old = getattr(self, "grid", None)
        self.cols, self.rows = cols, rows
        self.grid = [[" "] * cols for _ in range(rows)]
        if old:
            for y, row in enumerate(old[:rows]):
                self.grid[y][: len(row[:cols])] = row[:cols]

    def feed(self, chunk: bytes) -> None:
        text = self.pending + self.decoder.decode(chunk)
        index = 0
        while index < len(text):
            char = text[index]
            if char == "\x1b":
                rest = text[index:]
                if rest.startswith("\x1b["):
                    match = CSI.match(rest)
                    if not match:
                        self.pending = rest
                        return
                    self.control(match.group(1), match.group(2))
                    index += match.end()
                elif rest.startswith("\x1b]"):
                    match = re.match(r"\x1b\].*?(\x07|\x1b\\)", rest, re.S)
                    if not match:
                        self.pending = rest
                        return
                    index += match.end()
                else:
                    index += 2
            elif char == "\r":
                self.x = 0
                index += 1
            elif char == "\n":
                self.y = min(self.y + 1, self.rows - 1)
                index += 1
            elif char == "\b":
                self.x = max(0, self.x - 1)
                index += 1
            elif char == "\t":
                self.x = min(self.cols - 1, (self.x // 8 + 1) * 8)
                index += 1
            elif char < " ":
                index += 1
            else:
                if 0 <= self.x < self.cols and 0 <= self.y < self.rows:
                    self.grid[self.y][self.x] = char
                self.x += 1
                if self.x >= self.cols:
                    self.x, self.y = 0, min(self.y + 1, self.rows - 1)
                index += 1
        self.pending = ""

    def control(self, params: str, final: str) -> None:
        numbers = [int(p) for p in params.replace("?", "").split(";") if p.isdigit()]
        first = numbers[0] if numbers else 1
        second = numbers[1] if len(numbers) > 1 else 1
        if final in "Hf":
            self.y, self.x = max(0, first - 1), max(0, second - 1)
        elif final == "A":
            self.y = max(0, self.y - first)
        elif final == "B":
            self.y = min(self.rows - 1, self.y + first)
        elif final == "C":
            self.x = min(self.cols - 1, self.x + first)
        elif final == "D":
            self.x = max(0, self.x - first)
        elif final == "G":
            self.x = max(0, first - 1)
        elif final == "d":
            self.y = max(0, first - 1)
        elif final == "J":
            rows = range(self.rows) if first == 2 else range(self.y, self.rows)
            for y in rows:
                self.grid[y] = [" "] * self.cols
        elif final == "K":
            row = self.grid[self.y] if 0 <= self.y < self.rows else None
            if row:
                start = 0 if first == 2 else (self.x if first == 0 else 0)
                end = self.cols if first == 2 else (self.cols if first == 0 else self.x + 1)
                for x in range(start, end):
                    row[x] = " "
        elif final == "X":
            for x in range(self.x, min(self.cols, self.x + first)):
                self.grid[self.y][x] = " "

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.grid)


class Tui:
    """A mush process on a pty, with its painted screen decoded."""

    def __init__(self, binary: str, workspace: str, cols: int, rows: int, env_extra: dict):
        self.workspace = pathlib.Path(workspace)
        self.workspace.mkdir(parents=True, exist_ok=True)
        self.screen = Screen(cols, rows)

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(os.environ, **(env_extra or {}))
        pid = os.fork()
        if pid == 0:
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
        self.master, self.pid = master, pid

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
            self.screen.feed(chunk)

    def send(self, text: str, settle: float = 0.3) -> None:
        os.write(self.master, text.encode())
        self.pump(settle)

    def resize(self, cols: int, rows: int) -> None:
        self.screen.resize(cols, rows)
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)
        self.pump(0.6)

    def close(self) -> None:
        self.send("\x11", 0.3)
        self.send("\x11", 0.4)
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass


def main() -> int:
    parser = argparse.ArgumentParser(description="print mush screens as text")
    parser.add_argument("binary", nargs="?", default="target/debug/mush")
    parser.add_argument("workdir", nargs="?", default="/tmp/mush-screen")
    parser.add_argument("--sizes", default="200x50,120x32,80x24,60x17,40x10,30x8")
    parser.add_argument("--keys", default="", help="keystrokes after the first paint")
    parser.add_argument("--ask", default="", help="a message to send (needs a model)")
    parser.add_argument("--settle", type=float, default=1.5, help="seconds per screen")
    parser.add_argument("--url", default="", help="endpoint (default: a closed port)")
    parser.add_argument("--model", default="probe")
    args = parser.parse_args()

    binary = str(pathlib.Path(args.binary).resolve())
    if not pathlib.Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    sizes = []
    for spec in args.sizes.split(","):
        cols, rows = spec.strip().lower().split("x")
        sizes.append((int(cols), int(rows)))

    env = {
        "MUSH_URL": args.url or "http://127.0.0.1:1",
        "MUSH_PROVIDER": "custom",
        "MUSH_MODEL": args.model,
    }
    env.pop("MUSH_API_KEY", None)

    cols, rows = sizes[0]
    tui = Tui(binary, args.workdir, cols, rows, env)
    try:
        tui.pump(args.settle)
        if args.keys:
            tui.send(args.keys)
        if args.ask:
            tui.send(args.ask + "\r", settle=args.settle)
        for index, (cols, rows) in enumerate(sizes):
            if index:
                tui.resize(cols, rows)
            print(f"\n===== {cols}x{rows} =====")
            print(tui.screen.text())
    finally:
        tui.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
