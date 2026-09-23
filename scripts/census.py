#!/usr/bin/env python3
"""Where the lines are: production code, tests, comments, blanks.

A count without its method is a rumour. This is the method: walk every `.rs`
file under `crates/`, and put every line in exactly one of four columns:

- **blank**: nothing but whitespace;
- **comment**: nothing but comment text — a stripped `//`, `///` or `//!`, or
  a line inside a `/* ... */` block (Rust nests them);
- **tests**: code lines inside a `#[cfg(test)] mod` block — every such block
  in the file, not just the first — and every code line of a test-support
  file (below);
- **prod**: every other code line.

A code line is a non-blank line with at least one character outside a
comment; the insides of strings and char literals are code. The four columns
are a partition:

    total == blank + comment + tests + prod

so `prod` is production code lines, not a subtractive leftover. An earlier
version of this script counted every physical line of a test module as
`tests`, then computed `prod` as `total - blank - comment - tests`: the blank
and comment lines *inside* the test modules were subtracted twice, and
`crates/mush/src/app/mod.rs` — two thirds test — reported 49 production lines
for the file where its `App` lives. The summary's second line prints the
physical span of the `#[cfg(test)] mod` blocks alone, blanks and comments
included, because that is the number the old `tests` column held; both are
kept, and a test-support file's whole length does not join it. Numbers printed
before this fix are not comparable with the ones after it: re-run at the ref.

Test modules are found by a `#[cfg(test)]` attribute followed by a `mod`
declaration, and the block ends at the matching closing code brace. Braces
are counted on code only, through a stateful scan that blanks out comments,
`"..."` strings (escapes and trailing-`\\` continuations included),
`r#"..."#` / `br##"..."##` raw strings of any hash count, and `'{'` /
`b'{'` char literals. That scan earns its size: `app/mod.rs`'s test module
carries format strings and raw JSON, so a brace there is data, not structure,
and a count that reads it as structure ends the module early or never. A
whole-line rule alone would also call a `/* ... */` line production code.

Test support is not only an in-file `#[cfg(test)] mod`. A module *file* is
test support when the gate that brings it in is a *declaration* in its
parent — `#[cfg(<text>)]` immediately above `mod <name>;` — and that text
names `test` (the word: `test`, and the feature spelled `test-support`), or
when the file's own top carries `#![cfg(<text>)]` naming `test`. Reading the
declaration's text, rather than only the one spelling `#[cfg(test)]`, is what
puts
`crates/mush-core/src/scratch.rs` right: `lib.rs` declares it
`#[cfg(any(test, feature = "test-support"))]`, because a `#[cfg(test)]`
module *inside* `mush-core` would be invisible to the test binaries of the
crate that depends on it — the `test-support` feature is the gate the
*consumers'* tests see (the module's own doc says so). The declaration rule
is conservative: the name has to resolve to exactly one file by the module
layout (`lib.rs`, `main.rs`, `mod.rs` and a crate root under `bin/`,
`tests/`, `examples/` or `benches/` look beside themselves; any other module
file looks in the directory named after it), and a text that does not name
`test` leaves the file in prod. When in doubt, a file stays in prod.

The file is read with `splitlines()`: a trailing newline ends the last line
rather than adding one more blank line (the old `split("\\n")` invented a
blank line per file, and inflated `total` and `blank` with it).

Run it from the repository root:

    python3 scripts/census.py            # summary + per-file table
    python3 scripts/census.py --summary  # totals only

Why it exists (`docs/findings.md` §8.5): mush's growth has been mostly harness
and prose, not behaviour, and the per-road tests it bought did not catch the
cross-road class the last wave hunted. The census is how a wave says what it
bought. Re-run it after a wave and write the delta down next to the wave's
rows.
"""

import argparse
import os
import re
import sys

CFG_TEST = re.compile(r"#\[cfg\(test\)\]")
MOD_DECL = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*")
CFG_WORD = re.compile(r"(?<![A-Za-z0-9_])test(?![A-Za-z0-9_])")
MOD_FILE_DECL = re.compile(
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)


def mask(lines):
    """(has_code, masked) for each line.

    `masked` is the line's code with comments and the insides of string and
    char literals blanked, so a brace written in any of those is not a brace.
    `has_code` is true when the line has a character outside a comment;
    string contents are code.
    """
    out = []
    block = 0       # depth of /* ... */ (Rust nests them)
    raw = None      # hash count of an open r#"..."# string
    string = False  # an open "..." (or b"...", c"...") string
    for line in lines:
        code = []
        has_code = False
        i = 0
        while i < len(line):
            ch = line[i]
            if block:
                if line.startswith("/*", i):
                    block += 1
                    i += 2
                elif line.startswith("*/", i):
                    block -= 1
                    i += 2
                else:
                    i += 1
                continue
            if raw is not None:
                has_code = True
                if ch == '"' and line.startswith("#" * raw, i + 1):
                    i += 1 + raw
                    raw = None
                else:
                    i += 1
                continue
            if string:
                has_code = True
                if ch == "\\":
                    i += 2
                elif ch == '"':
                    string = False
                    i += 1
                else:
                    i += 1
                continue
            if line.startswith("//", i):
                break
            if line.startswith("/*", i):
                block = 1
                i += 2
                continue
            if ch == '"':
                hashes = 0
                j = i - 1
                while j >= 0 and line[j] == "#":
                    hashes += 1
                    j -= 1
                if j >= 0 and line[j] == "r":
                    raw = hashes
                else:
                    string = True
                has_code = True
                i += 1
                continue
            if ch == "'":
                has_code = True
                if line.startswith("'\\''", i):
                    i += 4
                elif i + 1 < len(line) and line[i + 1] == "\\":
                    k = i + 2
                    while k < len(line) and line[k] != "'":
                        k += 1
                    i = k + 1
                elif i + 2 < len(line) and line[i + 2] == "'":
                    i += 3
                else:
                    code.append(ch)     # a lifetime's quote
                    i += 1
                continue
            if ch in "{}":
                has_code = True
                code.append(ch)
                i += 1
                continue
            if not ch.isspace():
                has_code = True
            code.append(ch)
            i += 1
        out.append((has_code, "".join(code)))
    return out


def module_end(m, masked, base):
    """Last line of the block whose `mod` declaration is on line `m`.

    `base` is the brace depth before line `m`; the closing brace is the one
    that brings the code-only depth back to it.
    """
    depth = base
    opened = False
    for k in range(m, len(masked)):
        text = masked[k]
        depth += text.count("{") - text.count("}")
        if depth > base:
            opened = True
        elif "{" in text:
            return k        # braces opened and closed on one line
        if opened and depth == base:
            return k
    return len(masked) - 1


def test_ranges(lines, masked, before):
    """[(first, last)] for every `#[cfg(test)] mod` block in the file."""
    texts = [text for _, text in masked]
    ranges = []
    for i, text in enumerate(texts):
        match = CFG_TEST.search(text)
        if not match:
            continue
        m = None
        if MOD_DECL.match(text[match.end() :].lstrip()):
            m = i
        else:
            j = i + 1
            depth = 0
            while j < len(lines):
                nxt = texts[j]
                if depth == 0:
                    if not nxt.strip():
                        j += 1
                        continue
                    if not nxt.strip().startswith("#["):
                        break
                depth += nxt.count("[") - nxt.count("]")
                j += 1
            if j < len(lines) and MOD_DECL.match(texts[j].lstrip()):
                m = j
        if m is None:
            continue
        if "{" in texts[m]:
            end = module_end(m, texts, before[m])
        else:
            k = m + 1
            while k < len(lines) and not texts[k].strip():
                k += 1
            if k < len(lines) and texts[k].strip().startswith("{"):
                end = module_end(k, texts, before[k])
            else:
                end = m        # `mod tests;` — its body is another file
        ranges.append((i, end))
    return ranges


def names_test(text):
    """Whether a cfg's own text names `test` as a word.

    The word in the gate's text, not the attribute spelling: `#[cfg(test)]`
    obviously, and `feature = "test-support"` too — the feature a dependent
    crate's test binaries turn on, which an in-crate `#[cfg(test)]` module
    could not serve. `latest` is not the word.
    """
    return CFG_WORD.search(text) is not None


def module_dir(path):
    """The directory `mod <name>;` inside `path` looks in.

    A crate root — `lib.rs`, `main.rs`, or any file under `bin/`, `tests/`,
    `examples/` or `benches/` — and a `mod.rs` look beside themselves; any
    other module file looks in the directory named after it.
    """
    directory = os.path.dirname(path)
    stem = os.path.basename(path)[:-3]
    if stem in ("lib", "main", "mod") or os.path.basename(directory) in (
        "bin",
        "tests",
        "examples",
        "benches",
    ):
        return directory
    return os.path.join(directory, stem)


def gated_module_decls(lines, before):
    """Names declared as `#[cfg(<text>)] mod <name>;`, `<text>` naming test.

    `before` is each line's brace depth, so only module-level declarations
    are read: an inner `mod` lives under a directory (`tests/`) these rules
    do not describe, and an unresolved gate leaves the file in prod.
    """
    decls = []
    for i, line in enumerate(lines):
        if before[i]:
            continue
        match = MOD_FILE_DECL.match(line)
        if not match:
            continue
        attributes = []
        j = i - 1
        while j >= 0 and lines[j].lstrip().startswith("#["):
            attributes.append(lines[j])
            j -= 1
        text = " ".join(reversed(attributes))
        if "#[cfg(" in text and names_test(text):
            decls.append(match.group(1))
    return decls


def inner_test_gate(lines):
    """Whether the file's own top carries `#![cfg(...)]` naming `test`."""
    for line in lines:
        text = line.strip()
        if not text or text.startswith("//"):
            continue
        if not text.startswith("#!["):
            return False
        if text.startswith("#![cfg(") and names_test(text):
            return True
    return False


def test_support_files(entries):
    """The paths the tree declares for test builds only.

    A file is test support when its own top carries `#![cfg(...)]` naming
    `test`, or when a module-level `#[cfg(...)] mod <name>;` in its parent
    names `test` and the name resolves to exactly that one file. A gate that
    resolves to no file or to two is left alone: when in doubt, the file
    stays in prod.
    """
    known = {os.path.normpath(path) for path, _ in entries}
    support = set()
    for path, lines in entries:
        if inner_test_gate(lines):
            support.add(os.path.normpath(path))
        masked = mask(lines)
        before = []
        depth = 0
        for _, text in masked:
            before.append(depth)
            depth += text.count("{") - text.count("}")
        directory = module_dir(path)
        for name in gated_module_decls(lines, before):
            candidates = [
                os.path.normpath(os.path.join(directory, name + ".rs")),
                os.path.normpath(os.path.join(directory, name, "mod.rs")),
            ]
            present = [candidate for candidate in candidates if candidate in known]
            if len(present) == 1:
                support.add(present[0])
    return support


def split(lines, test_support=False):
    """(total, blank, comment, tests, prod, test_lines) for one file.

    `tests` and `prod` are code lines; `test_lines` is the physical span of
    the `#[cfg(test)] mod` blocks, blanks and comments included — the old
    `tests` column, which a test-support file does not grow. A test-support
    file's code lines are all tests, and it has no prod lines.
    """
    masked = mask(lines)
    total = len(lines)
    blank = sum(1 for line in lines if not line.strip())
    code = []
    comment = 0
    for line, (has_code, _) in zip(lines, masked):
        if line.strip() and has_code:
            code.append(True)
        else:
            code.append(False)
            if line.strip():
                comment += 1
    before = []
    depth = 0
    for _, text in masked:
        before.append(depth)
        depth += text.count("{") - text.count("}")
    in_test = [False] * total
    for first, last in test_ranges(lines, masked, before):
        for k in range(first, last + 1):
            in_test[k] = True
    span = sum(in_test)
    if test_support:
        in_test = [True] * total
    tests = sum(1 for i in range(total) if in_test[i] and code[i])
    prod = sum(1 for i in range(total) if not in_test[i] and code[i])
    return total, blank, comment, tests, prod, span


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", nargs="?", default=".", help="repository root")
    parser.add_argument("--summary", action="store_true", help="totals only")
    args = parser.parse_args()

    entries = []
    for dirpath, _, files in os.walk(os.path.join(args.root, "crates")):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            try:
                with open(path, encoding="utf-8") as handle:
                    lines = handle.read().splitlines()
            except OSError as error:
                print("census: cannot read {}: {}".format(path, error), file=sys.stderr)
                continue
            entries.append((path, lines))

    if not entries:
        print("census: no .rs files under {}/crates".format(args.root))
        return 1

    support = test_support_files(entries)
    rows = []
    for path, lines in entries:
        total, blank, comment, tests, prod, span = split(
            lines, os.path.normpath(path) in support
        )
        rows.append(
            (os.path.relpath(path, args.root), total, blank, comment, tests, prod, span)
        )

    rows.sort(key=lambda row: -row[1])
    total = sum(row[1] for row in rows)
    blank = sum(row[2] for row in rows)
    comment = sum(row[3] for row in rows)
    tests = sum(row[4] for row in rows)
    prod = sum(row[5] for row in rows)
    span = sum(row[6] for row in rows)
    print(
        "TOTAL {}  blank {}  comment {}  tests {}  prod {}".format(
            total, blank, comment, tests, prod
        )
    )
    print(
        "test modules span {} lines (blanks and comments inside included)".format(span)
    )
    if args.summary:
        return 0
    print()
    print("{:44} {:>6} {:>6} {:>6} {:>6} {:>6}".format("file", "total", "blank", "cmt", "tests", "prod"))
    for path, t, b, c, x, p, _ in rows:
        print("{:44} {:6} {:6} {:6} {:6} {:6}".format(path, t, b, c, x, p))
    return 0


if __name__ == "__main__":
    sys.exit(main())
