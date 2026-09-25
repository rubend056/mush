#!/usr/bin/env python3
"""Print what mush looks like, as plain text, at a range of terminal sizes.

The UI audit (docs/mush.md §4.5) is easy to redo and easy to skip; this makes it
one command. It drives the real binary over a pty — the pty is the controlling
terminal, exactly as a shell would give it — and renders the screen mush painted
as text, so it can be read (or pasted into an issue) without a screenshot.

A cell is a column, and a glyph is as wide as a terminal makes it: the standard
library's `unicodedata` is the East-Asian-Width table, so a Wide or Fullwidth
glyph takes two cells and everything else one — an Ambiguous glyph also one,
which is `unicode-width`'s default and what mush's grid assumes; painting such
a glyph wide is the terminal's own locale policy, held in
`docs/audits/paint-and-measure.md`. A combining mark takes no cell at all: it
joins the cell it follows. A wide glyph with only the row's last column left
wraps to the next row, as a terminal wraps it, rather than overrunning the row
or leaving half a glyph in it.

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
import unicodedata

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


def glyph_width(char: str) -> int:
    """The columns a terminal advances for one code point.

    The table is the standard library's own East-Asian-Width table — the one a
    terminal's `wcwidth(3)` reaches for: Wide and Fullwidth take two columns,
    everything else takes one — Ambiguous included, which is `unicode-width`'s
    default and the answer mush's own grid is built on. A combining mark takes
    none, because it attaches to the cell it follows.
    """
    if unicodedata.combining(char):
        return 0
    if unicodedata.east_asian_width(char) in ("W", "F"):
        return 2
    return 1


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
        # Where the cursor last wrote a base glyph — its first cell, for a
        # wide one. A combining mark joins that cell; a cursor move leaves it
        # behind, so it is forgotten then (and here, where the grid moved).
        self.last = None

    def blank(self, x: int, y: int) -> None:
        """Blank one cell, and a wide glyph's other half with it.

        A glyph is one unit to a terminal: writing into or erasing either of a
        wide glyph's two columns takes the glyph away, because half a glyph is
        not something a terminal can show. The empty string stands in a cell
        for the right half of the wide glyph written in the cell before it.
        """
        if not (0 <= x < self.cols and 0 <= y < self.rows):
            return
        if self.grid[y][x] == "" and x > 0:
            self.grid[y][x - 1] = " "
        if x + 1 < self.cols and self.grid[y][x + 1] == "":
            self.grid[y][x + 1] = " "
        self.grid[y][x] = " "

    def paint(self, x: int, y: int, char: str, width: int) -> None:
        """Write one base glyph at (x, y); a wide one takes the next cell too."""
        if not (0 <= x < self.cols and 0 <= y < self.rows):
            return
        self.blank(x, y)
        self.grid[y][x] = char
        self.last = (x, y)
        if width == 2 and x + 1 < self.cols:
            # The second cell may hold half of another wide glyph, whose own
            # first cell is one further right; blanking it takes that glyph
            # whole, after which the cell becomes this glyph's second half.
            self.blank(x + 1, y)
            self.grid[y][x + 1] = ""

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
                self.x, self.last = 0, None
                index += 1
            elif char == "\n":
                self.y = min(self.y + 1, self.rows - 1)
                self.last = None
                index += 1
            elif char == "\b":
                self.x, self.last = max(0, self.x - 1), None
                index += 1
            elif char == "\t":
                self.x = min(self.cols - 1, (self.x // 8 + 1) * 8)
                self.last = None
                index += 1
            elif char < " ":
                index += 1
            else:
                width = glyph_width(char)
                if width == 0:
                    if self.last is not None:
                        lx, ly = self.last
                        self.grid[ly][lx] += char
                else:
                    if width == 2 and self.x + width > self.cols:
                        # The wide glyph has only the row's last column left:
                        # a terminal wraps it to the next row's first two
                        # cells instead of splitting it, and the cell it could
                        # not fit in keeps what it had. At the bottom row
                        # there is nowhere to wrap to (this grid has no
                        # scrollback), so it lands on that row's own first
                        # cells — the same clamp a newline takes.
                        self.x = 0
                        self.y = min(self.y + 1, self.rows - 1)
                    self.paint(self.x, self.y, char, width)
                    self.x += width
                    if self.x >= self.cols:
                        self.x, self.y = 0, min(self.y + 1, self.rows - 1)
                index += 1
        self.pending = ""

    def control(self, params: str, final: str) -> None:
        if final in "HfABCDGdJKX":
            # These move the cursor or erase cells, so the cell a combining
            # mark would have joined is no longer the one it follows.
            self.last = None
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
                    self.blank(x, self.y)
        elif final == "X":
            if 0 <= self.y < self.rows:
                for x in range(self.x, min(self.cols, self.x + first)):
                    self.blank(x, self.y)

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
        help="check the screen decoder and its width arithmetic",
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
    been pushed off the grid raised instead of painting. The widths are the
    other half of the same question — a terminal measures a glyph by its
    East-Asian width and a combining mark by nothing at all, and the emulator
    must agree or a smoke assertion reads a column that is not on the glass.
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

    # Width is what a terminal measures, not one column per code point. The
    # table is `unicodedata`'s, in the three answers a TUI meets: Wide and
    # Fullwidth take two columns, Ambiguous one (`unicode-width`'s default and
    # mush's), and a combining mark none at all.
    check(
        "Wide and Fullwidth are two columns, Ambiguous and the rest one",
        glyph_width("日") == 2
        and glyph_width("☷") == 2
        and glyph_width("Ａ") == 2
        and unicodedata.east_asian_width("▦") == "A"
        and glyph_width("▦") == 1
        and glyph_width("a") == 1,
    )
    check("a combining mark is no column", glyph_width("\u0301") == 0)

    # The class this pins: `☷` (U+2637, TRIGRAM FOR EARTH) is East-Asian-Wide,
    # so a terminal gives it two cells, and the text after it stands a column
    # right of where a one-column-per-code-point emulator put it. The glyph is
    # deliberately *not* one of mush's marks any more — commit 332f7e1 dropped
    # it from `outline` for this very width; it is here as a fixture.
    screen = painted(b"\x1b[2J\x1b[1;1H\xe2\x98\xb7 15 defs")
    check(
        "text after a wide glyph lands in the terminal's column",
        screen.grid[0][0] == "☷"
        and screen.grid[0][1] == ""
        and screen.grid[0][3] == "1"
        and screen.text().splitlines()[0] == "☷ 15 defs",
        repr(screen.text().splitlines()[0]),
    )

    screen = Screen(5, 2)
    screen.feed("☷abcd".encode())
    lines = screen.text().splitlines()
    check(
        "a wide glyph's two cells move the row's wrap one column left",
        lines[0] == "☷abc" and lines[1] == "d",
        repr(lines),
    )

    # A wide glyph with only the row's last column left cannot fit: it wraps
    # to the next row, where a terminal puts it, and the cell it could not fit
    # in keeps what it had. The cursor never steps past the row's end.
    screen = Screen(4, 2)
    screen.feed(b"\x1b[1;4H\xe2\x98\xb7x")
    lines = screen.text().splitlines()
    check(
        "a wide glyph in the last column wraps to the next row",
        lines[0] == "" and lines[1] == "☷x" and (screen.x, screen.y) == (3, 1),
        repr(lines) + f"; cursor {screen.x},{screen.y}",
    )

    screen = Screen(5, 2)
    screen.feed("e\u0301x".encode())
    check(
        "a combining mark joins the cell before it instead of advancing",
        screen.grid[0][0] == "e\u0301" and screen.grid[0][1] == "x" and screen.x == 2,
        repr(screen.text().splitlines()[0]),
    )

    # A write into either half of a wide glyph takes the glyph whole: half a
    # glyph is not a screen a terminal can show.
    screen = Screen(4, 1)
    screen.feed("☷\x1b[1;2Hz".encode())
    check(
        "a write into a wide glyph's tail clears the glyph whole",
        screen.text().splitlines()[0] == " z",
        repr(screen.text().splitlines()[0]),
    )

    check("a literal non-ascii key survives", decode_keys("é") == "é")
    check("an escape decodes", decode_keys("\\e[B") == "\x1b[B")
    check("a hex escape decodes", decode_keys("\\x1bq") == "\x1bq")
    check("tab and enter decode", decode_keys("\\t") == "\t" and decode_keys("\\r") == "\r")
    check("an unknown escape is its own letter", decode_keys("\\q") == "q")

    print("all screen checks passed" if not failures else f"{failures} check(s) failed")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
