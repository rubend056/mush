#!/usr/bin/env python3
"""Where the lines are: production code, tests, comments, blanks.

A count without its method is a rumour. This is the method: walk every `.rs`
file under `crates/`, and split each into

- **blank** lines,
- **comment** lines (a stripped line starting with `//`) — note that this
  includes the comments *inside* test modules, so the columns overlap,
- **test** lines: from the first `#[cfg(test)]` attribute followed by a `mod`,
  to that module's closing brace (brace-counted, so a raw string containing a
  brace can skew it by a line or two),
- **prod** code: what is left.

Run it from the repository root:

    python3 scripts/census.py            # summary + per-file table
    python3 scripts/census.py --summary  # totals only

Why it exists (`docs/findings.md` §8.5): mush's growth has been mostly harness
and prose, not behaviour, and the per-road tests it bought did not catch the
cross-road class the last wave hunted. The census is how a wave says what it
bought. Re-run it after a wave and write the delta down next to the wave's
rows; the numbers in §8.5 are the ones this script printed at `f70374f`.
"""

import argparse
import os
import re
import sys

CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")
MOD_DECL = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+\w+")


def split(lines):
    """(total, blank, comment, tests) for one file's lines."""
    total = len(lines)
    blank = sum(1 for line in lines if not line.strip())
    comment = sum(1 for line in lines if line.strip().startswith("//"))
    start = None
    for i, line in enumerate(lines):
        if not CFG_TEST.match(line):
            continue
        j = i + 1
        while j < len(lines) and lines[j].strip().startswith("#["):
            j += 1
        if j < len(lines) and MOD_DECL.match(lines[j]):
            start = i
            break
    tests = 0
    if start is not None:
        depth = 0
        opened = False
        for line in lines[start:]:
            depth += line.count("{") - line.count("}")
            if "{" in line:
                opened = True
            if opened:
                tests += 1
                if depth <= 0:
                    break
    return total, blank, comment, tests


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", nargs="?", default=".", help="repository root")
    parser.add_argument("--summary", action="store_true", help="totals only")
    args = parser.parse_args()

    rows = []
    for dirpath, _, files in os.walk(os.path.join(args.root, "crates")):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            try:
                with open(path, encoding="utf-8") as handle:
                    lines = handle.read().split("\n")
            except OSError as error:
                print("census: cannot read {}: {}".format(path, error), file=sys.stderr)
                continue
            total, blank, comment, tests = split(lines)
            rows.append((os.path.relpath(path, args.root), total, blank, comment, tests))

    if not rows:
        print("census: no .rs files under {}/crates".format(args.root))
        return 1

    rows.sort(key=lambda row: -row[1])
    total = sum(row[1] for row in rows)
    blank = sum(row[2] for row in rows)
    comment = sum(row[3] for row in rows)
    tests = sum(row[4] for row in rows)
    prod = total - blank - comment - tests
    print(
        "TOTAL {}  blank {}  comment {}  tests {}  prod {}".format(
            total, blank, comment, tests, prod
        )
    )
    if args.summary:
        return 0
    print()
    print("{:44} {:>6} {:>6} {:>6} {:>6} {:>6}".format("file", "total", "blank", "cmt", "tests", "prod"))
    for path, t, b, c, x in rows:
        print("{:44} {:6} {:6} {:6} {:6} {:6}".format(path, t, b, c, x, t - b - c - x))
    return 0


if __name__ == "__main__":
    sys.exit(main())
