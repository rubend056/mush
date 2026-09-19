#!/usr/bin/env python3
"""Where a session.json's bytes actually are.

`docs/findings.md` studies behaviour; this studies the *file*, because the file
is what costs: `Session::save` re-serializes the whole thing on every save
(`crates/mush-core/src/session.rs:315`), so a session's size is its per-save
cost, its load-time parse, and the memory its snapshot copy takes on the UI
thread — all three at once.

The question it answers is one of *attribution*: a conversation that reaches
tens of megabytes got there either because one transcript ran away, or because
hundreds of children each stopped short of their own fold trigger and were then
kept forever. The two have opposite fixes, so guessing is expensive.

Read-only, and no dependency on mush being built. Works on a live file (a
snapshot of a moment); a 100 MB session needs a few hundred MB of RAM to parse.

    python3 scripts/session_blame.py ~/p/cemscale2/.mush/session.json --top 20
    python3 scripts/session_blame.py --json .mush/session.json

The `--top 20` output is small enough to paste back for analysis; the 100 MB
file never has to move.

The window the yardstick is drawn against is **read, not assumed**: the third
argument states it, else the session file's own `context` field (the one mush
stores when a human stated a window), else `MUSH_CONTEXT` in this shell, else
the home config; with none of those the sizes are still reported and the fold
trigger is left unnamed. It used to default to 128000, which is nobody's
number: mush's built-in default is 8192, and the workspace this was written for
runs at 500000.
"""

import argparse
import json
import os
import sys

# `Config::history_budget` is `(context_tokens - reserve) * 3` bytes
# (crates/mush-core/src/config.rs) with
# `reserve = min(SCHEMA_TOKENS + reply_cap + MARGIN_TOKENS, context/2)` and
# `reply_cap = min(MAX_REPLY_TOKENS, max(1024, context/8))`, and a transcript
# folds at nine tenths of the budget (`transcript::compaction_trigger`). Those
# numbers are what makes "each child stopped just short of its own ceiling"
# visible, so this script derives them from the window it is told about rather
# than comparing against one constant — a yardstick that does not name the
# window it belongs to is the rumour this file exists to avoid. The three moved
# together on the human's numbers (see `docs/findings.md` §8.30): the reply's
# share went from a quarter of the window to an eighth, the margin from 200
# tokens to 5000, and the trigger from three quarters of the budget to nine
# tenths. Change one there and one here, or this tool starts lying again —
# which is why the constants it used are printed in `--json`, to be compared
# against what `mush --print-config` reports.
SCHEMA_TOKENS = 1200
REPLY_SHARE_DIVISOR = 8
MAX_REPLY_TOKENS = 120_000
REPLY_CAP_FLOOR = 1024
MARGIN_TOKENS = 5_000
TRIGGER_NUMERATOR = 9
TRIGGER_DENOMINATOR = 10
# `Config::DEFAULT_CONTEXT_TOKENS` (config.rs:23): what mush assumes when the
# human stated no window *and* the endpoint advertised none. It is named here
# only to be quoted in the message for "no window is known" — it is **not** a
# default for this script, because the window that matters is the one the
# process that wrote the file was using, and assuming a number is how this
# script came to call a 500k session a 128k one.
BUILT_IN_CONTEXT = 8192


def window_from_environment():
    """`MUSH_CONTEXT` in *this* shell, if it is a token count.

    mush ranks it above a workspace's stored window, so it is worth reading —
    but it is this shell's environment, not the one the file was written in,
    and the caller says which source it used.
    """
    value = os.environ.get("MUSH_CONTEXT")
    if not value:
        return None
    try:
        tokens = int(value)
    except ValueError:
        return None
    return tokens if tokens > 0 else None


def config_path():
    """`mush_core::userconfig::config_path`, mirrored."""
    override = os.environ.get("MUSH_CONFIG")
    if override:
        return override
    xdg = os.environ.get("XDG_CONFIG_HOME") or os.path.join(os.path.expanduser("~"), ".config")
    return os.path.join(xdg, "mush", "config.json")


def window_from_home_config():
    """The window the human's own config states, if any."""
    try:
        with open(config_path()) as f:
            stated = json.load(f).get("context")
    except (OSError, ValueError):
        return None
    return stated if isinstance(stated, int) and stated > 0 else None


def window_of(args, session):
    """(tokens, source) for the yardstick, or `(None, why nothing states one)`.

    The order is the one that keeps the measurement honest rather than the one
    mush resolves config in: the *session's* own stated window first, because
    the file is the subject and a `/context` was a fact about this workspace;
    then this shell's `MUSH_CONTEXT` (mush ranks it above the session's, but a
    shell today is not the environment the file was written in); then the home
    config, which is the lowest layer that can state one. Nothing here guesses
    from the model: a derived window comes from the endpoint advertising it,
    and this script does not talk to the network.
    """
    if args.context is not None:
        return args.context, "the command line"
    stated = session.get("context") if isinstance(session, dict) else None
    if isinstance(stated, int) and stated > 0:
        return stated, "stated in this session.json (`context`)"
    from_env = window_from_environment()
    if from_env is not None:
        return from_env, "MUSH_CONTEXT in this shell"
    from_home = window_from_home_config()
    if from_home is not None:
        return from_home, config_path()
    return None, ("nothing states one: the session stores only a window the human stated, "
                  "mush assumes %d otherwise, and a window the endpoint advertised was never "
                  "stored — pass it as the third argument" % BUILT_IN_CONTEXT)


def budget_for(context_tokens):
    """(budget, trigger) bytes for a window, the way `Config` computes them."""
    reply_cap = min(MAX_REPLY_TOKENS, max(REPLY_CAP_FLOOR, context_tokens // REPLY_SHARE_DIVISOR))
    reserve = min(SCHEMA_TOKENS + reply_cap + MARGIN_TOKENS, context_tokens // 2)
    budget = max(0, context_tokens - reserve) * 3
    return budget, budget * TRIGGER_NUMERATOR // TRIGGER_DENOMINATOR


def human(n):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if abs(n) < 1024 or unit == "GiB":
            return "%.1f %s" % (n, unit) if unit != "B" else "%d B" % n
        n /= 1024.0


def msg_bytes(m):
    return len(json.dumps(m, indent=2).encode())


def messages_report(messages, top):
    by_role = {}
    rows = []
    for i, m in enumerate(messages):
        size = msg_bytes(m)
        role = m.get("role", "?")
        by_role[role] = by_role.get(role, 0) + size
        rows.append((size, i, role, (m.get("content") or "")[:70].replace("\n", " ")
                     if isinstance(m.get("content"), str) else "<structured>"))
    rows.sort(reverse=True)
    return by_role, rows[:top], rows


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("path")
    ap.add_argument("--top", type=int, default=10)
    ap.add_argument("context", type=int, nargs="?", default=None,
                    help="the session's window in tokens; without it the window comes from"
                         " the session file, then MUSH_CONTEXT, then the home config")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    raw = open(args.path, "rb").read()
    d = json.loads(raw)
    pretty = len(json.dumps(d, indent=2).encode())
    compact = len(json.dumps(d, separators=(",", ":")).encode())

    data = {"path": os.path.abspath(args.path), "bytes": len(raw),
            "compact_bytes": compact, "indent_cost": round(len(raw) / max(1, compact), 3)}

    tokens, window_source = window_of(args, d)
    data["window"] = {"tokens": tokens, "source": window_source}
    # The formula this file mirrors, in the output: a number derived from a
    # second copy of a rule is only trustworthy if the copy is visible.
    data["formula"] = {"schema_tokens": SCHEMA_TOKENS,
                       "reply_share_divisor": REPLY_SHARE_DIVISOR,
                       "max_reply_tokens": MAX_REPLY_TOKENS,
                       "reply_cap_floor": REPLY_CAP_FLOOR,
                       "margin_tokens": MARGIN_TOKENS,
                       "trigger": "%d/%d of the budget" % (TRIGGER_NUMERATOR,
                                                            TRIGGER_DENOMINATOR)}

    if not args.json:
        print("session: %s" % data["path"])
        print("on disk %s · compact would be %s (indentation costs %.2fx)"
              % (human(len(raw)), human(compact), data["indent_cost"]))

    messages = d.get("messages") or []
    agents = d.get("agents") or []

    if not args.json:
        print("\n-- top level")
        for k in sorted(d, key=lambda k: -len(json.dumps(d[k], indent=2).encode()))[:6]:
            print("  %-12s %10s" % (k, human(len(json.dumps(d[k], indent=2).encode()))))

    by_role, worst, all_msgs = messages_report(messages, args.top)
    data["root"] = {"messages": len(messages),
                    "bytes": sum(s for s, _, _, _ in all_msgs),
                    "by_role": by_role}
    if not args.json:
        print("\n-- root transcript: %d messages, %s"
              % (len(messages), human(data["root"]["bytes"])))
        for role, n in sorted(by_role.items(), key=lambda kv: -kv[1]):
            print("  %-10s %10s" % (role, human(n)))
        print("  largest:")
        for size, i, role, text in worst:
            print("    #%-5d %-9s %8s  %s" % (i, role, human(size), text[:60]))

    sizes = []
    for a in agents:
        msgs = a.get("messages") or []
        sizes.append((sum(msg_bytes(m) for m in msgs), a, len(msgs)))
    sizes.sort(key=lambda t: -t[0])
    total = sum(s for s, _, _ in sizes)
    data["agents"] = {
        "count": len(agents),
        "bytes": total,
        "share_of_file": round(total / max(1, len(raw)), 3),
        "largest": [{"id": a.get("id"), "status": a.get("status"),
                     "messages": n, "bytes": s, "brief": (a.get("brief") or "")[:60]}
                    for s, a, n in sizes[:args.top]],
    }
    if not args.json:
        print("\n-- children: %d agents, %s in transcripts (%.0f%% of the file)"
              % (len(agents), human(total), 100.0 * total / max(1, len(raw))))
        if sizes:
            # `sizes` is largest-first (the top list reads it that way), so the
            # percentiles are taken from its mirror image: reading them off the
            # descending list labelled the tenth percentile `p90`.
            asc = sorted(s for s, _, _ in sizes)
            mid = asc[len(asc) // 2]
            p90 = asc[min(len(asc) - 1, int(len(asc) * 0.9))]
            print("  median %s · p90 %s · largest %s"
                  % (human(mid), human(p90), human(asc[-1])))
            if tokens is None:
                print("  window unknown: %s" % window_source)
            else:
                budget, trigger = budget_for(tokens)
                at_ceiling = sum(1 for s in asc if s > trigger * 0.5)
                print("  window %d tokens (%s) -> budget %s, fold trigger %s"
                      % (tokens, window_source, human(budget), human(trigger)))
                print("  %d of %d hold more than half that trigger; a finished agent's"
                      " transcript is never folded again"
                      % (at_ceiling, len(asc)))
        for s, a, n in sizes[:args.top]:
            print("    #%-6s %-9s %4d msgs %9s  %s"
                  % (a.get("id"), str(a.get("status"))[:9], n, human(s),
                     (a.get("brief") or "")[:50].replace("\n", " ")))

    verdict = []
    if total > len(raw) * 0.5 and len(agents) > 20:
        verdict.append("the bytes are children, not one runaway transcript: %d agents "
                       "hold %.0f%% of the file (%s)" % (len(agents),
                                                         100.0 * total / len(raw), human(total)))
        verdict.append("retention, not growth: each child stopped short of its own fold "
                       "trigger and nothing prunes it afterwards")
    elif data["root"]["bytes"] > len(raw) * 0.5:
        verdict.append("the bytes are in the root transcript (%s of %s) — the one "
                       "conversation that is compacted, so check whether folding is "
                       "actually firing" % (human(data["root"]["bytes"]), human(len(raw))))
    else:
        verdict.append("no single owner: the file is spread across %d agents and the root"
                       % len(agents))
    if data["indent_cost"] > 1.3:
        verdict.append("indentation costs %.2fx — compacting the JSON would pay for itself"
                       % data["indent_cost"])
    else:
        verdict.append("indentation costs only %.2fx: the payload is long strings, so "
                       "pretty-printing is not the problem" % data["indent_cost"])
    data["verdicts"] = verdict
    if not args.json:
        print("\n-- verdicts")
        for v in verdict:
            print("  - %s" % v)
    else:
        print(json.dumps(data, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
