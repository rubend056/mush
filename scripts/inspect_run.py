#!/usr/bin/env python3
"""Read-only x-ray of a *running* mush instance.

`docs/findings.md` says the rows worth trusting hardest are the ones marked
**live** — "the screen and the transcript are the truth". This takes the
transcript of the *process*: one snapshot of a running `mush`, its children,
the fds it holds, the state files under `.mush/`, and how RSS/threads move over
a few seconds. That is the shape of every question the queue keeps asking —
is it spinning or blocked, is a stopped agent holding a live job, is a child a
zombie nobody reaped, is the state file actually being written.

**Strictly read-only.** It never writes to the socket, never signals, never
opens a file for writing. A byte injected into `mush.sock` is a real message to
a live session, and perturbing the thing under study is how you get a row that
is evidence about your probe instead of about mush. Everything here is a read
of `/proc` and the state directory.

Usage:

    python3 scripts/inspect_run.py ~/p/cemscale2/.mush
    python3 scripts/inspect_run.py --pid 12345 --samples 6 --interval 1
    python3 scripts/inspect_run.py ~/p/cemscale2/.mush --json > run.json

Run it against a directory outside the repo; the output is what comes back
here for analysis.

Two things this snapshot cannot do, and naming them is cheaper than being
fooled by them:

- **It is visible in its own picture.** Every line of output is a `run_command`
  job, so the probe's `sh`/`python3` appear among the target's children, each
  in its own pgid — which is exactly the shape of a real detached job. Rows
  matching the probe are labelled; a row that merely *looks* like a job may
  still be the probe.
- **A snapshot is one instant.** A child that is `Z` now may be reaped by the
  time you read the line, and a `wchan` of `do_epoll_wait` says the loop is
  parked, not that it is healthy — correlate with `cpu_s` between samples
  before calling anything stuck.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

PAGE = os.sysconf("SC_PAGE_SIZE")
HZ = os.sysconf("SC_CLK_TCK")


def read_text(path):
    try:
        with open(path, "r", errors="replace") as fh:
            return fh.read()
    except OSError:
        return None


def proc_stat(pid):
    """/proc/<pid>/stat as a dict. `comm` may contain spaces and parens, so
    split on the *last* ')' and index from there: tail[0] is field 3."""
    raw = read_text("/proc/%d/stat" % pid)
    if not raw:
        return None
    try:
        head, tail = raw.rsplit(")", 1)
        comm = head.split("(", 1)[1]
        f = tail.split()
        state = f[0]
        n = lambda i, d=0: int(f[i]) if i < len(f) and f[i].lstrip("-").isdigit() else d
    except (ValueError, IndexError):
        return None
    return {
        "pid": pid,
        "comm": comm,
        "state": state,
        "ppid": n(1),
        "pgrp": n(2),
        "session": n(3),
        "utime": n(11),
        "stime": n(12),
        "threads": n(17),
        "starttime": n(19),
        "vsize": n(20),
        "rss_pages": n(21),
    }


def cpu_seconds(st):
    return (st["utime"] + st["stime"]) / HZ


def rss_bytes(st):
    return st["rss_pages"] * PAGE


def uptime():
    raw = read_text("/proc/uptime")
    try:
        return float(raw.split()[0])
    except (AttributeError, IndexError, ValueError):
        return None


def etime(st, up):
    if up is None:
        return None
    return max(0.0, up - st["starttime"] / HZ)


def cmdline(pid):
    raw = read_text("/proc/%d/cmdline" % pid)
    if not raw:
        return "?"
    return " ".join(a for a in raw.split("\0") if a) or "?"


def cwd(pid):
    try:
        return os.readlink("/proc/%d/cwd" % pid)
    except OSError:
        return None


def all_stats():
    out = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        st = proc_stat(int(name))
        if st:
            out[st["pid"]] = st
    return out


def descendants(stats, root):
    kids = {}
    for st in stats.values():
        kids.setdefault(st["ppid"], []).append(st["pid"])
    seen, stack, order = set(), [root], []
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        order.append(pid)
        stack.extend(kids.get(pid, []))
    return order


def fd_table(pid):
    out = []
    d = "/proc/%d/fd" % pid
    try:
        names = os.listdir(d)
    except OSError:
        return out
    for n in sorted(names, key=lambda s: int(s) if s.isdigit() else 0):
        try:
            out.append((n, os.readlink(os.path.join(d, n))))
        except OSError:
            out.append((n, "<unreadable>"))
    return out


def classify(target):
    if target.startswith("socket:"):
        return "socket"
    if target.startswith("pipe:"):
        return "pipe"
    if target.startswith("anon_inode:"):
        return target
    if target.endswith(" (deleted)"):
        return "deleted"
    return "file"


def max_open_files(pid):
    raw = read_text("/proc/%d/limits" % pid)
    if not raw:
        return None
    for line in raw.splitlines():
        if line.startswith("Max open files"):
            return line.split()[3]
    return None


def listener_pids(sock_path):
    """Who holds a LISTEN on this unix socket, per `ss`.

    A bound unix socket's *name* is a fact only the kernel holds: the owning
    process's own fd table shows `socket:[inode]`, never the path. So the
    obvious discovery — scan cmdlines, cwds and fd targets for the state dir —
    misses precisely the process being looked for whenever mush was started
    from somewhere else (`/home/rubend/bin/mush` with cwd elsewhere). `ss -x`
    is the only place the path and the pid meet.
    """
    if not shutil.which("ss"):
        return []
    try:
        out = subprocess.run(["ss", "-xlpn"], capture_output=True, text=True,
                             timeout=5).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    pids = []
    for line in out.splitlines():
        if sock_path not in line:
            continue
        for part in line.split("pid=")[1:]:
            digits = ""
            for ch in part:
                if not ch.isdigit():
                    break
                digits += ch
            if digits:
                pids.append(int(digits))
    return sorted(set(pids))


def find_pid(hint, explicit):
    """Candidates: an explicit --pid, or every process that mentions `hint`
    in its cmdline, cwd, or fd targets, plus whoever listens on
    `<hint>/mush.sock` (the case the scan cannot see)."""
    if explicit:
        return [explicit]
    stats = all_stats()
    target = os.path.realpath(hint) if hint else None
    found = []
    for pid in sorted(stats):
        txt = "%s\n%s" % (cmdline(pid), cwd(pid) or "")
        if target and target in txt:
            found.append(pid)
            continue
        if target:
            for _, t in fd_table(pid):
                if t.startswith(target):
                    found.append(pid)
                    break
                continue
    if target:
        sock = target if target.endswith(".sock") else os.path.join(target, "mush.sock")
        found.extend(listener_pids(sock))
    if not found and hint is None:
        found = [p for p, st in stats.items() if "mush" in st["comm"]]
    return sorted(set(found))


def socket_fds(pid):
    """fd -> inode for every socket fd: the only handle on a socket's
    identity that the process itself exposes."""
    out = {}
    for n, t in fd_table(pid):
        if t.startswith("socket:[") and t.endswith("]"):
            out[n] = t[8:-1]
    return out


def ss_dump(flags):
    if not shutil.which("ss"):
        return ""
    try:
        return subprocess.run(["ss"] + flags, capture_output=True, text=True,
                              timeout=5).stdout
    except (OSError, subprocess.SubprocessError):
        return ""


def describe_sockets(pid):
    """What each socket fd actually is, joined back from `ss`.

    An fd table holds `socket:[inode]` and nothing else — no name, no peer, no
    netid. Two joins, because neither works alone: `ss -apn` names the owner
    (`pid=…,fd=…`), which is exact but goes silent for a process this user
    cannot see; `ss -aen` carries `ino:…` for every socket regardless, which is
    how a TCP connection to the provider is told apart from a unix socketpair.
    An unix socket may also have a row and no path: that is a socketpair half,
    or an accepted peer that has gone away — not a missing listener.
    """
    by_pid_fd = ss_dump(["-apn"])
    by_ino = ss_dump(["-aen"])
    out = {}
    for fd, ino in socket_fds(pid).items():
        row, how = None, None
        needle = "pid=%d,fd=%s" % (pid, fd)
        for line in by_pid_fd.splitlines():
            if needle in line:
                row, how = line.strip(), "pid+fd"
                break
        if row is None:
            for line in by_ino.splitlines():
                if ("ino:%s" % ino) in line:
                    row, how = line.strip(), "inode"
                    break
        peer = None
        if row:
            toks = row.split()
            if toks and toks[0] in ("tcp", "udp", "tcp6", "udp6") and len(toks) > 5:
                peer = toks[5]
        out[fd] = {"inode": ino, "row": row, "how": how, "peer": peer}
    return out


def state_files(state_dir):
    if not state_dir or not os.path.isdir(state_dir):
        return None
    now = time.time()
    rows = []
    for name in sorted(os.listdir(state_dir)):
        p = os.path.join(state_dir, name)
        try:
            st = os.stat(p)
        except OSError:
            continue
        kind = "sock" if (st.st_mode & 0o170000) == 0o140000 else "file"
        rows.append({"name": name, "kind": kind, "size": st.st_size,
                     "age_s": round(now - st.st_mtime, 1)})
    return rows


def state_probe(state_dir):
    """Size and mtime of `session.json`, the one state file whose cost grows
    with the conversation. Both are needed: the size says how much a *single*
    save writes, the mtime says how many saves there were."""
    if not state_dir:
        return None
    p = os.path.join(state_dir, "session.json")
    try:
        st = os.stat(p)
    except OSError:
        return None
    return {"size": st.st_size, "mtime": st.st_mtime}


def sample(pid, n, interval, state_dir=None):
    """Sample the target over time. Each sample also carries what the *watch*
    needs: the child pids (so a birth and a death are both visible, and a
    lifetime is measurable) and the state file (so a rewrite is countable)."""
    out = []
    for i in range(n):
        st = proc_stat(pid)
        if not st:
            break
        io = read_text("/proc/%d/io" % pid) or ""
        vals = {}
        for line in io.splitlines():
            k, _, v = line.partition(":")
            vals[k.strip()] = v.strip()
        sched = read_text("/proc/%d/sched" % pid) or ""
        switches = None
        for line in sched.splitlines():
            if line.startswith("nr_switches"):
                switches = line.split(":", 1)[1].strip()
        kids = []
        for p, kst in all_stats().items():
            if kst["ppid"] == pid:
                kids.append(p)
        out.append({
            "t": round(i * interval, 2),
            "state": st["state"],
            "rss": rss_bytes(st),
            "threads": st["threads"],
            "cpu_s": round(cpu_seconds(st), 3),
            "wchan": (read_text("/proc/%d/wchan" % pid) or "").strip(),
            "read_bytes": vals.get("read_bytes"),
            "write_bytes": vals.get("write_bytes"),
            "nr_switches": switches,
            "children": sorted(kids),
            "session": state_probe(state_dir),
        })
        if i + 1 < n:
            time.sleep(interval)
    return out


def watch_summary(samples, state_dir):
    """What the samples say once they are put beside each other.

    Three things here are invisible in any single snapshot, and each one is a
    question the queue keeps asking: how much disk a session costs per save
    (`write_bytes` against the file's own size — the amplification factor),
    whether the state file is being rewritten faster than the conversation
    grows, and how long children live (a pid seen in one sample and gone in the
    next is a child whose whole life fits inside the watch window).
    """
    out = {}
    if len(samples) < 2:
        return out
    span = samples[-1]["t"] - samples[0]["t"]
    out["span_s"] = round(span, 2)

    w0, w1 = samples[0].get("write_bytes"), samples[-1].get("write_bytes")
    if w0 and w1 and w1.isdigit() and w0.isdigit():
        wrote = int(w1) - int(w0)
        out["bytes_written"] = wrote
        if span > 0:
            out["bytes_written_per_s"] = round(wrote / span, 1)
    r0, r1 = samples[0].get("read_bytes"), samples[-1].get("read_bytes")
    if r0 and r1 and r1.isdigit() and r0.isdigit():
        out["bytes_read"] = int(r1) - int(r0)

    sess = [s.get("session") for s in samples if s.get("session")]
    if sess:
        sizes = [s["size"] for s in sess]
        rewrites = len({round(s["mtime"], 3) for s in sess})
        out["session_size_first"] = sizes[0]
        out["session_size_last"] = sizes[-1]
        out["session_grew"] = sizes[-1] - sizes[0]
        # One rewrite per distinct mtime; the last sample may still be inside
        # the write that is about to happen, so this is a floor, not a count.
        out["session_rewrites"] = max(0, rewrites - 1)
        if sizes[-1] > 0 and out.get("bytes_written"):
            out["amplification"] = round(out["bytes_written"] / sizes[-1], 2)

    born = set()
    for s in samples:
        born |= set(s["children"])
    first_seen = {}
    last_seen = {}
    for s in samples:
        for p in s["children"]:
            first_seen.setdefault(p, s["t"])
            last_seen[p] = s["t"]
    stats = all_stats()
    out["children_seen"] = len(born)
    out["children_gone"] = sorted(p for p in born if p not in stats)
    out["children_lived_through"] = sorted(p for p in born if p in stats)
    out["child_lives"] = [
        {"pid": p, "seen_from_s": first_seen[p], "seen_to_s": last_seen[p],
         "touched_s": round(last_seen[p] - first_seen[p], 2)}
        for p in sorted(born)
    ]
    return out


def watch_verdicts(watch):
    """The sentences the window earns, kept apart from the printing so the
    JSON carries them too."""
    out = []
    if not watch or watch.get("span_s", 0) <= 0:
        return out
    span = watch["span_s"]
    if watch.get("bytes_written") is not None:
        per_s = watch.get("bytes_written_per_s", 0.0)
        if per_s > 65536:
            out.append("%.2f MiB/s written to disk while nothing was being asked "
                       "of it" % (per_s / 1048576.0))
    if "amplification" in watch:
        out.append("a save costs %.1fx the state file's size (%.2f MiB written for a "
                   "%.2f MiB session): every save re-serializes the whole conversation"
                   % (watch["amplification"], watch["bytes_written"] / 1048576.0,
                      watch["session_size_last"] / 1048576.0))
    if watch.get("session_rewrites", 0) and watch.get("session_grew", 0) > 0:
        out.append("session.json grew %.1f KiB over %d rewrite(s) in %.1fs - the file "
                   "grows with the transcript and is never pruned"
                   % (watch["session_grew"] / 1024.0, watch["session_rewrites"], span))
    gone = watch.get("children_gone") or []
    if gone:
        out.append("%d child pid(s) died inside the %.1fs window - lives shorter than "
                   "the watch, or leaks that ended on their own (the inspector's own "
                   "job is one of them)" % (len(gone), span))
    return out


def verdicts(main_st, kids_stats, samples, fds, limits, state):
    v = []
    if not main_st:
        return ["process is gone"]
    if any(s["state"] == "R" for s in samples):
        v.append("state R in at least one sample: busy or spinning — check cpu_s delta")
    if len(samples) > 1:
        dcpu = samples[-1]["cpu_s"] - samples[0]["cpu_s"]
        el = samples[-1]["t"] - samples[0]["t"]
        if el > 0:
            v.append("cpu %.0f%% of one core over %.1fs" % (100.0 * dcpu / el, el))
        drss = samples[-1]["rss"] - samples[0]["rss"]
        if abs(drss) > 1024 * 1024:
            v.append("rss moved %+.1f MiB over %.1fs" % (drss / 1048576.0, el))
        dthr = samples[-1]["threads"] - samples[0]["threads"]
        if dthr:
            v.append("thread count moved %+d — suspicion of a thread leak" % dthr)
    zomb = [p for p, st in kids_stats.items() if st["state"] == "Z"]
    if zomb:
        v.append("unreaped children (Z): %s" % ", ".join(map(str, zomb)))
    stopped = [p for p, st in kids_stats.items() if st["state"] == "T"]
    if stopped:
        v.append("stopped children (T): %s" % ", ".join(map(str, stopped)))
    if limits and limits.isdigit() and len(fds) > 0.8 * int(limits):
        v.append("fd table is %d/%s — near the limit" % (len(fds), limits))
    if state:
        sess = [r for r in state if r["name"] == "session.json"]
        if sess and sess[0]["age_s"] > 60:
            v.append("session.json untouched for %.0fs" % sess[0]["age_s"])
    return v or ["nothing anomalous in this snapshot"]


def report(pid, state_dir, args):
    stats = all_stats()
    main_st = stats.get(pid) or proc_stat(pid)
    if not main_st:
        print("no such process: %d" % pid, file=sys.stderr)
        return 1
    up = uptime()
    tree = descendants(stats, pid)
    kids_stats = {p: stats[p] for p in tree[1:] if p in stats}
    fds = fd_table(pid)
    kinds = {}
    for _, t in fds:
        k = classify(t)
        kinds[k] = kinds.get(k, 0) + 1
    sock_fds = describe_sockets(pid)
    limits = max_open_files(pid)
    state = state_files(state_dir)
    samples = sample(pid, args.samples, args.interval, state_dir)
    watch = watch_summary(samples, state_dir)

    data = {
        "pid": pid,
        "comm": main_st["comm"],
        "state": main_st["state"],
        "cwd": cwd(pid),
        "cmdline": cmdline(pid),
        "pgid": main_st["pgrp"],
        "sid": main_st["session"],
        "etime_s": round(etime(main_st, up), 1) if etime(main_st, up) else None,
        "threads": main_st["threads"],
        "rss": rss_bytes(main_st),
        "fd_count": len(fds),
        "fd_kinds": kinds,
        "max_open_files": limits,
        "children": [
            {"pid": p, "comm": k["comm"], "state": k["state"],
             "etime_s": round(etime(k, up), 1) if etime(k, up) else None,
             "pgrp": k["pgrp"], "cmdline": cmdline(p)}
            for p, k in sorted(kids_stats.items())
        ],
        "state_dir": state_dir,
        "state_files": state,
        "sockets": sock_fds,
        "samples": samples,
        "watch": watch,
        "fds": [{"fd": n, "target": t, "kind": classify(t)} for n, t in fds],
    }
    data["verdicts"] = verdicts(main_st, kids_stats, samples, fds, limits, state)
    for line in watch_verdicts(watch):
        data["verdicts"].append(line)

    if args.json:
        print(json.dumps(data, indent=2))
        return 0

    def h(title):
        print("\n== %s" % title)

    print("mush inspector — read-only snapshot, %s" % time.strftime("%Y-%m-%d %H:%M:%S"))
    h("process")
    print("pid %d (%s) state %s  pgid %d sid %d" %
          (pid, main_st["comm"], main_st["state"], main_st["pgrp"], main_st["session"]))
    print("cwd   %s" % (cwd(pid) or "?"))
    print("cmd   %s" % cmdline(pid))
    if etime(main_st, up):
        print("up %.1fs  threads %d  rss %.1f MiB  wchan %s" %
              (etime(main_st, up), main_st["threads"], rss_bytes(main_st) / 1048576.0,
               (read_text("/proc/%d/wchan" % pid) or "?").strip()))
    h("children (%d)" % len(kids_stats))
    if not kids_stats:
        print("none")
    tree_rows = descendants(stats, pid)
    depth = {}
    for p in tree_rows:
        parent = stats[p]["ppid"] if p in stats else 0
        depth[p] = depth.get(parent, -1) + 1
    for p in tree_rows[1:]:
        st = kids_stats.get(p)
        if not st:
            continue
        cmd = cmdline(p)
        mark = ""
        if st["pgrp"] != main_st["pgrp"]:
            mark += " [own pgid %d = detached job]" % st["pgrp"]
        if st["state"] == "Z":
            mark += " [zombie]"
        if "inspect_run.py" in cmd:
            mark += " [this probe — the snapshot cannot see itself not looking]"
        print("  %s%d %s %s %.1fs%s" %
              ("  " * depth.get(p, 1), p, st["state"], st["comm"],
               etime(st, up) or 0.0, mark))
        print("  %s  %s" % ("  " * depth.get(p, 1), cmd[:160]))
    h("fds (%d)%s" % (len(fds), "  max %s" % limits if limits else ""))
    for k, c in sorted(kinds.items()):
        print("  %-10s %d" % (k, c))
    for n, t in fds:
        print("  %4s -> %s" % (n, t[:120]))
    h("state dir %s" % (state_dir or "(unknown)"))
    if state:
        for r in state:
            print("  %-14s %-4s %8d bytes  touched %.1fs ago" %
                  (r["name"], r["kind"], r["size"], r["age_s"]))
    else:
        print("  no state directory given or it is unreadable")
    h("sockets — what each fd actually is")
    if not sock_fds:
        print("  none")
    for fd, info in sorted(sock_fds.items(), key=lambda kv: int(kv[0])):
        peer = "  peer %s" % info["peer"] if info["peer"] else ""
        print("  %4s inode %-12s%s%s" %
              (fd, info["inode"], peer,
               "  (via %s)" % info["how"] if info["how"] else ""))
        print("       %s" % (info["row"] or "(no ss row: unconnected, or ss unavailable)"))
    h("samples (%.1fs apart)" % args.interval)
    for s in samples:
        print("  t=%-5s %s rss=%6.1fMiB thr=%-3s cpu=%-8s wchan=%-14s sw=%s" %
              (s["t"], s["state"], s["rss"] / 1048576.0, s["threads"], s["cpu_s"],
               (s["wchan"] or "-")[:14], s["nr_switches"]))
    h("watch — what only the samples together can say")
    if not watch or watch.get("span_s", 0) <= 0:
        print("  one sample only: nothing to compare (raise --samples)")
    else:
        print("  window %.1fs" % watch["span_s"])
        if "bytes_written" in watch:
            print("  wrote %.2f MiB (%.2f MiB/s) to disk, read %.2f MiB" %
                  (watch["bytes_written"] / 1048576.0,
                   watch.get("bytes_written_per_s", 0.0) / 1048576.0,
                   watch.get("bytes_read", 0) / 1048576.0))
        if "session_size_last" in watch:
            print("  session.json %.1f KiB → %.1f KiB (%+.1f KiB), %d rewrite(s) seen" %
                  (watch["session_size_first"] / 1024.0, watch["session_size_last"] / 1024.0,
                   watch["session_grew"] / 1024.0, watch["session_rewrites"]))
        if "amplification" in watch:
            print("  amplification: %.1f× the file's own size, written in this window" %
                  watch["amplification"])
        if "children_seen" in watch:
            print("  children seen %d, gone by the end %d" %
                  (watch["children_seen"], len(watch["children_gone"])))
            for c in watch["child_lives"]:
                fate = "gone" if c["pid"] in watch["children_gone"] else "alive"
                print("    pid %-8d %s  spanned %.1fs of the window" %
                      (c["pid"], fate, c["touched_s"]))
    h("verdicts")
    for v in data["verdicts"]:
        print("  - %s" % v)
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("state_dir", nargs="?", default=None,
                    help="a .mush directory, used to find the process and to list state")
    ap.add_argument("--pid", type=int, default=None)
    ap.add_argument("--samples", type=int, default=5)
    ap.add_argument("--interval", type=float, default=1.0)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    pids = find_pid(args.state_dir, args.pid)
    if not pids:
        print("found no mush process for that hint; pass --pid", file=sys.stderr)
        return 2
    if len(pids) > 1:
        print("hint matched several processes: %s" % pids, file=sys.stderr)
        print("re-run with --pid", file=sys.stderr)
        return 2
    return report(pids[0], args.state_dir, args)


if __name__ == "__main__":
    sys.exit(main())
