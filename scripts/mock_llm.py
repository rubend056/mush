#!/usr/bin/env python3
"""A scripted OpenAI-compatible model for deterministic agent tests.

Usage: mock_llm.py PORT

Serves /v1/models and /v1/chat/completions. Two scenarios, selected by the
root's first user message:

1. DEFAULT ("iso.txt"): a single isolated child.
   Root:  spawn_agent(isolated) -> end the turn early -> the child must wake it.
   Child: write_file(iso.txt) -> summary.
   (Exercises the wake-on-done path.)

2. CHAIN (user message contains "CHAIN"): a two-level delegation.
   Root:      spawn_agent #1 -> wait_agents([1]) -> final reply.
   Child #1:  spawn_agent #2 (its own subagent) -> wait_agents([2]) -> summary.
   Grandchild #2: write_file(deep.txt) -> summary.
   (Exercises depth-2 chains and nested worktrees.)

Subagents are told apart by "PARENT TASK" in the system prompt, and child vs
grandchild by "at depth 1" vs "at depth 2".

Used by the ignored tests in crates/mush/src/agent.rs.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


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

    def do_POST(self):  # /v1/chat/completions
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length))
        messages = request["messages"]
        joined = "\n".join(m.get("content") or "" for m in messages)
        system = messages[0].get("content") or "" if messages else ""
        is_subagent = "PARENT TASK" in system
        depth2 = "at depth 2" in system

        if is_subagent and depth2:
            # Grandchild (chain scenario only): the leaf that actually writes.
            if "wrote deep.txt" in joined:
                reply = {"role": "assistant", "content": "created deep.txt"}
            else:
                reply = self.tool_call("write_file", {
                    "path": "deep.txt",
                    "content": "deep work",
                })
        elif is_subagent:
            if "iso.txt" in system:
                # ISO scenario child: write the file itself.
                if "wrote iso.txt" in joined:
                    reply = {"role": "assistant", "content": "created iso.txt in my worktree"}
                else:
                    reply = self.tool_call("write_file", {
                        "path": "iso.txt",
                        "content": "isolated work",
                    })
            else:
                # CHAIN scenario child: delegate onwards, then wait.
                if "#2 done" in joined:
                    reply = {"role": "assistant", "content": "chain child done"}
                elif "spawned agent" in joined:
                    reply = self.tool_call("wait_agents", {"ids": [2], "timeout": 30})
                else:
                    reply = self.tool_call("spawn_agent", {
                        "brief": (
                            "create a file called deep.txt containing exactly: deep work; "
                            "you must delegate this to your own subagent"
                        ),
                        "isolated": True,
                    })
        elif "CHAIN" in joined:
            # Chain root: delegate, wait, report.
            if "#1 done" in joined:
                reply = {"role": "assistant", "content": "chain root done"}
            elif "spawned agent" in joined:
                reply = self.tool_call("wait_agents", {"ids": [1], "timeout": 30})
            else:
                reply = self.tool_call("spawn_agent", {
                    "brief": (
                        "delegate file creation to your own subagent: spawn one with "
                        "brief 'create a file called deep.txt containing exactly: deep work' "
                        "and isolated true, then wait for it, then report"
                    ),
                    "isolated": True,
                })
        else:
            # Default: single isolated child; root ends early and must wake.
            if "spawned agent" in joined:
                reply = {"role": "assistant",
                         "content": "child left running — I will handle its result when it finishes"}
            elif "#1 done" in joined:
                reply = {"role": "assistant", "content": "child finished"}
            else:
                reply = self.tool_call("spawn_agent", {
                    "brief": "create a file called iso.txt containing exactly: isolated work",
                    "isolated": True,
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