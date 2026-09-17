# mush — findings: the queue of record

`docs/refactor.md` §6 defers to this file for "what is broken"; that file is the
structural companion and says where a fix *belongs*. This file says what is
wrong, where it was seen, and where it stands.

Status column: ⬜ open · 🔄 being worked · ✅ fixed (and by which home).

Entries marked **live** were observed in a real mush session — the orchestrator
running this very repository — not reasoned about from the source. Those are the
ones worth trusting hardest: the screen and the transcript are the truth.

---

## 1. Observed live: the agent tree lies about who is working

Both of these were seen in the session that produced the M2.75/M2.8 work, while
four subagents and their children ran.

| ID | What | Status | Seen where |
|---|---|---|---|
| U1 | **A working agent is drawn as paused.** A row renders `⏸` for an agent that is still working, because the glyph comes from "has live children" rather than from the agent's own phase. The orchestrator's four children all showed `⏸` while each was mid-turn — the icon said "waiting" about agents that were busy, which is the exact class of lie §4.5 R0 set out to make impossible. | ⬜ | the tree pane, all four rows, whole run |
| U2 | **The pane title counts waiting agents as working.** The title reads `agents · 6 running · …` while some of those agents are parked waiting on children (and one was stopped). A count is a derived fact like any other: it must be computed from the phases, and it must say what it counts. | ⬜ | the agents pane title |

These two share one cause, and it is R0's cause again: something on the screen is
a stored *conclusion* ("has children", "busy") rather than a derived fact
(`Phase`, and the instant it began). `docs/refactor.md` §3.1's `AgentTree::busy`
was made a method; what is left is that the *row* and the *title* each derive
their own answer instead of reading that one.

## 2. Observed live: a delivered completion is invisible, and can be delivered twice

| ID | What | Status | Home |
|---|---|---|---|
| B20 | **A child's completion reaches the model but not the screen, and can be folded twice.** `fold_completions` (fixed below) pushes the `#N done: …` line into the actor's `messages` only; no `AgentEvent::Message` is emitted, so the UI's copy of the parent's transcript never shows it. The next idle `Run` then hands the UI transcript back, `absorb` finds no such line in it and re-arms delivery for the same child — so one completion can be folded into the model's transcript a second time. | ⬜ | `agent.rs`: the fold must either emit the line to the UI or record that the child has been announced in a way an adopted transcript cannot re-arm |

## 2.5 Observed live: steering a subagent is invisible, and may not arrive

Throughout the session above, the root agent steered its four children with
`agent_control {id, action: "message"}` — the "do not spawn any further
subagents" rule, the result of a child whose parent had never learned it
finished, and where to find a finished sibling's commit. The tool answered
`messaged agent #N` every time, and then:

- **No child's transcript contains a single one of them.** Every agent in
  `.mush/session.json` — the four implementers, their children, the read-only
  auditor — holds exactly one user message: its brief. Counts at the time of
  writing: #1 359 messages/1 user, #2 375/1, #3 277/1, #4 425/1, and so on for
  all ten agents.
- **An idle child was not woken by its own.** #4 had finished its run when the
  message reached it; a nudge to an idle actor is supposed to start a run, and
  #4 stayed at rest.

Two readings, and the difference matters:

1. The nudge *was* delivered — folded into the actor's `messages` at its next
   message boundary — and simply never emitted as `AgentEvent::Message`, so the
   UI's copy (which is what `session.json` holds, and what the human reads) has
   no record of it. That is finding B20's shape exactly: the model sees the
   line, the human does not. It also means the human's picture of a subagent's
   conversation is silently missing every word they said to it.
2. The message never left the UI at all, in which case a steering command
   reports success and does nothing.

Either way this is the same class as §1 and §2: the screen keeps a conclusion
("messaged agent #4") where it should keep a fact the human can check. It also
costs the orchestrator its only lever — with no delivery and no visibility,
"steer the running agent" is not a capability, and the honest workaround is to
spawn a fresh agent with the correction folded into its brief (which is what
this session had to do).

| ID | What | Status | Home |
|---|---|---|---|
| B22 | **A steering message to a subagent is invisible in that agent's transcript, and an idle target was not woken by it.** `agent_control message` answers success; no child's transcript shows the line. | ⬜ | `agent.rs` (`absorb`/`drain_mailbox` must emit the folded nudge to the UI the way the human's own typed message is echoed) and `app` (`agent_control`'s reply should say *delivered*, not *messaged*, when it cannot know) |

## 3. The contract bug that started this file

| ID | What | Status | Home |
|---|---|---|---|
| B21 | **A parent in a tool-calling chain never heard its child finish.** `ChildDone` was *recorded* by `drain_signals` between tool calls but only *folded* into the transcript on a turn where the model called no tools, so an orchestrator that kept working ran arbitrarily far past its child's completion — contradicting `docs/mush.md` §5.5. Seen live: agent #2 spawned #6; #6 finished its whole task (commit `2ab595c`); #2 made 67 further turns, every one with tool calls, and its transcript still held nothing but its brief. | ✅ | the fold moved to every message boundary (the tool-free turn *and* the gap after a batch's results), validated by an independent negative-checked test (`mush/9`) |

---

## 4. For the UX/UI review wave

The wave is expected to add to this file, not replace it: it should re-photograph
the real screen at `docs/mush.md` §4.5's sizes (`scripts/screen.py`, including
`--ask` when an endpoint is reachable), and fold U1/U2 in with everything it
finds — see §11.12 and §4.6 for the two questions that are already open, and
`docs/refactor.md` §6 for B3/B17, whose `Screen` view (Stage 3) is still unbuilt.
