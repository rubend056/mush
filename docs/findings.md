# mush — findings: the queue of record

`docs/refactor.md` §6 defers to this file for "what is broken"; that file is the
structural companion and says where a fix *belongs*. This file says what is
wrong, where it was seen, and where it stands.

Status column: ⬜ open · 🔄 being worked · ✅ fixed (and by which home).

Entries marked **live** were observed in a real mush session — the orchestrator
running this very repository — not reasoned about from the source. Those are the
ones worth trusting hardest: the screen and the transcript are the truth.

**The build the orchestrator itself ran is older than this file.** The session
that produced §1–§5 (and the harness rows in §5) drove a mush built before these
waves landed, and the docs of the day were older still; the §4.5 sweep above is
the exception — it drove a binary built at the wave boundary. So a "live" row is
evidence about *that* build, not about `master`: check the row against the source
before re-deriving it. Two of the §4.5 rows were already fixed by the waves that
landed in between (S5 in `README.md`/`docs/mush.md` §4; S8's (iii)).

---

## The open queue

The row tables owe four items; the structural queue in `docs/refactor.md` sits
beside them. This is the one list: each row carries what the defect costs and
where the fix belongs, and a closure is recorded on the row, never here. §8.23
closed three of the four it listed — H10 by `cc89598`, H17 by `fb012d1`, and
H16 by `8c1a860`, **which was then reverted on the human's decision**
(`de80f9c`, §8.25: the cut is gone, and the file is bounded by reaping instead)
— and opened two in their place:

- **H12** — per-agent token accounting, so a run's cost is visible while it is
  spent. (Open, unchanged.)
- **H16, residual** — the byte cut is gone (§8.25); what bounds the *file* is the
  history window (`CHILD_HISTORY = 50`) applied by `App::reap_history`, so the
  store only ever writes live tree nodes. The per-second deep copy of every kept
  transcript on the UI thread is now paid once a minute instead
  (`SESSION_DEBOUNCE = 60 s`), which is why it stopped being a hitch; an
  incremental save is still owed if a minute's rebuild ever shows.
- **H18** — a parent's own `control message`/`stop` to a *parked* child fails as
  "agent #N is gone": parking ends the actor thread, and the parent's mailbox
  send finds no receiver. The human's path (message, nudge, `/compact`) revives
  it; the parent's does not, and before parking it did (§8.23).
- **H19** — a reaped child's name stays in its parent's books
  (`state.children`/`completed`), so `status` can list a row that is no longer on
  screen. Deliberate in `ab54de3` — dropping it needs a "forget this child"
  message — but it is the one visible inconsistency the window left (§8.23).
- **H20** — the sentences the model is told that are **not** true, where the fix
  is wording in `crates/mush-core/src/prompt.rs` (§8.27 items 2, 5, 9, and the
  `status` schema's "title" in item 1). The human owns that file, so the four are
  recorded rather than edited: `wait`'s 600 s cap and its early release, what a
  job's result actually is (its one line, not its window), the 120 s kill when
  the job budget is full and the 8 MiB output ceiling, and the title a running
  child does not have. The code half of items 1, 3, 4, 6, 7 and 8 landed in
  `ff315d8` (`mush/87`), item 10 in `f34c4de` (`mush/88`) — see §8.29. A second
  wording item the wave added: the `edit_file` schema declares `replace_all` only
  inside `edits` items, though the code now honours a top-level one (item 3).

`docs/refactor.md` §11 is now the ledger of a queue closed except `R6` (judged
and left on purpose); each of its rows carries its price and the commit that
closed it.

The two §6 interactions with H9 are closed: an attach op no longer disarms the
human's armed quit (`626ac3d`), and a stopped agent that owns a live job is
named as stopped (`cc7aae3`).

---

## 1. Observed live: the agent tree lies about who is working

These were seen in the session that produced the M2.75/M2.8 work, while four
subagents and their children ran. They share one cause, and it is R0's cause
again: something on the screen was a stored *conclusion* ("has children",
"busy") rather than a derived fact (`Phase`, and the instant it began).
`docs/refactor.md` §3.1's `AgentTree::busy` was made a method, and the row and
the title now read that one derivation instead of each making their own.

| ID | What | Status | Seen where · closed by |
|---|---|---|---|
| U1 | **A working agent is drawn as paused.** A row renders `⏸` for an agent that is still working, because the glyph comes from "has live children" rather than from the agent's own phase. The orchestrator's four children all showed `⏸` while each was mid-turn — the icon said "waiting" about agents that were busy, which is the exact class of lie §4.5 R0 set out to make impossible. | ✅ | seen in the tree pane, all four rows, whole run — closed by `screen::phase_glyph` (a function of the node's own `Phase` only) with the children as a separate `⏸N` count (`ui::agent_line`), `7a68aea`, repainted from the `Screen` value at `7e123e1` |
| U2 | **The pane title counts waiting agents as working.** The title reads `agents · 6 running · …` while some of those agents are parked waiting on children (and one was stopped). A count is a derived fact like any other: it must be computed from the phases, and it must say what it counts. | ✅ | seen in the agents pane title — closed by `AgentTree::roster` deriving `working`/`waiting` from the phases (`app/tree.rs`), and the pane title (`app/screen.rs`) naming each count, `db72922` |
| U3 | **Scrolling up in a finished agent's transcript is undone by any other agent's news.** While the human reads the scrollback of an agent that has finished, a message from *any* agent still working snaps that pane back to the bottom. The pane's scroll position is a fact about the human's reading, and it is being reset by events that have nothing to do with the agent being read: `app/mod.rs` calls `Chat::scroll_to_bottom()` from seven sites, including the `Message`/`Status`/`Notice` arms of `on_agent`, and `Chat::scroll` is one number for the pane rather than a position owned by the conversation it belongs to. | ✅ | closed by `Chat` keeping a per-conversation `Reading` (`Holding`/`Following`), so only the pane's own agent moves it, and `painted` marking a held window in the title (`scrolled ↑N rows · PgDn`) — `fe62446`, the title mark in `6c175b0` |
| U4 | **A grandchild is not drawn under its parent, but after everything spawned before it.** The tree keeps `agents` as a `Vec` in spawn order (`AgentTree::agents.push`, `app/tree.rs`), and `ui.rs` only uses `node.depth` to indent: so a depth-2 agent appears below every previously spawned agent, not under the agent that spawned it. The parent link and the depth are both in the node — the *order* is the thing that was never derived. | ✅ | closed by `AgentTree::rows` walking pre-order over the parent links, so a child sits under its parent's subtree — `0527b0a` |


## 1.5 Observed live, again by the human using it

| ID | What | Status | Home |
|---|---|---|---|
| U5 | **The same fact is on screen three times.** The newest tool call / activity shows in the conversation pane (as the `⚙` line), again at the bottom of the agents pane, and again in the first line of the status bar — one fact, three homes, no reader. §4.5 R2 spent line one on "activity › status › hint"; the activity is already the row's and the transcript's, so line one is repeating what a human can already see, and the three surfaces need to be looked at together rather than one at a time. | ✅ | closed by `App::tree_line` — the bar's line one is the napping-root fact, or an event with no other home, ranked through `chat::Rank` — `a5f2a9f` |
| U6 | **An agent is a bare number.** Rows read `#2`, and everything else is inferred from a message; nothing names the *task*. A short title per agent, derived from its brief (and a command label for a job), would let the human tell two children apart without opening them — the brief is already in the node and in the transcript's first line. | ✅ | closed by `AgentNode::title` (`app/tree.rs`), derived from the brief on read — `bacc48b` |
| U7 | **A waiting agent still says `⠏ working…`.** When an orchestrator has ended its turn and is waiting on children (or on a job), the activity row claims work is in flight. It should say so differently from a model call that is actually in flight — the hourglass the human asked for — which is the same derived-facts rule as U1/U2, one surface further down. | ✅ | closed by `Phase::waiting` (`app/tree.rs`) telling a model call from `wait`; the row and the foot say which — `e47a680` |
| U8 | **A transient notice never leaves.** `· reply cut off at 20480 tokens — asking for smaller steps` and help output sit in the foot forever (until that agent runs again), so a line about *one moment* outlives it and pushes the conversation around. §4.6's per-kind lifetime answered this for failures and command answers; the "said" rank still has only one lifetime. Somebody must decide which notices are news and which are chatter — and repeated identical lines (`· model produced an empty reply` ×N) should collapse rather than repeat. | ✅ | closed by the chatter lifetime in `app/chat.rs` — `clear_notes_for`, `dismiss_said`, `SAID_TTL = 120 s` — and the `Notice.count` collapse, `b71f67e` |
| U10 | **Walking back up a deep tree costs one keypress per ancestor.** In the agents pane the only vertical moves are `j`/`k`, arrows, `g`/`G`: with twenty children under one parent, getting from a grandchild back to the root is twenty presses, or `g`, which loses the place you were reading. `←` should put the selection on the agent's **parent** (and the natural companion, `→`, on its first child), which is a fact the node already carries (`AgentNode::parent`) and which the painted order (U4) makes meaningful. It needs the same treatment as every other binding: one `Intent` in `app/keys.rs`'s table, the module doc and `--help`'s KEYS prose updated in the same commit, and a test at three levels of depth. | ✅ | closed by `Intent::TreeWalk` on `←`/`→` in `app/keys.rs` (plus `PickerMove(±PAGE)` for a deep picker); pinned at three levels of depth — `1980588` |
| U9 | **The default DeepSeek window/reply cap is far too small.** A real run was cut off at 20480 tokens; for the configuration mush ships, the default should be ~120k tokens (window, and the reply cap where the vendor accepts it) rather than a value that truncates ordinary work. Precedence must not change: a human-stated window still wins, and an endpoint-reported one still overrides the default. | ✅ | closed by `provider::PROVIDERS`' fallback of 120 000 and `Config::reply_cap` (a quarter of the window, floored at 1 024 and capped at 120 000) — `d84ecb6` |
| U11 | **Compaction happens with no visible state anywhere.** The human typed `/compact` and could see no indication that anything was happening: no "folding…" status, no progress, no answer to the only question a frozen pane raises — *may I keep typing?* Three quarters of the shape is already built (`/compact` says one line in the bar; `compact_history` emits `AgentEvent::Status("compacting on request…" / "context nearly full — summarizing…")` and later `AgentEvent::Compact`; §4.6's notice machinery exists), and the gaps are: (1) a `/compact` that arrives **while the actor is running** is *parked* (`state.compact_requested = true`, honoured only at the next message boundary in `agent.rs`) and nothing at all says so — the human waits for a bar line that has already faded; (2) nothing *distinguishes* a fold from an ordinary model call or from waiting on children, at the moment when the model call can take the whole 10-minute deadline (×3 with the transport retry); (3) nothing says whether sending is blocked. It is not: mush never blocks input, the words queue and are answered after the fold — which is exactly the fact the human needs and the screen never states. **And the sharpest form of it, found in the source after the human added "compaction doesn't wake up agents?? … that's odd": an idle fold is work the screen refuses to show at all.** `AgentTree::activity` (`app/tree.rs`) opens with `if !self.is_busy(id) { return; }`, so for an agent at rest the `AgentEvent::Status("compacting on request…")` the actor emits *before* its summarize call is **dropped on the floor**: the agent pays for a real blocking request (up to the 10-minute deadline, ×3 with the transport retry) while its row still reads `·`/`✓`/`✗` and every derived surface says "at rest". Only the *after* line (`context compacted — continuing from a summary`) ever appears. The one setter that could move the agent refuses to move an agent that is not already busy — and the idle fold's cancel flag is minted locally (`AtomicBool::new(false)` inside `compact_now`), so nothing can cancel it either. | ✅ | closed by `Phase::Compacting(Compacting::{Parked, Requested, NearlyFull})` with `Phase::compacting()`/`words()` in `app/tree.rs` — the row glyph (`≡`), the foot, the footer, the pane roster and `App::tree_line`'s sentence all read that one answer — by `compact_now` owning the fold's cancel flag (so Ctrl-C reaches an idle fold; it was minted and dropped inside the call and nothing could flip it), and by `agent.rs` emitting `Compacting` on *accept* (`Parked` from `drain_signals`, `Requested`/`NearlyFull` from the two callers). The row's own correction, verified against the source: the *automatic in-run* fold was already visible (its `Status` line survives for an already-busy agent); the genuinely silent cases were the fold requested at rest — whose `Status` the busy guard dropped — and the one parked behind a running tool, which emitted nothing at all. Merged in `fb7265d` |
| U12 | **The pane title and the bar disagree about a stopped or failed root that still has a child working.** `AgentTree::roster` counts a waiting agent only when the phase is `Idle \| Done` ("a failed or stopped agent waits for nothing"), while `App::tree_line` counts `busy_children > 0 && !phase.is_busy()` — so a stopped root over a running child gets `0 waiting` in the title, `waiting on 1 subagent(s) — the root resumes as they finish` on the bar, and `⊘ … ⏸1` on its row. The bar is right: the root *does* resume when the child's completion folds in (`absorb` → `Fold::Run`), and the row's `⏸N` already says so. Found by the duplication review of `fb7265d`. | ✅ | one predicate now: `AgentTree::napping` (`!node.phase.is_busy() && busy_children(id) > 0`) is read by the title's bucket and the bar (`642fda8`), and `the_bar_and_the_title_agree_on_who_the_root_waits_for` stops the root |
| U13 | **After a restart, a stored isolated agent whose worktree is gone keeps a branch its actor does not have.** `restore_agents` passes the stored `branch` straight to the node (`app/mod.rs`), while `revive` filters it on `worktree_path(root, id).exists()` and points the actor's workspace at the root — so the restored row offers `/diff`/`/merge` for a reclaimed directory and the footer paints a dead path, while a nudge is refused by the UI guard even though the actor would have run it in the root. Two surfaces contradicting the promise both restore paths make ("continues in the main checkout"). Found by the duplication review of `c4aa2e3`; untested (both restore tests store `branch: None`). | ✅ | one decision now: `agent::live_branch` is the one place a stored branch is filtered, shared by `restore_agents`, `revive` and the node (`642fda8`), and `a_restored_branch_whose_worktree_is_gone_is_dropped` stores one and asserts the node drops it, the nudge is delivered and `/diff` stops naming it |

## 2. Observed live: a delivered completion is invisible, and can be delivered twice

| ID | What | Status | Home |
|---|---|---|---|
| B20 | **A child's completion reaches the model but not the screen, and can be folded twice.** `fold_completions` (fixed below) pushes the `#N done: …` line into the actor's `messages` only; no `AgentEvent::Message` is emitted, so the UI's copy of the parent's transcript never shows it. The next idle `Run` then hands the UI transcript back, `absorb` finds no such line in it and re-arms delivery for the same child — so one completion can be folded into the model's transcript a second time. | ✅ | closed by `agent::push_line`, which folds the line into the actor's transcript *and* emits it as `AgentEvent::Message`, and by `absorb` marking whatever completion line an adopted transcript already carries as delivered, so it cannot re-arm — `bd81fa9` |
| B24 | **A failure the model has already read is folded into its transcript again — the whole tree's, at once.** Seen live: the orchestrator finished a run (a tool-free turn), and then mush woke it with a user message holding a *batch* of children's `#N failed: …` lines for agents whose work had already been redone and committed; the orchestrator's answer was "Nothing to re-run — these are the replayed failure notices for work that was already redone and committed". Three facts in the tree allow exactly that. (1) The delivery mark is **cleared** whenever an outcome is recorded again: `note_completion` (`agent.rs:1801`) does `state.delivered.remove(&id)` unconditionally, and the *recording* paths call it — `drain_signals` (`agent.rs:1723`, during a run) and `drain_mailbox` (`agent.rs:1766`, at a boundary) — so a `ChildDone` for an outcome the model has already read re-arms the fold that happens at the boundary. (2) What makes a completion look "already read" on adoption is a **string scan that only knows one of the three outcome shapes**: the `Run` arm looks for `format!("#{id} done:")` (`agent.rs:956`), so a transcript carrying `#N failed: …` or `#N stopped: …` is never recognised as having read that child — which is the very shape a replayed failure has. (3) The marks live only in the live actor's `ActorState`, so a restart loses them entirely while the restored transcript still holds the lines. The rule the code *intends* ("delivered once, folded into the transcript, never twice", `agent.rs:437`) needs an identity per outcome (a run sequence on `ChildDone`) instead of a set that is cleared by re-arrival and re-derived by matching text. | ✅ | closed by a per-child run counter: `ActorState::runs` is incremented in `actor_main` where a run's outcome is decided and travels on `AgentMsg::ChildDone { id, run, outcome }`; the parent keeps `completed: child → Completion { run, outcome }` and `delivered: child → run`, so the fold rule is "the recorded run differs from the run the model has read" — a re-record of the same run is inert and reaches no model, while a *second* run failing with byte-identical text still folds once. `note_job`'s unconditional `remove` is gone with the two boundaries that push a job line guarded, and the `Run`-adoption scan reads all three shapes through `Outcome::line` rather than `format!("#{id} done:")`. Merged in `fb7265d`; its five tests fail before and pass after (`a_child_run_reported_again_is_not_folded_twice`, `a_job_report_recorded_again_is_not_folded_twice`, `a_replayed_batch_delivers_each_unread_outcome_once`, `adoption_reads_a_failed_or_stopped_line_as_delivered`, and the guard `a_second_run_failing_the_same_way_is_news_again`). The integrator could not reproduce the *live* shape through the binary (one sender per run, one delivery per mailbox, so the interface cannot re-record the same run) and drove it at the seam instead |

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
| B22 | **A steering message to a subagent is invisible in that agent's transcript, and an idle target was not woken by it.** `agent_control message` answers success; no child's transcript shows the line. | ✅ | closed by `AgentMsg::Steer` → `push_line` (folded *and* emitted) and its `Fold::Run`, which starts an idle target, `5319b2b`; the reply half is H5 (`88bc03c`) |

## 2.75 Observed live: a hiccup on the wire kills a whole run

| ID | What | Status | Home |
|---|---|---|---|
| B23 | **A transient transport failure ends the run instead of being retried.** Several agents in this session died mid-work with `cannot reach https://api.deepseek.com: Connection reset by peer (os error 104)` — one of them had committed nothing, another was killed by the *harness* process dying around it, and a third lost a run's worth of edits. Every one was a transport hiccup, not a refusal: the endpoint had no opinion about the request. A bounded retry (three attempts with a timeout, backing off) for *transport* failures only — never for a cancellation, a status the endpoint chose, or a body it deliberately sent — would turn "the run is dead and its worktree is half-edited" into "the run paused for a second". Two rules make it honest: Ctrl-C must still abandon a request immediately (the cancel flag is polled between socket slices and must be checked between attempts), and the human must be told (`retrying — connection reset (2/3)`) rather than watching a spinner that looks stuck. | ✅ | closed by `model.rs::retrying` — `RETRY_ATTEMPTS = 3` over the `Clock` seam, transport failures only, the cancel flag read before every attempt and between backoff slices, each retry announced in the transcript — `7af7cef` |
| B25 | **A signal during a model read is reported as a failure of the endpoint.** Found while reproducing the fold's visible state: a `SIGWINCH` (a terminal resize) arriving during a held read surfaces as `could not compact: cannot reach http://…: Interrupted system call`, and the same read under an ordinary run ends the turn with that text — `EINTR` is a retryable read, not a refused connection, and it is currently classified like one (`http.rs`'s read loop treats the error as terminal, and `model.rs`'s transport classifier deliberately excludes `Interrupted`). A human who resizes the window while mush is answering can therefore lose a run to their own window manager, and the message blames the endpoint. | ✅ | closed by `http.rs::retrying_interrupted` (`f313916`, merged `883769f`): every IO site — write, flush, read, connect, TLS handshake — makes a signal-interrupted call again, with the cancel flag and the deadline consulted on each interrupt; `model.rs::transport` still excludes `Interrupted` and says why |
| B27 | **A framing error on the wire kills a run, and blames the endpoint.** Observed live: an agent's run died with `the endpoint's reply was refused: malformed chunk size: ""` — `read_chunked`'s error for an empty chunk-size line (`http.rs:864-899`). B23's retry covers *transport* failures and B25 covered signals; a chunk-framing failure is `InvalidData`, so it is classified as a body the endpoint deliberately sent: it is neither retried nor questioned. The stray empty line is at least as likely to be *our own* leftover framing — a kept connection returned to the pool with its chunked body unconsumed after an early stop — as the endpoint's opinion, which makes the diagnosis wrong as well as the verdict. And the run's death reaches its parent only when something later asks for the child's state, so a run can be dead for a while before anyone is told. | ✅ | closed by `c5694f1` (merged `7e0440f`), and the row's suspicion was half right: the break *was* ours, but it was not a pooled connection — `read_chunked` read a chunk-size line with `unwrap_or_default()`, so an EOF met where a size was expected became an empty size line, and `InvalidData` was classified as a refusal. Now a framing break is its own class (`http.rs::Framing`, `body_cut_off()`) mapped **before** the `InvalidData → Refused` arm, so a body past the 80 MB cap stays a refusal and is never retried, while `retrying` repeats `Transport | Framing` on a fresh connection and never a cancellation or a status the endpoint chose; the wording says the reply broke instead of blaming a refusal, and a non-focused child's failure now lands on the bar. The unusable-body/poisoned-pool hypothesis was refuted rather than fixed, and recorded in the `Pool`/`exchange` docs so it is not re-opened |

## 3. The contract bug that started this file

| ID | What | Status | Home |
|---|---|---|---|
| B21 | **A parent in a tool-calling chain never heard its child finish.** `ChildDone` was *recorded* by `drain_signals` between tool calls but only *folded* into the transcript on a turn where the model called no tools, so an orchestrator that kept working ran arbitrarily far past its child's completion — contradicting `docs/mush.md` §5.5. Seen live: agent #2 spawned #6; #6 finished its whole task (commit `2ab595c`); #2 made 67 further turns, every one with tool calls, and its transcript still held nothing but its brief. | ✅ | the fold moved to every message boundary (the tool-free turn *and* the gap after a batch's results), validated by an independent negative-checked test (`mush/9`, `e841e05`) |

---

## 4. For the next UX/UI review wave

The wave this section was written for has landed: U1–U10 are closed (§1, §1.5) and
§4.5 of `docs/mush.md` was rewritten against the tree. The instruction stands for
the next one: add to this file, do not replace it, and re-photograph the real
screen at §4.5's sizes (`scripts/screen.py`, including `--ask` when an endpoint is
reachable). The one home that was still unbuilt when this was written is
`docs/refactor.md` §6's **B17** — the `Screen` value and the draw sweep that
asserts painted text (Stage 3). B3 is closed: below the floor every key but
`Ctrl-Q` is refused.

---

## 4.5 Driven end to end: the stories a human tells

A usability pass drove the real binary over a pty through the stories a human
actually tells — isolate a child, read its work with `/diff`, land it with
`/merge`, restart, steer, quit — and reported where the story breaks for a
person in earnest. The `S` rows are that pass. What worked is at the end; do not
re-run it.

| ID | What | Status | Where it breaks |
|---|---|---|---|
| S1 | **After `/merge` (or `/discard`), re-running that agent writes into a worktree nothing can see.** Merge agent #1 (row `· merged mush/1 into HEAD · mush/1 deleted`, `git worktree list` back to the main tree, `git branch` back to `master`), focus it again, ask for one more change: the pane says `⚙ write_file extra.txt` / `wrote extra.txt`, but the file lands in `.mush/wt/1/extra.txt` — a *plain directory* `write_file` recreated at the reclaimed path. `git status` is clean, `git worktree list` lists nothing, `/diff 1` and `/merge 1` say `agent #1 has no worktree branch (not isolated)`, and `/worktrees` says there are none. Every surface denies the file exists, and the work can never be landed. `AgentTree::land` clears the branch (its comment says the new run "happens in the main checkout"); it does not — the node still carries the worktree path, so the next run's file I/O goes back to the dead path. | ✅ | closed by refusing, not by re-isolating: `App::worktree_gone` refuses the human's message before it is sent (the words stay in the message box) and `absorb` refuses `Nudge`/`Steer` in the actor, so `agent_control message` is covered too; the line is `agent #1 was merged — its worktree is gone; spawn a fresh agent or work in the root`. Re-isolate was rejected because it needs fresh worktree/branch plumbing and the branch back onto the row, whereas an actor has no business running in a workspace that is gone. Landed in `c4aa2e3`; two real-git tests (`a_nudge_to_a_merged_child_is_refused_not_run_in_the_phantom_path`, `…_discarded_…`) assert the path is not recreated and nothing is written |
| S2 | **`Enter` on an agent row focuses the transcript but not the keyboard — the message you type is eaten.** After `Enter` the pane is `agent #1` and the box prompt reads `#1 ›`, but the bar still says ` agents `: the tree kept the keyboard. Typing `hi again` sends nothing (0 model requests); the tree cursor moved and the visible pane silently switched back to the root — and a stray `c` in the text would have cancelled the agent. Only after a further `Tab` does the nudge reach the child. `README` promises "`Enter` on a row focuses that agent — the chat switches to its transcript and typing nudges it". | ✅ | `Enter` shows the row's transcript while the tree keeps the keyboard, and the bar's `agents` badge makes the split visible instead of silent (`157bbd0`, reversing `c4aa2e3`'s move of the keyboard with the focus; `Tab` is what puts the keys in the box) |
| S3 | **A `session.json` mush cannot parse is dropped silently, then overwritten.** A 648-byte session with a long conversation and one agent, if any field does not match the schema (a bisect showed `"status": "failed"` — the real encoding is `{"failed": "…"}` — is enough), comes back as an empty app with the empty-state hint, no warning on screen, no entry in `/notes`. `Session::load` returning `None` is indistinguishable from "there is no session file", and the first save rewrites the file: the old conversation is gone, with no backup. Docs §5.2 promise "Restarting mush brings the session back at rest", and mush is the only writer — so version skew or a hand edit reaches this. | ✅ | closed in `e747bc3`: `Session::load` returns an absent/unreadable outcome, `main.rs`/`app/mod.rs` paint the path, the parse reason and `kept as .mush/session.json.bak`, and the unreadable file is copied byte-identically to `.bak` before the first fresh save (`an_unreadable_session_is_told_apart_from_an_absent_one`, `the_unreadable_session_notice_names_the_file_the_reason_and_the_backup`, and the real-binary check: the `.bak`'s sha256 matched the original's after a clean quit that wrote a new session) |
| S4 | **Ctrl-Q does not kill a foreground `run_command`'s process group.** A root `run_command` of `sleep 10; touch marker` is still alive after mush exits cleanly, and the marker appears 10 s later; a `sleep 40` was resident the moment after quit. Detached *jobs* are killed correctly (the heartbeat job stops at Ctrl-Q). Docs §5.6 rule 2 promise "they die with … mush itself — its process groups are killed on exit". | ✅ | closed in `e747bc3` by `jobs::Foreground` + `Registry::hold(owner, job)`: a foreground tool call's process group holds a slot for the whole call, so `kill_all` (quit), `kill_owned` (`Stop`/`/new`/`Shutdown`) and `Registry::drop` reach it as they reach a job, while `Launch::started`/`Launch::held` keep the run-once guarantee and nothing is signalled in `Foreground::drop` (a finished command's pgid may be reused). Real binary: the `sh -c` is gone the moment mush exits, the marker never appears, a detached heartbeat job still stops, and a self-exit still reports its real status (`quitting_kills_a_running_foreground_command`, `a_foreground_command_killed_from_outside_reports_cancelled`, `the_three_ways_a_foreground_command_ends_are_not_confusable`) |
| S5 | **The docs and README disagree with the keys for stopping agents.** Live, on a build whose docs predated the key wave: `Ctrl-C` stops the focused agent and the child kept working; `Ctrl-X` stops them all. `mush --help` and `/help` say exactly that, and so do `README.md` (its anywhere-row and its key table now carry both) and `docs/mush.md` §4's key table — the drift the row recorded was real and is closed. A human who learned the keys from an *older* README pressed `Ctrl-C` on a runaway tree and one agent kept burning tokens. | ✅ | closed by the doc wave: `README.md` and `docs/mush.md` §4 both name `Ctrl-C` (focused) and `Ctrl-X` (all) |
| S6 | **`/compact` on a transcript with nothing in it does nothing and says nothing.** Immediately after launch, `/compact` paints the bar's `compacting #0…` and then the untouched empty state forever: 3 s later the same, `/notes` empty, nothing on the wire. After any run the documented refusal does appear (`· nothing to compact — this transcript is already short enough to send whole`), because the guard is `matches!(messages.first(), Some(system))`, which is false for an empty transcript and returns without a word. Docs §3 promise "The refusal is said out loud because a human typed a command — silence there is indistinguishable from a fold that quietly failed". | ✅ | closed in `e747bc3`: the empty transcript says the same refusal as the short one (`a_fold_of_an_empty_transcript_says_so_instead_of_nothing`), and the automatic trigger is still told nothing |
| S7 | **In a non-git workspace, `isolated: true` runs in the shared workspace and only the model is told.** In a plain directory, asking for an isolated child puts `iso.txt` in the root, the row has no branch, and the spawn line says nothing; the model's request *did* carry `(isolated unavailable: not a git repository; running in place)`, but no notice reaches the pane, `/notes` or the bar. Two "isolated" siblings would edit the same files while the human believes otherwise. Undocumented either way. | ✅ (`e747bc3`) then **superseded by §8.13/§8.17**: the `isolated` boolean is gone, so there is no degraded-isolation path left to silence — `base` *is* the switch, and a `base` git cannot resolve is refused before anything is created. The test this row named no longer exists, and §5.5's `isolated unavailable` sentence went with the §8.13 doc-sync; the row's `write_file` story went with the six-tool cut |
| S8 | **Four places where the screen or the docs read badly, none of them a lie at the seam.** (i) The commit subject is the brief **truncated at 60 chars with `…`** (`mush #1: create a file iso.txt containing exactly: isolated w…`), so a landed commit's history cannot be matched to the brief verbatim, while docs/README write it as `mush #N: <brief>`. (ii) `/diff` paints only the tail two rows of the diff above a `+N more lines · /notes` row, so no `+`/`-` line is visible until you type `/notes` — "read `/diff 2`" does not, by itself, show the change. (iii) the pane title says `agents · 2 working · 1 waiting` where docs §4.5 said `2 running` — ✅ corrected, and the example frame no longer draws the marker glyph the pane stopped having. (iv) `scripts/mock_llm.py`'s `TURNS` scenario waits for the phrase "turn limit", which the prompt no longer contains, so that scripted run ends on the loop guard instead; `README` and `docs/mush.md` §10 already agree that no test refers to the script, so only the scenario is stale. | ✅ | (i)/(ii)/(iii) landed in `c4aa2e3` (`subject_brief` takes the brief's first line and cuts at a word boundary; `/diff` folds each file's git preamble into its first hunk row, so a `+`/`-` line is visible without `/notes`; the pane-title wording was corrected); (iv) in `f70374f` — `scripts/mock_llm.py`'s `TURNS` scenario waits for `runaway guard`, the wrap-up instruction's real phrase |

**The same pass, driven and found sound (do not re-run).** Isolate → commit →
`/diff` → `/merge` on a real repo (`git worktree list` → `.mush/wt/1 [mush/1]`;
one commit whose subject is the brief; `iso.txt | 1 +`; the diff in `/notes`;
merge lands the file, reclaims the worktree and deletes the branch; a second
`/merge`/`/diff` says `was already merged`). Restart: the tree comes back with
its rows at rest, the landed state persists, a leftover worktree is re-registered
and `/worktrees` reports it, `/discard` removes the worktree and branch and keeps
the row. `/diff`'s edges: `no agent #99`, `not isolated`, `±0 — nothing changed`,
a clean no-op merge. `/notes` reads the whole foot (a diff, a refusal, a stored
failure) as a scrollable popup. Restart mid-run: rows at rest, **no replay**, no
phantom file. `/compact`: idle folds into a summary, a request mid-reply parks
and is honoured at the boundary, the automatic fold runs before the next request.
Detached job: `[still running — detached as #c1…]`, one `#c1 done: exit 0 · 6s`
folded in, killed at quit. Scrollback/typing seam: a child finishing while the
root is scrolled back 3 pages with a draft leaves both alone. A detached HEAD
repo: the child gets `mush/1`, `/merge` fast-forwards the detached HEAD. Tree
pane: pre-order nesting, `▼N`/`▲N`, `PgDn`, cursor clamp, `Enter`/`Esc`.

---

## 5. What made orchestrating mush hard (the harness, seen from inside)

The root agent of the session this file was written in ran a hundred-odd
subagents in parallel worktrees, and drove five review waves and five fix waves
through them. These are the gaps that cost it real work — each one is a fact it
could not get, or a fact that went stale, not a missing feature. `M3` (attach)
and `M6` (per-agent accounting) are the milestones they belong to; the rest are
cheap.

| ID | What | Cost, in this session | Where it belongs |
|---|---|---|---|
| H1 | **No live view of a subagent.** The only window into the tree was `.mush/session.json` (3.7 MB, the *UI's* copy), so "did #2 ever see #6's result?" had to be answered by hand-parsing JSON. That copy cannot show what the actor knows — `delivered`, parked commands — which is the root of `B20`/`B22`. | The session's central diagnostic was archaeology, and the first diagnosis was wrong *because* the file could not say what the actor held. | ✅ the tree half (`M3`, `f29b352`), then the actor-side facts (`3b6602d`): `status` lists each finished isolated run's worktree fact — its branch, and committed, clean or failed-to-commit — through `AgentMsg::Work`, the attach roster carries `result_unread`/`unread_children`, `session.json` stores `result_unread`, and a spawn reply names the branch it made. Deliberately not persisted: parked commands, which belong to a run a restart kills and would put unread words in front of the model |
| H2 | **A run that was cut off looks exactly like one that finished.** Agents #1 and #2 died when the harness process was SIGTERM'd and #18 died on a transport reset; on screen and in the file they were simply "idle", with no summary and no marker, and their work sat uncommitted until someone went looking. | Two runs' worth of work recovered by hand; four later agents spent their first minutes finishing someone else's tail. | ✅ written to `session.json`, painted on the row as `⚠ cut off`, and reported to the parent as `cut off, nothing committed` (`eab825e`, folded in from `mush/90`: `4e12e52`, `9fdce7f`) |
| H3 | **An isolated run's automatic commit says `mush #N: <the whole brief as typed>`.** It saved the biggest branch of the wave from a killed process — and then had to be amended by hand because the subject was an 800-word paragraph. | One commit message rewritten; the auto-commit hides what a merge body then has to explain. | ✅ `commit_subject` takes the brief's first line cut at a word boundary (or the outcome) for the subject and keeps the brief in the body (`c4aa2e3`) |
| H4 | **Nothing tells the human that a child finished**; only the parent's transcript hears it (after the fold fix), and the row's mark changing is all the screen says. | A whole review pass answered "did #2 see #6?"; the human asked the same question. | ✅ the child's row wears `✉` for a result nobody has read, the parent's `✉N` counts them, and `result_unread`/`unread_children` ride the roster (`eab825e`, from `mush/90`, then `3b6602d`) |
| H5 | **Steering was not a capability you could trust.** `agent_control message` answered `messaged agent #N` for four agents, none of whose transcripts held the line, and an idle target was not woken. Fixed on `mush/15` (`AgentMsg::Steer` → `push_line`), but the *reply* still says "messaged" whether or not anything happened. | Four agents worked for an hour without the rule they were sent — including "do not spawn any more subagents". | ✅ fully: the delivery (`AgentMsg::Steer` → `push_line`, folded *and* emitted; it is work to answer, so an idle target wakes) and the reply (`88bc03c` — at-rest "this resumes it" or mid-run "read at its next step", never a flat "messaged") |
| H6 | **Timing-sensitive tests in a suite that runs while ten agents build on one box.** `wait_commands_returns_a_jobs_report` failed about half the time under load (a test racing a clock it had told to lie; fixed), and `a_frame_fits_in_a_60fps_budget…` is still load-sensitive. | Every flake costs an agent a retry it cannot tell from a real failure, and a gate that is green "usually" is not a gate. | ✅ `e8d80c2`: the budget test says it needs an idle box and is `#[ignore]`d — run it deliberately, alone, with `cargo test -- --ignored a_frame_fits`; the other flaky test (`wait_commands_returns_a_jobs_report`) was already fixed by its clock |
| H7 | **A spawn cannot name its base.** An isolated child branches from its parent's working tree — usually right, occasionally exactly wrong ("start from `master`"), and then merges had to be done by hand. | Merge labour; one branch re-cut. | ✅ (`8a833ed`) then superseded by §8.13: `spawn_agent {base}` resolves a branch, tag or sha before anything is created and its reply names the commit read back from the new worktree's HEAD; §8.13 removed the `isolated` boolean, so a base *is* the isolation switch |
| H8 | **No picture of the machine.** Each parallel worktree pays its own `cargo build`, so with a dozen agents the box is the bottleneck and nothing on screen says so. | Self-imposed serialisation; the same tree compiled many times. | ✅ for the warning half (`d753db8`): the pane title carries the whole tree's running-command count (`2 jobs`), so the box's load is a fact on screen; a shared `target/` was not added — the worktrees still each build alone, which the title now shows |
| H9 | **Quitting kills the agents' process groups — including agents mid-task.** Correct per §5.6, and exactly how #1/#2 lost their runs when the harness went down. | Two runs. | ✅ `integ57`, merged as `ae16cb2`: the first `Ctrl-Q`/`/quit` arms a two-step quit and paints what it will kill (`Ctrl-Q again quits · kills #0 run_command + 1 job`), composed from the one in-flight list `Ctrl-C` already uses, bounded to 72 columns with what does not fit counted as `+N more`; the second press quits, `Ctrl-C` or any typing key disarms, and nothing live quits on one press. The ranking (`Quit` outranks the tree's own line) lives in `app/screen.rs`'s `bar_word`. A detached mode was not added |
| H10 | **Worktrees and branches accumulate and nothing prunes them.** Twenty-two were live at the end, most finished and merged; `/worktrees` also claims "none" while one is on disk (`P10`). | Disk, and two audits that counted the source twice until it was cleaned. | ✅ for discovery (`e63a84c`): a worktree git says is gone gets no row. **Still owed is a `mush prune`** — nothing reclaims a finished, merged worktree and its branch — and the `/worktrees` reporter that once counted what was on disk was deleted in `ad5b791` (§8.17) |
| H11 | **Docs and code drift silently, and the drift *is* a finding.** One wave left `docs/mush.md`'s key table, §4.5's glyphs, §4.6 in full, §8's deadline and §9's milestones stale, plus `docs/refactor.md`'s checklist statuses; every reviewer spent budget on it and one fix wave existed only for sentences the docs asserted. | Repeated re-derivation, and a doc-sync wave owed at the end of every wave. | ✅ in practice, not by a test: this file's one row per finding is the status record, and each wave's `Record …` commit moves it in the same commit as the code |
| H12 | **Context and reply caps were the quiet bottleneck.** A 20 480 reply cap truncated real work mid-task (`U9`), and several agents burned turns on runaway guards and compaction instead of the task. | Several runs cut off mid-edit. | ⬜ still owed: per-agent token accounting (`M6`, §11.2), so a run's cost is visible while it is spent; the shipped defaults are done (`U9`) |
| H13 | **The exclusive machine lock is machine-wide, and a refused command is a trap.** The harness's lock is taken by a *command* (`exclusive=true`), which the tool's own guidance recommends "for anything timing- or port-sensitive" — and every review brief flags the 60fps frame test as load-sensitive, so a reviewer takes it to get one clean number. While it is held, every sibling's command *and the orchestrator's own* is refused ("#N holds the machine; retry when it finishes"); the refusal is an **error, not a queue**, so an agent that retries it is doing exactly what `LOOP_ROUNDS` counts, and mush's own guard then kills the run: `#65` and `#66` died mid-work this way, and the orchestrator could not even read a file for the duration. | Two agents' runs lost (one integrator, one fixer), a stalled wave, and an orchestrator blind at a moment it had to inspect. | ✅ all three (`4cb4739`, `1df1a53`): a refusal before anything ran is `ToolError::Refused` and never a loop round, the refusal names the holder and says not to retry in a loop, a sibling's command queues for the lock (bounded at 30 s, cancel-aware), and the root is exempt from a lock it did not take and is told it ran beside `#N`'s exclusive command. A root *exclusive* command is still refused — two claims to own the machine is what the lock prevents |
| H14 | **A run stopped as a loop cannot be resumed.** `Ctrl-C`-stopped agents resume when you message them (that is the promise on the row: `stopped · re-send to resume`), but two loop-stopped agents (`✗ the run was stopped as a loop: the same tool call repeated 6 times with nothing changed in between`) re-stopped **immediately and identically** on the nudge — so the one place a human would first try to recover is where resuming does not work. Either the repeated call is still in the window the guard counts, or the resumed run's first call is counted against the old rounds; either way the orchestrator had to spawn a fresh agent with a rebuilt brief (cheap only because the dead one's work had already been committed — `c3f5984`). | Two agents re-spawned instead of nudged; the loop-stop's own advice ("re-send to resume") is unactionable. | ✅ (`4cb4739`): a loop-stop records the count it stopped at, and the next run opens with the guard's own words, so a nudge resumes it (`a_loop_stopped_run_resumes_with_a_warning`) |
| H15 | **`wait_agents` answers from history, and nothing says what a wait releases.** After the crash the orchestrator re-spawned six children; its first `wait_agents` (no ids, a long timeout) returned the summary of `#10` — a child of the *previous* process, finished before it died — which reads exactly like a live completion. Spooked, it then named one id, and `ids=[…]` means "wait only for that one", the opposite of the intent (be woken by *any*). The tool's own description calls the answer "its summary" and never states the release rule, so the call has to be reasoned out from the implementation: no ids = first finish, `ids` = only those, `all` = every child, `timeout` = a bound. | A blind 900 s wait while two children were dead, and one result that reported the past as if it were news. | ✅ (`4cb4739`): `status` is a bounded listing (`Outcome::digest`; `✉` marks a result nobody has read), an unread result comes over in full where an already-read one is a digest, and the descriptions state the release rule — later narrowed again by the twelve-to-six cut (`ba04c49`), where `wait` takes no arguments and releases when every child and every job the agent owns has finished |

**What already made it easier, and should not be traded away:** one worktree per
isolated agent with its own branch, and mush committing that worktree's work when
the run ends (it saved the whole `mush/4` milestone from a killed process); the
`ModelClient`/`Machine`/`Clock`/`Events` fakes, which let every fix be tested with
no socket, no subprocess and no sleep; `scripts/screen.py` and `smoke.py`, which
are the only reason a UX review could quote painted rows as evidence;
`docs/refactor.md`'s symbol-level map, which made a 20 000-line tree navigable by
agents with no memory of each other; and this file's one-line-per-defect shape,
which is what let five waves hand work to each other without losing an item.

## 6. The attach boundary, reviewed (M3, `f29b352`)

The socket (`attach.rs`, `<root>/.mush/mush.sock`, newline-delimited JSON with
`read`/`agents`/`focus`/`edit`) was reviewed against the real binary, driving raw
lines with `python3` at a held socket. It is sound where it matters — one wire
type, one parser and one `encode`; `App` stays the only effector (the socket
thread only sends `Msg::Attach`); a malformed line is answered from the socket
thread and the connection survives; `id` is echoed on every reply including
errors; `advance` never steps the revision back *within* a conversation. What
follows is what it does not hold.

| ID | What | Status | Home |
|---|---|---|---|
| A1 | **The revision steps backwards across `/new`, so a client silently desyncs and a stale `edit` lands.** `Chat::clear`/`forget` drop the counter, so it restarts at 0 and collides with a revision the client already holds; nothing on the wire carries the conversation's identity, though `ConversationId` exists for exactly that. Raw wire, real binary: after a first message `read` says `revision 1` with `line 0 = "first message"`; after `/new` it says `{"lines":[],"revision":0}`; after a second message `revision 1` again with the new line at `line 0`; a client polling `since=1` never sees it, and its `base=1` edit is **accepted** (`{"id":5,"ok":{"revision":2}}`). | ✅ | `Chat::clear` bumps every known revision so it never steps back, and the `read`/`agents` payload carries `conversation` (`9b02a3b`); `a_revision_never_steps_back_across_new_transcripts`, `a_read_revision_is_scoped_to_its_conversation` and `attach_edit_with_a_stale_base_conflicts_and_changes_nothing` pin it |
| A2 | **One idle client wedges the whole attach surface.** `accept_loop` is serial and `ask` has no read timeout, so a connection that opens and says nothing blocks every other client: holding one open, `mush agents` did not return in 8 s (`TIMEOUT`); closing it returned immediately. | ✅ | one thread per connection in `attach::accept_loop`, and a spawn that fails ends only that connection (`9b02a3b`); `an_idle_client_does_not_wedge_the_socket` holds one client open and answers a second |
| A3 | **`mush read` cannot frame a multi-line transcript line.** `print_lines` writes the decoded text raw, so a line containing `\n` (the wire is correct: `"text":"first\nsecond"`) prints as two lines under one index and no external parser can tell continuation from a new line. | ✅ | `main.rs::escape_line` escapes backslashes and the newline, carriage-return and tab characters before a transcript line is printed (`9b02a3b`); `a_read_line_is_escaped_onto_one_line` |
| A4 | **`edit send` does not take the path a typed message takes, though its comment claims it does.** The typed path (`send_message`) trims, refuses an empty box and parses commands; `attach_edit` calls `expect_human(text)` + `deliver(text)` with the raw text. Sending `"text":""` with `send:true` is accepted, adds an empty user line to the transcript and starts a run. | ✅ | `attach_edit` trims a send and refuses an empty one with `bad_request`, and its comment states a client's words are a message, never a command (`9b02a3b`); `an_empty_attach_send_is_refused_and_changes_nothing` |
| A5 | **The `--` escape is claimed but half exists, and a directory named after a subcommand is unopenable.** `Cli::detect`'s doc says "`--` is the escape hatch a human has", but it only escapes as the first argv (`mush -- agents` skips detection — a second arg then errors with "only one directory may be given", proving `agents` was taken as the directory), it is rejected *inside* a subcommand (`mush read --` → ``unknown option `--` for `mush read` ``), and `mush agents` in a directory literally named `agents` runs the attach CLI instead of opening that directory. Nothing in `--help` names either form. | ✅ | `Cli::detect`'s `--` arm makes everything after it positional and the escape is documented where `detect` does it (`9b02a3b`); `a_double_dash_ends_the_options` and `the_attach_subcommands_parse` |
| A6 | **`bad_request` carries two failures that are not the request's fault:** "mush is shutting down" and "the UI dropped the request", so a client cannot tell a transient shutdown from a malformed request. | ✅ | `ReplyError::unavailable` is its own `kind`, carrying both "mush is shutting down" and "the UI dropped the request" (`9b02a3b`); `unavailable_is_its_own_kind` |
| A7 | **A spawn failure leaks the socket file.** `serve` builds the `Guard` before `spawn`, and the `?` on the spawn returns while the listener is dropped, leaving the file with no guard to remove it (cosmetic: the next run clears it). | ✅ | the guard is built before the spawn thread, so a failed spawn returns through its `Drop` and the socket file goes with it (`9b02a3b`); `a_thread_that_will_not_start_takes_the_socket_with_it` injects the failing start through `serve_with` and checks the bound socket went with the failed serve (`61d633f`) |
| A8 | **`attach_worktree` reports a worktree that is gone** — a path whenever `branch` is `Some`, including a merged/discarded agent, which is the case `worktree_gone` exists to detect; the roster carries no `landed`. | ✅ | `attach_worktree` names the path only when the node keeps a branch *and* the path is on disk, else the root (`9b02a3b`); `the_roster_does_not_report_a_dead_worktree` |
| A9 | **A stale socket is not always refused the instant the live listener is dropped, so a test that says it is flakes.** `attach::tests::a_stale_socket_is_cleared_and_a_live_one_is_not_stolen` failed about a third of the time under the narrow `cargo test --bin mush attach` filter (14/40 here) while the full suite stayed green. `serve`'s liveness probe (`UnixStream::connect` then drop), run against a live listener, leaves a pending, never-accepted connection in that listener's backlog, and a just-closed listener can keep answering for a moment while the kernel tears the socket down — so the next `serve` read the stale file as live and refused to clear it (`EADDRINUSE`). The narrow filter is what opens the window; the suite runs too fast and too parallel to show it. | ✅ | one test's timing, not production: `serve` clears a stale *file* exactly as it promises, and no code changed. The test now gives the stale phase a root of its own — so the live phase's probe cannot outlive its listener — and waits, bounded, for the file to actually refuse before asking `serve` to clear it (`0ad0f07`); 100/100 green under the same filter |

Two interactions with H9 (`ae16cb2`) worth stating rather than inheriting: the
arm *is* the status line (`quit_armed()` reads `status_line()`), so any attach op
that calls `App::say` — `focus`, and `edit`'s draft arm — **silently disarms a
quit the human armed**; that should be a decision, not a side effect of `say`.
And H9's `Phase::doing` is a third phase-word derivation beside M3's
`Phase::label`/`detail` and `screen.rs`'s `phase_glyph`/`phase_detail`; `doing()`
collapses `Stopped|Done|Failed` to `"idle"`, so the quit line can read
`#0 idle + 1 job` for a stopped agent that owns a live job (see refactor §11).

## 7. The `Screen`, reviewed (B17, `7e123e1`)

`ui.rs` went from 1 024 to 298 lines and `app/screen.rs` (1 064) took the
derivation; the reviewer read both against the old file, function by function,
with a string-literal and a constant census, and then *injected eight bugs* to
ask which painted fact each test really pins. The value holds: `Screen` caches
nothing (`App` has no `Screen` field; `screen(&self, area)` only reads), every
constant, fallback and rule has one home, `Rank` precedence has one home with the
colour left in `ui::rank_style`, transcript windowing/foot has one owner, and the
`Shot` harness derives its facts from the value rather than from the buffer (that
is why three of the injections fail). What the injections found instead:

| ID | What | Status | Home |
|---|---|---|---|
| V1 | **The `▲N`/`▼N` counts are a model of the `List`'s scroll, never compared with the rows actually painted.** The pane's window geometry (`inner`, `footer_rows`, `list_area`) is derived in `app/screen.rs` to compute the counts and *again* in `ui.rs` to place the list; the counts are arithmetic over the first, by assumption. Proof: deleting the painter's separator row (`ui.rs`'s `footer_rows`) fails **only** `page_keys_move_the_tree_cursor_a_page_and_clamp_at_both_ends`, at 80×24, with a message about the highlight — the 15×14 sweep does not notice, because its "twenty agents" state asserts the title's words and never `▲/▼`. So the two can drift and the count can lie about the window. | ✅ | refactor `R25` |
| V2 | **The size-tier boundaries are pinned by nothing.** Moving `screen.rs`'s `area.width < 80 \|\| area.height < 20` to `< 19` (or `< 79`) leaves all 398 tests green, though the sweep re-lays out 15 sizes — its own doc claims its `words` are "the ones a bug in the size tiers would take away". The bar's edge at 24 *is* pinned (`the_facts_line_survives_at_80x24`, which fails on 24→25). | ✅ | `the_size_tiers_paint_the_two_layouts` pins 79×24 and 60×19 against the side-by-side layout (`f70374f`) |
| V3 | **The rewrite dropped the old sweep's both-focus-states pass.** `the_layout_survives_every_size` drew every size in both focus states on purpose ("a border is painted differently when it is focused and a focused-but-tiny pane is the awkward case"); `Focus::Agents` is now painted at exactly one size in the whole suite (120×32), because no sweep state changes focus from `App::new`'s `Focus::Chat`. The practical loss is small (focus changes the border colour and the chat cursor) but the doc should not claim a sweep it does not run. | ✅ | the sweep paints both focus states at the two presentation sizes (`f70374f`) |
| V4 | **A moved unit test lost its last assertion.** `ui.rs`'s `an_error_outranks_the_tree_line` ended with `assert!(text.contains("/help"))` — "nothing to say is the hint"; the version at `screen.rs` stops at `bar_word(None, None).is_none()`, so it checks the derivation, not the painter's fallback. No functional gap (the sweep's `fresh` state covers `Tab cycles panes` at 15 sizes), but the rule lost its direct test with the move. | ✅ | `an_empty_bar_paints_the_hint` asserts the painted bar names `/help` (`f70374f`) |
| V5 | **`roomy`'s doc contradicts its test** — prose added by this integration: the doc says it is asserted "at every size at least 80×24" while the test checks two exact sizes. | ✅ | the `roomy` doc names the two presentation sizes (200×50 and 120×32), matching its test (`f70374f`) |
| V6 | **The bar's sanitize invariant is documented as total but not held.** `app/mod.rs` says "every string the bar can show is created by `say`/`fail`", while `bar_word(self.status_line(), tree.as_deref())` lets `tree_line`'s string reach the bar *outside* `set_status`'s sanitize door. Harmless today (numbers and fixed words) and fragile the moment an agent label lands in that sentence. Part of `R10`. | ✅ | refactor `R10` |
| V7 | **The sweep's blind spots, named:** hidden-row counts and derived-line counting are caught by their own `the_sweep_*` tests but not by the 15×14 sweep; tier boundaries (V2) and window/count drift (V1) by nothing (the latter only incidentally). Three of eight injections slipped past the sweep, which is the honest measure of what "15 sizes × 14 states" buys. | ✅ | the sweep now reads every frame's title back against the cells: `▲N`/`▼N` name exactly the rows the window hides and the ids painted inside the window are the rows between them, so a count or scroll offset that drifts from the painter fails at any size and any state (`e0afdb8`); V2's tier edge is pinned by `the_size_tiers_paint_the_two_layouts` |

---

## 7.5 The signal that is not a failure (`f313916`, merged `883769f`)

One class: a signal that interrupts the wire (`SIGWINCH` above all) is not the
endpoint refusing the request, so `http.rs` makes the call again rather than
ending the run — B25's row carries the closure and the tests that pin it. The
`Screen` census in `docs/refactor.md`'s header (`ui.rs` 298 / `screen.rs`
1 064) is still right.

---

## 8. The class: one fact, N spellings

Every row above is an instance; this section names the disease, because the
next blind audit should hunt the class instead of waiting for the next
instance of it.

**The rule the tree states, and the code kept breaking:** a fact that is not a
*value at the seam where it is born* gets re-derived by every reader. The
readers then disagree — and a reader that re-derives a *delivery* hands the
model a result twice. `derived-not-stored, one owner per fact` was applied to
the screen and never to the model-facing facts: `delivered`, `revisions`,
`napping`, a stored `branch`, the list window.

The three shapes this wave hit, each with its rows:

| Shape | What it looks like | The rule that closes it |
|---|---|---|
| **A read that is not recorded** | `B26`: `agent_status` printed every child's whole final message on every call and recorded nothing, so the fold printed the same report again; `wait_agents` answered from history with no mark of new-vs-old. | **One result, one delivery.** A child's outcome (a job's report) reaches a model exactly once, through a road that asks `record_child`/`record_job`; a *listing* renders a bounded digest and never a body; a repeated wait answers `already read`, not the body again. |
| **A side effect inside a poll** | `B27`: `wait_for_results` rendered-and-marked *every* ready child while returning only the first, so one wait marked bodies read that the model was never handed. | **A poll is a question.** Anything called in a loop to ask about state is pure; delivery happens only for what the answer actually returns. |
| **A conclusion stored, or re-derived** | `U12` (two predicates for the same nap), `U13`/`A8` (a branch the actor does not have), `V1`/`R25` (a window the counts only assume), `A1` (a revision that restarts at 0). | **Derive once, where the reader can check.** If two surfaces can disagree, one of them is a finding. |

### 8.1 The two rows this wave added

| ID | What | Status | Home |
|---|---|---|---|
| B26 | **A parent that polls re-reads every child's whole report, and the same report can be folded again.** Seen live (the session whose `.mush/session.json` this file was written from): the root's context held 173,593 chars, of which `agent_status` answers were 63,576 (30.3%, four calls of 3.1–34.2 KB); `#64`'s full report appeared **five times** in that context (msgs 2, 15, 43, 45, 58). Three roads did it: (1) `status_tool` rendered `Outcome::Finished(summary)` — the child's *entire final message* — on every call and, taking `&ActorState`, could not mark a read; (2) the fold at the next message boundary then pushed the same text again as `#N done: …`, because the delivery mark is the only thing it reads and nothing had set it; (3) an un-ided `wait_agents` answered any *recorded* outcome, read or unread, with no way to tell — the model itself wrote "The wait answered with #64's already-read summary — H15 again". The tool's description called `agent_status` a description, so polling it was the rational move. | ✅ | `status_tool` is a listing: `Outcome::digest` is one line (first line cut at `DIGEST_COLUMNS`, plus the size of the whole) and `✉` marks an unread result; `record_child`/`record_job` (folded in from `mush/91`) is the one home of "mark as read and say whether it was fresh", and `wait_tool` answers a repeat with the digest and `(already read)`. `every_delivery_road_hands_a_result_over_once` is the sweep: wait, fold and wake, one table — the body arrives once, nothing re-folds it, a listing never carries it, a repeat never replays it. Landed in `45116ad` |
| B27 | **A wait for the first result marked the others read.** `wait_for_results` called its `result` closure for *every* candidate each poll — and that closure both rendered the line and wrote the delivery mark — then returned only the first. So `wait_agents` with no ids marked every ready child read and handed over one; the other bodies could never be delivered (the fold saw them read), and the model could not know they were lost. Found by writing the road sweep for B26, not by reading the wait. | ✅ | `WaitResults { is_ready, deliver }`: `is_ready` is pure, `deliver` runs only for the ids the call returns (the first, every one with `all`, or what the deadline found). `a_wait_returns_the_first_result_or_all_of_them` now asserts the second child stays unread after a first-result wait. Landed in `45116ad` |

### 8.2 The blind-audit recipe

The wave's own review method, written down so the next audit can start here:

1. **Census the facts, then their derivations.** List every fact a frame or a
   model answer can carry (a phase, a count, `unread`, a revision, a branch, a
   window, a summary…). For each, grep its derivation sites and its *string
   shapes*. More than one site, or more than one shape, is a finding before
   any bug is seen: `R25`/`V1`, `U12`, `U13` and `B26` all fell out of this
   step alone.
2. **Sweep the roads.** For each fact, table every road that hands it to a
   reader and assert the invariant *across* the table, not per road. A
   per-road test cannot see a road that forgot to record — which is why 21k
   lines of tests missed B26.
3. **Polls are pure; counts are arithmetic.** Anything a loop calls to ask a
   question must not mutate; any count must be computed over the thing it
   counts, at answer time (`V1`, `R25`).
4. **Inject a bug per fact and name the failing test.** A fact no injection
   can break is a fact no test pins; a blind spot is a finding, not an
   embarrassment (V7's method).

---

## 8.5 Where the lines are (the census)

`scripts/census.py` is the method — run it from the repository root. The
numbers below are what it printed at `f70374f`, next to the same count at the
first commit past 9k lines (`143325a15`, the "4x LOC" that started this):

| | `143325a15` | `f70374f` | growth |
|---|---|---|---|
| total | 9,200 | 41,123 | 4.5x |
| **prod** (blank/comments/tests stripped) | 4,183 | **7,679** | **1.8x** |
| tests (inside `mod tests` blocks) | 3,065 | 21,277 | 6.9x |
| comments | 1,255 | 9,468 | 7.5x |

The fix wave's own delta, `eab825e..f70374f` (five commits, ~20 findings):
**prod +64, tests +1,197, comments +708** — behaviour moved by sixty-four
lines and the harness around it by twelve hundred.

The two big files are 46% of the tree and 58% of the tests: `app/mod.rs`
(9,665 total, 6,642 test) and `agent.rs` (9,130, 5,640 test).

What the census says, and what it does not:

- The behaviour is ~7.7k lines and the harness ~21k. A fast offline suite is
  worth paying for, so the *volume* is not the defect — the **shape** is. The
  tests pin roads and sentences one at a time, which is exactly why they could
  not see a fact with two spellings or a road that forgot to record. Coverage
  that cannot span two roads is prose with assertions.
- Comments are 23% of all lines. A rule with one home needs one sentence, so
  comment mass is a proxy for rules living in prose — and the measurable
  consequence is drift (`H11`, `R10`, `V5`), every one of which is recorded in
  this file.
- Run the census per wave and write the delta beside the wave's rows. A wave
  that grows prod and tests together is buying coverage; one that grows
  comments faster than prod is buying prose.

---

## 8.75 The §8 wave's closures (`eab825e`..`f70374f`)

The wave's census is §8.5's. As a class it closed the delivery facts (B26/B27,
and the fold-in of `mush/90`/`mush/91` — H2's `CutOff` outcome, H4's `✉`/`✉N`
marks, R3/R7's one home for once-only delivery), the attach boundary's eight
rows (A1–A8), U12/U13's two derivations, and the `Screen` review's V1–V6 plus
S8's (iv). Each row now carries its own closure; the rows still open are in
**The open queue**.

---

## 8.9 The T1/T2 wave's closures (`f70374f`..`d753db8`)

A criticality sort ranked the open queue by what each item costs a real
session; the wave did the top two tiers. As a class it closed the agent-side of
the wire (H1's actor-side facts, H5's reply), the lock trap (H13's third fix,
`1df1a53`), the spawn base (H7), the machine's load line (H8's warning half),
and the refactor items `A19` (`http::resolve_bounded`, `7dc5e1b`) and `R21`.
Census at `d753db8`: total **41,875** (was 41,123), **prod 7,783 (+104)**,
tests 21,691 (+414), comments 9,662 (+194).

---

## 8.11 The agent-contract audit (`d8c75e2`..`00571b4`)

A subagent read the working tree (read-only) for claims **an agent reads** that
the code does not honour — prompts, schemas, tool results, refusals — after the
prompt/schema dedup pass. It verified ~15 claims sound (delivery once per run,
wrap-up and truncation answering "was not run", base spawns, job reports,
edit-batch semantics, path enforcement, wait defaults) and found the rows
below, all now fixed but the last:

| What an agent read | What the code did | Closed by |
|---|---|---|
| the resume story: a message to an at-rest child starts a run | a resume never re-entered the parent's `running` book, so a wait answered the stale result, `agent_status` said stopped while the child worked, and the one-shared-child guard could be bypassed by a resume — two shared children in one tree | `cec9713`: `control_tool` re-arms the book and the UI sends `AgentMsg::ChildRunning` on a human nudge |
| "any command that outlives 60s detaches by itself" | auto-detach needs a free job slot (8 machine-wide); with none, a long command was killed at 120 s, and a launch refused at the deadline threw the output away | `ab90c68`: the output is snapshotted before hand-over, a refused launch reports "ran Ns, could not become a job," and a no-room timeout names the budget |
| "wait_agents blocks until a child finishes" | it returned the first *recorded* result, even one already read, while a sibling still ran | `cec9713`: while a candidate runs only an unread result is ready; a fresh result outranks an already-read one for the single answer (later superseded by the twelve-to-six cut, where `wait` takes no arguments and hands over every result once) |
| "An isolated subagent works in its own copy" | degraded isolation reached the child's brief and a human notice, but the parent's tool result only omitted `on mush/N` | `3e006c5`: the result carries `(isolated unavailable: …; running in place)` |
| a sibling's lock refusal advised `wait_commands` | no tool can wait on another agent's job | `3e006c5`: it says do not retry in a loop, do other work and try once after |
| depth-1/2 agents have the orchestration tools | the delegation policy lived only in the root prompt, and `brief` had no description | `3e006c5`: one `DELEGATION` block, included exactly when the subagent gets the tools |
| "only one shared child may run at a time" | the guard counted *any* running sibling, so an isolated one blocked a shared spawn with a false sentence | `cec9713`: only children that share the workspace count, and the refusal names the one that blocks |
| the file tools read any path | `list_files` hid every dotfile (`.github/`, `.gitignore`) and stopped at its limit silently | `3e006c5`: dotfiles are listed (build/VCS dirs still skipped) and the limit is reported — the tool itself was later removed by the twelve-to-six cut, so the fix's home is gone |
| "Read a file." | a capped read gave no size and no way to the rest | `3e006c5`: "N of M bytes shown … `sed -n`" — `read_file` too was later removed by the twelve-to-six cut, so the fix's home is gone |
| "**Workspace jail.** Every agent path is resolved against the root and rejected if it escapes (`..`, absolute paths). The agent cannot touch `/etc`." (`docs/mush.md` §2) | only `edit_file` ever did that, and only to its own `path`; `run_command` was always a real shell with nothing confining it, and the file tools read any path | reworded to what the code does: the workspace is named in the prompt, commands run with cwd at its root, and the rules say never to touch paths outside it (`prompt.rs` `RULES`) — §2 of `docs/mush.md` now says plainly that this is a convention, not a jail (the twelve-to-six cut) |
| "cut off … 4 times in a row" | the counter never reset, so scattered truncations were called consecutive | `00571b4` |
| a child at rest, for one drain, while it has resumed | the run-start report the last wave added announces a resume to the parent's `running` book (`AgentMsg::ChildRunning`, sent as the run begins), but leans on the parent draining the child's `ChildDone` first and the ordering is not enforced: a child ends run N with its result unread, the human nudges it (a `Steer` is sent), the parent's boundary drains the completion and clears the running mark, and the child's `ChildRunning(N+1)` lands one drain later — `drain_mailbox` takes whatever `try_iter` holds and the report was not in that snapshot, so a drain between the two clears the mark for one drain (a momentary wrong `status` and one-shared-child guard answer, no crash) | ⬜ **open** — a run sequence the parent can compare (`ChildDone` already carries one) or a drain that settles before it acts |

**Deliberately left, each for a stated reason:** a *stopped* child does not wake
its parent (`Outcome::is_news` — the human's stop is not news, and the line is
in the transcript for the next run); the loop guard still counts a timed-out
wait as an unchanged repeat (no result changed, which is what the guard is for,
and a run gets six waits, not one); the root's lock exemption is learned from
the result note rather than stated in advance; and `RUNAWAY_TURNS`'s wrap-up
turn explains itself when it fires.

**Census at `00571b4`** (the six commits above plus the dedup): total 42,448
(was 41,875), **prod 7,808 (+25)**, tests 22,065 (+374), comments 9,804
(+142). Test-heavy on purpose: every trip-level row has a test that fails
without its fix. The audit itself ran against a `prompt.rs` that was being
edited by hand, so its prompt quotes are a snapshot; the executor-side rows are
not.

---

## 8.13 The spawn contract: a name, and one switch (`15648ae`)

The tool that writes the tree's rows could not name them, and the flag that
meant "give this child its own tree" was a boolean the executor had to trust.
One contract now says both: `spawn_agent(brief, title, base?)`.

- **U14 ✅ — `spawn_agent` can name its child.** `title` ("A 3 word description
  of this agent's brief.") is a *required* schema argument, because a row's name
  is the only way to know what an agent is doing at a glance; it travels in
  `AgentEvent::Spawned`, is stored in `session.json`, and comes back on restore.
  A blank or missing title still falls back to `AgentNode::title`'s handle
  derived from the brief (U6), so a stale call or a server that ignores
  `required` cannot leave a row nameless. Pinned by
  `a_spawn_carries_the_name_its_caller_gave`,
  `the_row_prefers_the_title_its_caller_gave`, and the restore assertion in
  `a_stored_conversation_restores_its_agents_with_a_live_mailbox`.
- **H7, corrected — `base` *is* isolation.** The `isolated` boolean and the
  whole degradation path are removed: pass `base` and the child gets its own
  worktree and branch forked from that ref; omit it and the child shares the
  workspace, where the one-shared-child rule applies. Anything git refuses is
  now a *failed delegation* (``cannot start from `{name}`: …``) instead of a
  child that silently runs in the shared checkout — superseding §8.11's
  `(isolated unavailable: …)` row, its notice, and its test. An unknown ref is
  refused before anything is created. Pinned by
  `a_spawn_that_cannot_make_its_worktree_is_refused`,
  `a_named_base_is_resolved_before_anything_is_created`, and
  `a_spawn_forks_from_the_named_base_and_says_so`.

**Census at `15648ae`:** total 42,477 (was 42,448), **prod 7,807 (−1)**, tests
22,106 (+41), comments 9,792 (−12). The production side is a net removal: the
name rides a path the brief already travelled, and deleting the degradation
path paid for it. Owed in the doc-sync pass: `docs/mush.md` §3's signature and
tool table (`brief, title, base?`) and §5.5's `isolated unavailable` sentence,
which no longer exists.

---

## 8.15 The registry outlives the checkout (P13, `e63a84c`)

`rm -rf .mush` deletes the checkouts and the session, but not
`.git/worktrees/*`: git keeps naming every deleted `mush/<id>` worktree (each
with a `prunable` line), and `discover_worktrees` registered all of them as
`leftover worktree — found on startup` rows on every launch. 67 phantoms in the
reference workspace, and no command could clear them.

- **Discovery asks the disk, not just the registry.** `Worktree::on_disk()` is
the one predicate; a registry entry whose checkout is gone gets no row.
- **`/worktrees` reconciles.** The re-scan ran `git worktree prune` and its
  line said `cleared N stale git entries (branches kept)`.

**Amended by §8.17:** the `/worktrees` half is gone with the command family it
belonged to. The discovery half stands and is what fixed the phantom rows;
pruning the registry is now `git worktree prune`, run where git is run. The
row this fixed had no test of its own left — it is pinned by the fixture in
`porcelain_worktrees_parse_in_every_shape` (a `prunable` block parses, and
`on_disk` is what judges it) and by every `isolation`/leftover test that
still registers a real worktree.

**Census at `e63a84c`:** total 42,614 (was 42,477), **prod 7,819 (+12)**, tests
22,186 (+80), comments 9,829 (+37).

---

## 8.17 The command table, cut to what mush owns (`ad5b791`)

The table had grown to seventeen slash commands, six of them wrappers around
`git` (`/worktrees`, `/diff`, `/merge`, `/discard`) or around a row
(`/forget`), one duplicating a key (`/new`, which is Ctrl-N), and one
reporting what the bar already shows (`/context`). Removed all eight; nine
rows remain, every one a fact only mush owns:

| kept | why it is mush's |
|---|---|
| `/provider`, `/model`, `/url`, `/key`, `/models` | config and endpoint facts |
| `/compact` | an actor operation with no key |
| `/notes` | the reader of the foot's `+N more` line |
| `/help`, `/quit` | the table and the exit |

The Context *meter* stays on screen; only its setter and reporter went.

The screen did not lose facts with the commands: a row's footer names
`.mush/wt/<id>` and `git diff HEAD...mush/<branch>` (git's own spellings, no
mush wrapper), and the landed story (`merged into HEAD` / `discarded`) is
stored in the session. Discovery is automatic, so `rm -rf .mush` cannot
resurrect a row. Nineteen tests died with the commands they pinned (456 →
437, 3 ignored; mush-core 108 after its prune test went too).

**Census at `ad5b791`:** total 41,343 (was 42,614), **prod 7,534 (−285)**,
tests 21,503 (−683), comments 9,600 (−229). Net −1,271 lines.

**Owed in the doc-sync pass:** `README.md`’s chat-command list and its
“Isolated agents” section (the three commands and `/worktrees`), plus
`docs/mush.md` §3 (spawn signature and tool table) and §5.5 (`isolated
unavailable`), already owed from §8.13.

---

## 8.19 `/help` that can be read (U15, `960e073`)

`/help` wrote one forty-odd-line notice into the transcript's foot. The foot
shows two rows and counts the rest, so the human saw this:

```
  +45 more lines · /notes
· mush keys:
    anywhere:
```

That is the cap working as designed — and useless as help: the rest was a
keypress away in a list called `/notes`, which nobody who typed `/help` has any
reason to guess. The `·` even made mush's own chatter look like part of the
table.

- **`/help` now opens the picker `/notes` uses** (`PickerKind::Help`), at the
  top, scrollable with `j`/`k` and `PgUp`/`PgDn`, titled `help · line N/M`,
  `Esc` closes. `help_opens_a_readable_list` pins the popup and the painted
  title; `help_advertises_compact` and `help_names_the_whole_key_table` read
  the list instead of a notice.
- **The tables render at the popup's width.** `keys::help_table_at(width)` and
  `commands::table_at(providers, width)` keep the keys/usage column and hang a
  description that does not fit under its own column; `help_table()`/`table()`
  are the `usize::MAX` forms `mush --help` prints, unchanged.
- **Two seams retired.** `Chat::note` (root-only) is test-only: a line that
  answers a command either opens a surface or is tagged with its agent, and the
  compaction note now uses `note_for(AgentId::ROOT, …)` explicitly. The
  briefly-added “help lands in the focused pane” rule went with the foot
  notice — the popup has no pane to land in.

**Census at `960e073`:** total 41,447 (was 41,343), **prod 7,568 (+34)**, tests
21,531 (+28), comments 9,635 (+35).

---

## 8.21 Observed live: the counter that skips, and the file that never forgets (H16, H17, `6fc7435`)

The human asked three questions from the other side of a screen: why a run's
children are numbered with gaps, how long a child lives, and why a second
session's `session.json` had reached 100 MB and was rewritten on every
conversation. All three were answered against a live session — the one
orchestrating this repository — and the source behind it. Two read-only
instruments came out of it and are in `scripts/`: `inspect_run.py` (the
process: the tree, the fds, every socket fd joined back from `ss` by inode, and
samples that turn `write_bytes` into the amplification factor of a save) and
`session_blame.py` (where a session file's bytes are). Both name what they
measure; neither connects to the socket, because a byte injected into a live
session is a real message.

**The counter is shared on purpose, and that is most of the gap.** One
`Arc<AtomicU64>` per conversation (`agent.rs:911`) is cloned into every actor,
the tree, the UI handle and the job registry, and is indexed from exactly two
places: `spawn_tool` (`agent.rs:2693`) and `Registry::launch` (`jobs.rs:802`).
Children render `#N` and jobs `#cN` (`jobs.rs:119`), so a session that ran
twelve detached commands shows children `1, 2, 14, 15, …` with nothing wrong
anywhere. Two residues widen the jumps:

- **H17 — the number is spent before git can fail.** The id is taken at
  `agent.rs:2693`, and both failure arms of the worktree it then asks for
  (`agent.rs:2695-2705`) return *after* it: an error to the model, no node, no
  trace, and a gap on the screen that nothing explains.
- **A branch with no checkout reserves nothing.** `discover_worktrees` skips a
  `mush/<id>` whose `on_disk()` checkout is gone (`git.rs:186-205`,
  `app/mod.rs:737-744`), so it never reaches `reserve_ids` (`app/tree.rs:660`)
  — while `git worktree add -b mush/<id>` still refuses the name
  (`git.rs:277-305`). §8.15's P13 fixed the registry side of this; the branch
  side is what was left. Found live here: `mush/2` and `mush/3` were this
  session's own children's merged work (`1d84477`, `25a092c`), and the next
  isolated spawn died on `a branch named 'mush/2' already exists` until they
  were deleted by hand. They are also **H10's missing specimen**: merged, never
  reclaimed, invisible until git refused to reuse the name.

**H16 — a save carries the whole conversation, and nothing bounds it.**
`session_snapshot` (`app/mod.rs:2201-2270`) deep-copies the root transcript and
every child's on the UI thread; `Session::save` (`mush-core/src/session.rs:310-317`)
serializes the lot and writes it as one file, once per
`SESSION_DEBOUNCE = 1s` while anything is dirty (`app/mod.rs:287`, `:873-887`).
A transcript folds only when its own endpoint window fills (`transcript.rs:46`,
`config.rs:403` — about 284 KB of history at the large window), a *finished*
child is never folded again, and the number of children is bounded by nothing:
`MAX_AGENTS = 16` counts **running** agents (`agent.rs:150`, `:2636`) and
`MAX_DEPTH = 3` bounds depth, not breadth. The file is therefore the sum over
every child the run ever had.

Measured here, on one child: that child's transcript alone was 318.9 KiB of a
675 KiB session after ~80 messages; `agents` outweighed `messages`; the file
grew 73 KiB → 675 KiB in 25 minutes of work; and a five-second window showed
0.82 MiB written for a 0.41 MiB file — one full rewrite per save, 2.0x. Two
suspects are cleared by measurement rather than argument: indentation costs
1.03x (the payload is long strings), and the writer's own thread is not the
cost — the cost is the *size of what each save carries*.

**Decisions taken by the human, this wave:**

- A cap on what a save carries: 256 KiB of stored transcript per child, and a
  high ceiling on the root — the human's own words may be trimmed, but never
  silently: a cut file must read as cut.
- **No archives.** *Everything needs a cap, even a high one*; an archive of
  children is one more lifetime to reason about, so the reaped transcript is
  gone.
- `MAX_WORKTREES = 70` — above the 50-child window, so reaping history can
  never be what refuses a spawn.
- Jobs: **the concurrency cap stays at 8**, plus a **hardcoded 4-hour ceiling on
  a job's wall life** (`JOB_MAX_AGE`, `jobs.rs`) — no knob, because a ceiling a
  config can raise is not a ceiling on the disk every agent shares. `MAX_JOBS`
  bounds how many jobs may exist, not how long one may hold its slot, its
  process group and its scratch files: without the ceiling, a hung `detach`ed
  command held all three until mush quit, since a job has no tool call to time
  it out. The human's first answer was 200 concurrent jobs; the arithmetic
  killed it — scratch is 8 MiB per command (`CMD_OUTPUT_LIMIT`), so 200 jobs is
  a legal 1.6 GiB of `/tmp`, where 8 jobs beside at most 16 foreground commands
  is about 192 MiB. *Everything needs a cap, even a high one* — but the cap has
  to bound the thing it multiplies.
- Worktree reclamation is automatic **merged-or-clean only**: an unmerged or
  dirty branch is kept and named, and mush never merges anything itself.

**Where the patches are:** the id split and the job hygiene one-liners are in
flight on `mush/4`; the session bound is in flight on `mush/5`; child reaping
(`Chat::forget`, actor parking, the last-50 window) and worktree reclamation
(the residue pass over `discover_worktrees`, `git::reclaim`) follow in their own
worktrees, each with tests — the human's condition on the reclamation was
"thorough testing" before it touches a branch.

**Census at `6fc7435`:** total 42,702 (was 41,447 at `960e073`), **prod 7,013**,
tests 22,705, comments 10,177. The deltas belong to the repository's own commits
between those two points — this section added no Rust (its instruments are
Python, which the census does not count), and the prod column is down because
the wave's lines went to tests and comments, the trade §8.5 warns about.

---

## 8.23 The wave §8.21 opened: five landings, and the specs that were wrong (`fb012d1`..`cc89598`)

The three questions — why the numbers skip, how long a child lives, why a 100 MB
file is rewritten once a second — became five patches, each written in its own
worktree and landed only after the suite had been run on the merged tree:

| landing | what it closed |
|---|---|
| `fb012d1` | **H17.** Two id spaces (`AgentId`/`JobId`, one `Display` each), a number given back when a spawn fails before git could create anything, and a floor reserved for every `mush/<id>` git still names — checkout or not, which is the half P13 left. |
| `91f17ac` | the job ceiling: `JOB_MAX_AGE` = 4 h of wall time, hardcoded, one sentence (`#c3 killed: it ran past the 4h ceiling · 4h00m · cargo run`) and the schema to match, because the schema is where a promise to the model lives. |
| `8c1a860` | **H16.** `cap_transcript` bounds every stored transcript — 256 KiB a child, 32 MiB the root, whose cut is *marked* in the file and shown on load. Pure, idempotent, never splitting a call from its results. |
| `ab54de3` | the last-50 window: `Chat::forget`, four steps in one place, and actor *parking*, so a finished child's thread ends while its row and its transcript stay. |
| `cc89598` | **H10.** Reclamation: merged or clean removes; unmerged, dirty or unresolvable is kept and named; `-d` only, never `-D`; `MAX_WORKTREES = 70` refuses a spawn before an id is taken. |

**Two specs were wrong and the code was right** — the most useful thing the wave
produced, and the reason a brief is not a design:

- *"The system prompt is regenerated, so a revived child still knows its task"*
  was false. `prompt::subagent_prompt` takes no brief, and `agent::revive` seeds
  `user(brief)` only when a transcript is empty — so trusting the prompt would
  have handed a revived child nothing to do. The cut keeps the transcript's
  opening segment instead, and the child cap honestly reads "256 KiB plus the
  opening message".
- *"No descendant with work in flight"* left the case that matters: a reaped row
  with an unread reply or an unlanded branch below it strands both — nothing can
  read a report whose row is gone. The predicate closes the walk over ancestors
  (`kept_above`), counts a live job as work in flight (a `Shutdown` would kill it
  through `kill_owned`, a command a human may be waiting on), and parks *leaves
  only*, because a parent's mailbox is the channel its children's completions
  travel on.

**Disclosed, and now the queue's own:** H18 (a parent's steer or stop of a parked
child fails as "gone") and H19 (a reaped child's name stays in its parent's
books). Not rows, but recorded because a later wave should not have to
rediscover them: a microscopic completion-versus-sweep race in reclamation
(a completion sent but not yet in the tree, and the sweep takes the directory a
wake is about to use); `git branch -d` measuring against the root checkout's HEAD,
so a nested branch merged into an unmerged parent is removed with its branch kept
— said out loud rather than hidden by `-D`; a no-commit run recorded as
`Landed::Merged`, which reads as "merged into HEAD" (a third variant needs
`StoredLanded`, in core); and one full-suite flake
(`the_notes_popup_opens_on_the_head_of_the_newest_note`, seen once in ~5 runs
with the reclamation patch, never reproduced alone or under 14x load, not tied to
its diff) that the next wave should either freeze with a clock or catch.

**Merge repair, recorded because it is the kind of thing a merge hides.**
mush/16 and mush/17 each defined a `kept` on `AgentTree` — one a setter (this
node's work was kept, and why), one a predicate (is this row exempt from the
window) — and git merged them textually into one `impl`. The tree did not build
until the setter became `mark_kept`/`mark_reclaimed` and the predicate kept its
name: a verb and a question, apart at every call site. `cc89598` carries the
repair, because the tree must build at every commit.

**Census at `cc89598`:** total 46,557 (was 42,702 at `6fc7435`), **prod 7,127
(+114)**, tests 24,882 (+2,177), comments 11,512 (+1,335). Read that the way §8.5
asks: five patches, 3,855 lines, and 114 of them behaviour. The wave bought its
closures with tests and prose — these are the wave's own numbers, and they say
the next one should be judged on the prod column.

**Reverted:** the H16 landing §8.23 records above was undone on the human's
decision — see §8.25.

---

## 8.25 The cut that was reverted, and the minute that replaced it (`de80f9c`)

The store's byte cap (`8c1a860`, §8.23's H16 landing) is gone. The human's rule
is the reason, in their words: *"the cut comes from reaping children; the file
should never have that kind of heuristic because then we'd just have edge cases
and drift"* — and the cap did not even bound what it claimed. Its own test
asserted a file of `children × (256 KiB + head) + 10%` (`session.rs:1002`): linear
in stored children, a per-child budget rather than a bound on the file. On the
file it was written for (~100 MB, ~300 children, an average child of ~340 KiB) it
fired on nearly every child, and children carry no marker, so nothing said so —
while the root, whose cap was 32 MiB, never tripped at all.

What bounds the file now is **the tree forgetting children**: `CHILD_HISTORY = 50`
(`app/tree.rs`) applied every frame by `App::reap_history`, so a save only ever
writes live tree nodes. No byte heuristic, marker, notice or magic number remains
in the store, and the proof is a grep — `cap_transcript`, `SESSION_AGENT_BYTES`,
`SESSION_ROOT_BYTES`, `root_dropped`, `truncation_notice`, `bound_stored`,
`size_label`, `Dropped`: no matches under `crates/`, `scripts/` or `inspect/`.
The one lever left on the file's size is that one number, `CHILD_HISTORY`.

Three things that were *not* the cut were kept, because other code depends on
them: `Session::save` consuming the snapshot the writer hands it (now standing on
its own reason, not the cut's), `repair_tool_pairs`/`sanitize_tool_calls`, and
`Phase::CutOff`/`StoredStatus::CutOff`/`cut_off_notice` — a different CutOff, a
run that never ended, sharing only the word.

`SESSION_DEBOUNCE` went 1 s → 60 s in the same landing. What the interval buys is
the UI thread: a save rebuilds the snapshot — every live transcript, cloned — and
that rebuild is what a long run would otherwise pay once a second. What it costs
is the crash window: at most a minute of machine-generated conversation (streamed
responses and tool results), and never the human's own turn. The root send, a
fold, a new chat and quitting were already written before they returned; a message
typed at a *child* was not and now is
(`a_message_to_a_child_is_on_disk_before_the_send_returns`,
`the_session_debounce_is_a_minute`).

The revert is 909 lines out and 81 in; `mush-core` lost the fifteen cap tests
(138 → 123) and `mush` stayed at 489, the two marker tests replaced by the two
above. Both smoke scenarios pass on the merged tree.

**A flake, diagnosed while verifying this landing.**
`a_hand_merge_is_marked_landed_by_the_next_git_read` fails about one run in six
*under load* — two `cargo test` binaries at once — on `master` as much as on this
landing's branch, and the panic names the cause rather than a symptom:
`fatal: Unable to create '…/.git/worktrees/1/index.lock': File exists`. The test
drives git in the same repository the app's background git worker is reading, so
the test's own `commit` loses the race; the worktree then stays clean, the sweep
reclaims it — correctly — and the row never says *kept*, which is what the
assertion was about. Test hygiene, not product behaviour, and it is the workflow
this repository is developed in: children running `cargo test` in parallel
worktrees. The helper that drives git in the tests should tolerate a concurrent
process (a short retry on the lock) rather than panic. **Fixed** by `c5694f1`
(merged `7e0440f`): the test now waits for the git read whose answer it asserts
on, instead of racing the background thread — found independently while the B27
branch was adding a test of its own.

---

## 8.26 The simplification wave: three landings, and what the class bought

`docs/simplification-review.md` is the output of six blind, read-only readers
hunting one class — two mechanisms answering one question. This wave implemented
the items whose files were free, as three branches with disjoint file sets, each
verified here before merge (whole-workspace `cargo test`, `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, both smoke scenarios) and again on
the merged tree.

**`929128e` (merged `f989c3a`) — one in-flight predicate, one busy count, one
refusal.** "This agent has work in flight" had four spellings (`kept`,
`may_park`, `App::in_flight`, `cancel_cursor_row`); one `AgentTree::in_flight`
owns it now, and the `jobs_live` pre-checks stay as a documented *cost filter*,
not a second rule that can drift. `Stopped`'s `Heard`/`Gone` arms had no
production reader (`cancel_requested` returns `bool`: true means the mailbox was
dead with a run in flight, i.e. cut off), and `impl Display for CommandError`
had no production caller.

The drift the reader predicted was **real**: `deliver` answered a client's
refused message *after* committing it to the root's transcript and flushing the
session, so words that never ran were in the conversation the next run would
read. `deliver` now refuses without touching the transcript, the box or the
revision, and the human's own key is the one caller that puts the words back in
the box (`a_refused_attach_send_leaves_the_root_transcript_unchanged`).

One edit was left over by the branch and done at merge time (`a70657e`):
`busy_children` had lost its last production caller, and `agent_row`'s only
caller was the attach roster — a loop over every row — so asking per row rebuilt
the whole busy map once per node: the quadratic R29 had removed from the frame
had moved to the roster. `App::rows(nodes)` is now the one row builder the pane
and the roster share, and the wrapper and `agent_row` are gone.

**`adb8a24` (merged `2c6b53f`) — the registry's dead arms, one bounded status.**
The `Refused::Machine` arm for "you hold the machine" (every `Held` is built by a
*not-you* test), `finish`'s impossible `None` (and the `0s` line its fallback
would have invented), `label(id)`'s callers inside the module, the `0..8`
fixed-point cap in `window_line` whose arithmetic is its own bound, and
`input.rs`'s `(lines, 0, 0)` fallback, whose row could never be missing.
`Record`'s `live`/`line`/`tail` are one `State` now, so the fourth combination is
unrepresentable and the two readers that defended it are gone.

Two items are worth more than their line count. `status`'s headline carried the
command **whole** on top of a window bounded on its own terms — a 2 KB script is
an ordinary `run_command` — so every headline now cuts to
`STATUS_COMMAND_COLUMNS`, and a test drives a 2 KB command through both the
running and the ended form. And the agent counter and the lost-number pool are
one `Agents` under one lock (T1 §13): the guard they replaced was unreachable,
and the window it was written against — a number the repository has just named,
drawn out of the pool between two locks — is closed by the lock rather than by
the guard.

**`bcdd127` (merged `e7f02de`) — a real bug, then eight mechanisms.** The `/model`
popup marked the row the *cursor* sat on instead of the row being painted (the
comparison read `items[picker.cursor]` inside the loop over the window), so every
visible row wore `•` while the picker opened on the current model and **none** did
after one `j`. A blind reader found that without running anything; a frame-paint
test through `ui::draw` into a `TestBackend` pins it now. With it: `elide`'s
`min_kept` and its `kept == 0` arm (both returned the floor), `PickerPane`'s
`show_hint` and the empty-popup early return (the painter's own `Block::inner`
already answers it), four keys pinned by two tests each plus the help-copy
asserts, `Rank`'s unused `PartialOrd`/`Ord`, `notes_report`'s mark re-derived and
its lead measured in bytes rather than columns, `Reading::Holding`'s validity rule
spelled three times (one `held(len)`), and the three "which pane has the
keyboard" fields, now `Panes.focus`.

It still parses `" · "` to read a model row's id. That reader dies with the
review's Tier 3 §7 (the picker's item string is a data format with three
readers), which is unstarted.

**Refused, deliberately — each one a decision rather than a deletion:**

- `StoredStatus::Running`: deleting it would store `cut_off` for a run that is
  live and break old files' deserialization (§8.23's ruling, unchanged).
- `-y`/`--yes`/`AUTO_APPROVE`: its only reader prints back that it was given.
  Deleting it turns `mush -y` into an unknown option — the human's call.
- the attach `id` field: one response per request makes correlation invisible *in
  tree*, which is not the same as unused. Out-of-tree clients are the question.
- `worktree_add`'s `.git` probe (Tier 3 §3): `git worktree add` checks out the
  **repository** root, so a workspace that is a subdirectory of a repository
  would give a child a different cwd from its parent's. That is a promise to
  settle before code, not a one-line fix.
- the `agent.rs` halves of T1 §9/§11/§12 (`label`, the 10 ms literal, 60 vs 40):
  the file belonged to another branch; they are in flight with §8.27.

**Census** (`scripts/census.py`, method in §8.5). At `491113a`: total 46,472 ·
**prod 7,245** · tests 24,716 · comments 11,490. On the merged tree: total
46,591 · **prod 7,127** · tests 24,836 · comments 11,600. 118 lines out of
production, 120 lines of harness in — which is the honest shape of this wave:
two of the three branches are net deletions of production code, and the harness
grew by the tests that pin the two real bugs (the `/model` bullet, the refusal
that used to land in the transcript).

The contract audit that ran beside the wave — what the model is *told* versus
what happens to it — is §8.27.

---

## 8.27 The contract audit: what the model is told, and what happens to it

One blind, read-only reader (`mush/78`, no `docs/`, no commits, no second
worktree) went over the model-facing surface: the root and subagent system
prompts, every tool schema, and every line a tool hands back — `run_command`'s
output and detach line, the `edit_file` confirmation, `spawn_agent`'s return,
`status`'s listing, `control`'s replies, `wait`'s digest and its release rule,
job finish and kill lines, the refusals, the error sentences a parent receives
when a child finishes — and asked of each: is this true, and is the limit the
model is told about the limit that is enforced? Both directions. It ran nothing,
so its "what a model does with it" is reasoning from code, not observation; the
mismatches below are code facts.

| # | what is false | disposition |
|---|---|---|
| 1 | `status` promises "each child's state and title or branch" while a running child prints only `#3 ◐ running` — no title (it lives in the UI tree) and no branch | ✅ `ff315d8` (`mush/87`): print the branch when mush can name it (an isolated child's is `mush/<id>`); no title source invented. The schema sentence is `prompt.rs` (H20) |
| 2 | `wait` "blocks until everything you own has finished" — it gives up at 600 s and any message ends it early, and the context says neither | ⬜ the human's file (`prompt.rs`): name the cap, the timeout sentence and the early release |
| 3 | `edit_file`'s description offers a top-level `replace_all`; only `edits[].replace_all` is read, so the refusal tells the model to set the flag it just set | ✅ `ff315d8` (`mush/87`), fixed in code: the single-pair path honours a top-level `replace_all`. The schema's `properties` are still `prompt.rs` (H20) |
| 4 | **`exclusive=true` is not exclusive against its own owner.** `machine_free_for` answers `Ok` for the holder, `take_machine` then overwrites the record that names the exclusive job with a `(agent, command, None)`, and that call's release frees the machine while the job still runs — so a sibling's benchmark is admitted beside it. The promise ("Siblings are refused, not interleaved") silently stops holding | ✅ `ff315d8` (`mush/87`): `take_machine` refuses **any** existing holder (it is the record's only writer), and an exempt non-exclusive call leaves the record alone. The sentence for the holder's own second claim — deleted by `2c6b53f` as unreachable, which is what made the bug invisible — is back |
| 5 | a job's result is not handed over "in full": `wait`'s digest carries the job's one-line report, whose output is `preview_tail`'s last 400 chars of a 2 KB window | ⬜ the human's file (`prompt.rs`): say what a job's result is |
| 6 | "you are told when it finishes" — a job killed by the 4 h ceiling, the 8 MiB output limit or a stop is written to the transcript but does not wake its owner (`is_news()` is true only for `Exited`) | ✅ `ff315d8` (`mush/87`): a job mush *killed* is news; a `Stopped` outcome still does not restart a run |
| 7 | the root is told "this call queued and the lock was still held" when its refusal is immediate — no queue, no 30 s | ✅ `ff315d8` (`mush/87`): `Refused::root_message`, chosen by `machine_refusal` at all three sites |
| 8 | `control message` replies "it was at rest, so this resumes it" — but if the child's worktree is gone the child drops the steer on the floor, and the parent then waits 600 s for a result that cannot arrive. The human's own path refuses the same message up front | ✅ `ff315d8` (`mush/87`): `message_agent` refuses up front, in the child's own words (`worktree_gone_line`) |
| 9 | "A command that outlives 60s detaches by itself" holds only while the job budget has room (otherwise it is killed at 120 s), and a job also dies past 8 MiB — a ceiling no prompt or schema states | ⬜ the human's file (`prompt.rs`): the budget's consequence, and the output ceiling beside "4h" |
| 10 | turns leave the context with no marker: `needs_compaction` fires only while the history still fits, and past the whole budget `trim_history` drains the oldest turns silently | ✅ `f34c4de` (`mush/88`): one line in the request saying the oldest turns were dropped |

**What it checked and found consistent** — worth as much as the list above,
because these are the promises that hold: the root and every subagent share one
`RULES`/`MACHINE` block by construction; `edit_file`'s missing/ambiguous arms
match its words; a batch lands all-or-nothing; `detach`'s 60 s and the 4 h
ceiling are the constants they are described as; `exclusive`'s sibling story is
true for siblings (item 4 is about the holder's own second call); `spawn_agent`'s
return is the sentence that was shipped; `base`, the shared-child guard, and the
depth/agent/worktree refusals each name their limit; the foreground result keeps
the head and a job's window keeps the tail, and each says so; `status`'s job half
matches `status_for`; `control`'s target spelling accepts what `status` prints;
and a child's finish folds in and starts the parent's run, exactly as
"Ending your turn while children still run is fine" promises.

**Left standing, on purpose:** the prompt's "Never touch paths outside the
workspace" is an instruction, not a boundary — only `edit_file` enforces it, and
`run_command` is an unsandboxed shell; and `RUNAWAY_TURNS = 200` exists although
the prompt says a run "runs until it stops calling tools" (the guard announces
itself in a wrap-up turn, so the two sentences are not the same sentence). Both
are decisions, not drift.

---

## 8.28 What bounds the file now, measured on a running mush

The instrument is `scripts/inspect_run.py` (read-only: `/proc`, the state
directory, the socket's inode — it never writes to the socket and never
signals), and the attribution is `scripts/session_blame.py`. Both were pointed
at the orchestrator's own session while it ran (pid 2660028, up 3.5 h, 27
threads, RSS 116 MiB).

**The write rate, before the minute.** That process is the *old* binary, built
before §8.25's revert: `SESSION_DEBOUNCE` was one second there. Over eight
seconds of `session.json` mtimes and `/proc/<pid>/io`: three rewrites of a
13.9 MB file, each delta exactly the file's size, nothing in between —
**~5 MB/s sustained, ~400 GB/day**, all of it the same bytes re-encoded. That is
the cost the 60 s debounce and the reaping now divide: the same session would
write ~0.23 MB/s, and a smaller file besides. It is also the honest reason the
byte cap was ever proposed.

**Where the bytes are.** 13.3 MiB on disk: 12.1 MiB of it in **23 children**
(90%), 788 KiB in the root transcript, and indentation costs only 1.03× — the
payload is long strings, so pretty-printing is not the problem. A child's
transcript is folded while it runs and **never again once it finishes**, so each
child is pinned at whatever it reached: median 549 KiB, p90 768 KiB, largest
817.3 KiB against a fold trigger of **820.9 KiB** (window 500k tokens → budget
1.1 MiB → trigger ¾ of it). Every child stopped just short of its own trigger.

**The bound that replaced the cap.** (Superseded by §8.30: the fold trigger is
now nine tenths of a *larger* budget — this session's window gives ≈ 56 MiB
rather than the 41 MiB derived below. The arithmetic and the lever are the
same.) No byte heuristic is involved and none is
needed: what the store can hold is

    CHILD_HISTORY (50) × the transcript's own fold trigger + the root's (folded, so under it)

which for this session's window is 50 × 820.9 KiB + <820.9 KiB ≈ **41 MiB**, and
for a 128k window ≈ 10 MiB. Both numbers move with the *model's window*, which is
the one thing the model actually has to fit into — not with a magic constant. The
lever is the one number behind reaping, `CHILD_HISTORY`, and the live tree (23
children) is comfortably inside it.

**A tool bug this measurement exposed, fixed in the same commit:**
`session_blame.py` was comparing children against the *budget* while calling it
the fold trigger, and read its percentile off the largest-first list (so the
number printed as `p90` was the tenth percentile). It now derives
`(budget, trigger)` from the window it is told about and prints both. A count
without its method is a rumour — including when the count is the tool's own.

---

## 8.29 The contract wave: one real bug, three branches, and a workspace lock (`e7816a4`..`ff315d8`)

§8.27's audit closed here, together with the leftovers of the six-auditor
review, as three branches with disjoint file sets — each verified in its own
worktree before merge (`cargo test`, `cargo fmt --check`, `cargo clippy
--all-targets -- -D warnings`, the smoke scenarios) and again on the merged tree
— plus one feature the human approved mid-flight and one script. The merged tree
runs `515` mush tests and `128` mush-core tests, `0` failed, `4` ignored (the
known-flaky git test passed), fmt and clippy clean, and all three smoke
scenarios pass.

**`ff315d8` (`mush/87`) — the lock that was not exclusive.** The audit's item 4
was a real defect with a three-step mechanism: `machine_free_for` answers `Ok`
for the holder, so an agent holding the machine could call `exclusive=true` again;
`take_machine` then *overwrote* the holder record — erasing `Some(job)`, the name
of the detached job the first claim had become — and the second call's own
`release_machine` freed the machine while that job still ran, admitting the
sibling benchmark the lock exists to refuse. The sentence for exactly this state
("you hold the machine…") had been **deleted by `2c6b53f` as unreachable**, with
the proof written into the comment: the only two builders of a `Held` were
`machine_free_for` (which answers `Ok` for the holder) and `launch` (which refused
only a *non-owner* holder). The comment was right about those two call sites and
wrong about the third: `take_machine` was itself a builder, via its own
overwrite. A comment that proves a state unreachable is evidence about the code,
not about the machine — and the state came back the moment the record's writer
changed. Now `take_machine` is the record's only writer and refuses **any**
existing holder, `launch` refuses a second *job* claim even for the owner
(`claimed.is_some()`) while the legitimate auto-detach handover — a foreground
call giving its own `None` claim to the job it became — still passes, and an
exempt non-exclusive call by the holder never touches the record. Three tests
fail on the old hunk (`the_holder_cannot_claim_the_machine_itself_twice`,
`a_second_exclusive_job_is_refused_even_for_its_owner`,
`a_second_exclusive_call_from_the_holder_is_refused`); a fourth
(`a_holder_runs_beside_its_own_exclusive_job_without_losing_it`) passes before and
after and is labelled as the guard it is.

The branch's other five items: `control message` to a child whose worktree is
gone is refused up front in the child's own words (item 8 — the parent used to
wait 600 s for a result a landed child could never produce); a job mush *killed*
by the 4 h ceiling or the output limit is news and wakes its owner while
`Stopped` still does not (item 6); `status` names a running child's branch
`mush/<id>` and invents no title (item 1's code half); a top-level
`replace_all` is honoured (item 3); and the root's immediate refusal no longer
claims a queue it never sat in (item 7), through one `machine_refusal` chooser
rather than three call sites picking their own words. Leftovers, by row id: T1 §5
(`Asked.tools` — one reader, a restatement of `tool_schemas.len()`) gone; T1 §9
(the 60-vs-40 pair: `beside_note` and both `Refused::Machine` arms now cut a
command at one bound, `STATUS_COMMAND_COLUMNS` keeping its own where a headline
needs one); T1 §11 **partial** — agent.rs's three callers spell `{id}` while
`jobs::label` survives for the two callers in `app/mod.rs`; T1 §12
(`wait_bounded` sleeps `jobs::POLL`); T3 §6 (`TRUNCATION_INSTRUCTION` reaches the
transcript through `push_line`).
**Refused, and why:** `jobs::label` cannot be deleted — two of its five callers
are in `app/mod.rs`, which this branch did not own — and `Held.agent`/`Record.owner`
were not retyped to `AgentId` because the `u64` flows into `app/tree.rs`
(`live_for(id.0)`, `hold(AgentId::ROOT.0, …)`) and `app/mod.rs`
(`kill_owned(id.0)`), i.e. out of the branch.

**`f34c4de` (`mush/88`) — the environment, read once.** `resolve` read the seven
`MUSH_*` variables three times and parsed three of them under two policies
(`Overrides::from_env` built the base leniently, `from_env_checked` re-read the
same three strictly), and `Config::from_env`'s doc claimed two knobs its body
never applied. One `EnvText::read` now names and reads each variable exactly
once; the two readers parse those fields with their own policy, and `resolve`
hands the *same* read to the base config and to `resolve_with`. The honest
delta: `Config::from_env()` now applies `MUSH_REASONING_EFFORT`/`MUSH_THINKING`,
the two knobs its doc always claimed — no shipping path calls it (`rg
"Config::from_env"` finds one ignored live-endpoint test), and `resolve`'s
`Config` is unchanged for every environment by construction. The identity reads
of `env.temperature`/`env.max_completion_tokens` are gone (no environment
spelling exists), and the test that planted one now pins that such a layer is
*not* read. Item 10: `trim_history` dropped turns in silence, so a model
contradicts a fact it "already read" with nothing to say why the fact is gone —
one `DROPPED_TURNS_NOTE` line now travels in the request. Three details that
matter: it is a **user** line (mush's other out-of-band notes are; an assistant
line would be a fabricated turn, and a thinking endpoint refuses a replayed
assistant turn with no `reasoning_content`); it is removed from the vec *before*
the `user_indices` arithmetic and put back after, so the drain measures exactly
the transcript it always measured; and it is counted against the budget from the
first drop, so the line explaining the trim cannot be what pushes the request
past the window. It cannot accumulate in `session.json`: the vec is the actor's
working copy, while the stored copy is the UI's, fed only by emitted events.
Four new tests, one of them for the transcript that fits (no note) and one for
the shape that cannot be cut (no note either — a note is a fact about what
happened, not a hedge). **Refused: items C4 and C5.** The review's `stored_bytes`
`unwrap_or(0)` and `session.rs`'s `cost` helper were read at `eddaa0f`, and the
byte cut that contained them was reverted by `de80f9c` (§8.25) — the functions
are gone, so there is nothing to widen or delete. A review is a snapshot of a
tree, and two of its five claims were about code that no longer existed.

**`b847555` (`mush/93`) — a picker row carries its id.** `Picker::items` was
`Vec<String>` of `{id} · {tokens}` labels, and *two* readers parsed the label
back: the bullet in `screen.rs` compared the text before the first `" · "`, and
`pick` did the same before applying the model. One model id containing the
separator was therefore drawn as one thing and chosen as another. Items are
`PickerItem { id, label }` now; the label string is byte-identical and still the
only thing defanged, while `id` is data. Same class, reported and not fixed
(`app/mod.rs` was this branch's file but the code is a test helper):
`assert_window_counts` still recovers painted facts by parsing the painted
frame — deliberately, since that is what it asserts.

**`e7816a4` — one mush per workspace.** Two processes on one directory write the
same `session.json`, and that write is a whole-file replace on a minute's
debounce: the two conversations erased each other in turn, and whichever saved
last is what survived a crash. The socket said *something* about a second process
(`bind` fails against a live listener), but a failed attach is deliberately not
fatal, so the second process started anyway and the damage was silent. The lock
is `flock(2)`, exclusive and non-blocking, on `<root>/.mush/lock`, **deliberately
never unlinked**: unlinking is what makes a lock file racy, since a third process
opening the path in between gets a new inode and locks that. The kernel drops the
lock when the process dies, so there is no stale-lock case, nothing to clean up
after a `kill -9`, and no pid-reuse question; the pid is *written* inside only so
the refusal can name who to quit. It is taken in the TUI path only — after
`ensure_mush_dir`, before `Session::read`, so a refused start leaves the store as
it found it — and the subcommands and `--print-config` return earlier by design,
which is what makes the refusal's "ask it things with `mush agents`" true. The
syscall comes from `rustix` rather than a hand-written `extern "C"`: the
workspace forbids `unsafe`, and `File::try_lock` needs Rust 1.89 while the MSRV
is 1.74 (`rustix` was already in the tree under `tempfile` and `rustls`).

Evidence, on this machine, with the real binary: a second start exits `1` with
`mush: another mush is already running in this workspace (pid 3541494) — quit it
first, or ask it things with `mush agents`", and `.mush/` keeps exactly the
files the running process made; `mush agents <dir>` still answers while the lock
is held (0 agents, exit 0); `kill -9` on the holder's session leaves the file in
place and the next start takes the lock and rewrites the pid. That last check is
the one a pid file fails, which is the whole argument for `flock`. The scenario
is permanent: `scripts/smoke.py --lock` needs no endpoint and asserts the exit
code, that the refusal names the holder, that it points at `mush agents`, that a
subcommand still reaches the running mush, and that the workspace reopens once
the holder is gone.

**`13f005d` — `session_blame.py`'s window is read, not assumed.** The script drew
its budget and fold trigger against a hardcoded `128000` and called it "the window
a session gets when nothing states one". Neither half was true: mush's built-in
default is `8192`, and a window the endpoint advertises is what most workspaces
actually run at — the store this was written for runs at `500000`, so every
trigger the tool named was off by **4×** and the "each child stopped just short of
its own ceiling" verdict was drawn against a line drawn in the wrong place. The
window now comes from the first of: the third argument, the session file's own
`context` field (the one mush stores when a human states a window), `MUSH_CONTEXT`
in this shell, the home config (`MUSH_CONFIG`, mirrored from
`userconfig::config_path`). Each answer names its source; with none of the four
the sizes are still reported and the fold trigger is left unnamed rather than
guessed; `--json` carries the tokens and the source together. The only constant
left is `BUILT_IN_CONTEXT = 8192`, quoted in the message for "no window is known".

**The census, before and after** (`git archive a003e36 crates | tar -x -C /tmp/x`
then `python3 scripts/census.py /tmp/x`; §8.5's method):

| | before (`a003e36`) | after (`ff315d8`) | Δ |
|---|---|---|---|
| total | 46,604 | 47,831 | +1,227 |
| production | 7,056 | 7,061 | **+5** |
| tests | 24,886 | 25,698 | +812 |
| comments | 11,633 | 11,971 | +338 |
| blank | 3,029 | 3,101 | +72 |

The production column is the wave's most interesting number. A new module (the
workspace lock, 32 production lines), a real concurrency fix, a rewritten
environment reader and six model-facing corrections together cost **five** lines
of production code, because every one of them came with deletions. 99.6% of what
the wave added is harness and prose — which is the shape §8.5 was written to make
visible, and the reason a wave is now planned around *what a claim costs to
prove* rather than around a line budget.

**Left after this wave.** The queue's open rows are unchanged in kind: H12
(per-agent token accounting, and note that `mush_core::Usage` is decoded and
`agent.rs`'s `RunUsage` already folds it per run — what is missing is a *live*
count, not a source), H16 residual (the per-minute deep copy of every kept
transcript), H18 (a parent's steer to a *parked* child still fails as "gone" —
`ff315d8` fixed the adjacent worktree-gone case, not parking), H19 (a reaped
child's name stays in its parent's books), and H20 (four sentences in the
human's `prompt.rs`, plus the `edit_file` schema's `replace_all`). Of
`docs/refactor.md`'s structural rows: T1 §7 (`-y`/`--yes`/`AUTO_APPROVE`, the
human's call), T2 §17 (the attach `id` field, a wire contract), T3 §3
(`worktree_add`'s `.git` probe refuses a workspace that is a subdirectory of a
repository — settle the promise before the code), T1 §3/§4 and T2 §18/§19 (each
needs lines deleted in `prompt.rs`; §18/§19's *cap* halves are void since the
revert). The hue is §8.30.

---

## 8.30 The context budget, re-tuned — and the number that lived in ten places

The human's numbers, in their words: *"make the fold trigger at 90% of budget and
the reserve be 1/8th of the window + schema + 5000 margin"*. Both are one line
each in `crates/mush-core/src/config.rs`:

- `REPLY_SHARE_DIVISOR` 4 → **8**: the reserve's window share *is* the reply cap
  (`reply_cap` reads the same constant), so the two cannot disagree about what a
  reply costs.
- the margin 200 → **5 000** tokens: the room a *turn adds* between two requests,
  because a tool result lands in the next prompt — a request that spent its whole
  reply cap and then a tool result is the request that overflows.
- `transcript::compaction_trigger` 3/4 → **9/10** of the budget: the fold
  replaces the conversation with a summary the model then works from, so it
  should happen as late as the request asking for it still fits.

| window | reply cap | reserve | history budget | fold fires at | hard-drop at |
|---|---|---|---|---|---|
| 8 192 (default) | 1 024 (floor) | 4 096 (half-window cap) | 12 288 B = 50.0% | 11 059 B = **45.0%** | 50.0% |
| 120 000 (DeepSeek) | 15 000 | 21 200 | 296 400 B = 82.3% | 266 760 B = **74.1%** | 82.3% |
| 128 000 | 16 000 | 22 200 | 317 400 B = 82.7% | 285 660 B = **74.4%** | 82.7% |
| 500 000 | 62 500 | 68 700 | 1 293 900 B = 86.3% | 1 164 510 B = **77.6%** | 86.3% |

Against the old numbers (a quarter of the window, 200 margin, 3/4 trigger) the
fold used to fire at 43.4% / 55.4% / 56.0% of the window. Two consequences worth
stating, because they are the price of the same two lines:

- **The reply cap drops**: 120 000 → 62 500 on a 500k window, 30 000 → 15 000 on
  the shipped 120k preset. It is a ceiling, not a target, and it is *derivable*
  from the window rather than fixed — but a run that wants one huge write in one
  reply now has less room for it.
- **The store's bound rises with the trigger** (§8.28's arithmetic: what the file
  can hold is `CHILD_HISTORY × the fold trigger + the root`): 50 × 820.9 KiB ≈
  41 MiB becomes 50 × 1.11 MiB ≈ **56 MiB** for the 500k window. The lever is
  still `CHILD_HISTORY`, and the file the human is looking at is 13 MiB.

**The number lived in ten places, and that is the finding.** Changing the share
touched, in one go: the constant; two product strings (`--help`, and the home
config's own field help, which is written into the human's file); a test in
`agent.rs`; two tests in `main.rs`; two tests in `config.rs`; the manual twice;
and `scripts/session_blame.py`'s hardcoded mirror of the formula — ten edits for
one decision, nine of which were *restatements*. What it is now:

| home | before | after |
|---|---|---|
| the number | `REPLY_SHARE_DIVISOR` | unchanged, one constant |
| the words ("an eighth of the window") | typed into `--help` and `userconfig.rs` | `pub const REPLY_SHARE_WORDS` beside the divisor, interpolated by both |
| the cap's arithmetic | `request_reserve` re-derived `window / DIVISOR` while `reply_cap` added a 1 024 floor of its own | the reserve reads `reply_cap()` itself |
| tests that name a cap | 3 literals in 3 files | derive from `cfg.reply_cap()`; the arithmetic is pinned in `config.rs`, which is where the constant lives |
| the `bytes / 3` heuristic (T2 §19's code half) | spelled in `config.rs` (×3) and `app/chat.rs` (÷3, three test expressions too) | `pub const BYTES_PER_TOKEN`, read by both |
| the manual | three tenths/quarters spelled out | the rule, the numbers once, and a pointer at `mush --print-config` |
| the diagnostic script | a silent mirror | still a mirror (a separate program cannot import a Rust const) — but it prints the constants it used in `--json`, to be compared against `--print-config` |

What is *left* deliberately: `prompt.rs` still multiplies by 3 in one test
expression, and that file is the human's; `docs/refactor.md`'s ledger rows and
§8.28's measurements keep their historical numbers, because they are records of
what was true when they were measured — this section is what supersedes them.

`--print-config` is the surface that makes the small-window case honest: for the
built-in 8 192 the row reads `1024 tokens as max_tokens`, and the numbers above
come from exactly that command.

## 8.31 A window that says whose it is (`aaec739`, `mush/100`)

The human's feature, approved mid-flight and landed as one branch: two mush
windows on two workspaces were indistinguishable — same borders, same focus
badge, same selected row, same eight colours — and the screen held no fact that
could fix it, because every colour was a constant opinion rather than a
derivation. `crates/mush/src/theme.rs` (new, 143 production lines) is that fact.

**The decision.** `FNV-1a 64` of the workspace's canonical path, modulo thirty,
indexes a table of named hues. The hash is written out rather than taken from
`DefaultHasher`, whose output is documented as unstable across Rust releases: a
workspace whose colour changed when mush was rebuilt is the very confusion the
hue exists to prevent. The path is canonicalized first, so `cd work` and `cd
work/.` are one window in one colour, and hashed as typed when it does not
resolve, because `--print-config` describes a workspace that is allowed not to
exist yet.

**The palette is spaced perceptually, not by name.** Thirty hues sampled around
the CIELAB hue circle with alternating lightness, every one in the L\* 60–85
band (so the `Color::Black` text mush paints on the badge and the selected rows
still reads), minimum pairwise ΔE2000 ≈ 11.8 — a collision is rare *and* two
windows are told apart at a glance. The tests hold the ends a test can hold: a
lightness band computed in the test's own arithmetic, an RGB-distance floor, and
300 paths reaching all thirty buckets.

**Chrome wears the hue; content does not.** Seven sites in `ui.rs` — the focused
borders, the picker's frame and its selected row, the message prompt, the bar's
badge, the selected agent row, an activity line — paint `Theme::accent()`. The
alert red, the notice yellow, the dim gray and the body gray stay fixed: what
happened reads the same in every workspace, only *whose window this is* changes.
`Theme::default()` is exactly the old fixed palette, which is what keeps every
text-reading test painting what it always painted — and `painted_with` plus
`a_themed_frame_repaints_only_the_chrome` (diffs a themed frame against a plain
one: text identical, every changed cell wore `Cyan` before and the hue after, the
bar's badge included) is what proves the threading cannot quietly fall out.

**The form is the terminal's answer.** `COLORTERM=truecolor`/`24bit`, or a `TERM`
naming a direct-colour mode, gets the hue's own bytes; anything else gets the
nearest of the terminal's 256, searched over the 6×6×6 cube and the gray ramp
(the first sixteen entries are the terminal's own palette, chosen by name) —
computed from the two standard formulas rather than tabled as 240 rows, and
pinned by hand twice (teal → 37, and the whole table checked against a palette
rebuilt in the test's own terms). The `Color` variant *is* the form, so nothing
can describe a window as truecolor while holding an index.

**`MUSH_THEME` overrules it, and a typo costs a message.** A hue's name, `256`
(demand the indexed form even on a truecolor terminal), `off` (the fixed palette
of every version before this one), `auto` (the unset spelling). An unknown value
is a startup error naming the value and listing every spelling that works, built
from the table. The three variables are read once, at the edge, by one
`EnvText::read()` — the same house rule §8.29's C1 enforced for the other
`MUSH_*` readers — and everything below it is a pure function of that value.

**`--print-config` gained the row, and it is true for the terminal it ran on**:
`theme  olive (truecolor, from the workspace path)`, or `(indexed, …)`, or
`(…, MUSH_THEME)`, or `off (MUSH_THEME)`. The form word is the accent's own
variant, so the sentence cannot disagree with what a frame will paint. Verified
live on the merged tree: three workspaces → `apricot`, `amber`, `fawn`; each of
them `(indexed, …)` with no `COLORTERM`; `MUSH_THEME=teal` → `teal (truecolor,
MUSH_THEME)`; `MUSH_THEME=256` on a truecolor terminal → `(indexed, …, 
MUSH_THEME=256)`; `off` → `off (MUSH_THEME)`; `tale` → exit 1 with all thirty
names; and `.mush/` is still not created by a dump.

A real pty (34×110, `pty.fork`, the whole session killed with `killpg`) shows the
bytes rather than the intent: `/tmp/mush-theme-pty` hashes to olive `#9AA845`,
and the captures carry `ESC[38;2;154;168;69m` with `COLORTERM=truecolor`,
`ESC[38;5;107m` without it (107 = `#87AF5F`, the cube entry nearest olive),
`MUSH_THEME=off` with no `Rgb` anywhere, and `MUSH_THEME=teal` the teal bytes.
One caveat the capture taught us: `ESC[38;5;6m` appears in *every* capture — the
footer's `#0 you(rootagent)` is a `Color::Cyan` in `app/screen.rs`, not one of
the seven accent sites.

**What did not move, and is now a decision owed.** The *conversation's* colours:
the cyan `you ›` and `brief ›` voices and the magenta `parent ›` in `app/chat.rs`,
and the footer's cyan agent id in `app/screen.rs`. They are speech rather than
chrome, and a voice whose colour changed per window would be one more thing to
learn — but a themed window still shows fixed cyan sentences, so whether the
voices should follow the hue is a ruling, recorded in §8.32.

**Cost.** +19 tests (15 theme, 2 `ui.rs`, 1 `app`, 1 `main.rs`), mush 515 → 534,
mush-core untouched at 128, fmt and clippy clean, all three smoke scenarios
passing. The census at the merge: **48 920** total · **7 161** prod · **26 316**
tests · **12 273** comments — against 47 831 / 7 061 / 25 698 / 11 971 at
`ff315d8`. A feature that is mostly a palette and its guarantees costs 143
production lines and 413 test lines in its own file, and 100 production lines
net across the crate.

One merge note, because it is the kind of thing that looks like a bug later: the
branch was cut from `ff315d8` and `b87bb40` (the budget re-tune) landed before
it, so `main.rs` conflicted — in exactly one test line, where the re-tune had
added `let cap = plain.reply_cap();` above a `describe` call that the theme
branch had changed to take a `Theme`. Both survived; no fix was lost.
