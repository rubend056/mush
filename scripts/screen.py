#!/usr/bin/env python3
"""Print what mush looks like, as plain text, at a range of terminal sizes.

The UI audit (docs/mush.md §4.5) is easy to redo and easy to skip; this makes it
one command. It drives the real binary over a pty — the pty is the controlling
terminal, exactly as a shell would give it — and renders the screen mush painted
as text, so it can be read (or pasted into an issue) without a screenshot.

Usage:
    python3 scripts/screen.py [BINARY] [WORKDIR]
        [--sizes 200x50,120x32,80x24,60x17,40x10,30x8]
        [--keys STR] [--keys-after STR] [--ask STR] [--settle SECONDS]
        [--url URL] [--model NAME]

`--keys` sends keystrokes after the first paint (`\\t` is Tab, `\\x1b` Escape).
`--keys-after` sends them after `--ask` has settled and between the screens, so
each size shows the pane where the batch before it left it: `\\e[5~` is PageUp,
and a call block taller than one pane is walked a screen at a time.
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

CSI = re.compile(r"\x1b\[([\x20-\x3f]*)([@-~])", re.S)
# An escape sequence that has not completed after this many characters is
# treated as garbage and dropped, so one odd byte cannot stall rendering.
MAX_PENDING = 64

# The escapes `--keys` understands. `unicode_escape` used to decode this
# argument, which is the wrong tool twice over: it turns a literal `é` into the
# two latin-1 characters `Ã©`, and it leaves `\e` (the usual way to write ESC on
# a command line) as a literal backslash.
KEY_ESCAPES = {
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "a": "\a",
    "b": "\b",
    "f": "\f",
    "v": "\v",
    "e": "\x1b",
    "0": "\0",
    "\\": "\\",
    "'": "'",
    '"': '"',
}
KEY_ESCAPE = re.compile(r"\\(x[0-9a-fA-F]{2}|u[0-9a-fA-F]{4}|.)", re.S)


def decode_keys(text: str) -> str:
    """Decode the escapes in `--keys`, and nothing else.

    A literal non-ASCII character is left alone: the bytes sent to the pty are
    UTF-8, and the terminal is promised those bytes."""

    def one(match: "re.Match") -> str:
        body = match.group(1)
        if body[0] in "xu" and len(body) > 1:
            return chr(int(body[1:], 16))
        return KEY_ESCAPES.get(body, body)

    return KEY_ESCAPE.sub(one, text)


class Screen:
    """Just enough terminal to render ratatui: a grid, a cursor, and the
    control sequences it actually emits (no colours — this review is about
    what is on the screen, not what it looks like)."""

    def __init__(self, cols: int, rows: int):
        self.x = self.y = 0
        self.resize(cols, rows)
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def resize(self, cols: int, rows: int):
        old = getattr(self, "grid", None)
        self.cols, self.rows = cols, rows
        self.grid = [[" "] * cols for _ in range(rows)]
        if old:
            for y, row in enumerate(old[:rows]):
                self.grid[y][: len(row[:cols])] = row[:cols]
        self.clamp()

    def clamp(self) -> None:
        """Keep the cursor on the grid.

        A terminal can be resized smaller under a cursor that was already low,
        and a program can name a row past the bottom (\x1b[999;1H). Both used to
        leave x/y out of range and the next erase or write raised IndexError
        instead of painting."""
        self.x = min(max(self.x, 0), max(self.cols - 1, 0))
        self.y = min(max(self.y, 0), max(self.rows - 1, 0))

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
                        if len(rest) > MAX_PENDING:
                            # Unparsable: drop the escape and render the rest.
                            index += 1
                            continue
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
        numbers = [int(p) for p in re.findall(r"\d+", params)]
        # J and K default to 0 (cursor to end); the rest default to 1.
        first = numbers[0] if numbers else (0 if final in "JK" else 1)
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
                if first == 2:
                    start, end = 0, self.cols
                elif first == 1:
                    start, end = 0, min(self.cols, self.x + 1)
                else:
                    start, end = min(self.x, self.cols), self.cols
                for x in range(start, end):
                    row[x] = " "
        elif final == "X":
            if 0 <= self.y < self.rows:
                for x in range(self.x, min(self.cols, self.x + first)):
                    self.grid[self.y][x] = " "

        # A terminal clamps the cursor to the screen, whatever a program asks
        # for: `\x1b[999;1H` puts it on the last row. Without this the next
        # character was silently dropped by the writer's bounds check, so the
        # review tool hid a whole paint instead of showing it on the bottom row.
        self.clamp()

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
    parser.add_argument(
        "--keys-after",
        default="",
        help="keystrokes sent after --ask settles, between the printed screens",
    )
    parser.add_argument("--ask", default="", help="a message to send (needs a model)")
    parser.add_argument("--settle", type=float, default=1.5, help="seconds per screen")
    parser.add_argument("--url", default="", help="endpoint (default: a closed port)")
    parser.add_argument("--model", default="probe")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="check the screen decoder against escapes a TUI really emits",
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()

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

    try:
        keys = decode_keys(args.keys) if args.keys else ""
        keys_after = decode_keys(args.keys_after) if args.keys_after else ""
    except ValueError as exc:
        print(f"bad --keys value {args.keys!r}: {exc}", file=sys.stderr)
        return 2

    cols, rows = sizes[0]
    tui = Tui(binary, args.workdir, cols, rows, env)
    try:
        tui.pump(args.settle)
        if keys:
            tui.send(keys)
        if args.ask:
            tui.send(args.ask + "\r", settle=args.settle)
        for index, (cols, rows) in enumerate(sizes):
            if index:
                tui.resize(cols, rows)
            print(f"\n===== {cols}x{rows} =====")
            print(tui.screen.text())
            if keys_after:
                # Between screens, not once: the printed screen is the pane
                # where the last batch left it, so a call block taller than
                # one pane is walked (PgUp) one screen per size.
                tui.send(keys_after, settle=0.6)
    finally:
        tui.close()
    return 0


def self_test() -> int:
    """The decoder's own regressions: no binary, no pty, no model.

    What this catches is not what ratatui normally emits but what it can be made
    to emit by a smaller terminal or an odd keystroke, which is exactly how the
    decoder was wrong before: an `X` (erase characters) after a cursor that had
    been pushed off the grid raised instead of painting.
    """
    failures = 0

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        failures += 0 if ok else 1
        print(f"{'PASS' if ok else 'FAIL'}  {name}{(' — ' + detail) if detail else ''}")

    def painted(seq: bytes, cols: int = 20, rows: int = 5) -> "Screen":
        screen = Screen(cols, rows)
        screen.feed(seq)
        return screen

    screen = painted(b"\x1b[2J\x1b[1;1Ha\x1b[1;3Hc")
    check("cursor addressing paints where it says", screen.text().splitlines()[0] == "a c")

    screen = painted(b"\x1b[999;1Hx")
    check(
        "a row past the bottom is clamped onto the last one",
        screen.text().splitlines()[-1] == "x",
        repr(screen.text()),
    )

    screen = Screen(20, 20)
    screen.feed(b"\x1b[15;5H")
    screen.resize(20, 5)
    screen.feed(b"x")
    check(
        "a shrink under a low cursor paints on the last row",
        screen.text().splitlines()[-1].strip() == "x",
        repr(screen.text()),
    )

    screen = painted(b"\x1b[2;3H\x1b[Kfilled")
    lines = screen.text().splitlines()
    check("erase-to-end keeps the columns before it", lines[1] == "  filled")

    screen = painted(b"one\x1b]0;title\x07two")
    check("OSC is swallowed to its terminator", screen.text().splitlines()[0] == "onetwo")

    check("a literal non-ascii key survives", decode_keys("é") == "é")
    check("an escape decodes", decode_keys("\\e[B") == "\x1b[B")
    check("a hex escape decodes", decode_keys("\\x1bq") == "\x1bq")
    check("tab and enter decode", decode_keys("\\t") == "\t" and decode_keys("\\r") == "\r")
    check("an unknown escape is its own letter", decode_keys("\\q") == "q")

    print("all decoder checks passed" if not failures else f"{failures} check(s) failed")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
