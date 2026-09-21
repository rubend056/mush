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

5. TURNS (user message contains "TURNS"): every turn calls run_command until
   the request carries the wrap-up instruction ("runaway guard"), which is
   answered with a summary. (Exercises the final turn of a run.)

6. ORPHAN (user message contains "ORPHAN"): the first turn runs
   `sleep 10; touch /tmp/mush-orphan-marker` as an ordinary *foreground*
   `run_command` and waits for it; the turn after the command's result says it
   finished. Quitting mush while it runs is the S4 check: the `sh`, its `sleep`
   and its marker must not outlive mush.

Subagents are told apart by "mush subagent" in the system prompt, and child
vs grandchild by "at depth 1" vs "at depth 2". The parent's task travels as
subagent's first user message, so task keywords ("iso.txt") are found in the
joined transcript rather than in the system prompt.
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

MARKER = sys.argv[2] if len(sys.argv) > 2 else None


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

    def do_POST(self):  # /v1/chat/completions
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length))
        messages = request["messages"]
        joined = "\n".join(m.get("content") or "" for m in messages)
        system = messages[0].get("content") or "" if messages else ""

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
        elif "TURNS" in joined and "runaway guard" in joined:
            # The wrap-up turn: tools are withdrawn and a summary is asked for.
            # The phrase is the one the real instruction carries
            # (`WRAP_UP_INSTRUCTION`); "turn limit" was the old prompt's wording
            # and the scripted run fell through to the loop guard (S8 iv).
            reply = {"role": "assistant",
                     "content": "wrapped up: the work done so far is in the workspace"}
        elif "TURNS" in joined:
            # Keep taking turns until the run hits its limit.
            reply = self.tool_call("run_command", {"command": "true"})
        else:
            is_subagent = "mush subagent" in system
            depth2 = "at depth 2" in system

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
