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

## 1. Observed live: the agent tree lies about who is working

These were seen in the session that produced the M2.75/M2.8 work, while four
subagents and their children ran. They share one cause, and it is R0's cause
again: something on the screen was a stored *conclusion* ("has children",
"busy") rather than a derived fact (`Phase`, and the instant it began).
`docs/refactor.md` §3.1's `AgentTree::busy` was made a method, and the row and
the title now read that one derivation instead of each making their own.

| ID | What | Status | Seen where · closed by |
|---|---|---|---|
| U1 | **A working agent is drawn as paused.** A row renders `⏸` for an agent that is still working, because the glyph comes from "has live children" rather than from the agent's own phase. The orchestrator's four children all showed `⏸` while each was mid-turn — the icon said "waiting" about agents that were busy, which is the exact class of lie §4.5 R0 set out to make impossible. | ✅ | seen in the tree pane, all four rows, whole run — closed by `ui::phase_glyph` (a function of the node's own `Phase` only) with the children as a separate `⏸N` count (`ui::agent_line`) |
| U2 | **The pane title counts waiting agents as working.** The title reads `agents · 6 running · …` while some of those agents are parked waiting on children (and one was stopped). A count is a derived fact like any other: it must be computed from the phases, and it must say what it counts. | ✅ | seen in the agents pane title — closed by `AgentTree::roster` deriving `working`/`waiting` from the phases (`app/tree.rs`), and `ui::agents_title` naming each count |
| U3 | **Scrolling up in a finished agent's transcript is undone by any other agent's news.** While the human reads the scrollback of an agent that has finished, a message from *any* agent still working snaps that pane back to the bottom. The pane's scroll position is a fact about the human's reading, and it is being reset by events that have nothing to do with the agent being read: `app/mod.rs` calls `Chat::scroll_to_bottom()` from seven sites, including the `Message`/`Status`/`Notice` arms of `on_agent`, and `Chat::scroll` is one number for the pane rather than a position owned by the conversation it belongs to. | ✅ | closed by `Chat` keeping a per-conversation `Reading` (`Holding`/`Following`), so only the pane's own agent moves it, and `painted` marking a held window in the title (`scrolled ↑N rows · PgDn`) |
| U4 | **A grandchild is not drawn under its parent, but after everything spawned before it.** The tree keeps `agents` as a `Vec` in spawn order (`AgentTree::agents.push`, `app/tree.rs`), and `ui.rs` only uses `node.depth` to indent: so a depth-2 agent appears below every previously spawned agent, not under the agent that spawned it. The parent link and the depth are both in the node — the *order* is the thing that was never derived. | ✅ | closed by `AgentTree::rows` walking pre-order over the parent links, so a child sits under its parent's subtree |

These four shared one cause, and it is R0's cause again: something on the screen
was a stored *conclusion* ("has children", "busy") rather than a derived fact
(`Phase`, and the instant it began). `docs/refactor.md` §3.1's `AgentTree::busy`
was made a method, and the *row* and the *title* now read that one derivation
instead of each working out its own answer.

## 1.5 Observed live, again by the human using it

| ID | What | Status | Home |
|---|---|---|---|
| U5 | **The same fact is on screen three times.** The newest tool call / activity shows in the conversation pane (as the `⚙` line), again at the bottom of the agents pane, and again in the first line of the status bar — one fact, three homes, no reader. §4.5 R2 spent line one on "activity › status › hint"; the activity is already the row's and the transcript's, so line one is repeating what a human can already see, and the three surfaces need to be looked at together rather than one at a time. | ✅ | closed by `App::tree_line` — the bar's line one is the napping-root fact, or an event with no other home, ranked through `chat::Rank` (`ui::bar_line`) |
| U6 | **An agent is a bare number.** Rows read `#2`, and everything else is inferred from a message; nothing names the *task*. A short title per agent, derived from its brief (and a command label for a job), would let the human tell two children apart without opening them — the brief is already in the node and in the transcript's first line. | ✅ | closed by `AgentNode::title` (`app/tree.rs`), derived from the brief on read |
| U7 | **A waiting agent still says `⠏ working…`.** When an orchestrator has ended its turn and is waiting on children (or on a job), the activity row claims work is in flight. It should say so differently from a model call that is actually in flight — the hourglass the human asked for — which is the same derived-facts rule as U1/U2, one surface further down. | ✅ | closed by `Phase::waiting` (`app/tree.rs`) telling a model call from `wait_agents`/`wait_commands`; the row and the foot say which |
| U8 | **A transient notice never leaves.** `· reply cut off at 20480 tokens — asking for smaller steps` and help output sit in the foot forever (until that agent runs again), so a line about *one moment* outlives it and pushes the conversation around. §4.6's per-kind lifetime answered this for failures and command answers; the "said" rank still has only one lifetime. Somebody must decide which notices are news and which are chatter — and repeated identical lines (`· model produced an empty reply` ×N) should collapse rather than repeat. | ✅ | closed by the chatter lifetime in `app/chat.rs` — `clear_notes_for`, `dismiss_said`, `SAID_TTL = 120 s` — and the `Notice.count` collapse |
| U10 | **Walking back up a deep tree costs one keypress per ancestor.** In the agents pane the only vertical moves are `j`/`k`, arrows, `g`/`G`: with twenty children under one parent, getting from a grandchild back to the root is twenty presses, or `g`, which loses the place you were reading. `←` should put the selection on the agent's **parent** (and the natural companion, `→`, on its first child), which is a fact the node already carries (`AgentNode::parent`) and which the painted order (U4) makes meaningful. It needs the same treatment as every other binding: one `Intent` in `app/keys.rs`'s table, the module doc and `--help`'s KEYS prose updated in the same commit, and a test at three levels of depth. | ✅ | closed by `Intent::TreeWalk` on `←`/`→` in `app/keys.rs` (plus `PickerMove(±PAGE)` for a deep picker); pinned at three levels of depth |
| U9 | **The default DeepSeek window/reply cap is far too small.** A real run was cut off at 20480 tokens; for the configuration mush ships, the default should be ~120k tokens (window, and the reply cap where the vendor accepts it) rather than a value that truncates ordinary work. Precedence must not change: a human-stated window still wins, and an endpoint-reported one still overrides the default. | ✅ | closed by `provider::PROVIDERS`' fallback of 120 000 and `Config::reply_cap` (a quarter of the window, floored at 1 024 and capped at 120 000) |
| U11 | **Compaction happens with no visible state anywhere.** The human typed `/compact` and could see no indication that anything was happening: no "folding…" status, no progress, no answer to the only question a frozen pane raises — *may I keep typing?* Three quarters of the shape is already built (`/compact` says one line in the bar; `compact_history` emits `AgentEvent::Status("compacting on request…" / "context nearly full — summarizing…")` and later `AgentEvent::Compact`; §4.6's notice machinery exists), and the gaps are: (1) a `/compact` that arrives **while the actor is running** is *parked* (`state.compact_requested = true`, honoured only at the next message boundary in `agent.rs`) and nothing at all says so — the human waits for a bar line that has already faded; (2) nothing *distinguishes* a fold from an ordinary model call or from waiting on children, at the moment when the model call can take the whole 10-minute deadline (×3 with the transport retry); (3) nothing says whether sending is blocked. It is not: mush never blocks input, the words queue and are answered after the fold — which is exactly the fact the human needs and the screen never states. **And the sharpest form of it, found in the source after the human added "compaction doesn't wake up agents?? … that's odd": an idle fold is work the screen refuses to show at all.** `AgentTree::activity` (`app/tree.rs`) opens with `if !self.is_busy(id) { return; }`, so for an agent at rest the `AgentEvent::Status("compacting on request…")` the actor emits *before* its summarize call is **dropped on the floor**: the agent pays for a real blocking request (up to the 10-minute deadline, ×3 with the transport retry) while its row still reads `·`/`✓`/`✗` and every derived surface says "at rest". Only the *after* line (`context compacted — continuing from a summary`) ever appears. The one setter that could move the agent refuses to move an agent that is not already busy — and the idle fold's cancel flag is minted locally (`AtomicBool::new(false)` inside `compact_now`), so nothing can cancel it either. | ✅ | closed by `Phase::Compacting(Compacting::{Parked, Requested, NearlyFull})` with `Phase::compacting()`/`words()` in `app/tree.rs` — the row glyph (`≡`), the foot, the footer, the pane roster and `App::tree_line`'s sentence all read that one answer — by `compact_now` owning the fold's cancel flag (so Ctrl-C reaches an idle fold; it was minted and dropped inside the call and nothing could flip it), and by `agent.rs` emitting `Compacting` on *accept* (`Parked` from `drain_signals`, `Requested`/`NearlyFull` from the two callers). The row's own correction, verified against the source: the *automatic in-run* fold was already visible (its `Status` line survives for an already-busy agent); the genuinely silent cases were the fold requested at rest — whose `Status` the busy guard dropped — and the one parked behind a running tool, which emitted nothing at all. Merged in `fb7265d` |
| U12 | **The pane title and the bar disagree about a stopped or failed root that still has a child working.** `AgentTree::roster` counts a waiting agent only when the phase is `Idle | Done` ("a failed or stopped agent waits for nothing"), while `App::tree_line` counts `busy_children > 0 && !phase.is_busy()` — so a stopped root over a running child gets `0 waiting` in the title, `waiting on 1 subagent(s) — the root resumes as they finish` on the bar, and `⊘ … ⏸1` on its row. The bar is right: the root *does* resume when the child's completion folds in (`absorb` → `Fold::Run`), and the row's `⏸N` already says so. Found by the duplication review of `fb7265d`. | ⬜ | one predicate — `AgentTree::napping(id) = !node.phase.is_busy() && busy_children(id) > 0` — read by `roster`, `tree_line` and the `⏸N` mark, with `the_bar_and_the_title_agree_on_who_the_root_waits_for` extended to stop the root |
| U13 | **After a restart, a stored isolated agent whose worktree is gone keeps a branch its actor does not have.** `restore_agents` passes the stored `branch` straight to the node (`app/mod.rs`), while `revive` filters it on `worktree_path(root, id).exists()` and points the actor's workspace at the root — so the restored row offers `/diff`/`/merge` for a reclaimed directory and the footer paints a dead path, while a nudge is refused by the UI guard even though the actor would have run it in the root. Two surfaces contradicting the promise both restore paths make ("continues in the main checkout"). Found by the duplication review of `c4aa2e3`; untested (both restore tests store `branch: None`). | ⬜ | one decision at restore: filter the stored branch once in `restore_agents` and hand the same value to `revive` and to the node, with a test that stores a branch whose worktree is gone and asserts the node drops it, the nudge is delivered, and `/diff` stops naming it |

## 2. Observed live: a delivered completion is invisible, and can be delivered twice

| ID | What | Status | Home |
|---|---|---|---|
| B20 | **A child's completion reaches the model but not the screen, and can be folded twice.** `fold_completions` (fixed below) pushes the `#N done: …` line into the actor's `messages` only; no `AgentEvent::Message` is emitted, so the UI's copy of the parent's transcript never shows it. The next idle `Run` then hands the UI transcript back, `absorb` finds no such line in it and re-arms delivery for the same child — so one completion can be folded into the model's transcript a second time. | ✅ | closed by `agent::push_line`, which folds the line into the actor's transcript *and* emits it as `AgentEvent::Message`, and by `absorb` marking whatever completion line an adopted transcript already carries as delivered, so it cannot re-arm |
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
| B22 | **A steering message to a subagent is invisible in that agent's transcript, and an idle target was not woken by it.** `agent_control message` answers success; no child's transcript shows the line. | ✅ | closed by `AgentMsg::Steer` → `push_line` (folded *and* emitted) and its `Fold::Run`, which starts an idle target — the delivery half; what is left is H5's honest reply |

## 2.75 Observed live: a hiccup on the wire kills a whole run

| ID | What | Status | Home |
|---|---|---|---|
| B23 | **A transient transport failure ends the run instead of being retried.** Several agents in this session died mid-work with `cannot reach https://api.deepseek.com: Connection reset by peer (os error 104)` — one of them had committed nothing, another was killed by the *harness* process dying around it, and a third lost a run's worth of edits. Every one was a transport hiccup, not a refusal: the endpoint had no opinion about the request. A bounded retry (three attempts with a timeout, backing off) for *transport* failures only — never for a cancellation, a status the endpoint chose, or a body it deliberately sent — would turn "the run is dead and its worktree is half-edited" into "the run paused for a second". Two rules make it honest: Ctrl-C must still abandon a request immediately (the cancel flag is polled between socket slices and must be checked between attempts), and the human must be told (`retrying — connection reset (2/3)`) rather than watching a spinner that looks stuck. | ✅ | closed by `model.rs::retrying` — `RETRY_ATTEMPTS = 3` over the `Clock` seam, transport failures only, the cancel flag read before every attempt and between backoff slices, each retry announced in the transcript |
| B25 | **A signal during a model read is reported as a failure of the endpoint.** Found while reproducing the fold's visible state: a `SIGWINCH` (a terminal resize) arriving during a held read surfaces as `could not compact: cannot reach http://…: Interrupted system call`, and the same read under an ordinary run ends the turn with that text — `EINTR` is a retryable read, not a refused connection, and it is currently classified like one (`http.rs`'s read loop treats the error as terminal, and `model.rs`'s transport classifier deliberately excludes `Interrupted`). A human who resizes the window while mush is answering can therefore lose a run to their own window manager, and the message blames the endpoint. | ⬜ | `http.rs`'s read loop (retry `ErrorKind::Interrupted` inside the read, before any classification) and `model.rs`'s classifier (keep excluding `Interrupted` from *transport* if the read loop handles it, and say so in the comment); a test that interrupts a read and expects the reply, not a `cannot reach` |

## 3. The contract bug that started this file

| ID | What | Status | Home |
|---|---|---|---|
| B21 | **A parent in a tool-calling chain never heard its child finish.** `ChildDone` was *recorded* by `drain_signals` between tool calls but only *folded* into the transcript on a turn where the model called no tools, so an orchestrator that kept working ran arbitrarily far past its child's completion — contradicting `docs/mush.md` §5.5. Seen live: agent #2 spawned #6; #6 finished its whole task (commit `2ab595c`); #2 made 67 further turns, every one with tool calls, and its transcript still held nothing but its brief. | ✅ | the fold moved to every message boundary (the tool-free turn *and* the gap after a batch's results), validated by an independent negative-checked test (`mush/9`) |

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
| S2 | **`Enter` on an agent row focuses the transcript but not the keyboard — the message you type is eaten.** After `Enter` the pane is `agent #1` and the box prompt reads `#1 ›`, but the bar still says ` agents `: the tree kept the keyboard. Typing `hi again` sends nothing (0 model requests); the tree cursor moved and the visible pane silently switched back to the root — and a stray `c` in the text would have cancelled the agent. Only after a further `Tab` does the nudge reach the child. `README` promises "`Enter` on a row focuses that agent — the chat switches to its transcript and typing nudges it". | ✅ | closed by `App::focus_cursor_row` setting `Focus::Chat` when it focuses a row, so the bar badge, the paintable border and `keys::key` all read one value; the keymap stays a pure table and `App` the only effector. Landed in `c4aa2e3`; `enter_on_a_row_moves_the_keyboard_with_the_focus` (the letters reach *that* agent, the cursor does not move, the agent is not cancelled) and `a_c_in_the_chat_types_a_c_and_cancels_nothing` (the inverse) pin it, and the pty reproduction typed the agents pane's own keys (`gcj write extra.txt`) into the child's mailbox |
| S3 | **A `session.json` mush cannot parse is dropped silently, then overwritten.** A 648-byte session with a long conversation and one agent, if any field does not match the schema (a bisect showed `"status": "failed"` — the real encoding is `{"failed": "…"}` — is enough), comes back as an empty app with the empty-state hint, no warning on screen, no entry in `/notes`. `Session::load` returning `None` is indistinguishable from "there is no session file", and the first save rewrites the file: the old conversation is gone, with no backup. Docs §5.2 promise "Restarting mush brings the session back at rest", and mush is the only writer — so version skew or a hand edit reaches this. | ✅ | closed in `e747bc3`: `Session::load` returns an absent/unreadable outcome, `main.rs`/`app/mod.rs` paint the path, the parse reason and `kept as .mush/session.json.bak`, and the unreadable file is copied byte-identically to `.bak` before the first fresh save (`an_unreadable_session_is_told_apart_from_an_absent_one`, `the_unreadable_session_notice_names_the_file_the_reason_and_the_backup`, and the real-binary check: the `.bak`'s sha256 matched the original's after a clean quit that wrote a new session) |
| S4 | **Ctrl-Q does not kill a foreground `run_command`'s process group.** A root `run_command` of `sleep 10; touch marker` is still alive after mush exits cleanly, and the marker appears 10 s later; a `sleep 40` was resident the moment after quit. Detached *jobs* are killed correctly (the heartbeat job stops at Ctrl-Q). Docs §5.6 rule 2 promise "they die with … mush itself — its process groups are killed on exit". | ✅ | closed in `e747bc3` by `jobs::Foreground` + `Registry::hold(owner, job)`: a foreground tool call's process group holds a slot for the whole call, so `kill_all` (quit), `kill_owned` (`Stop`/`/new`/`Shutdown`) and `Registry::drop` reach it as they reach a job, while `Launch::started`/`Launch::held` keep the run-once guarantee and nothing is signalled in `Foreground::drop` (a finished command's pgid may be reused). Real binary: the `sh -c` is gone the moment mush exits, the marker never appears, a detached heartbeat job still stops, and a self-exit still reports its real status (`quitting_kills_a_running_foreground_command`, `a_foreground_command_killed_from_outside_reports_cancelled`, `the_three_ways_a_foreground_command_ends_are_not_confusable`) |
| S5 | **The docs and README disagree with the keys for stopping agents.** Live, on a build whose docs predated the key wave: `Ctrl-C` stops the focused agent and the child kept working; `Ctrl-X` stops them all. `mush --help` and `/help` say exactly that, and so do `README.md` (its anywhere-row and its key table now carry both) and `docs/mush.md` §4's key table — the drift the row recorded was real and is closed. A human who learned the keys from an *older* README pressed `Ctrl-C` on a runaway tree and one agent kept burning tokens. | ✅ | closed by the doc wave: `README.md` and `docs/mush.md` §4 both name `Ctrl-C` (focused) and `Ctrl-X` (all) |
| S6 | **`/compact` on a transcript with nothing in it does nothing and says nothing.** Immediately after launch, `/compact` paints the bar's `compacting #0…` and then the untouched empty state forever: 3 s later the same, `/notes` empty, nothing on the wire. After any run the documented refusal does appear (`· nothing to compact — this transcript is already short enough to send whole`), because the guard is `matches!(messages.first(), Some(system))`, which is false for an empty transcript and returns without a word. Docs §3 promise "The refusal is said out loud because a human typed a command — silence there is indistinguishable from a fold that quietly failed". | ✅ | closed in `e747bc3`: the empty transcript says the same refusal as the short one (`a_fold_of_an_empty_transcript_says_so_instead_of_nothing`), and the automatic trigger is still told nothing |
| S7 | **In a non-git workspace, `isolated: true` runs in the shared workspace and only the model is told.** In a plain directory, asking for an isolated child puts `iso.txt` in the root, the row has no branch, and the spawn line says nothing; the model's request *did* carry `(isolated unavailable: not a git repository; running in place)`, but no notice reaches the pane, `/notes` or the bar. Two "isolated" siblings would edit the same files while the human believes otherwise. Undocumented either way. | ✅ | closed in `e747bc3` for the notice half: `isolated unavailable: not a git repository — it shares this workspace` reaches the parent's pane while the row carries no branch (`a_degraded_isolation_is_said_to_the_human_too`), and `docs/mush.md` §5.5 now names the in-place fallback in the same sentence that promises the worktree, so both halves of the row are closed |
| S8 | **Four places where the screen or the docs read badly, none of them a lie at the seam.** (i) The commit subject is the brief **truncated at 60 chars with `…`** (`mush #1: create a file iso.txt containing exactly: isolated w…`), so a landed commit's history cannot be matched to the brief verbatim, while docs/README write it as `mush #N: <brief>`. (ii) `/diff` paints only the tail two rows of the diff above a `+N more lines · /notes` row, so no `+`/`-` line is visible until you type `/notes` — "read `/diff 2`" does not, by itself, show the change. (iii) the pane title says `agents · 2 working · 1 waiting` where docs §4.5 said `2 running` — ✅ corrected, and the example frame no longer draws the marker glyph the pane stopped having. (iv) `scripts/mock_llm.py`'s `TURNS` scenario waits for the phrase "turn limit", which the prompt no longer contains, so that scripted run ends on the loop guard instead; `README` and `docs/mush.md` §10 already agree that no test refers to the script, so only the scenario is stale. | 🔄 | (i)/(ii)/(iii) landed in `c4aa2e3`: `subject_brief` takes the brief's **first line** and cuts at a word boundary with `…` (docs' `<brief>` wording is the imprecise side and was left to the doc hand), and `/diff` now keeps the head *and* makes it useful — each file's git preamble (`diff --git`, `index`, mode, `---`, `+++`) folds into the file's first hunk row, so a `+`/`-` line is visible without `/notes` (`the_diff_pane_shows_the_first_hunks_not_gits_preamble`). Also corrected: the row's own premise was stale — the foot kept the head, not the tail; the defect was the preamble. **(iv) still open**: `scripts/mock_llm.py`'s `TURNS` scenario waits for a phrase the prompt no longer contains, so that scripted run ends on the loop guard |

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
| H1 | **No live view of a subagent.** The only window into the tree was `.mush/session.json` (3.7 MB, the *UI's* copy), so "did #2 ever see #6's result?" had to be answered by hand-parsing JSON. That copy cannot show what the actor knows — `delivered`, parked commands — which is the root of `B20`/`B22`. | The session's central diagnostic was archaeology, and the first diagnosis was wrong *because* the file could not say what the actor held. | ✅ for the tree half, `M3` landed at `f29b352`: `mush agents` reads the roster the tree pane paints over `.mush/mush.sock` (id, parent, phase, title, branch, worktree, activity, working children, summary, revision) and `mush read --agent N` reads a transcript with a revision, so "did #2 see #6's result?" no longer needs `.mush/session.json`. **Still open from this row:** what the *actor* holds (`delivered`, parked commands), whether a result is finished-but-unread, and whether a worktree is dirty — `agent_status` should carry each child's branch, whether its worktree is dirty, its last activity, and whether a result is finished-but-unread — and the session file should carry the delivery/parked facts |
| H2 | **A run that was cut off looks exactly like one that finished.** Agents #1 and #2 died when the harness process was SIGTERM'd and #18 died on a transport reset; on screen and in the file they were simply "idle", with no summary and no marker, and their work sat uncommitted until someone went looking. | Two runs' worth of work recovered by hand; four later agents spent their first minutes finishing someone else's tail. | a `CutOff`/`Interrupted` outcome distinct from `Done`/`Failed`/`Stopped`, written to `session.json`, painted on the row, and reported to the parent as "cut off, nothing committed" |
| H3 | **An isolated run's automatic commit says `mush #N: <the whole brief as typed>`.** It saved the biggest branch of the wave from a killed process — and then had to be amended by hand because the subject was an 800-word paragraph. | One commit message rewritten; the auto-commit hides what a merge body then has to explain. | `agent.rs`'s `commit_subject`: a subject from the brief's first line (or the outcome), the brief in the body |
| H4 | **Nothing tells the human that a child finished**; only the parent's transcript hears it (after the fold fix), and the row's mark changing is all the screen says. | A whole review pass answered "did #2 see #6?"; the human asked the same question. | a `Notice` or bar line on `ChildDone` in the *parent's* pane, and a row mark for "result unread" |
| H5 | **Steering was not a capability you could trust.** `agent_control message` answered `messaged agent #N` for four agents, none of whose transcripts held the line, and an idle target was not woken. Fixed on `mush/15` (`AgentMsg::Steer` → `push_line`), but the *reply* still says "messaged" whether or not anything happened. | Four agents worked for an hour without the rule they were sent — including "do not spawn any more subagents". | ✅ delivery (`AgentMsg::Steer` → `push_line`, folded *and* emitted; it is work to answer, so an idle target wakes); left is the honest reply (delivered / parked / undeliverable) |
| H6 | **Timing-sensitive tests in a suite that runs while ten agents build on one box.** `wait_commands_returns_a_jobs_report` failed about half the time under load (a test racing a clock it had told to lie; fixed), and `a_frame_fits_in_a_60fps_budget…` is still load-sensitive. | Every flake costs an agent a retry it cannot tell from a real failure, and a gate that is green "usually" is not a gate. | the `Clock`/`Machine`/`Events` seams exist for this: no test may depend on wall-clock availability, and a budget test should say "on an idle box" or be `#[ignore]`d |
| H7 | **A spawn cannot name its base.** An isolated child branches from its parent's working tree — usually right, occasionally exactly wrong ("start from `master`"), and then merges had to be done by hand. | Merge labour; one branch re-cut. | an optional base (branch or sha) on `spawn_agent`, and mush saying which commit a child started from |
| H8 | **No picture of the machine.** Each parallel worktree pays its own `cargo build`, so with a dozen agents the box is the bottleneck and nothing on screen says so. | Self-imposed serialisation; the same tree compiled many times. | `M2.8`'s registry shows jobs now; the other half is a shared `target/` or an honest warning |
| H9 | **Quitting kills the agents' process groups — including agents mid-task.** Correct per §5.6, and exactly how #1/#2 lost their runs when the harness went down. | Two runs. | ✅ `integ57`, merged as `ae16cb2`: the first `Ctrl-Q`/`/quit` arms a two-step quit and paints what it will kill (`Ctrl-Q again quits · kills #0 run_command + 1 job`), composed from the one in-flight list `Ctrl-C` already uses, bounded to 72 columns with what does not fit counted as `+N more`; the second press quits, `Ctrl-C` or any typing key disarms, and nothing live quits on one press. The ranking (`Quit` outranks the tree's own line) lives in `app/screen.rs`'s `bar_word`. A detached mode was not added |
| H10 | **Worktrees and branches accumulate and nothing prunes them.** Twenty-two were live at the end, most finished and merged; `/worktrees` also claims "none" while one is on disk (`P10`). | Disk, and two audits that counted the source twice until it was cleaned. | ✅ the `/worktrees` message now counts what is on disk under `.mush/wt` and says how many of those are registered (`App::worktree_report`, finding P10); left is a `mush prune` |
| H11 | **Docs and code drift silently, and the drift *is* a finding.** One wave left `docs/mush.md`'s key table, §4.5's glyphs, §4.6 in full, §8's deadline and §9's milestones stale, plus `docs/refactor.md`'s checklist statuses; every reviewer spent budget on it and one fix wave existed only for sentences the docs asserted. | Repeated re-derivation, and a doc-sync wave owed at the end of every wave. | a status row that must move when a finding closes (or a test that fails when a documented status and the code disagree), and docs updated in the same commit as the code |
| H12 | **Context and reply caps were the quiet bottleneck.** A 20 480 reply cap truncated real work mid-task (`U9`), and several agents burned turns on runaway guards and compaction instead of the task. | Several runs cut off mid-edit. | done for the shipped defaults (`U9`); what is left is per-agent accounting (`M6`, §11.2) so the cost is visible while it is spent |

| H13 | **The exclusive machine lock is machine-wide, and a refused command is a trap.** The harness's lock is taken by a *command* (`exclusive=true`), which the tool's own guidance recommends "for anything timing- or port-sensitive" — and every review brief flags the 60fps frame test as load-sensitive, so a reviewer takes it to get one clean number. While it is held, every sibling's command *and the orchestrator's own* is refused ("#N holds the machine; retry when it finishes"); the refusal is an **error, not a queue**, so an agent that retries it is doing exactly what `LOOP_ROUNDS` counts, and mush's own guard then kills the run: `#65` and `#66` died mid-work this way, and the orchestrator could not even read a file for the duration. | Two agents' runs lost (one integrator, one fixer), a stalled wave, and an orchestrator blind at a moment it had to inspect. | three separable fixes: a refusal should say "wait, do not retry" (or offer a bounded wait), the orchestrator should be exempt from a lock it did not take, and a call that was refused *before running* must not count as "nothing changed in between" for the loop guard |
| H14 | **A run stopped as a loop cannot be resumed.** `Ctrl-C`-stopped agents resume when you message them (that is the promise on the row: `stopped · re-send to resume`), but two loop-stopped agents (`✗ the run was stopped as a loop: the same tool call repeated 6 times with nothing changed in between`) re-stopped **immediately and identically** on the nudge — so the one place a human would first try to recover is where resuming does not work. Either the repeated call is still in the window the guard counts, or the resumed run's first call is counted against the old rounds; either way the orchestrator had to spawn a fresh agent with a rebuilt brief (cheap only because the dead one's work had already been committed — `c3f5984`). | Two agents re-spawned instead of nudged; the loop-stop's own advice ("re-send to resume") is unactionable. | a resumed run should start a fresh round count (or the stop should not leave the repeated call in the window), and the row's own words should not promise a resume the guard refuses |

| H15 | **`wait_agents` answers from history, and nothing says what a wait releases.** After the crash the orchestrator re-spawned six children; its first `wait_agents` (no ids, a long timeout) returned the summary of `#10` — a child of the *previous* process, finished before it died — which reads exactly like a live completion. Spooked, it then named one id, and `ids=[…]` means "wait only for that one", the opposite of the intent (be woken by *any*). The tool's own description calls the answer "its summary" and never states the release rule, so the call has to be reasoned out from the implementation: no ids = first finish, `ids` = only those, `all` = every child, `timeout` = a bound. | A blind 900 s wait while two children were dead, and one result that reported the past as if it were news. | the answer should separate what is *new* (an `✉` result nobody has read) from what was already read; the tool's description should state the release rule; and a failure should end a wait (F1/F4) |

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
| A1 | **The revision steps backwards across `/new`, so a client silently desyncs and a stale `edit` lands.** `Chat::clear`/`forget` drop the counter, so it restarts at 0 and collides with a revision the client already holds; nothing on the wire carries the conversation's identity, though `ConversationId` exists for exactly that. Raw wire, real binary: after a first message `read` says `revision 1` with `line 0 = "first message"`; after `/new` it says `{"lines":[],"revision":0}`; after a second message `revision 1` again with the new line at `line 0`; a client polling `since=1` never sees it, and its `base=1` edit is **accepted** (`{"id":5,"ok":{"revision":2}}`). | ⬜ | `Chat`'s revision must be process-monotone (`clear`/`forget` must not reset it), and the epoch belongs in the `read`/`agents` payload; test: `revision(ROOT)` never decreases across `new_chat()`, plus a read → `/new` → `edit` that must `conflict` |
| A2 | **One idle client wedges the whole attach surface.** `accept_loop` is serial and `ask` has no read timeout, so a connection that opens and says nothing blocks every other client: holding one open, `mush agents` did not return in 8 s (`TIMEOUT`); closing it returned immediately. | ⬜ | a thread per connection (or a read timeout / idle cap) plus a read timeout in the CLI; test: hold A open, send on B, assert an answer via `recv_timeout` — it fails today |
| A3 | **`mush read` cannot frame a multi-line transcript line.** `print_lines` writes the decoded text raw, so a line containing `\n` (the wire is correct: `"text":"first\nsecond"`) prints as two lines under one index and no external parser can tell continuation from a new line. | ⬜ | escape newlines (or offer JSONL / `--json`); test: a `print_lines` unit test over a body with an embedded newline |
| A4 | **`edit send` does not take the path a typed message takes, though its comment claims it does.** The typed path (`send_message`) trims, refuses an empty box and parses commands; `attach_edit` calls `expect_human(text)` + `deliver(text)` with the raw text. Sending `"text":""` with `send:true` is accepted, adds an empty user line to the transcript and starts a run. | ⬜ | trim and refuse empty; state explicitly whether a client's text is ever a command; test: an empty/whitespace `send` is `bad_request` and changes nothing |
| A5 | **The `--` escape is claimed but half exists, and a directory named after a subcommand is unopenable.** `Cli::detect`'s doc says "`--` is the escape hatch a human has", but it only escapes as the first argv (`mush -- agents` skips detection — a second arg then errors with "only one directory may be given", proving `agents` was taken as the directory), it is rejected *inside* a subcommand (`mush read --` → ``unknown option `--` for `mush read` ``), and `mush agents` in a directory literally named `agents` runs the attach CLI instead of opening that directory. Nothing in `--help` names either form. | ⬜ | stop `detect` at `--` and document it in the ATTACH block; test: `parse(&["read","--"])`, and that `["--","agents"]` is not a subcommand |
| A6 | **`bad_request` carries two failures that are not the request's fault:** "mush is shutting down" and "the UI dropped the request", so a client cannot tell a transient shutdown from a malformed request. | ⬜ | an `unavailable`/`shutting_down` reply kind; test: `ReplyError::describe` for each kind |
| A7 | **A spawn failure leaks the socket file.** `serve` builds the `Guard` before `spawn`, and the `?` on the spawn returns while the listener is dropped, leaving the file with no guard to remove it (cosmetic: the next run clears it). | ⬜ | remove the file (or drop the guard) on that error path |
| A8 | **`attach_worktree` reports a worktree that is gone** — a path whenever `branch` is `Some`, including a merged/discarded agent, which is the case `worktree_gone` exists to detect; the roster carries no `landed`. | ⬜ | report the path only when the worktree is on disk (or carry `landed`); test: the roster of a discarded agent says it is gone |

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
| V1 | **The `▲N`/`▼N` counts are a model of the `List`'s scroll, never compared with the rows actually painted.** The pane's window geometry (`inner`, `footer_rows`, `list_area`) is derived in `app/screen.rs` to compute the counts and *again* in `ui.rs` to place the list; the counts are arithmetic over the first, by assumption. Proof: deleting the painter's separator row (`ui.rs`'s `footer_rows`) fails **only** `page_keys_move_the_tree_cursor_a_page_and_clamp_at_both_ends`, at 80×24, with a message about the highlight — the 15×14 sweep does not notice, because its "twenty agents" state asserts the title's words and never `▲/▼`. So the two can drift and the count can lie about the window. | ⬜ | one `AgentsPane::list_area: Rect` set where the pane is laid out and read by both (refactor §11 `R25`); protecting test: an assertion that reads which rows the list window holds next to the counts |
| V2 | **The size-tier boundaries are pinned by nothing.** Moving `screen.rs`'s `area.width < 80 \|\| area.height < 20` to `< 19` (or `< 79`) leaves all 398 tests green, though the sweep re-lays out 15 sizes — its own doc claims its `words` are "the ones a bug in the size tiers would take away". The bar's edge at 24 *is* pinned (`the_facts_line_survives_at_80x24`, which fails on 24→25). | ⬜ | a test that 79×24/80×24 and 60×19/60×20 paint the stacked vs the side-by-side layout (the tree's row width, or the `▲/▼`-free title) |
| V3 | **The rewrite dropped the old sweep's both-focus-states pass.** `the_layout_survives_every_size` drew every size in both focus states on purpose ("a border is painted differently when it is focused and a focused-but-tiny pane is the awkward case"); `Focus::Agents` is now painted at exactly one size in the whole suite (120×32), because no sweep state changes focus from `App::new`'s `Focus::Chat`. The practical loss is small (focus changes the border colour and the chat cursor) but the doc should not claim a sweep it does not run. | ⬜ | set both focus states in the sweep, or say in the sweep's doc that it paints one |
| V4 | **A moved unit test lost its last assertion.** `ui.rs`'s `an_error_outranks_the_tree_line` ended with `assert!(text.contains("/help"))` — "nothing to say is the hint"; the version at `screen.rs` stops at `bar_word(None, None).is_none()`, so it checks the derivation, not the painter's fallback. No functional gap (the sweep's `fresh` state covers `Tab cycles panes` at 15 sizes), but the rule lost its direct test with the move. | ⬜ | restore the assertion on the painted bar |
| V5 | **`roomy`'s doc contradicts its test** — prose added by this integration: the doc says it is asserted "at every size at least 80×24" while the test checks two exact sizes. | ⬜ | one of the two sentences |
| V6 | **The bar's sanitize invariant is documented as total but not held.** `app/mod.rs` says "every string the bar can show is created by `say`/`fail`", while `bar_word(self.status_line(), tree.as_deref())` lets `tree_line`'s string reach the bar *outside* `set_status`'s sanitize door. Harmless today (numbers and fixed words) and fragile the moment an agent label lands in that sentence. Part of `R10`. | ⬜ | either route `tree_line` through the door or weaken the doc to say exactly which strings bypass it |
| V7 | **The sweep's blind spots, named:** hidden-row counts and derived-line counting are caught by their own `the_sweep_*` tests but not by the 15×14 sweep; tier boundaries (V2) and window/count drift (V1) by nothing (the latter only incidentally). Three of eight injections slipped past the sweep, which is the honest measure of what "15 sizes × 14 states" buys. | ⬜ | for the UX wave: a sweep state that asserts `▲/▼` and `!contains("more lines")`, plus V2's tier test |

---

## 7.5 Status moves owed by this session (apply in the doc-sync wave)

- `B25` (line 110) → **✅** closed by `f313916` (`http.rs::retrying_interrupted`),
  merged `883769f`: every IO site (read, request head/body/flush, connect, TLS
  handshake) retries `ErrorKind::Interrupted` with the cancel flag and the
  deadline consulted on every interrupt, so a `SIGWINCH` mid-read can no longer
  end a run as `cannot reach …: Interrupted system call`. Three tests fail before
  it and pass after; `model.rs::transport` still excludes `Interrupted` and now
  says why. Reproduced and re-verified over a real pty (`/tmp/b25-repro.py`).
- `H9` (line 191) → **✅** (already recorded in its row).
- The `Screen` census in `docs/refactor.md`'s header (`ui.rs` 298 / `screen.rs`
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

## 8.75 Status moves owed by this wave (apply in the doc-sync)

Closed by the wave that wrote §8 (commits `eab825e`..`f70374f`):

- **Folded in from the stopped agents' worktrees**: `mush/90` (H2's `CutOff`
  outcome and H4's `✉`/`✉N` result marks) and `mush/91` (R3/R7's
  `record_child`/`record_job`, R5's fold-that-came-to-nothing, and one
  `Registry::kill` walk) — `eab825e`, `45116ad`.
- **B26, B27** — §8.1.
- **H13** ✅ — a refusal before anything ran is `ToolError::Refused` and never a
  loop round, and the sibling's refusal names the holder, its command, and says
  not to retry (`4cb4739`). The third fix (a lock to queue on) is still a
  proposal.
- **H14** ✅ — a loop-stop records its count, and the next run opens with the
  guard's own words, so a nudge can resume it (`4cb4739`).
- **H15** ✅ — a wait with no ids says it returns the first finish; an unread
  result comes over in full and an already-read one as a digest; `agent_status`
  says it is a listing, not a delivery (`4cb4739`, schema reserve 1700→1750).
- **U12** ✅ — `AgentTree::napping` read by the title's bucket and the bar
  (`642fda8`).
- **U13** ✅ — `agent::live_branch` is the one decision about a stored branch,
  shared by `revive` and the node (`642fda8`).
- **A1–A8** ✅ — the revision is process-monotone and the payload carries the
  conversation; one thread per connection and a bounded CLI wait; `mush read`
  escapes newlines; a client's send is a message (empty refused); `--` ends the
  options; `unavailable` is its own kind; the socket guard is built before the
  spawn; the roster reports the main checkout when a worktree is gone
  (`9b02a3b`). A7 has no test (a thread-spawn failure is not scriptable here).
- **V1–V6** ✅ — one `AgentsPane::list_area` and the window/counts test; the
  tier-boundary test; both focus states at the presentation sizes; the painted
  `/help` fallback; the `roomy` doc; `tree_line` through `sanitize`
  (`f70374f`). V7's blind spots are the tests those rows added.
- **S8(iv)** ✅ — `scripts/mock_llm.py`'s `TURNS` scenario waits for `runaway
  guard`, the phrase the wrap-up instruction really carries.

Still owed, in this file's own terms: **H5**'s honest reply half, **H1**'s
actor-side facts (what the actor holds, a finished-but-unread result on the
wire, delivery/parked in the session file), **H6** (the load-sensitive frame
test), **H7/H8/H10/H12** (spawn base, machine picture, prune, per-agent
accounting — milestones), and **H13**'s third fix. **§8.9 closed H1, H5, H7,
H8's warning half and H13's third fix; what it did not do is listed there.**

For the doc hand (`README.md`, `docs/mush.md`): the rows now wear `✉`/`✉N`
and `⚠ cut off`, `agent_status` is a bounded listing, a wait distinguishes
unread from already-read, and `/new` steps the attach revision forward — none
of which the glyph tables or the attach prose say yet.

---

## 8.9 Status moves owed by the T1/T2 wave (`f70374f`..`d753db8`)

A criticality sort ranked the open queue by what each item costs a real session
— runs that die or hang, then surfaces that lie or cost work, then polish. The
wave above did the first two tiers: six commits, closed below. The row statuses
in §§1–7 are still to be moved in the doc-sync pass, as usual.

- **H13** ✅ — the third fix: a sibling's command queues for the machine lock
  (bounded at 30 s, cancel-aware, on the actor's clock) and is refused only when
  the lock outlasts that; the root is exempt from a lock it did not take and is
  *told* in its own result that it ran beside `#N`'s exclusive command
  (`1df1a53`). A root *exclusive* command is still refused — two claims to own
  the machine is what the lock exists to prevent.
- **A19** ✅ (refactor checklist, never a findings row) — `http::resolve_bounded`
  runs the name lookup on its own thread and bounds the *wait* at 10 s on the
  Clock, so a hung resolver is a `TimedOut` naming the host instead of a request
  that outlives every deadline (`7dc5e1b`). `connect`'s doc no longer admits the
  hole.
- **H1** ✅ — the actor-side facts are on the wire and in the file:
  `agent_status` lists each finished isolated run's worktree fact (branch;
  committed, clean, or failed-to-commit) through `AgentMsg::Work`, paired by run
  and never delivered; the attach roster carries `result_unread`/
  `unread_children`; `session.json` stores `result_unread`; a spawn reply names
  the branch it made (`3b6602d`). **Deliberately not persisted**: parked
  commands — a parked command belongs to a run a restart kills, and replaying it
  into a restored transcript would put words the model never read in front of it.
- **H5** ✅ — the reply half: `agent_control message` answers at-rest (“this
  resumes it”) or mid-run (“read at its next step”) instead of a flat “messaged”
  (`88bc03c`).
- **H7** ✅ — `spawn_agent {base}` resolves a branch, tag or sha to a commit
  before anything is created, refuses a shared child with a base and a base that
  cannot produce a worktree (instead of degrading into the wrong history), and
  the reply names the commit read back from the new worktree's HEAD (`8a833ed`).
- **H8** ✅ for the warning half — the pane title carries the whole tree's
  running-command count (`2 jobs`), so the box's load is a fact on screen; a
  shared build target was not added (`d753db8`).
- **R21** ✅ (refactor queue) — `attach_agents` serializes `App::agent_row` plus
  the wire-only extras, and `Phase::detail` is gone with its only reader
  (`3b6602d`).
- Census at `d753db8`: total **41,875** (was 41,123), **prod 7,783 (+104)**,
  tests 21,691 (+414), comments 9,662 (+194). Five finding rows and one refactor
  item for ~104 production lines; the rest is the tests that pin them.

Still open from the sort, all Tier 3 — no run dies and no surface lies: **H6**
(the load-sensitive frame test), **H10** (prune), **H12** (per-agent
accounting), **A7**'s missing test, **V7**'s sweep blind spots, and the refactor
queue's `D9`, `D10`, `R9`, `R10`, `R26`–`R29`.

---

## 8.11 The agent-contract audit (`d8c75e2`..`00571b4`)

A subagent read the working tree (read-only) for claims **an agent reads** that
the code does not honour — prompts, schemas, tool results, refusals — after the
prompt/schema dedup pass. It verified ~15 claims sound (delivery once per run,
wrap-up and truncation answering "was not run", base spawns, job reports,
edit-batch semantics, path enforcement, wait defaults) and found the rows
below, all now fixed:

| What an agent read | What the code did | Closed by |
|---|---|---|
| the resume story: a message to an at-rest child starts a run | a resume never re-entered the parent's `running` book, so a wait answered the stale result, `agent_status` said stopped while the child worked, and the one-shared-child guard could be bypassed by a resume — two shared children in one tree | `cec9713`: `control_tool` re-arms the book and the UI sends `AgentMsg::ChildRunning` on a human nudge |
| "any command that outlives 60s detaches by itself" | auto-detach needs a free job slot (8 machine-wide); with none, a long command was killed at 120 s, and a launch refused at the deadline threw the output away | `ab90c68`: the output is snapshotted before hand-over, a refused launch reports "ran Ns, could not become a job," and a no-room timeout names the budget |
| "wait_agents blocks until a child finishes" | it returned the first *recorded* result, even one already read, while a sibling still ran | `cec9713`: while a candidate runs only an unread result is ready; a fresh result outranks an already-read one for the single answer |
| "An isolated subagent works in its own copy" | degraded isolation reached the child's brief and a human notice, but the parent's tool result only omitted `on mush/N` | `3e006c5`: the result carries `(isolated unavailable: …; running in place)` |
| a sibling's lock refusal advised `wait_commands` | no tool can wait on another agent's job | `3e006c5`: it says do not retry in a loop, do other work and try once after |
| depth-1/2 agents have the orchestration tools | the delegation policy lived only in the root prompt, and `brief` had no description | `3e006c5`: one `DELEGATION` block, included exactly when the subagent gets the tools |
| "only one shared child may run at a time" | the guard counted *any* running sibling, so an isolated one blocked a shared spawn with a false sentence | `cec9713`: only children that share the workspace count, and the refusal names the one that blocks |
| the file tools read any path | `list_files` hid every dotfile (`.github/`, `.gitignore`) and stopped at its limit silently | `3e006c5`: dotfiles are listed (build/VCS dirs still skipped) and the limit is reported |
| "Read a file." | a capped read gave no size and no way to the rest | `3e006c5`: "N of M bytes shown … `sed -n`" |
| "cut off … 4 times in a row" | the counter never reset, so scattered truncations were called consecutive | `00571b4` |

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
  Pinned by `worktrees_clears_dead_git_entries_and_keeps_the_branch`.
- **`/worktrees` reconciles.** The re-scan runs `git worktree prune` and its
  line says `cleared N stale git entries (branches kept)`. **No new command** —
  clearing dead entries is part of the re-scan the command already promises.
  The prune touches only `.git/worktrees/` administration: every branch,
  commit, and stored transcript remains, so the work is still there when a
  failing test later wants it. Pinned by
  `pruning_takes_the_dead_registry_entry_and_leaves_the_branch`.

**Census at `e63a84c`:** total 42,614 (was 42,477), **prod 7,819 (+12)**, tests
22,186 (+80), comments 9,829 (+37).
