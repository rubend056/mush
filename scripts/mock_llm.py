#!/usr/bin/env python3
"""A scripted OpenAI-compatible model, for hand-driven runs.

Usage: mock_llm.py PORT [MARKER]

Serves /v1/models and /v1/chat/completions for a message a human types. Nothing
in the repo calls this script and no test uses it: the actor scenarios run
in-process over a scripted client, `scripts/screen.py` needs no endpoint, and
`scripts/smoke.py` talks to a real one. What it is good for is watching a tree
work without a model — `scripts/screen.py --url http://127.0.0.1:PORT --ask
"…"` on a pty.

It scripts the ten tools mush has (`edit_file`, `read_file`, `write_file`,
`list_files`, `search`, `run_command`, `spawn_agent`, `status`, `control`,
`wait`): a file is written by `run_command` with a shell
redirect, a child is isolated with `base`, and a parent waits with `wait`, which
takes no arguments. Scenarios, selected by the root's first user message:

1. DEFAULT ("iso.txt"): a single isolated child.
   Root:  spawn_agent(brief, title, base) -> end the turn early -> the child
          must wake it.
   Child: run_command(printf > iso.txt) -> summary.
   (Exercises the wake-on-done path.)

2. CHAIN (user message contains "CHAIN"): a two-level delegation.
   Root:      spawn_agent #1 -> wait -> final reply.
   Child #1:  spawn_agent #2 (its own subagent) -> wait -> summary.
   Grandchild #2: run_command(printf > deep.txt) -> summary.
   (Exercises depth-2 chains and nested worktrees.)

3. COMPACT (the request's final user message asks for a summary):
   the mock condenses the conversation instead of following the script,
   which lets tests verify that a full context window gets folded into a
   summary and the run continues.

4. STEER (user message contains "STEER"): the first request is held open for
   1.5 s — after writing MARKER, so the test knows the reply is in flight —
   and answers "first reply". Request "STEERME" answers "steered".
   (Exercises a nudge that arrives while a reply is being generated.)

5. TURNS (user message contains "TURNS"): every turn repeats the *same*
   `run_command` with nothing changed in between, until the run's loop guard
   stops it after `LOOP_ROUNDS` (5) identical rounds. (Exercises a run that
   ends early because it stopped making progress; nothing counts turns.)

6. ORPHAN (user message contains "ORPHAN"): the first turn runs
   `sleep 10; touch /tmp/mush-orphan-marker` as an ordinary *foreground*
   `run_command` and waits for it; the turn after the command's result says it
   finished. Quitting mush while it runs is the S4 check: the `sh`, its `sleep`
   and its marker must not outlive mush.

7. SHAPES (user message contains "SHAPES"): every tool-call shape in one
   conversation, so a hand-driven `scripts/screen.py` run can show how each one
   renders at a given width — a short ask with a short outcome, a long ask, two
   long refusals (a missing file, an unknown `control` target), a failure
   (`exit 3`), an ask whose result has not landed yet (a foreground
   `sleep 6`), a windowed `read_file` with a `search` and a `status`, a
   spawn_agent -> wait -> control sequence, a child's `#N done: …` report row
   landing between two call rows, and prose between call rows. The turn number
   is read off the transcript (the tool results so far), so the server keeps no
   state and a second run replays the same shapes. See "Seeing the shapes".

Seeing the shapes (scenario 7). The workdir is a fresh git repository with one
commit, because the child is isolated with `base: HEAD`; `--settle` has to
outlast the scripted conversation (the `sleep 6` is deliberate). The block is
taller than one pane, so `--keys-after` sends PageUp between the screens and the
three sizes walk it: the first screen is the tail of the conversation, the next
two show the rows above it, and between them every shape is on screen. (The
chat pane holds the keys; `\\e[5~` is PageUp, ten rows a press.)

    rm -rf /tmp/mush-shapes && mkdir -p /tmp/mush-shapes
    git -C /tmp/mush-shapes init -q
    git -C /tmp/mush-shapes -c user.email=mock@mush -c user.name=mock \\
        commit -q --allow-empty -m "shapes baseline"
    python3 scripts/mock_llm.py 8731 &
    python3 scripts/screen.py target/debug/mush /tmp/mush-shapes \\
        --url http://127.0.0.1:8731 --model mock \\
        --sizes 133x45,90x40,133x45 --settle 20 \\
        --keys-after '\\e[5~\\e[5~\\e[5~' \\
        --ask "SHAPES: show me every call shape"
    kill %1

The same scenario with a short `--settle` and a fresh workdir catches shape 5
the way it can only be caught while it runs — the transcript ends on the
`sleep 6` call, whose in-flight header is on the pane; the run reaches that
call in about a second, so a settle anywhere in the sleep lands mid-call (a
second workdir, because mush resumes the first one's conversation):

    rm -rf /tmp/mush-shapes-flight && mkdir -p /tmp/mush-shapes-flight
    git -C /tmp/mush-shapes-flight init -q
    git -C /tmp/mush-shapes-flight -c user.email=mock@mush -c user.name=mock \\
        commit -q --allow-empty -m "shapes baseline"
    python3 scripts/screen.py target/debug/mush /tmp/mush-shapes-flight \\
        --url http://127.0.0.1:8731 --model mock \\
        --sizes 133x45,90x40 --settle 5 \\
        --ask "SHAPES: show me every call shape"
    kill %1

The mock holds its port until it is killed, and `MOCK_TRACE=1` in its
environment prints one line per request — who asked, how many tool results its
transcript holds, the tail of what it read — which is how a scripted turn is
debugged.

Subagents are told apart by "mush subagent" in the system prompt, and child
vs grandchild by "at depth 1" vs "at depth 2". The parent's task travels as
subagent's first user message, so task keywords ("iso.txt") are found in the
joined transcript rather than in the system prompt.
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

MARKER = sys.argv[2] if len(sys.argv) > 2 else None
# `MOCK_TRACE=1` prints one line per request (who, how many tool results, the
# tail of the transcript) to stderr: what a scenario's next turn was decided
# from. Quiet unless asked for.
TRACE = os.environ.get("MOCK_TRACE")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # /v1/models
        body = json.dumps({"object": "list", "data": [{"id": "mock"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def tool_call(self, name, arguments):
        return {
            "role": "assistant",
            "tool_calls": [{
                "id": "call",
                "type": "function",
                "function": {"name": name, "arguments": json.dumps(arguments)},
            }],
        }

    def write(self, path, content):
        """A file written the way an agent writes one now: the shell does it.

        The `echo` is not decoration — it is what tells the next turn, which
        reads the transcript's tool results, that the file is on disk."""
        return self.tool_call("run_command", {
            "command": f"printf '{content}' > {path} && echo wrote {path}",
        })

    def prose_call(self, text, name, arguments):
        """A turn that says a sentence *and* makes a call.

        A tool-free turn is what ends a run, so prose that has to sit between
        two call rows rides with the call that follows it."""
        call = self.tool_call(name, arguments)
        call["content"] = text
        return call

    def shapes(self, messages, joined, system):
        """Scenario 7: every tool-call shape, once, in one conversation.

        The turn is read off the transcript — the tool results mush has folded
        in so far — so the server holds no state a second run would inherit.
        The order is deliberately not 1..9: the slow call (shape 5) sits after
        the spawn so the child finishes while it runs, and its report row lands
        between two call rows.
        """
        turn = sum(1 for m in messages if m.get("role") == "tool")
        if "mush subagent" in system:
            # The child writes in its own worktree, so the run's end commits
            # that work (the worktree survives as unmerged work, which is what
            # lets the parent's later `control` message resume this child),
            # and then reports.
            if "one more line" in joined:
                return {"role": "assistant",
                        "content": "shapes child: one more line read, still nothing to change"}
            if turn == 0:
                return self.tool_call("run_command", {
                    "command": "printf 'child work\\n' > child_out.txt && echo wrote child_out.txt",
                })
            return {"role": "assistant", "content": "shapes child wrote child_out.txt"}

        long_ask = (
            "seq 1 20 | tail -3 2>&1 && echo the-ask-is-long-on-purpose-so-that-a-90-column-pane-"
            "has-to-wrap-it-and-say-where-it-breaks"
        )
        script = [
            # 1 — short ask, short outcome.
            self.tool_call("run_command", {"command": "echo hi"}),
            # 2 — a long ask (a real command, 120+ characters).
            self.tool_call("run_command", {"command": long_ask}),
            # 3 — long outcomes: a missing file and a child that does not exist.
            self.tool_call("read_file", {"path": "no_such_file.txt"}),
            self.tool_call("control", {"id": "9", "action": "stop"}),
            # 4 — a failure.
            self.tool_call("run_command", {"command": "exit 3"}),
            # 6 — details: build a file, window it, search, list the board.
            self.tool_call("run_command", {
                "command": "seq 1 40 | sed 's/^/line /' > target_file.txt "
                           "&& echo wrote target_file.txt",
            }),
            self.tool_call("read_file", {"path": "target_file.txt", "offset": 1, "limit": 5}),
            self.tool_call("search", {"pattern": "line", "path": "."}),
            self.tool_call("status", {}),
            # 7/8 — spawn a child; it finishes while the slow call below runs,
            # so its `#1 done: …` report lands between two call rows.
            self.tool_call("spawn_agent", {
                "brief": "SHAPES child: create child_out.txt in your worktree containing "
                         "exactly: child work, then report",
                "title": "shapes child",
                "base": "HEAD",
            }),
            # 5 — an ask with no result yet (the in-flight header is shape 5).
            self.tool_call("run_command", {"command": "sleep 6 && echo slow"}),
            # 7 — wait, then control the child in sequence.
            self.tool_call("wait", {}),
            self.tool_call("control", {"id": "#1", "action": "message", "text": "one more line"}),
            # 9 — prose between call rows.
            self.prose_call("the child is awake again, so I look at the board before I stop",
                            "status", {}),
            self.tool_call("wait", {}),
            # The plain-text reply that ends the run.
            {"role": "assistant", "content": "every call shape ran - this sentence ends the run"},
        ]
        return script[min(turn, len(script) - 1)]

    def do_POST(self):  # /v1/chat/completions
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length))
        messages = request["messages"]
        joined = "\n".join(m.get("content") or "" for m in messages)
        system = messages[0].get("content") or "" if messages else ""
        is_subagent = "mush subagent" in system
        depth2 = "at depth 2" in system
        if TRACE:
            who = "child" if is_subagent else "root"
            results = sum(1 for m in messages if m.get("role") == "tool")
            print(f"[mock] {who} results={results} joined={joined[-240:]!r}",
                  file=sys.stderr, flush=True)

        # A SLOWCHAIN or SLOWISO first message asks for the same tree with a
        # subagent that holds its reply open. The flag is on the server because
        # every agent talks to the same one; the sleep itself is on the subagent
        # turn below. It keeps a parent and its child at work together for long
        # enough to photograph the tree with a hand-driven `scripts/screen.py`.
        if "SLOWCHAIN" in joined or "SLOWISO" in joined:
            self.server.slow = True
        if "Summarize everything important" in joined:
            # Context compaction: reply with a summary instead of a scripted turn.
            reply = {"role": "assistant",
                     "content": "mock summary: the original task and progress were condensed"}
        elif "ORPHAN" in joined and "[exit" not in joined:
            # A long foreground command: no `detach`, so mush waits on it (see
            # scenario 6). The command's own result carries `[exit`, so the turn
            # after it is the reply above.
            reply = self.tool_call("run_command", {
                "command": "sleep 10; touch /tmp/mush-orphan-marker",
            })
        elif "ORPHAN" in joined:
            reply = {"role": "assistant", "content": "the command finished"}
        elif "STEER" in joined and not getattr(self.server, "steer_started", False):
            # Hold the first reply so the test can nudge while it is in flight;
            # the marker says the request is in hand, so the test never races.
            self.server.steer_started = True
            if MARKER:
                with open(MARKER, "w") as handle:
                    handle.write("in flight")
            time.sleep(1.5)
            reply = {"role": "assistant", "content": "first reply"}
        elif "STEERME" in joined:
            reply = {"role": "assistant", "content": "steered"}
        elif "SHAPES" in joined:
            # Scenario 7: the call shapes, one turn each (the command is in the
            # module docstring, "Seeing the shapes").
            reply = self.shapes(messages, joined, system)
        elif "TURNS" in joined:
            # The same call, unchanged, every turn: nothing counts turns any
            # more (finding H45), so what stops this hand-driven run is the
            # loop guard — the same tool batch `LOOP_ROUNDS` (5) rounds over.
            reply = self.tool_call("run_command", {"command": "true"})
        else:
            # The SLOW knob: a subagent's first reply is held, so the tree above
            # it is still doing something when the screen is caught. The root is
            # deliberately never held — what these shots need is the tree.
            if (
                getattr(self.server, "slow", False)
                and is_subagent
                and "wrote " not in joined
            ):
                time.sleep(3)

            if is_subagent and depth2:
                # Grandchild (chain scenario only): the leaf that actually writes.
                if "wrote deep.txt" in joined:
                    reply = {"role": "assistant", "content": "created deep.txt"}
                else:
                    reply = self.write("deep.txt", "deep work")
            elif is_subagent:
                if "extra.txt" in joined and "wrote extra.txt" not in joined:
                    # A nudge after the child's first run. Used by the S1
                    # evidence run: the child's worktree is gone by then, so a
                    # run here would recreate the dead path as a plain
                    # directory. mush refuses it instead.
                    reply = self.write("extra.txt", "phantom")
                elif "iso.txt" in joined:
                    # ISO scenario child: write the file itself.
                    if "wrote iso.txt" in joined:
                        reply = {"role": "assistant", "content": "created iso.txt in my worktree"}
                    else:
                        reply = self.write("iso.txt", "isolated work")
                else:
                    # CHAIN scenario child: delegate onwards, then wait.
                    if "#2 done" in joined:
                        reply = {"role": "assistant", "content": "chain child done"}
                    elif "spawned agent" in joined:
                        reply = self.tool_call("wait", {})
                    else:
                        reply = self.tool_call("spawn_agent", {
                            "brief": (
                                "create a file called deep.txt containing exactly: deep work; "
                                "you must delegate this to your own subagent"
                            ),
                            "title": "deep chain",
                            "base": "HEAD",
                        })
            elif "CHAIN" in joined:
                # Chain root: delegate, wait, report.
                if "#1 done" in joined:
                    reply = {"role": "assistant", "content": "chain root done"}
                elif "spawned agent" in joined:
                    reply = self.tool_call("wait", {})
                else:
                    reply = self.tool_call("spawn_agent", {
                        "brief": (
                            "delegate file creation to your own subagent: spawn one with "
                            "brief 'create a file called deep.txt containing exactly: deep work', "
                            "then wait for it, then report"
                        ),
                        "title": "chain root",
                        "base": "HEAD",
                    })
            else:
                # Default: single isolated child; the root ends its turn early
                # and must wake. The child's result is checked first, because by
                # then the transcript holds both lines.
                if "#1 done" in joined:
                    reply = {"role": "assistant", "content": "child finished"}
                elif "spawned agent" in joined:
                    reply = {"role": "assistant",
                             "content": "child left running — I will handle its result when it finishes"}
                else:
                    reply = self.tool_call("spawn_agent", {
                        "brief": "create a file called iso.txt containing exactly: isolated work",
                        "title": "iso child",
                        "base": "HEAD",
                    })

        body = json.dumps({
            "choices": [{"message": reply, "finish_reason": "stop"}],
            "error": None,
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
