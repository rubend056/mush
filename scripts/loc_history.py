#!/usr/bin/env python3
"""LOC by day, drawn: the census's four series out of git history, as a PNG.

`scripts/census.py` says where the lines are today; this says where they came
from. The count is census's own (`split`, `test_support_files`), imported
rather than restated, so the picture cannot drift from
`python3 scripts/census.py`. Each point is the **last commit of a day**, and
days without commits are simply absent from the axis; a history longer than
about forty-five points is thinned to that many, the last day always kept, so
a long one still draws in seconds.

matplotlib is not the standard library, and it is needed — but only to draw:
the log parsing, the count and `--self-test` are standard-library work, and
they run on a machine where matplotlib was never installed. A run that needs
the chart without it refuses with the install line:

    python3 -m pip install matplotlib

Usage:
    python3 scripts/loc_history.py [OUT.png] [--self-test]

Without OUT.png the chart is written to `.mush/loc-history.png` under the
repository root. `.mush/` is git-ignored, which is what a PNG wants: an
artifact, not a file to commit. Commits are read from the repository this
script lives in, and each one's tree is unpacked under the system temporary
directory, which is removed again however the unpacking ends.

`--self-test` checks the git-free parts — the log parse, the day's-last-commit
rule, the cap and the census arithmetic — with no repository, no network, no
git and no matplotlib, and prints what it proved.
"""

import argparse
import io
import math
import os
import subprocess
import sys
import tarfile
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)  # the repository this script belongs to
sys.path.insert(0, HERE)  # census.py sits beside this file
import census  # noqa: E402

CAP = 45  # drawn points, about; a longer history is thinned to that many
SERIES = ("blank", "comment", "tests", "prod")
MATPLOTLIB = (
    "loc_history.py: matplotlib is not installed, and it is what draws the PNG — "
    "run: python3 -m pip install matplotlib"
)


def git(*args):
    """The stdout of `git` run at the repository root.

    A non-zero exit raises `CalledProcessError`; `main` turns that into a
    sentence instead of a traceback.
    """
    return subprocess.run(
        ("git",) + args, cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout


def parse_log(text):
    """[(day, sha)] from `git log --date=short --format=%ad %H` output.

    Git prints newest first. Blank lines are skipped, so an empty repository
    (`""`) is no points rather than a crash.
    """
    points = []
    for line in text.splitlines():
        words = line.split()
        if len(words) >= 2:
            points.append((words[0], words[1]))
    return points


def daily(points):
    """Each day's last commit, oldest day first.

    `points` is newest first, git's order, so the *first* sighting of a day is
    that day's last commit. Sorting the days is the order the chart draws them
    left to right.
    """
    seen = {}
    for day, sha in points:
        seen.setdefault(day, sha)
    return sorted(seen.items())


def cap(points, limit=CAP):
    """`points` thinned to about `limit` of them, the last day always kept.

    A point a day over years is unreadable, and slow to count. The step is
    `ceil(n / limit)`, so the kept days are every step-th day plus the last one
    when the step skipped it; at most `limit` + 1 points come back, and the
    last of them is always the newest day.
    """
    if len(points) <= limit:
        return list(points)
    step = math.ceil(len(points) / limit)
    kept = points[::step]
    if kept[-1] != points[-1]:
        kept.append(points[-1])
    return kept


def points(text):
    """The chart's x axis from log text: each day's last commit, capped."""
    return cap(daily(parse_log(text)))


def samples():
    """The same points, from this repository's own history."""
    return points(git("log", "--date=short", "--format=%ad %H"))


def extract(sha, dest):
    """Unpack commit `sha`'s `crates/` tree under `dest`.

    `git archive` writes the commit's subtree as a tar and the standard
    library's `tarfile` unpacks it, so no system tar is involved. A revision
    git cannot name and bytes tarfile cannot read both raise; the caller's
    `TemporaryDirectory` removes `dest` either way.
    """
    done = subprocess.run(
        ("git", "archive", sha, "crates"), cwd=ROOT, capture_output=True, check=True
    )
    with tarfile.open(fileobj=io.BytesIO(done.stdout)) as bundle:
        try:
            bundle.extractall(dest, filter="data")
        except TypeError:  # Python before 3.12 has no `filter` argument
            bundle.extractall(dest)


def entries(dest):
    """[(path, lines)] for every `.rs` file under `dest/crates`.

    `path` is relative to `dest` — `crates/...` — the spelling census's
    `test_support_files` resolves module declarations against.
    """
    out = []
    for dirpath, _, files in os.walk(os.path.join(dest, "crates")):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            with open(path, encoding="utf-8", errors="replace") as handle:
                out.append((os.path.relpath(path, dest), handle.read().splitlines()))
    return out


def series_of(entries):
    """The four series summed over [(path, lines)], by census's own method.

    `census.test_support_files` says which files are test support and
    `census.split` classifies one file's lines; this only adds the files up,
    so the chart's numbers are the ones `python3 scripts/census.py` prints.
    """
    support = census.test_support_files(entries)
    series = {name: 0 for name in SERIES}
    for path, lines in entries:
        _total, blank, comment, tests, prod, _span = census.split(
            lines, os.path.normpath(path) in support
        )
        for name, value in (
            ("blank", blank),
            ("comment", comment),
            ("tests", tests),
            ("prod", prod),
        ):
            series[name] += value
    return series


def count(sha):
    """The four series for one commit's `crates/` tree, census's method.

    The tree is unpacked into a `TemporaryDirectory` under the system temp
    directory — a tool in `scripts/` leaves nothing in the repository — and
    that directory goes away however the unpacking ends. The lines are read
    into memory before it does.
    """
    with tempfile.TemporaryDirectory(prefix="mush-loc-history-") as scratch:
        extract(sha, scratch)
        read = entries(scratch)
    return series_of(read)


def drawing():
    """The pyplot to draw with, or None when matplotlib is not installed.

    Imported here, not at the top of the file: everything above this is
    standard-library work, and `--self-test` must run on a bare machine.
    """
    try:
        import matplotlib

        matplotlib.use("Agg")
        from matplotlib import pyplot as plt
    except ImportError:
        return None
    return plt


def draw(plt, days, series, out):
    """The chart, to `out`. Nothing above this needs matplotlib."""
    labels = [day[5:] for day, _ in days]  # mm-dd: the year would not fit
    ink = {"bg": "#10131a", "grid": "#252a35", "text": "#c8ccd4"}
    colours = {
        "prod": "#5ad1e6",
        "tests": "#7ee081",
        "comment": "#e8c46a",
        "blank": "#9aa0a8",
    }
    figure, (top, bottom) = plt.subplots(
        2,
        1,
        figsize=(11, 6.5),
        dpi=130,
        sharex=True,
        gridspec_kw={"height_ratios": [3, 1]},
    )
    figure.patch.set_facecolor(ink["bg"])
    for axis in (top, bottom):
        axis.set_facecolor(ink["bg"])
        axis.grid(color=ink["grid"], linewidth=0.8)
        axis.set_axisbelow(True)
        for side in axis.spines.values():
            side.set_color(ink["grid"])
        axis.tick_params(colors=ink["text"], labelsize=9)

    top.set_title(
        "LOC by day · crates/**/*.rs · counted by census.py · last commit of each day",
        color=ink["text"],
        fontsize=11,
        loc="left",
        pad=12,
    )
    for name, values in series.items():
        top.plot(
            labels,
            values,
            color=colours[name],
            linewidth=2,
            marker="o",
            markersize=3.5,
            label=name,
        )
        top.annotate(
            f"{values[-1]:,}",
            (len(labels) - 1, values[-1]),
            color=colours[name],
            fontsize=9,
            xytext=(6, -3),
            textcoords="offset points",
        )
    top.set_ylim(0, max(max(values) for values in series.values()) * 1.12)
    top.yaxis.set_major_formatter(lambda value, _pos: f"{value / 1000:.0f}k")
    top.legend(
        facecolor=ink["bg"],
        edgecolor=ink["grid"],
        labelcolor=ink["text"],
        loc="upper left",
        framealpha=0.9,
        fontsize=9,
    )

    for name, colour in (("tests", colours["tests"]), ("comment", colours["comment"])):
        bottom.plot(
            labels,
            [
                part / prod if prod else 0
                for part, prod in zip(series[name], series["prod"])
            ],
            color=colour,
            linewidth=2,
            marker="o",
            markersize=3,
            label=f"{name} : prod",
        )
    bottom.set_ylabel("ratio", color=ink["text"], fontsize=9)
    bottom.legend(
        facecolor=ink["bg"],
        edgecolor=ink["grid"],
        labelcolor=ink["text"],
        loc="upper left",
        framealpha=0.9,
        fontsize=8,
    )

    figure.tight_layout()
    figure.savefig(out, facecolor=ink["bg"])


def main():
    parser = argparse.ArgumentParser(
        description="draw the census's four LOC series, one point per day's last commit"
    )
    parser.add_argument(
        "out",
        nargs="?",
        default=os.path.join(ROOT, ".mush", "loc-history.png"),
        help="where to write the PNG (default: %(default)s)",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="check the git-free parts: the log parse, the day rule, the cap, the arithmetic",
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    # Ask for matplotlib before the counting, not after it: a bare machine
    # should be told what to install, not left to wait for the refusal.
    plt = drawing()
    if plt is None:
        print(MATPLOTLIB, file=sys.stderr)
        return 2

    try:
        days = samples()
    except subprocess.CalledProcessError as error:
        print(
            f"loc_history.py: {' '.join(error.cmd)} failed: "
            f"{(error.stderr or '').strip() or error}",
            file=sys.stderr,
        )
        return 1
    if not days:
        print(f"loc_history.py: no commits in {ROOT}; nothing to draw", file=sys.stderr)
        return 1

    series = {name: [] for name in SERIES}
    for _day, sha in days:
        try:
            row = count(sha)
        except subprocess.CalledProcessError as error:
            print(
                f"loc_history.py: cannot read {sha}: "
                f"{(error.stderr or '').strip() or error}",
                file=sys.stderr,
            )
            return 1
        except tarfile.TarError as error:
            print(f"loc_history.py: cannot unpack {sha}: {error}", file=sys.stderr)
            return 1
        for name in series:
            series[name].append(row[name])

    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    draw(plt, days, series, args.out)
    print(args.out)
    return 0


def self_test():
    """The git-free checks: no repository, no network, no git, no matplotlib.

    What the chart stands on is three pure steps — the log parse, the
    day's-last-commit rule, the cap — and the census arithmetic. A change that
    would make the picture lie fails here, on a machine with no history to
    draw.
    """
    failures = 0

    def check(name, ok, detail=""):
        nonlocal failures
        failures += 0 if ok else 1
        print(f"{'PASS' if ok else 'FAIL'}  {name}{(' — ' + detail) if detail else ''}")

    log = "2024-01-03 c\n2024-01-03 mid\n2024-01-02 b\n2024-01-01 a\n"
    check(
        "a log line is a day and a sha, in git's newest-first order",
        parse_log("2024-01-02 aa\n2024-01-01 bb\n")
        == [("2024-01-02", "aa"), ("2024-01-01", "bb")],
    )
    check(
        "blank lines are skipped",
        parse_log("\n \n2024-01-02 aa\n\n") == [("2024-01-02", "aa")],
    )
    check(
        "an empty log is no points, not a crash",
        points("") == [] and points("\n \n") == [],
    )
    check(
        "newest-first input comes back oldest-first",
        points(log)
        == [("2024-01-01", "a"), ("2024-01-02", "b"), ("2024-01-03", "c")],
        repr(points(log)),
    )
    check(
        "the first sighting of a day is that day's last commit",
        dict(points(log))["2024-01-03"] == "c",
        repr(dict(points(log))),
    )

    few = [("day-%02d" % i, "sha%02d" % i) for i in range(1, 4)]
    check("a short history is uncapped", cap(few) == few)

    many = [("day-%03d" % i, "sha%03d" % i) for i in range(100)]
    kept = cap(many)
    check(
        "the cap keeps the last day, and the step lands on the days it says",
        kept == many[::3] and len(kept) <= CAP + 1,
        f"{len(kept)} of {len(many)} points",
    )

    # 91 days is the old script's off-by-one: the step is 3 and day 90, the
    # last, is already kept, so appending the last day again repeated it.
    ninety_one = [("day-%03d" % i, "sha%03d" % i) for i in range(91)]
    check(
        "the last day is kept once, not twice",
        cap(ninety_one) == ninety_one[::3],
        f"{len(cap(ninety_one))} points",
    )

    # 90 days is the other side: the step is 2, so the last day is not one of
    # the kept ones and has to be added back.
    ninety = [("day-%03d" % i, "sha%03d" % i) for i in range(90)]
    check(
        "the last day is added back when the step skips it",
        cap(ninety) == ninety[::2] + [ninety[-1]],
        f"{len(cap(ninety))} points",
    )

    demo = [
        ("crates/demo/src/lib.rs", ["pub fn real() {}", ""]),
        (
            "crates/demo/src/helper.rs",
            ["#![cfg(test)]", "", "// fixture", "pub fn fixture() {}"],
        ),
    ]
    series = series_of(demo)
    check(
        "the four series are census's, summed over the tree, gated files as tests",
        series == {"blank": 2, "comment": 1, "tests": 2, "prod": 1},
        repr(series),
    )
    check(
        "the series add up to the lines read",
        sum(series.values()) == sum(len(lines) for _, lines in demo),
    )

    print(
        "all loc_history checks passed" if not failures else f"{failures} check(s) failed"
    )
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
