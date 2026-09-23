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
  spent. (Open, unchanged.) The A audit found a concrete instance worse than
  "not visible while spent": the endpoint's own counts are reported on no road
  a run can end by except a clean, tool-free end, and a fold's usage is dropped
  — **✅ that instance is closed by `d710c2e` (§8.83):** every ending reports the
  endpoint's own numbers (`run_turns` owns the accumulator, a fold's usage
  included, and an idle fold says “for this fold”). What H12 still owes is the
  live half: a run's cost on screen while it is being spent, not at the end.
- **H16, residual** — the byte cut is gone (§8.25); what bounds the *file* is the
  history window (`CHILD_HISTORY = 50`) applied by `App::reap_history`, so the
  store only ever writes live tree nodes. The per-second deep copy of every kept
  transcript on the UI thread is now paid once a minute instead
  (`SESSION_DEBOUNCE = 60 s`), which is why it stopped being a hitch; an
  incremental save is still owed if a minute's rebuild ever shows. **✅ the
  file's bound itself is closed by `3a37c02` (§8.66):** the store and the meter
  now read `Chat::bounded_transcript` — the system prompt, the transcript and
  the same `trim_history` an actor gets — and the A audit's counterexample (a
  window whose fold cannot fit, A8) was that commit's finding; the pane keeps
  its full record on purpose.
- **H18** — ✅ fixed by `af6ba05` (`mush/109`, §8.32): a parent whose send finds no
  actor behind its child's mailbox now hands the *command* to the UI in an event
  (`AgentEvent::ChildAsleep`), and the UI delivers it through the door the
  human's own message uses, reviving a parked child and starting nothing for an
  id that really is gone. Two residuals the fix named rather than papered over:
  **H22** below, and the instant a child is reaped between the failed send and
  the UI's read, where the wake lands nowhere — the next `wait` blocks to its cap,
  which "forget this child" (H19) is what would fix. Both landed in `mush/125`
  (§8.34): the stale mailbox outright, and the reaped child's name by the forget
  message the race left owed.
- **H19** — ✅ fixed by `mush/125` (§8.34): the reap's fifth step tells the
  parent to forget the child (`AgentMsg::ForgetChild`, read before `tree.reap`),
  and the id is tombstoned so a report still in flight from it cannot re-open
  the book. Only `ChildBook` clears a tombstone — a row handed back is proof the
  forget is stale.
- **H20** — the sentences the model is told that are **not** true, where the fix
  is wording in `crates/mush-core/src/prompt.rs` (the `status` schema's "title"
  in §8.27 item 1, items 5 and 9, and the `edit_file` schema's top-level
  `replace_all` in item 3). The human owns that file, so these are recorded
  rather than edited: the title a running child does not have, what a job's
  result actually is (its one line, not its window), the 120 s kill when the
  job budget is full and the 8 MiB output ceiling, and `replace_all` declared
  inside `edits` items only. Item 2 — `wait`'s 600 s cap and the early release a
  message causes — landed with §8.35 (`5eba64a`), because the wait the lock
  refusal now points at had to say what it is; the same section's first audit
  (`4aad8a7`) fixed five sentences that named that wait, and its second read
  (`fd0e615`) exempted a `wait` that slept from the loop guard and fixed four
  more sentences. One item is recorded and left: `exclusive`'s "Siblings are
  refused, not interleaved" is true of every subagent's command and merely
  silent about the root's exemption, so it is incomplete rather than false. The
  code half of items 1, 3, 4, 6, 7 and 8 landed in `ff315d8` (`mush/87`), item 10
  in `f34c4de` (`mush/88`) — see §8.29.
- **H21** — a reclaimed branch was called "merged into HEAD" whatever it was: a
  read-only child that never committed, and a nested child whose work went into
  its *parent's* branch, both landed on the row as a merge into HEAD. Fixed in
  `mush/122` (§8.33) by asking the two questions apart — is the branch's work in
  the base's history, and did the run commit anything of its own. **⬜ residual,
  still open — the C audit found the fix does not survive a restart (§8.52):** a
  restored agent carries no fork revision (`AgentSession` has no such field, and
  the restore passes the node's `fork`, always `None`), while `git::landing`
  answers `Merged` when it cannot ask — so a read-only child that never
  committed can be stored as landed and stays so through every restart. The
  audit agrees with §8.33's safer guess and pushes back on its permanence: an
  additive `fork: Option<String>` beside `landed` would retire this and H27.
- **H22** — ✅ fixed by `mush/125` (§8.34): `App::deliver_to_actor` hands the
  parent the `Sender<AgentMsg>` the revival built (`AgentMsg::ChildMailbox`), so
  the book names the live actor and the next `control` delivers instead of
  taking the wake path again.
- **H23** — an attach client's `edit --agent N` draft lands in the **focused**
  agent's message box while the ack names `#N`: `Chat::set_draft` replaces the one
  box the human owns, and the `send` half of the same command aims focus at `id`
  for its turn, which the draft half has no equivalent of. Waiting on a ruling:
  aim focus for the turn, or make the ack and `attach.rs`'s doc say where a draft
  goes.
- **H24** — below 24 rows the bar silently drops the model, the endpoint and the
  context meter (`bar_rows` picks one row, and `facts_line` is their only home),
  and `bar_rows`' doc justifies the trade by what the compact footer carries,
  which is not those three. Waiting on a ruling: give the one-row bar a third
  fact, or say in the doc that the trade is the decision.
- **H25** — ✅ fixed by `mush/125` (§8.34): an actor that starts with empty
  books is handed the tree's rows — one `ChildBook` per child, at a session
  restore (`App::seed_children`) and at the wake of a revived agent
  (`App::deliver_to_actor`, before the send that starts its run). The review
  round is what found the wake path missing from the first patch.
- **H26** — ✅ fixed by `mush/125` (§8.34): a doubled attach flag is refused by
  name before its value is read (`require_once`) and `mush focus` takes one id
  in either order; `NOTHING_RUNNING` says `Ctrl-N drops every transcript`;
  `machine::End::Unknown` carries a status that names neither an exit code nor a
  signal (and is reachable — `ExitStatus::from_raw(0x7f)` names neither); and
  `edit`'s pinned usage line brackets `[--base R]` as the parser does.
- **H27** — `App::fork_base` falls back to `"HEAD"` when a parent's branch is
  gone, so a nested child's reclamation is measured against the root's tip rather
  than the branch its work actually went into. Found while landing §8.33. It errs
  toward keeping — a branch that is not an ancestor of `HEAD` is left alone and
  the row says why — so it is a row and not a defect. (The C audit's additive
  fork revision for H21 would retire this row too — §8.52.)
- **H28** — `lock::tests` flakes under the full suite. Two of its five tests were
  each seen once in a full run this wave (`…acquire_is_refused_and_drop_…` at
  `lock.rs:124`, and `the_lock_file_is_not_removed_when_the_holder_leaves`, seen
  by `mush/125`), and neither failed in 40 isolated runs across two trees. The
  assert that flaked is that the lock is takeable again once its holder is
  dropped — if that were ever real rather than a flake, a workspace lock would
  outlive the process that held it. One more, once, in §8.37's full run:
  `the_turn_limit_ends_with_a_summary` (`agent.rs`) did not end within its
  `WAIT`; alone it passes in 2.7 s and the next full run was green, so it is the
  same class — a loaded suite outrunning a wall-clock wait, not a broken road.
  The next wave should freeze these with a clock or catch them, the same way
  §8.26 left its own full-suite flake. The E audit read the presumed cause away:
  a stale `/tmp` lock cannot outlive its holder — `flock` dies with the fd and
  the process, proved with a SIGKILLed holder, and the tests `remove_dir_all`
  their root before taking it — so the shapes left are `open`/`flock` failing
  before the lock (ENOSPC/EMFILE) and the wall-clock shape stands (§8.52).
  **✅ closed by `926a12a` (§8.83, H58):** the lock's refusal now calls the pid
  the lock file's last known holder, and the one-off failure's shape was the
  one the E audit did not name — a `fork` copying the process's open lock
  descriptions into a child, kept until the child execs, so the sibling tests'
  drop-and-reacquire assertions failed on a free lock (16–17 of 20 runs red with
  the child forked inside the dead-holder test, 12 of 12 green without it). The
  dead pid is taken once at the first `root()` call, before any test can hold a
  lock, and 25 of 25 runs of `cargo test -p mush lock::tests` are green. The
  `open`/`flock` shape (ENOSPC/EMFILE) stands as an environment failure a bool
  assertion would mis-blame on the lock; the wall-clock shape is the suite's
  known load flake.
- **H29** — ✅ settled by the third read (the file-tool wave's audit, §8.36), and by
  reading rather than a test, because there is no race to drive: no road leads
  into `Registry::launch`'s `Machine` arm with a *sibling* holding the lock.
  Every exclusive caller reaches `launch` through `run_command`'s `take_machine`,
  which refuses any existing holder — the owner included — and only that actor
  thread writes claims, so between the claim and the handover no other agent can
  hold the machine; the arm's other half (`claimed.is_some()` for the owner)
  needs a second exclusive job, which `take_machine` refuses before `launch` is
  ever called. The doc comment now says so (the arm "is a guard, not a road"),
  and `Refused::message`'s own doc no longer names a road that looks like none.
  What the same read found *beside* it was real and is fixed with a test: a
  sibling refusal that never queued (the exclusive claim losing the race between
  the lock check and `take_machine`) borrowed the queued road's "this call
  queued and the lock was still held" — `Refused::unqueued_message` now owns that
  sentence, and the shared road back has one home (`Refused::lock_road`).
- **H30** — a child seeded from a tree row whose phase is not an ending reads
  as `#2 ◐ running` while nothing about it is actually running. `seed_parent`
  sends `ChildBook { outcome: seeded_outcome(&node.phase, …) }`, which is `None`
  for `Phase::Idle`/`Thinking`/`Activity(…)`/`Compacting(…)`/`Cancelling`
  (`app/mod.rs`), `note_child_book` then records no completion and marks nothing
  running, and `child_listing`'s `None` arm prints `◐ running` — so after a
  restore, `status` claims a live child that `wait` answers "nothing of yours is
  running or unread" about. Found by §8.35's second read while checking that
  sentence; recorded rather than fixed because neither surface is simply wrong:
  the *row* is where the phase came from, and what a restored, actor-less child
  should read as is the same question H18/H22 were about. Settled by a live
  restore with a child saved mid-phase. The C audit says it is worse than
  recorded: the restored row is what `git status` and `wait` disagree with, so
  one screen answers "is it working?" and "is its work reachable?" oppositely —
  still open (§8.52).
- **H31** — the cut to six tools rested on a premise two facts broke: a machine
  lock refuses *every* `run_command`, reads included, so an agent blinked at a
  sibling's benchmark had no way to re-read a file; and a shell cannot carry an
  image, so a screenshot on disk was invisible to a model that can see. ✅
  reversed in two landings (§8.36): `read_file`, `write_file`, `list_files` and
  `search` came back beside `edit_file` (`1452477`), all of them working beside a
  held lock, and an image now travels in a tool result (`bfa0b12`), which is the
  half only a typed tool could ever build.
- **H32** — every `run_command` already runs with its cwd at the agent's *own*
  workspace root (`machine.rs`'s `Shell::spawn` passes `current_dir(cmd.root)`,
  and the root is `actor.ws.root()` — for an isolated agent, its worktree), and
  `RULES` says so in as many words — yet models keep writing
  `cd /home/rubend/p/mush && cargo fmt` (observed live by the human, in the root
  and in children). In the root that is ~25 wasted tokens a call; in a child's
  worktree it is worse than waste: the absolute path a `cd` names is usually the
  *parent's* checkout (the brief, or the human's task text, names it), so the
  work lands in the shared tree while the child's branch stays at its base —
  the "merge git is asked to do is a lie" M2.6 exists to prevent. What is not
  established: whether the `cd` is pure habit or a symptom. Both prompts say
  where a command runs — the root's `RULES` ("a command runs with its cwd at the
  workspace root") and a child's "You work at … It is your workspace root" — so
  what is *missing* is not the fact but the price of leaving it, and only a
  worktree child can pay that price. ✅ (`5602acc`+'s wave, §8.36): the isolated
  child's sentence says it ("every command already starts there — so never `cd`
  to an absolute path a brief or a task names: that is another checkout, and
  work done there lands outside your branch"), and the `subagent_prompt` doc no
  longer claims a path it prints is "not offered as something to type". Nothing
  was added to `RULES` (paid by everyone) or to `command`'s property (a second
  home for the rule). Held in reserve if the habit survives the sentence, since
  it lands *after* the damage: a note appended to that call's own result when a
  leading `cd` names an absolute path outside the actor's workspace ("`mush:`
  that cd left your workspace …"), zero schema bytes and read where the model
  looks next.
- **H33** — models `sleep 50`/`sleep 55` to wait (observed live by the human),
  and there is no case for it: `wait` blocks on the thing itself — its own
  children and jobs (`in_flight`) and, for a subagent, a sibling's machine lock —
  polling its mailbox every 50 ms, so the last of its own work finishing *ends*
  the wait and the results travel in the same tool answer (an earlier completion
  is folded in and waits with the rest); a completion also folds in on its own at
  the next batch boundary and wakes a napping agent when it is news
  (`AgentMsg::CommandDone` → `Fold::Run`; a stopped child or a stopped job is not
  news, `is_news`), so an agent that simply ends its turn is told anyway. A
  sleeping agent learns nothing a waiting one does not learn sooner:
  `wait_bounded` drains signals mid-command but folds nothing, so a result that
  lands during a `sleep` waits for the sleep to end. Sleeping has one cost `wait`
  does not: 5 identical rounds stop the run as a loop (`count_round` exempts a
  repeated *wait* through `state.waited`, and a repeated `sleep` is an identical
  batch with nothing changed in between). ✅ (`5602acc`+'s wave, §8.36): `DELEGATION`'s
  last bullet says "never `sleep` to wait: a finish arrives on its own, and a
  repeated `sleep` is stopped as a loop" — the block belongs to exactly the
  agents that have children or jobs, and `run_command`'s schema does not name
  `wait` a third time. The 50-55 s shape still reads as a dodge of the
  identical-batch guard (alternating the duration resets `count_round`
  altogether), and one state still lets the screen and `wait` disagree — H30's
  restored child — but a sleep cannot fix that one either: the row never
  changes, and the honest moves there are `control message` or ending the turn.
  The *peek* half of the habit has a shape now (`wait({on})`, H34): a sleep made
  the model blind for its duration, where a targeted wait ends on any unread
  result, a message, or the target itself.
- **H34** — `wait` is all-or-nothing, so a model that wants a *peek* at one
  thing reaches for `sleep`: `wait({})` blocks on every child and job at once,
  `status` is a listing (and polling it is what the loop guard stops), and
  ending the turn is the only other road. Observed live: the human asked their
  root agent why it slept, and it answered "no good reason — that was a slip.
  `wait` is exactly the tool for it, and my instructions literally say never
  `sleep` to wait … I reached for `sleep` because `wait` also blocks on the two
  subagents that are still running, and I wanted a peek at the job only — but
  `status` (which I'd already used) was the right peek, and if I wanted the
  result I should have called `wait`. Worse: the run got stopped at 1m09s in the
  process, so the test results are gone and I have to redo it." **✅ one real
  defect under it, fixed with zero schema bytes** (`0291027`): a job's report
  was delivered once and then **recapped bare** on every later `wait` —
  `wait_digest`'s job loop fell back to `report.line` when `record_job`
  answered `None` — and `done_jobs` is never pruned, so the recap grew with
  every job the session had ever run, and a wait whose only book entry was a
  read job answered it instead of `NOTHING_TO_WAIT_FOR`, contradicting
  `unread_result`'s own rule ("a job's line … is a recap, not news"). Measured
  on a fabricated state: one read job answered the identical unread-looking
  line, three read jobs 151 B, 32 read jobs 1 942 B, where the truth is 58 B.
  Now a job's line travels only when `record_job` says it is fresh, and the
  entry guard counts only *unread* job reports — a read child still keeps its
  marked digest and still holds the wait, because it can run again and
  `every_delivery_road_hands_a_result_over_once` pins that. **✅ the missing
  shape** (`076e164`): one optional target — `wait({ on: "c2" })`, named as
  `status` prints it, a bare `wait` unchanged — blocks on that one child or job
  while the rest of the books run on, and never on the machine lock (bare
  `wait` stays the one road back the lock refusal names). What it does not
  narrow is its attention, which the human asked for in as many words: a
  cancellation, the human's or a parent's words, and any result nobody has read
  — a child that failed, a job that ended — all end the wait and are handed
  over, with the target's own state named beside them, so a targeted wait cannot
  sit on a failure for ten minutes. It is argued against H15 the only way that
  trap allows: one shape, one meaning, the no-argument default untouched, and a
  wrong type refused rather than read as "everything" (`on` must be one target
  as `status` names it). The call cost +309 schema bytes, so `SCHEMA_TOKENS`
  moved 1 900 → 2 000 by the constant's own rule (the 128k history budget
  315 300 → 315 000; the 8k default is unchanged because its reserve is
  window-capped). The alternative — a sentence in `wait`'s schema saying a
  finish arrives on its own — only restates `DELEGATION`, which every root
  already pays for, and was not taken. Not settled: whether the stop that lost
  the run was the loop guard (which ends only the run, so the job's
  `CommandDone` later wakes the actor) or a human Stop (which kills the job),
  and whether the shape measurably curbs the reach for `sleep` — only a live
  model can say that. The parenthetical this row carried — "`done_jobs` is
  never pruned" — is measured now: 1,000 jobs left `done_jobs` and
  `delivered_jobs` each holding 1,000 entries (61,893 bytes) and 1,000
  `forget_child` calls left `forgotten` at 1,000, while the registry's own
  `MAX_JOBS + JOB_HISTORY = 16` pruning has no twin in the actor's books (A16 —
  **✅ closed by `d8ae94d`, §8.83:** the job books stop at `2 * jobs::MAX_JOBS`,
  an undelivered report is never dropped, and the `forgotten` set is gone with
  its absence as the tombstone, H57).
- **H35** — ✅ fixed by `b50a4ef`, with the end-to-end shape in `258ffb5`
  (§8.39): a child restored from a stored session was revived with a dead
  parent channel (`App::restore_agents` passed `parent: None`), and nothing
  could repair it — the road that wires a parent (`App::deliver_to_actor`) is
  reached only after a send into the child's mailbox fails, and a restored
  child's actor is alive. Its completion reached nobody: the row turned `✓`
  while the parent's books kept the child running, the bar kept saying "waiting
  on 1 subagent(s) — the root resumes as they finish", and a `wait` burned its
  whole 600 s cap before answering "still running". The restore door now passes
  the tree's live mailbox for the parent (`ReviveSpec.parent`),
  `AgentMsg::Adopt` gives the root's actor the conversation a completion folds
  into, and a failed `parent_tx` send travels to the UI
  (`AgentEvent::ParentAsleep`) instead of vanishing. The tree's `✉` marks can
  re-light over a result the parent has read (recorded in §8.39, not fixed
  there); the end-to-end test asserts the books instead. **The residual this row
  recorded is ✅ closed by `4702c94` (§8.66):** the run's ending is emitted
  before the parent is told, so the ordering is a contract and a `ResultRead`
  can never be re-armed by a `Done` that lands after it; the A audit measured
  the stale mark worse than §8.39 recorded — it also pinned the child's thread
  and kept its node out of the fifty-node window (57 nodes stored where ≤50
  hold, threads alive at rest) — and the fix is pinned by
  `the_runs_end_reaches_the_ui_before_the_parent_hears_the_report`,
  `a_read_that_lands_before_the_end_is_re_armed_and_pinned` and
  `twenty_children_end_read_and_park`. One consequence is code-read only, not
  staged: after a restore a `#N done:` can be delivered twice.
- **H36** — ✅ fixed by `be26cdb` + `8dd29e5` (§8.42), reported live: an image
  was priced by its **file bytes** (`Message::weight` added
  `bytes.len() + path + mime`) at the budget's 3 bytes per token, so a 724 KB
  screenshot counted as **247,132 tokens** where a 1920×1080 picture costs
  ≈2,765 — about 90× over, in the one direction that hurts. The trimmer sheds
  images *before* it drops turns, so a picture that fit a 500k window's budget
  was stripped before the model ever looked at it (the model received the
  placeholder and nothing else), and the meter a human watches jumped by a
  quarter of a million for one screenshot: "ctx jumped from 300k to 598.7k with
  just a single 700kb image", "nope, I restarted just now so this is after the
  images change". Images are now priced by the pixels their own header names
  (`PIXELS_PER_TOKEN`, `Image::pixels`: png `IHDR`, jpeg `SOFn`, gif's screen
  descriptor, webp's three chunk shapes), the fallback is the byte count (which
  errs high, the safe way), the attach gate compares the picture against the
  room the conversation has *left* rather than the whole budget, and the 2 MB
  cap stays what it always was — a transport measure, not a token one.
- **H37** — ✅ fixed by `3aaf284`..`7013015` (§8.42), reported live in the same
  message: the box's images could not be taken back. `Backspace` popped one
  only on an *empty* box, so with a draft in progress the only key that removed
  an image was `Esc`, which took the words with it — and nothing remembered
  either loss ("once an image is pasted into the input there's really no way to
  remove it... and really no easy way to clear what you've typed into the
  input"). Now `Backspace` at the very start of the box pops the newest
  attachment (the empty box is that same rule with nothing above the cursor),
  `Ctrl-U` clears the words and keeps the images, and `Ctrl-Z` puts back what
  the box last lost — one slot, spent by a send — while `Esc` names what it
  cleared and the road back.
- **H38** — ✅ fixed by `3b37d38` (§8.43), reported live twice in one sitting: a
  picture pasted by **path** from outside the workspace carried the human's own
  path, and the placeholder left when the session file shed its bytes promised a
  road the model does not have — `Workspace::resolve` refuses absolute paths by
  design, so `read_file` could not reach `/home/rubend/screens/….png`, and a
  shell cannot carry an image back. Both of the test pastes the human sent landed
  here, and the second showed the model could not even *measure* a file it had
  just been handed. Now a picture whose file resolves outside the root is copied
  into `.mush/paste/` as it attaches — same writer, naming and cap as `Ctrl-V` —
  and the copy is the path the `Image` carries, so every path a model is told to
  re-read is one its own tools resolve.
- **H39** — ✅ fixed by `3e184a3` + `9dfd20e` (§8.43), reported live: "what about
  pasting MULTIPLE images it doesn't seems to be handled correctly" — four
  space-separated paths pasted at once. The paste reader accepted exactly one
  name (`pasted_name` answers `None` at the first bare space, which is how it
  tells a path from prose), so the whole gesture landed in the box as text and
  nothing attached. `Workspace::pasted_images` now splits a paste into words
  (quoted spans, `\ ` escapes, `file://` and its `%20`s kept whole) and attaches
  them all in paste order when **every** word names an image file; one non-image
  word makes the whole paste text, exactly as one path's typo did. The batch
  exposed a second defect: four copies written in the same millisecond took the
  same `pasted-<millis>` name and overwrote one another, so an attached picture
  could point at another's bytes (`create_new` plus a `-2` suffix now, pinned by
  a test).
- **H40** — ✅ fixed by `3dfcd86`, `13a0a68`, `71857d8`, `2b9c860`, `ca5c26b`
  (§8.43), from the human's question about `trim_history` and the context cache:
  the trimmer cut to the **brim**, so a conversation sitting at the ceiling was
  cut again on the very next request — and every cut rewrites the front of the
  prompt, the prefix an endpoint's cache had warmed. It also shed **image
  payloads first**, a road that only made sense while a picture was priced by its
  bytes (H36): with pixels it identified nothing. Now a cut starts only when the
  transcript is over the window and stops at four fifths (`trim_target`), which
  leaves the next pressure to the fold at nine tenths, and a picture goes with
  its turn like any other words. The first shape tried — "trim whenever it is
  over four fifths" — was measured wrong before it shipped: five thousand quiet
  turns parked at the watermark, folded **zero** times and cut **3,932** times.
  The trigger/cut *pair* is the fact; either number alone is a trap.
- **H41** — ✅ fixed by `c0f8973` + `827ad1a` + `ee4db8b` + `73bfc14` + `00096c6`,
  merged by `700fbdc` (§8.44): the image reader's edges. `image_at` opened a
  path before it stat'ed it, so a FIFO named like an image blocked the UI
  thread (the human's paste) and parked an actor (the model's `read_file`) — a
  probe was still blocked after 3 s — and an over-cap file was read *whole*
  before the 2 MB cap refused it (a 128 MiB sparse png moved the probe's peak
  RSS 3,172 kB → 134,136 kB; after, 3,224 → 3,228 kB). The clipboard lied twice
  about the same buffer: stdout was drained to `READ_CAP = cap + 1`, so a
  picture of *any* size past the cap was refused as "a png of 2,097,153 bytes"
  (a 4,194,314-byte png), and a reader killed at the 2 s `DEADLINE` read as "the
  clipboard holds no image". And the placeholder formatted the path raw, so a
  newline in one put a line of its own into the model's view of the transcript.
  Now `fs::metadata` decides the shape before any open, the cap comes from the
  stat, the whole read is bounded to cap + 1 bytes because a file can grow
  between the two, and a read that hits the bound is refused without a size
  (`image_too_big` takes `Option<u64>`); `Drained { bytes, filled }` carries the
  truncation and a cut picture is refused with its true size unknown;
  `Answer::TimedOut` has its own sentence naming the wait and the file-paste
  road; and `one_line` runs the path through `text::sanitize` with `\n` escaped.
- **H42** — ✅ fixed by `3a7cca0` + `31cd9d0` + `7ef2301` + `bf9b897` + `9b29e80`
  + `7c8bd98` + `edac88f` + `5e74f8d`, merged by `c78695b` + `b6eaf91` (§8.44):
  the attach gate. A picture attached while a **child** was focused was resolved
  and copied by the *root's* workspace, so the placeholder named a path the
  child's own worktree cannot read, and the room was weighed with
  `used_weight_for(AgentId::ROOT)` whichever agent was focused. And nothing
  bounded a batch: a paste of eight pictures at the transport's 2 MB cap put
  16 MB in the box and 22 MB into the request JSON, while `docs/mush.md` claimed
  the room and the window bounded it — the room only warns, and a picture is
  priced by its pixels (a 2 MB file of a 100×100 png weighs fourteen tokens, so
  it passes every token bound the window has). Now `App::agent_root(id)` is the
  one answer to where an agent's tools resolve, `carry_images(id, images)`
  re-copies through the receiving workspace's own reader unless it reads the
  bytes back identically, both doors carry for the focused agent and `deliver`
  asks again for the agent that actually receives the message; both attach
  doors weigh the focused agent's room and refuse past `history_budget()` and past
  `BOX_IMAGE_BYTES` (`IMAGE_FILE_CAP × 8`), while the room warning still warns
  and attaches; the batch's line stopped selling the byte-priced trim; and
  `image_rows` paints a tool result's picture in the pane.
- **H43** — ✅ fixed by `e34e49a` + `152ad7a` + `d021dad` + `b76dbc8` + `1e8e4f5`
  + `991be92`, merged by `48d6934` (§8.45): what goes on the wire. `cmd_cap` was
  budget/4 while the trim leaves the fifth between its 4/5 stopping point and
  the ceiling, so a full-size result on a just-cut transcript landed past the
  ceiling and was cut again instead of folded (measured at 8k: 9,593 B + 3,076 B
  = 12,669 against 12,288; with `cap = budget − trim_target` = 2,458 it lands
  12,055 and folds); one turn's results were unbounded as a batch (four
  `run_command`s left the next request at 13,768 B against 12,288); nothing
  weighed the request against the window, so an over-window request went out and
  died on the endpoint's 400 (a 2,560×1,440 png → 18,041 B against 12,288); the
  fold's reply cap was a constant no window touched (a fold at a 16 k window
  asked 19,595 tokens against 16,000; an idle `/compact` sent a 40,833-byte
  request over an 8,192-token window); and `vision_capable` was never asked
  when a request's parts were assembled, so a mid-run `Ctrl-P` replayed `[0, 1]`
  image parts to a blind model. Now `cmd_cap` is the relation with
  `trim_target`, `ActorState::turn_room` shares that fifth across a batch, the
  assembled request is weighed (the newest turn's own results shed
  largest-first, then refused before the wire), `compaction_reply_cap` is the
  window's leftover floored at 1,024 with a fold that cannot fit not attempted,
  and request assembly asks the vision gate — a blind model's request gets the
  placeholders.
- **H44** — ✅ fixed by `70d1d9c` + `38f07cc` + `6e3ba79` + `9738a37` (+
  `b435331`, `556498e`), merged by `1608de9` (§8.46): the meter's own numbers
  were not the run's. `trim_history`'s dropped-turns note lived in the actor's
  list alone — the pane, `.mush/session.json` and `used_weight_for` were short
  of the request by 205 B (68 tokens), and the human never saw the sentence the
  model was given; `used_weight_for` added the *root's* system prompt for every
  agent id, over-reporting a focused leaf by 1,613 B ≈ 537 tokens; and
  `context_meter` marked `full`/`over` against the window while the trim, the
  fold and the attach room all cut at `history_budget()`, so on the 8 K default
  the fold fired at what read as 45 % of the window and the marks were
  unreachable. Now the note is emitted as a `Message` and put back in its place
  by `place_dropped_note`, the actor publishes its own prompt
  (`AgentEvent::SystemPrompt`) and the meter weighs `system_for(id)`, the line
  is `ctx {used}/{budget}{ full| over} (fold {trigger}) {~}{window}`, and
  `--print-config` prints the schemas and the history budget beside the reply
  cap.
- **H45** — ✅ fixed by `7141353` + `da6a59b` (+ `8398525`, `b1f1572`), §8.47: a
  fixed ceiling on a run's turns was a bound on honest work, and it fired on
  real work — a subagent's read-only docs scan was cut off with `stopped after
  200 turns without finishing (runaway guard)`, while the constant's own doc
  called 200 "past any real task". `RUNAWAY_TURNS = 200` and the wrap-up turn
  that existed only to soften it are removed, not raised; `LOOP_ROUNDS` (a
  repeated batch, not a count) stays as the one early end, and the human's Stop
  is the only outer bound. Pinned by
  `a_run_past_200_turns_ends_when_the_model_stops_calling_tools`.
- **H46** — ✅ fixed by `c69e4d8`, §8.48: a `write_file` whose `content` was
  longer than `result_cap(actor, state)` — on a big window the fixed
  `CMD_CAP = 16 000` bytes — was refused with "content is N bytes — over the
  cap on one write; write the first part…", the only place in the tree where
  a *result* cap bounded an *input*. The bytes had already travelled in the
  tool call, so the refusal saved the conversation nothing and cost a turn and
  the model's work. The check is gone, `write_tool` no longer takes `state`,
  and what bounds a write is the tool's one-line answer, the request's own
  over-window refusal (`over_window_line` — the bytes are already on disk, and
  a later trim sheds the turn) and the human's Stop. Pinned by
  `a_write_past_the_old_cap_lands_whole` and
  `a_write_over_the_window_ends_the_turn_and_leaves_the_file`.
- **H47** — ✅ fixed by `f954cfb` + `ccd9766` + `7bd7f9b`, merged by `5e296b1`
  (§8.49): reported live — "selection of text/paragraphs that are scoped to
  conversation output or to whatever pane is currently rendering", because a
  drag "get[s] all the lines from agents pane along with whatever paragraph I
  want from the right conversation pane". The cause is that mush never captures
  the mouse (finding K3), so the terminal's own selection is a rectangle of
  screen cells and cannot be scoped to a pane. Now `Ctrl-F` gives the focused
  pane the whole screen (`ccd9766`; `Tab` still cycles which one) and `Ctrl-Y`
  (`7bd7f9b`) opens a modal cursor over the conversation's *source* lines,
  whose `Enter` copies `Message::text()` exactly to the system clipboard
  through `f954cfb`'s new `clipboard::write_text` (`wl-copy` / `xclip` /
  `pbcopy`, the image readers' own 2 s deadline). Pinned by
  `zen_gives_the_focused_pane_the_two_panes_width_at_every_size`,
  `zen_tabs_between_the_full_screen_panes`,
  `zen_keeps_the_agents_counts_in_the_conversation_panes_title`,
  `ctrl_y_opens_the_select_mode_and_a_letter_is_not_typing`,
  `a_reply_is_copied_as_the_message_wrote_it`,
  `a_tool_result_is_copied_byte_exact` and
  `the_select_mode_takes_the_keyboard_from_both_panes_and_not_the_box`.
- **H48** — ✅ fixed by `4cfab77` + `f3e55c2` + `a617686`, merged by `a5b327f`
  (§8.50): the human's question — "what about MD rendering on TUI (ik this is a
  rabbit hole so the simplest way we could implement it, is it even worth
  it?)" — against a pane that painted a reply's `**important**`, `## Section`
  and `[text](url)` as the markers themselves. The answer is the small one:
  `markdown_rows` reads each source line on its own into styled runs (strong,
  emphasis, code, strike, one to three `#` headings, list markers kept, fenced
  code, links as `text (url)`), and only the model's reply is read that way —
  tool results, `run_command` output, the human's own lines, briefs, notices
  and reasoning rows stay byte-identical. Nothing is rewritten, so the copy
  road copies the model's own bytes. The work found and fixed a wrap bug on
  the way (`4cfab77`: `wrap_text(" bcd日", 4)` painted five columns in a
  four-column body). Pinned by
  `a_reply_is_read_as_markdown_and_its_bytes_are_left_alone`,
  `a_tool_result_is_painted_byte_for_byte_as_data`,
  `only_the_reply_is_read_as_markdown`,
  `a_markdown_reply_never_paints_past_the_pane` and
  `a_wrapped_row_never_outgrows_its_width`.
- **H49** — ✅ fixed by `15e6d9e` + `35404e4`, merged by `525b2fa` (§8.56): the
  TUI audit's first blocker — a fold replaced the transcript under the select
  mode while the paint road kept indexing the old cursor, so one frame panicked
  on the UI thread and took every actor, worktree job and draft with it. The
  audit's probe read `index out of bounds: the len is 1 but the index is 7`; now
  `replace_transcript` drops a mode over that conversation and `Chat::painted`
  takes the same `clamped_cursor` the key road takes, pinned by
  `a_fold_while_selecting_does_not_panic_the_frame`,
  `a_fold_leaves_the_select_mode_behind` and
  `the_frame_clamps_a_cursor_left_past_the_transcript`.
- **H50** — ✅ fixed by `6833275`, merged by `525b2fa` (§8.56): the TUI audit's
  second blocker — `rows()` de-duplicated by id while the cursor was bounded by
  the storage length, so two stored rows of one id painted one row and `G` then
  indexed the painted vector with the storage cursor (`index out of bounds: the
  len is 2 but the index is 2`, release builds too). Now the walk keys by a
  node's *place* in `agents`, `row_count()` is `rows().len()` and the pane
  indexes with `get`, pinned by
  `rows_paints_every_node_even_when_two_share_an_id` and
  `the_pane_never_indexes_past_the_rows_it_painted`; the restore's refusal of a
  duplicate id is C9's door (`7d80582`, §8.57).
- **H51** — ⬜ open, recorded rather than fixed by `66dba03` (§8.66): the guard
  that stops a late event from re-creating a reaped agent's transcript leaves
  the same one-frame window able to `Shutdown` a run just started (the
  `park_history` race, §8.21). The audit did not stage it; the cost is a run
  killed by a door it never reached, and the shape belongs beside the wake
  paths H18/H22/H25 ruled on.
- **H52** — ⬜ open, recorded rather than fixed by `cd15737` (§8.62): the E2/E3
  branch met the E4 `Drop` fix on the same `Foreground`, and the reconciliation
  keeps both rules — but a command that ended *by itself* and whose end is first
  seen in `Foreground::drop` (an unwinding panic between two `wait_bounded`
  polls) is not swept there, because `Drop` ends only a running command. E4's
  rule for a reaped leader wins that corner and no test pins it.
- **H53** — ⬜ open, the named cost of `8b029f0` (§8.59): an ignored-only run is
  kept now, and a child that merely compiled keeps its branch and checkout,
  spending one of the `MAX_WORKTREES` slots until the human discards it. The
  reclaim rule pays this deliberately (H10's trade); what is owed is a discard
  road — from the row or a command — or a sweep that can tell "compiled" from
  "delivered".
- **H54** — ⬜ open, the arithmetic half of F7, named and left by `efd5213`
  (§8.83): the spawn cap counts every worktree against `HEAD`
  (`git::unlandable`), while the sweep asks each node against the branch its own
  parent holds and the fork it was created at (`App::reclaim_worktrees`), so a
  nested child merged into its parent's branch is counted unlandable and the
  refusal hands the model a false fact. The sentence now says exactly what was
  measured; the count is unchanged, and the audit's
  `a_nested_child_merged_into_its_parent_is_not_counted` is owed to whoever gives
  the spawn road the tree's base/fork pairs — `App` holds them
  (`AgentNode::branch`, `fork`), the spawn road is an actor thread with no handle
  on it, and the fix is a handle or a pre-computed set of pairs handed to
  `spawn_tool`, not a second count beside `unlandable`.
- **H55** — ⬜ open, F13's other half, named and left by `d7f12a2` (§8.83): the
  guard counts every live writer of a directory now, but the sentence that
  promises the rule — `mush-core/src/prompt.rs:58`'s "only one such child may run
  at a time" — is still the per-parent claim, and so are the doc at
  `agent.rs:1806` and the manual's `README.md:154`/`docs/mush.md:895`. The exact
  sentence says the rule counts writers in the directory and that a shared child
  may still delegate into the tree its own run is in; `prompt.rs` is the human's
  file.
- **H56** — ⬜ open, B12's second half, named and left by `fe83f8a` (§8.83): the
  deserializer is as loose as its doc, but a reply that still does not parse ends
  the run with `could not parse model response` (`agent.rs`'s
  `ModelError::Malformed` arm) where the audit asks for a refusal the model can
  answer — the shape the `ModelError::Status` arm already gives a 400. Belongs in
  `agent.rs`'s request-error classification, with the acceptance test that a
  malformed reply is answered rather than fatal.
- **H57** — ✅ decided by `d8ae94d` (§8.83), recorded because the audit proposed
  the opposite: A16's `forgotten` set is gone and the absence of a book *is* the
  tombstone; the audit's "drop a tombstone once no in-flight report can name it"
  is refused in `ActorState::children`'s doc — a parent cannot observe the child
  actor's death (the tree drops its sender, but the child holds its own `my_tx`
  and its thread may still send), so a parent-side drop would either swallow a
  real report or stay as unbounded as the set it replaced. A late report from an
  id no book names is still swallowed, pinned by the forget tests.
- **H58** — ✅ fixed by `926a12a` (§8.83): E8's sentence calls the pid the lock
  file's last known holder — the flock is the lock — and the one-off
  `lock::tests` failure H28 recorded is not a stale `/tmp` lock (`flock` dies
  with the fd and the process, the file is never unlinked, and every test removes
  its root before acquiring) but a `fork` copying open lock descriptions into a
  child (CLOEXEC closes them only at exec): 16–17 of 20 runs red with the child
  forked inside the dead-holder test, 12 of 12 green without it. The dead pid is
  now taken once, at the first `root()` call, before any test can hold a lock,
  and 25 of 25 runs of the filter are green. What stands is the other shape:
  `acquire` failing *before* the flock (ENOSPC, EMFILE) is mis-blamed by the bool
  assertion — an environment failure, not the lock.
- **H59** — ⬜ open, named and left by `11ad7dd` (§8.84): the attach printers are
  defanged (C8), but they still `print!`, so a broken pipe panics rather than
  returning an error — the old road's failure shape, kept deliberately in that
  commit. Belongs in `main.rs`'s printer road, where the answer a closed pipe
  deserves is the one the TUI's own writes give.
- **H60** — ⬜ open, the schema reserve and its coupling, named by `09c9446` and
  `7106564` (§8.78) and `c96aae1` (§8.84): `tool_schemas()` sits at 5,985 bytes
  against `SCHEMA_TOKENS * 3 = 6,000`; B16's fuller sentence does not fit, and
  F14's description and F4's 8 MiB sentence are terse for the same reason.
  Raising `SCHEMA_TOKENS` is not free — it feeds `request_reserve` and
  `fold_request_fits`, and at 2,100 it stops `agent.rs`'s
  `compaction_folds_overflowing_history_into_a_summary` (a 6,000-token window)
  from folding at all — and the 8 MiB number cannot be read from its one home
  (`jobs::CMD_OUTPUT_LIMIT`) because `mush` depends on `mush-core` and not the
  reverse. Owed to the pass that owns `prompt.rs` and `agent.rs`: spend the
  reserve, move the constant, or split the schemas per road.
- **H61** — ✅ a design boundary stated by `6e7dac4` (§8.82): `/context auto`
  gives up *this workspace's* statement (the session layer's) and
  `Config::forget_context` drops the road before `rederive_context`; a window
  stated in the home file is a different layer (CLI > env > session > home) that
  only a fresh resolve re-reads, and mush never edits that file. `/context auto`
  therefore cannot forget a home-config window for good, and the start-up chain
  says so by re-reading the statement.
- **H62** — ✅ a design boundary stated by `d85cfd1` and `9430058`
  (§8.81–§8.82): a window stated in the home file is machine-global —
  `UserConfig::context` applies to every workspace on the machine — which is why
  the session's per-workspace statement is the more specific one and waits above
  it, and why `/context N` writes the session and not the home file. A human
  cannot state one window for one workspace through the home file.
- **H63** — ⬜ open, the test suite's scratch roots, measured at `7338d81`:
  every test's scratch root is `mush-<label>-<pid>` (one helper per module:
  `workspace::tests::temp_workspace`, `agent::tests::scratch_dir`,
  `app::tests::test_app`, `lock::tests::root`, and the rest), their
  `remove_dir_all` runs *before* `create_dir_all` and never after, and the suite
  leaves them behind. `find /tmp -maxdepth 1 -name 'mush-*' -type d` counts
  **16,084 directories** (plus 1,804 files) from **100 distinct process ids**,
  **99 of them dead**; 12,494 of the directories belong to dead pids, and the
  trees hold **≈2.1 GiB** of `/tmp`'s 2.9 GiB. The count is a snapshot that grows
  with every test run; a larger reading given for this row (25,541 directories,
  375 pids, ≈4.3 GiB) could not be reproduced at this base. The fix belongs in
  the tests' scratch-root helpers — one `Scratch` type whose `Drop` removes the
  root, or one helper they all call — and a child is being sent for it.
- **H64** — ⬜ open, the code half of H55, re-read and left by #123's pass (§8.88):
  `crates/mush-core/src/prompt.rs:58` still says "without one the child works in
  this workspace, and only one such child may run at a time", while the rule
  `spawn_tool` enforces is the *directory's* live writers, tree-wide — the
  `writers()` static keyed by the canonical workspace root, booked for a shared
  run through `WriterGuard`, minus the spawner's own children (its books are the
  finer answer for them) and never the spawner itself (`d7f12a2`, §8.83). The
  manual's two halves were repaired (`20c30a7` README, `e5f67a1` docs/mush.md),
  so what H55 owes now is the code alone: the prompt's sentence, the doc at
  `agent.rs:1806`, and the refusal at `agent.rs:4507` ("already runs in this
  shared workspace, and only one shared child may run at a time") all quote the
  per-parent claim as the rule. `prompt.rs` is the human's file.
- **H65** — ⬜ open, the doc `retrying` carries, re-read by #123 and left (§8.88):
  `crates/mush/src/model.rs:303`'s "resolving a host has no timeout (docs/mush.md
  §8)" is false since the wire phases were bounded — `http::resolve_bounded`
  (`http.rs:970`) ends its wait at the smaller of `RESOLVE_TIMEOUT` (10 s,
  `http.rs:954`) and `Watch::left`, and a spent wait answers through
  `Watch::spend` — and the same doc's "costs milliseconds" (`model.rs:301`) is
  false of the case it names: a refused dial pays `RETRY_BACKOFF` 500 ms and then
  1 000 ms (`model.rs:260`), the 1.5 s sum `model.rs`'s own test asserts
  (`RETRY_BACKOFF + RETRY_BACKOFF * 2`, `:1170`).
- **H66** — ⬜ open, one sentence in code: `crates/mush/src/http.rs:3005`'s "the
  cap that is a quarter of it" (in `live_endpoint_accepts_the_shipped_reply_cap`'s
  comment) where `Config::reply_cap` divides by `REPLY_SHARE_DIVISOR = 8`
  (`mush-core/src/config.rs:69`) and `REPLY_SHARE_WORDS` spells "an eighth of the
  window" (`:75`). §123 corrected the two documents that said a quarter
  (`fc31aff`, §8.88); the code's own comment still says it.
- **H67** — ⬜ open, one spelling written twice: `crates/mush/src/app/mod.rs:4265`
  builds the copy refusal as `"cannot copy {from} into {}/.mush/paste: {e}"`
  inline, where `workspace::PASTE_REL` (`mush-core/src/workspace.rs:1844`, read by
  `paste_dir`/`paste_rel` and by every message about a paste) is the one spelling
  (R44, §8.72).
- **H68** — ⬜ open, two sentences in `crates/mush/src/jobs.rs`, re-read by #123
  and left (§8.88): `STATUS_WINDOW`'s doc (`:104`) says "Spent on the windows
  rather than on the list, so no job is ever dropped from a status for being
  old", while the registry keeps at most `MAX_JOBS` live plus `JOB_HISTORY = 8`
  finished (`:86`; the eviction at `:1461`, the listing's own doc at `:933`) — an
  old finished job *is* dropped and no status can list it; and `Live::kill`'s doc
  (`:483`, "Killing is idempotent and goes through the handle rather than the
  flag") reads as an either/or over a body (`:487`) that stores `stop` first and
  then takes the handle. The same line is §8.86's A23 residual — "Stop it and
  everything it started" is the process group mush gave the command, and a
  command that left it is outside cleanup's reach (`d4c596c`, §8.83).
- **H69** — ⬜ open, the module doc's own list: `crates/mush/src/theme.rs:7`
  names the accent sites as "the focused border, the picker's frame and selection,
  the message prompt, the bar's badge, the selected agent row and an activity
  line" — seven — where `ui::select_painted` (`ui.rs:263`) paints two more with
  `theme.accent()` (the cursor's row as the hue's characters on `Black`, the
  selection's rows as `Black` on the hue), so the count is nine, as the manual's
  decision log now says (`bff3e2a`, §8.88). The list is the last surface that
  stops at seven.

`docs/refactor.md` §11 is the ledger: its older queue is closed except `R6`
(judged and left on purpose), and the four blind duplication passes of §8.70
have added `R30`–`R73` to it as open rows — each carries its price and, once
closed, the commit that closed it: `R37`, `R39`, `R41` and `R44` by the store
wave (§8.72); `R60` by `1afa368` and `R72` by `d56a10c` (§8.80); `R66`'s live
defect by `9f0a12c` (§8.81, its one-`enter` refactor deliberately not done —
§8.76).

The two §6 interactions with H9 are closed: an attach op no longer disarms the
human's armed quit (`626ac3d`), and a stopped agent that owns a live job is
named as stopped (`cc7aae3`).

**The campaign the record now carries, and the wave that finished its queue.**
§8.51–§8.71 write down the six blind audits (104 findings: 0/8/15, 0/6/10,
0/7/5, 2/4/20, 0/3/7, 0/6/11), the ~20 fix waves that answered them and the four
duplication passes that followed; §8.72–§8.86 carry the wave that closed the rest
of the queue — 45 findings and one half of three more — plus the output view, the
wire's one deadline, the human's window report, `/context` and the quieter
cursor. Their closure sheet is
§8.52, whose head names the snapshot: **91 fixed, 4 partial (A19, B12, F7, F13),
9 open (A13, A14, A15, A18, A21, A22, C7, F3, F17)**. The ledger deltas the
audits owed are on H12, H16, H21, H27, H28, H30, H34 and H35 (H28 is closed by
`926a12a`, H58), the narrowed B23/B27 retry class is on those rows in §2.75,
H49–H53 are the rows the campaign opened, and H54–H63 are the rows this wave
opened or settled. H64–H69 are the sentences the three documents' pass found
still drifting in code (§8.88), and §8.87 is the human's `#58` report answered.

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
| U7 | **A waiting agent still says `⠏ working…`.** When an orchestrator has ended its turn and is waiting on children (or on a job), the activity row claims work is in flight. It should say so differently from a model call that is actually in flight — the hourglass the human asked for — which is the same derived-facts rule as U1/U2, one surface further down. | ✅ | closed by `Phase::waiting` (`app/tree.rs`) telling a model call from `wait`; the row and the foot say which — `e47a680`. The foot's half is superseded in §8.43: it painted *no* line for a parked run, and now paints the run's own words (`waiting on results.`), which keeps this row's point and drops the silence |
| U8 | **A transient notice never leaves.** `· reply cut off at 20480 tokens — asking for smaller steps` and help output sit in the foot forever (until that agent runs again), so a line about *one moment* outlives it and pushes the conversation around. §4.6's per-kind lifetime answered this for failures and command answers; the "said" rank still has only one lifetime. Somebody must decide which notices are news and which are chatter — and repeated identical lines (`· model produced an empty reply` ×N) should collapse rather than repeat. | ✅ | closed by the chatter lifetime in `app/chat.rs` — `clear_notes_for`, `dismiss_said`, `SAID_TTL = 120 s` — and the `Notice.count` collapse, `b71f67e` |
| U10 | **Walking back up a deep tree costs one keypress per ancestor.** In the agents pane the only vertical moves are `j`/`k`, arrows, `g`/`G`: with twenty children under one parent, getting from a grandchild back to the root is twenty presses, or `g`, which loses the place you were reading. `←` should put the selection on the agent's **parent** (and the natural companion, `→`, on its first child), which is a fact the node already carries (`AgentNode::parent`) and which the painted order (U4) makes meaningful. It needs the same treatment as every other binding: one `Intent` in `app/keys.rs`'s table, the module doc and `--help`'s KEYS prose updated in the same commit, and a test at three levels of depth. | ✅ | closed by `Intent::TreeWalk` on `←`/`→` in `app/keys.rs` (plus `PickerMove(±PAGE)` for a deep picker); pinned at three levels of depth — `1980588` |
| U9 | **The default DeepSeek window/reply cap is far too small.** A real run was cut off at 20480 tokens; for the configuration mush ships, the default should be ~120k tokens (window, and the reply cap where the vendor accepts it) rather than a value that truncates ordinary work. Precedence must not change: a human-stated window still wins, and an endpoint-reported one still overrides the default. | ✅ | closed by `provider::PROVIDERS`' fallback of 120 000 and `Config::reply_cap` (a quarter of the window, floored at 1 024 and capped at 120 000) — `d84ecb6` |
| U11 | **Compaction happens with no visible state anywhere.** The human typed `/compact` and could see no indication that anything was happening: no "folding…" status, no progress, no answer to the only question a frozen pane raises — *may I keep typing?* Three quarters of the shape is already built (`/compact` says one line in the bar; `compact_history` emits `AgentEvent::Status("compacting on request…" / "context nearly full — summarizing…")` and later `AgentEvent::Compact`; §4.6's notice machinery exists), and the gaps are: (1) a `/compact` that arrives **while the actor is running** is *parked* (`state.compact_requested = true`, honoured only at the next message boundary in `agent.rs`) and nothing at all says so — the human waits for a bar line that has already faded; (2) nothing *distinguishes* a fold from an ordinary model call or from waiting on children, at the moment when the model call can take the whole 10-minute deadline (×3 with the transport retry); (3) nothing says whether sending is blocked. It is not: mush never blocks input, the words queue and are answered after the fold — which is exactly the fact the human needs and the screen never states. **And the sharpest form of it, found in the source after the human added "compaction doesn't wake up agents?? … that's odd": an idle fold is work the screen refuses to show at all.** `AgentTree::activity` (`app/tree.rs`) opens with `if !self.is_busy(id) { return; }`, so for an agent at rest the `AgentEvent::Status("compacting on request…")` the actor emits *before* its summarize call is **dropped on the floor**: the agent pays for a real blocking request (up to the 10-minute deadline, ×3 with the transport retry) while its row still reads `·`/`✓`/`✗` and every derived surface says "at rest". Only the *after* line (`context compacted — continuing from a summary`) ever appears. The one setter that could move the agent refuses to move an agent that is not already busy — and the idle fold's cancel flag is minted locally (`AtomicBool::new(false)` inside `compact_now`), so nothing can cancel it either. | ✅ | closed by `Phase::Compacting(Compacting::{Parked, Requested, NearlyFull})` with `Phase::compacting()`/`words()` in `app/tree.rs` — the row glyph (`≡`), the foot, the footer, the pane roster and `App::tree_line`'s sentence all read that one answer — by `compact_now` owning the fold's cancel flag (so Ctrl-C reaches an idle fold; it was minted and dropped inside the call and nothing could flip it), and by `agent.rs` emitting `Compacting` on *accept* (`Parked` from `drain_signals`, `Requested`/`NearlyFull` from the two callers). The row's own correction, verified against the source: the *automatic in-run* fold was already visible (its `Status` line survives for an already-busy agent); the genuinely silent cases were the fold requested at rest — whose `Status` the busy guard dropped — and the one parked behind a running tool, which emitted nothing at all. Merged in `fb7265d` |
| U12 | **The pane title and the bar disagree about a stopped or failed root that still has a child working.** `AgentTree::roster` counts a waiting agent only when the phase is `Idle \| Done` ("a failed or stopped agent waits for nothing"), while `App::tree_line` counts `busy_children > 0 && !phase.is_busy()` — so a stopped root over a running child gets `0 waiting` in the title, `waiting on 1 subagent(s) — the root resumes as they finish` on the bar, and `⊘ … ⏸1` on its row. The bar is right: the root *does* resume when the child's completion folds in (`absorb` → `Fold::Run`), and the row's `⏸N` already says so. Found by the duplication review of `fb7265d`. | ✅ | one predicate now: `AgentTree::napping` (`!node.phase.is_busy() && busy_children(id) > 0`) is read by the title's bucket and the bar (`642fda8`), and `the_bar_and_the_title_agree_on_who_the_root_waits_for` stops the root |
| U13 | **After a restart, a stored isolated agent whose worktree is gone keeps a branch its actor does not have.** `restore_agents` passes the stored `branch` straight to the node (`app/mod.rs`), while `revive` filters it on `worktree_path(root, id).exists()` and points the actor's workspace at the root — so the restored row offers `/diff`/`/merge` for a reclaimed directory and the footer paints a dead path, while a nudge is refused by the UI guard even though the actor would have run it in the root. Two surfaces contradicting the promise both restore paths make ("continues in the main checkout"). Found by the duplication review of `c4aa2e3`; untested (both restore tests store `branch: None`). | ✅ | one decision now: `agent::live_branch` is the one place a stored branch is filtered, shared by `restore_agents`, `revive` and the node (`642fda8`), and `a_restored_branch_whose_worktree_is_gone_is_dropped` stores one and asserts the node drops it, the nudge is delivered and `/diff` stops naming it |
| U14 | **A run parked in a `wait` wears the working icon.** Finding U7 taught the row's *words* (`waiting on results 3s`), the transcript's foot and the row's footer to tell a model call from a run parked on somebody else's result — and stopped one surface short of the glyph, which is the surface a glance reads. Observed live in the session running this repository: the root was parked in a `wait` on a child, its row read `◐ #0 ⏸1 root  waiting on results 3s`, and the human asked why the icon said working. The same fact was wrong in two more places: `Phase::label` answered `working` to the attach roster, and `AgentTree::roster` counted a parked run in the title's `N working`. | ✅ | one derivation, four readers: `Phase::waiting` now reaches the glyph (`⧗`, the one hourglass `unicode-width` calls a single column — `⌛` measures two), `Phase::label` (`waiting`), and `roster`'s buckets, so the title counts a parked run beside the napping parents it already counted there; `busy_counts`/`is_busy` stay "a run is in flight", which is what `⏸N` and the bar's promise read (§8.38) |
| U15 | **The foot is the one surface that does not name what the run is doing.** A tool call in flight — a twenty-minute `cargo test` — wore the same `working.` as a model call, and a run parked in a `wait`, which the row beside it names (`⧗ waiting on results 5s`), painted no foot line at all (U7's fix chose silence: it kept the point and cost the fact). | ✅ | one derivation, three surfaces: `Phase::words` (the row builds on it through `phase_detail`, the foot paints `words + the dot beat`), `Pane.words` replaces `busy`/`compacting`, and `Phase::doing` answers `waiting` for a parked wait so the roster, the quit warning and the foot share one word — `235c3bc`..`a185aa7` (§8.43) |

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
| B23 | **A transient transport failure ends the run instead of being retried.** Several agents in this session died mid-work with `cannot reach https://api.deepseek.com: Connection reset by peer (os error 104)` — one of them had committed nothing, another was killed by the *harness* process dying around it, and a third lost a run's worth of edits. Every one was a transport hiccup, not a refusal: the endpoint had no opinion about the request. A bounded retry (three attempts with a timeout, backing off) for *transport* failures only — never for a cancellation, a status the endpoint chose, or a body it deliberately sent — would turn "the run is dead and its worktree is half-edited" into "the run paused for a second". Two rules make it honest: Ctrl-C must still abandon a request immediately (the cancel flag is polled between socket slices and must be checked between attempts), and the human must be told (`retrying — connection reset (2/3)`) rather than watching a spinner that looks stuck. | ✅ | closed by `model.rs::retrying` — `RETRY_ATTEMPTS = 3` over the `Clock` seam, transport failures only, the cancel flag read before every attempt and between backoff slices, each retry announced in the transcript — `7af7cef`. **Narrowed by `b6a59c3`
(§8.58):** `retrying` now repeats only `Unsent` — a dial that never connected or
a write that did not hand the whole request over — because a reset *after* the
request left could bill the human twice; the run ends and the human can ask
again. The row's finding stands; the retried class does not. |
| B25 | **A signal during a model read is reported as a failure of the endpoint.** Found while reproducing the fold's visible state: a `SIGWINCH` (a terminal resize) arriving during a held read surfaces as `could not compact: cannot reach http://…: Interrupted system call`, and the same read under an ordinary run ends the turn with that text — `EINTR` is a retryable read, not a refused connection, and it is currently classified like one (`http.rs`'s read loop treats the error as terminal, and `model.rs`'s transport classifier deliberately excludes `Interrupted`). A human who resizes the window while mush is answering can therefore lose a run to their own window manager, and the message blames the endpoint. | ✅ | closed by `http.rs::retrying_interrupted` (`f313916`, merged `883769f`): every IO site — write, flush, read, connect, TLS handshake — makes a signal-interrupted call again, with the cancel flag and the deadline consulted on each interrupt; `model.rs::transport` still excludes `Interrupted` and says why |
| B27 | **A framing error on the wire kills a run, and blames the endpoint.** Observed live: an agent's run died with `the endpoint's reply was refused: malformed chunk size: ""` — `read_chunked`'s error for an empty chunk-size line (`http.rs:864-899`). B23's retry covers *transport* failures and B25 covered signals; a chunk-framing failure is `InvalidData`, so it is classified as a body the endpoint deliberately sent: it is neither retried nor questioned. The stray empty line is at least as likely to be *our own* leftover framing — a kept connection returned to the pool with its chunked body unconsumed after an early stop — as the endpoint's opinion, which makes the diagnosis wrong as well as the verdict. And the run's death reaches its parent only when something later asks for the child's state, so a run can be dead for a while before anyone is told. | ✅ | closed by `c5694f1` (merged `7e0440f`), and the row's suspicion was half right: the break *was* ours, but it was not a pooled connection — `read_chunked` read a chunk-size line with `unwrap_or_default()`, so an EOF met where a size was expected became an empty size line, and `InvalidData` was classified as a refusal. Now a framing break is its own class (`http.rs::Framing`, `body_cut_off()`) mapped **before** the `InvalidData → Refused` arm, so a body past the 80 MB cap stays a refusal and is never retried, while `retrying` repeats `Transport | Framing` on a fresh connection and never a cancellation or a status the endpoint chose; the wording says the reply broke instead of blaming a refusal, and a non-focused child's failure now lands on the bar. The unusable-body/poisoned-pool hypothesis was refuted rather than fixed, and recorded in the `Pool`/`exchange` docs so it is not re-opened. **Narrowed and
still classed at `38d0438`:** the same commit (`b6a59c3`, §8.58) drops the
framing retry this row describes, so `retrying` repeats `Unsent` only; and the
class's last road was closed by `729e8fc` (§8.79): a hex-parsable chunk size
past the body cap is now a `Framing` break naming the size claim and the cap,
never the endpoint's refusal. |

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
| H13 | **The exclusive machine lock is machine-wide, and a refused command is a trap.** The harness's lock is taken by a *command* (`exclusive=true`), which the tool's own guidance recommends "for anything timing- or port-sensitive" — and every review brief flags the 60fps frame test as load-sensitive, so a reviewer takes it to get one clean number. While it is held, every sibling's command *and the orchestrator's own* is refused ("#N holds the machine; retry when it finishes"); the refusal is an **error, not a queue**, so an agent that retries it is doing exactly what `LOOP_ROUNDS` counts, and mush's own guard then kills the run: `#65` and `#66` died mid-work this way, and the orchestrator could not even read a file for the duration. | Two agents' runs lost (one integrator, one fixer), a stalled wave, and an orchestrator blind at a moment it had to inspect. | ✅ all three (`4cb4739`, `1df1a53`): a refusal before anything ran is `ToolError::Refused` and never a loop round, the refusal names the holder and says not to retry in a loop, a sibling's command queues for the lock (bounded at 30 s, cancel-aware), and the root is exempt from a lock it did not take and is told it ran beside `#N`'s exclusive command. A root *exclusive* command is still refused — two claims to own the machine is what the lock prevents. The refusal's road back landed later (§8.35, `5eba64a`): a subagent's `wait` blocks while another agent holds the machine, so "try once after it finishes" became an instruction a model can actually follow — the wait the first three fixes could only say was missing. The sentences around that wait were audited in the same wave (`4aad8a7`): the timeout names `control stop #N` when the holder is the asker's own child, the root's wait is said not to block, and the refusal says the road back can be two waits, because one with an unread result to hand over comes back holding the lock. The wave's second read (`fd0e615`) had to teach the loop guard the same lesson one level down: a `wait` that slept is not a repeat of itself, or the "wait again" this row's road back names would have stopped the run as a loop on its fifth taking |
| H14 | **A run stopped as a loop cannot be resumed.** `Ctrl-C`-stopped agents resume when you message them (that is the promise on the row: `stopped · re-send to resume`), but two loop-stopped agents (`✗ the run was stopped as a loop: the same tool call repeated 6 times with nothing changed in between`) re-stopped **immediately and identically** on the nudge — so the one place a human would first try to recover is where resuming does not work. Either the repeated call is still in the window the guard counts, or the resumed run's first call is counted against the old rounds; either way the orchestrator had to spawn a fresh agent with a rebuilt brief (cheap only because the dead one's work had already been committed — `c3f5984`). | Two agents re-spawned instead of nudged; the loop-stop's own advice ("re-send to resume") is unactionable. | ✅ (`4cb4739`): a loop-stop records the count it stopped at, and the next run opens with the guard's own words, so a nudge resumes it (`a_loop_stopped_run_resumes_with_a_warning`) |
| H15 | **`wait_agents` answers from history, and nothing says what a wait releases.** After the crash the orchestrator re-spawned six children; its first `wait_agents` (no ids, a long timeout) returned the summary of `#10` — a child of the *previous* process, finished before it died — which reads exactly like a live completion. Spooked, it then named one id, and `ids=[…]` means "wait only for that one", the opposite of the intent (be woken by *any*). The tool's own description calls the answer "its summary" and never states the release rule, so the call has to be reasoned out from the implementation: no ids = first finish, `ids` = only those, `all` = every child, `timeout` = a bound. | A blind 900 s wait while two children were dead, and one result that reported the past as if it were news. | ✅ (`4cb4739`): `status` is a bounded listing (`Outcome::digest`; `✉` marks a result nobody has read), an unread result comes over in full where an already-read one is a digest, and the descriptions state the release rule — later narrowed again by the twelve-to-six cut (`ba04c49`), where `wait` takes no arguments and releases when every child and every job the agent owns has finished — reopened once, deliberately, by H34: `wait` gained a single optional `on` target, the no-argument call unchanged and a wrong type refused rather than defaulted, which is the one shape the trap allows |

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
| total | 9,185 | 41,093 | 4.5x |
| **prod** (blank/comments/tests stripped) | 4,689 | **11,863** | **2.5x** |
| tests (inside `mod tests` blocks) | 2,559 | 17,094 | 6.7x |
| comments | 1,255 | 9,468 | 7.5x |

The fix wave's own delta, `eab825e..f70374f` (five commits, ~20 findings):
**prod +155, tests +542, comments +341** — behaviour moved by a hundred and
fifty-five lines and the harness around it by five hundred.

The two big files are 46% of the tree and 57% of the tests: `app/mod.rs`
(9,664 total, 5,189 test) and `agent.rs` (9,129, 4,498 test).

What the census says, and what it does not:

- The behaviour is ~12k lines and the harness ~17k. A fast offline suite is
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
Census at `d753db8`: total **41,844** (was 41,093), **prod 12,050 (+187)**,
tests 17,424 (+330), comments 9,662 (+194).

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

**Deliberately left, each for a stated reason:** the loop guard still counts a
timed-out wait as an unchanged repeat (no result changed, which is what the
guard is for, and a run gets six waits, not one); the root's lock exemption is
learned from the result note rather than stated in advance; and
`RUNAWAY_TURNS`'s wrap-up turn explains itself when it fires. One entry left
this list later: a *stopped* child now wakes its parent (§8.41 — the human's
ruling, on the ground that a parent depending on a child has to hear that the
result is not coming, whichever hand stopped it; the line names the hand, and
only a park stays quiet).

**Census at `00571b4`** (the six commits above plus the dedup): total 42,417
(was 41,844), **prod 12,149 (+99)**, tests 17,724 (+300), comments 9,804
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
  `a_spawn_forks_from_the_named_base_and_says_so`. **The F audit's F9 is this
  row's class, and it was still open at `38d0438`'s base:** `base="HEAD"`
  resolved in the application root, not the caller's workspace, so a nested
  child forked from someone else's history while the reply named the wrong sha.
  Fixed by `6f47149` (§8.65), pinned by
  `a_nested_base_head_forks_from_the_parents_worktree` and
  `the_actor_and_the_ui_agree_on_the_base`.

**Census at `15648ae`:** total 42,446 (was 42,417), **prod 12,142 (−7)**, tests
17,771 (+47), comments 9,792 (−12). The production side is a net removal: the
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

**Census at `e63a84c`:** total 42,583 (was 42,446), **prod 12,171 (+29)**, tests
17,834 (+63), comments 9,829 (+37).

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
mush wrapper), and the landed story (`merged` / `nothing committed` /
`discarded`) is stored in the session. Discovery is automatic, so `rm -rf .mush` cannot
resurrect a row. Nineteen tests died with the commands they pinned (456 →
437, 3 ignored; mush-core 108 after its prune test went too).

**Census at `ad5b791`:** total 41,312 (was 42,583), **prod 11,757 (−414)**,
tests 17,280 (−554), comments 9,600 (−229). Net −1,271 lines.

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

**Census at `960e073`:** total 41,416 (was 41,312), **prod 11,800 (+43)**, tests
17,299 (+19), comments 9,635 (+35).

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
  reclaimed, invisible until git refused to reuse the name. **The F audit's F1
  is this rule's blind spot, fixed by `8b029f0` (§8.59):** "nothing unmerged or
  dirty is ever touched" was false for ignored-only output — it read as clean
  and was swept, taking the run's only copy; the reading is ignored-aware now
  and the kept sentence names the paths, at the cost of one `MAX_WORKTREES`
  slot (H53).

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

**Census at `6fc7435`:** total 42,671 (was 41,416 at `960e073`), **prod 11,591**,
tests 18,127, comments 10,177. The deltas belong to the repository's own commits
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
rediscover them (H18 and H19 are both fixed now: `mush/109`, `mush/125`; §8.32,
§8.34): a microscopic completion-versus-sweep race in reclamation
(a completion sent but not yet in the tree, and the sweep takes the directory a
wake is about to use); `git branch -d` measuring against the root checkout's HEAD,
so a nested branch merged into an unmerged parent is removed with its branch kept
— said out loud rather than hidden by `-D`; a no-commit run recorded as
`Landed::Merged`, which reads as "merged into HEAD" (the third variant that needs
`StoredLanded`, in core, is §8.33's landing, `mush/122`); and one full-suite flake
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

**Census at `cc89598`:** total 46,525 (was 42,671 at `6fc7435`), **prod 12,271
(+680)**, tests 19,738 (+1,611), comments 11,512 (+1,335). Read that the way §8.5
asks: five patches, 3,854 lines, and 680 of them behaviour. The wave bought its
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

**Census** (`scripts/census.py`, method in §8.5). At `491113a`: total 46,440 ·
**prod 12,183** · tests 19,778 · comments 11,490. On the merged tree: total
46,559 · **prod 12,088** · tests 19,875 · comments 11,600. 95 lines out of
production, 97 lines of harness in — which is the honest shape of this wave:
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
| 1 | `status` promises "each child's state and title or branch" while a running child prints only `#3 ◐ running` — no title (it lives in the UI tree) and no branch | ✅ `ff315d8` (`mush/87`): print the branch when mush can name it (an isolated child's is `mush/<id>`); no title source invented. ✅ `1452477` (the file-tool wave, §8.36): the schema sentence followed the listing — "each child's state and branch … `✉` marks a result you have not read" — because the sentence must describe what the call answers; the other direction (putting the title in the listing) needs the title threaded into the actor's books and stays open |
| 2 | `wait` "blocks until everything you own has finished" — it gives up at 600 s and any message ends it early, and the context says neither | ✅ `5eba64a` (§8.35): the schema owns the machine clause, the 10-minute cap and the early release, and `SCHEMA_TOKENS` moved for it (1200 → 1300) |
| 3 | `edit_file`'s description offers a top-level `replace_all`; only `edits[].replace_all` is read, so the refusal tells the model to set the flag it just set | ✅ `ff315d8` (`mush/87`), fixed in code: the single-pair path honours a top-level `replace_all`. ✅ `1452477`: the second shape is gone (one `edits` list), and the description no longer offers the flag as the fix for a *missing* match — "A missing `old_string` is refused; a non-unique one is refused unless `replace_all` is set" |
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
| total | 46,572 | 47,798 | +1,226 |
| production | 12,032 | 12,225 | **+193** |
| tests | 19,910 | 20,534 | +624 |
| comments | 11,633 | 11,971 | +338 |
| blank | 2,997 | 3,068 | +71 |

The production column is the wave's most interesting number. A new module (the
workspace lock, 43 production lines), a real concurrency fix, a rewritten
environment reader and six model-facing corrections together cost **193** lines
of production code, because every one of them came with deletions. 84.3% of what
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
human's `prompt.rs`, plus the `edit_file` schema's `replace_all`). H18 and H19
have both landed since — `mush/109` and `mush/125` (§8.32, §8.34). Of
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
derivation. `crates/mush/src/theme.rs` (new, 229 production lines) is that fact.

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
passing. The census at the merge: **48 886** total · **12 467** prod · **21 010**
tests · **12 273** comments — against 47 798 / 12 225 / 20 534 / 11 971 at
`ff315d8`. A feature that is mostly a palette and its guarantees costs 229
production lines and 327 test lines in its own file, and 242 production lines
net across the crate.

One merge note, because it is the kind of thing that looks like a bug later: the
branch was cut from `ff315d8` and `b87bb40` (the budget re-tune) landed before
it, so `main.rs` conflicted — in exactly one test line, where the re-tune had
added `let cap = plain.reply_cap();` above a `describe` call that the theme
branch had changed to take a `Theme`. Both survived; no fix was lost.

## 8.32 The surface a human reads, audited — and four branches (`2da5f11`..`af6ba05`)

A blind, read-only auditor was pointed at the whole *human-facing* surface at
`2bb414f`: the `--help` text, the key table, the command table, the bar, the row
footers, the startup and refusal messages, the attach protocol, and the prose in
`docs/` where it names behaviour. Its brief was to report, for each claim, the
text as written and the code that contradicts it — no proposed rewrites, no
taste. It returned 16 findings in four classes (A: a sentence false about a
limit or a state; B: a reachable state with no sentence; C: one fact spelled
twice; D: prose and taste), each with a file, a line and a reachable path — plus
a long list of what it had checked and found honest, which is what makes the
rest worth acting on rather than re-litigating.

**What landed, in the order the branches were cut** (each verified in its own
worktree — `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D
warnings`, the smoke scenarios — merged `--no-ff`, and the merged tree verified
again: **550** mush tests, **130** mush-core, 0 failed, 4 ignored, all three
smoke scenarios passing):

- **`2da5f11` (`mush/108`) — the help, the dump and two startup errors.** The
  ATTACH block documented `mush focus [DIR] ID` and `mush edit [DIR] …` while the
  parser takes the value first, so a human following `--help` got an error
  blaming the argument they had just typed; the lines now match the parser and a
  test pins all four usage lines *and* the two orders the parser refuses. `mush
  /typo` answered `No such file or directory (os error 2)` — no path, no advice —
  and an unmakeable `.mush/` answered with a second bare errno; both now name the
  path and what it is for, in the shape the file-where-a-directory-was-wanted
  case already had. And `--print-config` read the stored session through
  `Session::load`, which answers `None` for a file that is there and unreadable
  exactly as it does for no file at all — the one dump whose job is to *show* the
  precedence chain had a link silently missing: it reads `Stored` now and prints a
  `session` row (`none` / `read (N messages)` / `unreadable — <reason>`, the
  reason sanitized like the model id), and the notice for a session it could not
  set aside now says the consequence (the conversation starts empty, the file is
  still there, the next save replaces it) instead of stopping at the reason.
- **`fe52542` (`mush/107`) — seven sentences that were not so.** `/key` with no
  key promised "memory only" while the arm below writes the secret into the home
  config in plain text: wrong in the one direction that costs a secret, and it
  names the file now. The facts line painted a provider's own short endpoint
  over a request whose URL was a proxy (a *named* provider keeps its knobs when
  `--url` is given, and `/url` never touches the provider), so the label asks
  whether the endpoint in use is the provider's own before it shortens, and a URL
  the provider does not own is spelled out. A job a signal killed read `exit -1`
  — a number no command returns, telling an OOM kill and a `SIGSEGV` apart from
  neither — and is now `killed by signal 9` from the machine's own distinction
  (`machine::End`), through the job's line, the owner's transcript and `status`.
  The bar's cursor line, the one producer with no column budget, bounds the brief
  it carries while keeping `agent #id: ` whole; a typo'd command names `/help`
  inside the same discipline. `Voice::Mush`'s mark was spelled twice with one
  spelling unreachable — the table is the only spelling now, and the renderer
  calls it. Ctrl-N's help says what it stops and drops, and `/notes` says it
  reads every note about the focused agent, which is what the picker lists.
- **`3f93cfd` (`mush/118`) — a flag a subcommand does not take.** Found by the
  branch above while it was in the same file: `mush agents --since 3`, `mush
  focus 1 --base 9` and `mush read /w --send` parsed and then dropped the value
  on the floor, against this file's own rule (the one the unknown-option arm and
  the second directory already follow). One table now gives each subcommand its
  flags, `Cli::detect` asks it before reading any value, and a miss is refused by
  name — naming the flag, the subcommand and `--help` — while nothing new is
  silently honoured.
- **`af6ba05` (`mush/109`) — a parked child its parent can reach.** H18: a
  parent's `control message`/`stop` read an empty mailbox as "agent #N is gone",
  and an empty mailbox says no such thing — parking ends the *actor thread* and
  leaves the node, the id and the transcript where they were. The parent holds no
  transcript and so cannot rebuild the actor; the UI can. The command now travels
  to the UI in an event (`AgentEvent::ChildAsleep`) and is handed over through
  the door a human's own message uses — reviving a parked child, starting nothing
  for an id that really is gone — and the reply says what happened ("its actor
  was parked, so mush is waking one") rather than claiming a delivery the parent
  cannot check.

**What the wave did not do, and why.** Four findings were put to the human
together with the fixes, because each needs a ruling rather than a correction:
**A4** (the row that says "merged into HEAD" about a run that never committed,
and about a nested child whose work went into its *parent's* branch) — the
ruling was to fix it in the git layer rather than in the wording, and it is
`mush/122` (§8.33); **B4** (an attach client's `edit --agent N` draft lands in the
focused agent's box while the ack names `#N`) and **B5** (below 24 rows the bar
silently loses the model, the endpoint and the context meter) — both are H23 and
H24 above, waiting; and **C2** (the row's "merged into HEAD" against
`Landed::past()`) folded into A4, since it is the mechanism A4 travelled through.
**D2** — the quit warning's phase word inside the list of what dies ("kills #0
idle + 1 job") — was judged and left: the list is what dies, the word whose it
is, and `docs/refactor.md` R22 already argued the case.

Two corrections to the *record* came out of the same conversation rather than
out of the audit, and both are the kind only a reader of the live thing can
make. The manual's enumeration of what `--print-config` prints had lost
`auto-approve` and the theme (and now the session row), and `$MUSH_THEME` was in
no human-facing text at all. And the 256 KiB per-transcript cap — quoted at me
in conversation, and *still asserted by two doc comments* (`app/tree.rs:496`,
`app/chat.rs:597`) — was reverted by the human's own decision in §8.25 and is
nowhere in the code: `mush/122` corrects both comments to what actually bounds a
stored transcript (the fold, and for a finished child the size it froze at).

**The instrument was wrong too.** Writing this section meant quoting the census,
and the census's `prod` column is `total − blank − comment − tests`, which
double-subtracts the blanks and comments *inside* test modules: it reported
`app/mod.rs` as 49 production lines out of 10,858, at the same time as that file
holds the whole `App` and a test module that starts at line 3,242. The reported
figure is exactly `K_prod − B_test − C_test`, so every "prod" number in this
document's history is understated by the comments and blank lines of the test
directories — the same class of defect §8.28 found in `session_blame.py`, and
fixed the same way: `mush/123` turns the columns into the partition the
docstring claims they are, hand-checks `app/mod.rs` end to end, and re-measures
the four refs this record quotes. Every census figure in this file and in
`docs/refactor.md` has now been re-run with the fixed script at `1f324bd`;
figures printed before the fix are not comparable with the ones after it.

---

## 8.33 A removal says which nothing the branch was (A4, `mush/122`)

The row a reclaimed agent wears said **merged into HEAD** whatever had happened,
and both halves of that were wrong. A read-only child — one that ran, read and
committed nothing — wore it, so mush claimed a merge nobody performed. A nested
child whose branch went into its *parent's* branch wore it too, naming a ref its
work was never in. The finding (A4, with C2 folded into it: the row's sentence
against `Landed::past()`'s one word) was put to the human with the rest of the
surface audit, and the ruling was to fix it in the git layer rather than in the
wording: the row was not mis-worded, it was told the wrong fact.

**One test was answering two questions.** `reclaimable` asked
`ahead_of(branch, base) == 0` — “the branch adds nothing to the base” — and every
surface read that as “merged”. The predicate was right; the reading collapsed
three states into one. Two questions tell them apart:

- **Is the branch's work in the base's current tip?** The base stays a **name**
  (the caller's `base`, or `HEAD` for a child of the root), resolved at reclaim
  time: a merge made by hand while the run was going moves the base's tip onto
  the branch's work, and only re-resolving the name can see it. Handing over the
  revision `worktree_add` was given would make a merged branch look *ahead* of
  its base, and mush would stop reclaiming merged work.
- **Did the run commit anything of its own?** The **fork revision** — the commit
  the worktree was created at. `spawn_tool` already resolved it for the spawn
  reply's `at <sha>`; it is resolved once and carried now, on the actor
  (`Actor::fork`) and on the node (`AgentNode::fork`), because both removal paths
  need it.

`git::Landing { Merged, NothingCommitted }` is the second answer's home, carried
by `Reclaimable::Landable(Landing)` and `Reclaimed::Removed { branch_kept,
landing }`, and `Landed` (the row) and `StoredLanded` (the session file) gained
the third variant, so a restart paints the same row. Both removal paths carry the
answer to the row: `agent::reclaim_own_worktree` → `AgentEvent::Reclaimed
{ landing }`, and `App::sweep_worktrees`, which re-asks `git::reclaim` at the
moment it removes — base name and fork revision both re-read, because the
decision was a snapshot.

**Where the fork is unknown, mush keeps the answer it has always given, and says
why.** A revived agent and a leftover found on disk have no fork revision — the
session file never held one — and `App::discover_worktrees` sweeps against
`HEAD`. There the two states are the same git shape, and “nothing committed”
would be a guess; it is the guess that costs most, because it erases a real run's
work from the row, so those paths answer `Merged` and the rustdoc says so.
Removal safety is otherwise untouched: uncommitted work is never removed,
`Some(non-zero)` and a git refusal stay `Kept`, and a **squash** or cherry-pick
still looks unmerged to git, so the branch is kept and the row says why.

**The word, per surface.** `Landed::past()` stays one spelling, for the
participles a sentence can use — `merged`, `discarded` — and gained `nothing
committed`. The row paints the bare word now (`screen::agent_detail`), not a
sentence: the row has no base fact at all, which is what made “into HEAD” false
even for a merge. The nudge refusal cannot say “was nothing committed”, so that
landing's refusal is worded around the fact instead (“agent #2 committed nothing
— its worktree is gone”), still claiming no merge; `Landed::past()`'s doc records
the split rather than bending the word to the refusal's grammar. A third surface
the brief had not named was wrong the same way and is fixed with them:
`agent::worktree_gone_line` now names all three endings.

The second commit in the same landing is the other half of §8.32's record
correction: the two doc comments that still asserted the reverted 256 KiB cap
(`app/tree.rs`, `app/chat.rs`) now say what bounds a stored transcript — the fold
at nine tenths of the history budget, a finished child's frozen at the size it
reached, and the file at `CHILD_HISTORY × that fold trigger + the root`.

**Verification.** `a_merged_row_does_not_claim_head` fails on `af6ba05` with
`left: "merged into HEAD"`, `right: "merged"`. The landing's tests pin the three
`StoredLanded` values through the nudge refusal; a base that moved on after the
spawn (still `nothing committed`); a read-only child swept as `nothing committed`
with its worktree *and* branch gone; and a real parent/child branch pair whose
nested merge is painted with no “HEAD” in the line. `cargo test` 554 + 132 (from
550 + 130), fmt and clippy clean, three smoke scenarios pass.

**Residuals, named rather than papered over.** `App::fork_base` falls back to
`"HEAD"` when a parent's branch is gone, so a nested child's reclamation is
measured against the root's tip rather than the branch its work went into (H27;
it errs toward keeping). And §8.26's “two of the three branches are net deletions”
is all three (−26, −35, −34) once §8.32's re-run is applied — corrected above,
not left as a claim the numbers no longer support.

**Census at the merge** (`3c33cc6`): total 50,617 · **prod 12,688** · tests
21,836 · comments 12,887. Of that 612-line wave, 70 lines are production
(12,618 → 12,688) and 297 are test code: the fix is one new enum (`git::Landing`)
and a third variant in the two that mirror it, and most of the diff is the prose
that says which fact each surface now has.

---

## 8.34 A parent's books follow the tree (H19, H22, H25, and four leftovers, `mush/125`)

Three rows in the queue were one defect seen from three sides. A parent's books
about its children — `ActorState::children`, `completed`, `delivered`, `running`,
`shared`, `work` — were written at a spawn and never reconciled with the app's
tree again: a parent restored from a session started with *empty* books and
answered "no such child agent #N" about a row on screen (H25); a child the
history window reaped stayed in its parent's books for good (H19); and a child
revived after parking left its parent holding the sender of the actor that no
longer existed, so every later `control` took the wake path again (H22) — the
message landed anyway, which is why it went unnoticed for a session.

**The rule now: the tree is the truth, and the books are seeded where an actor
learns who its children are and reconciled by messages while it runs.** Three
new `AgentMsg` variants carry the tree's facts, and each is book-keeping —
`Fold::Idle`, honoured in the idle drain, mid-run and at a message boundary
alike, so none of them starts or resumes a run:

- `ChildBook { id, cmd, outcome, read, shared }` — a row. Sent by
  `App::seed_parent`, once per child, from two places: `App::seed_children`
  after a restore has put every row back, and `App::deliver_to_actor` for an
  agent it has just revived — *before* the send that hands over the command
  which starts that run, so a `status` in the run's own first request already
  sees the books. The outcome is recorded under `NO_RUN` (0: `runs` starts at 0
  and is incremented where a run ends, so no actor ever claims it), the `read`
  flag is the row's `✉` mark so a `wait` still hands an unread result over
  exactly once, and `shared` is the fact the one-shared-child rule reads.
- `ChildMailbox { id, cmd }` — the sender a revival built.
- `ForgetChild { id }` — the reap. `App::reap_history` is five documented steps
  now, and this one reads the parent *before* `tree.reap`, the last moment the
  node still says whose child it was. It is a plain send into the parent's
  mailbox: a parked parent has no books left to correct, a restored one
  re-derives them from a tree that no longer has the child, and neither is
  woken just to drop a name.

**A forgotten id is tombstoned, and every per-child book respects the tombstone**
— `note_completion`, `record_child`, `note_running`, `note_work`,
`note_mailbox` — so a report still travelling from a reaped child cannot re-open
a book the reap closed and arm a fold the tree has no row for. Only `ChildBook`
clears it: the tree can only hand a row back for a node that exists, so a handed
row is proof the forget is stale. The ids are never reused (`crate::ids`), so the
memory cannot name a new child by mistake.

**The four leftovers (H26).** A doubled attach flag is refused by name before its
value is read (`require_once`, wired into `--agent`/`--since`/`--base`/`--send`,
the precedent `mush/118` set), and `mush focus` refuses a positional id and
`--agent` together in either order instead of silently dropping one. The bar's
`NOTHING_RUNNING` says `Ctrl-N drops every transcript` — the half of that key a
human cannot undo, and the line is only ever read when nothing is running, so the
half worth naming is what goes. `machine::ended`'s last resort is no longer `-1`:
`End::Unknown`, `Ended::Unknown` and `JobOutcome::Unknown` are the state of a
status that names neither an exit code nor a signal, because `-1` reads as a code
a reader could act on. And `edit`'s pinned usage line brackets `[--base R]`, as
the parser (`base: 0`) has always meant.

**One brief was wrong and the code was right.** The plan said `End::Unknown` was
unreachable for a unix child and should be kept honest anyway. It is reachable:
`ExitStatus::from_raw(0x7f)` — a stopped wait status with no stop signal — makes
both `code()` and `signal()` answer `None`. The landing corrects the three
comments that claimed no unix status reads that way and pins the case with a real
test, which is the §8.31/§8.32 pattern again: the register of what is told is not
the register of what happens.

**What the review found, which the patch did not.** The first round seeded books
only at restore, so a parent woken after `WARM_CHILDREN` parked it still answered
"no children and no jobs" — the same defect on the path that actually runs during
a session. The second round gave the seeding one home (`App::seed_parent`) with
both callers, guarded `note_mailbox` with the tombstone like every other book,
and corrected the `forgotten` field's own doc, which claimed a tombstone is never
cleared while the code clears it. Twelve tests in the landing, each verified to
fail with its production line reverted and then restored.

**Verification, and a merge that was clean by text and broken by field.** The
branch is 562 + 130 green; master after the merge is **566 + 132**, fmt and clippy
clean, three smoke scenarios pass. The merge itself was the wave's one new
failure mode: `mush/122` had added `tree::Spawn::fork` while `mush/125`'s four new
hand-made `Spawn` initializers were written against a tree without it, so git
merged without a single conflict and the *test* build failed with four "missing
field" errors — the binary built and the smoke scenarios passed while the suite
could not compile. Fixed on the merge's own commit (`a954358`), and a rule for the
next merge: build the merged tree before believing the merge. `lock::tests` also
flaked once in a full run here and once for `mush/125`, never in 40 isolated runs
across two trees — H28.

**Census at the merge** (`a954358`): total 51,967 · **prod 12,874** · tests
22,506 · comments 13,326. The wave's 1,350 lines are 186 of production and 670 of
test code: three messages, one tombstone and ten tests, wrapped in the prose that
says which fact lives where.

---

## 8.35 Two the models said out loud: a fold that 400s, and a lock with no wait (`ad06be5`, `b91d836`, `13a337d`, `5eba64a`, `602515c`, `4aad8a7`, `fd0e615`)

Neither row came from a reader of the source; both are the human watching the
models and reporting what came back. That is the register's whole value: one is
a provider error on a *stored* session, the other is a model saying it had
nothing to do.

**A `/compact` on an old session was rejected for its tool calls (`ad06be5`).**
The session had not been opened for days; the fold went out and the provider
answered `An assistant message with tool calls must be followed by tool call
ids`. A stored conversation can hold both shapes a strict server rejects — a
call whose result was never recorded (the process went away between the
assistant's message and its results) and the human's own words between a call
and them (they typed while the tools ran) — and `repair_tool_pairs` was applied
at exactly one door: `AgentMsg::Run`, the hand-over a *message* makes. A
`/compact` on a restored actor is the other door, and it is the sharper one: the
fold is not a run, so the stored conversation travels in its first request
there, with nothing in between to repair it. `revive` — a child's stored
messages, restored after a restart — was the third. The rule now has one home,
`adopted`: every conversation the actor did not build is repaired before it
becomes the actor's own, and the three doors call it. The test hands the fold a
transcript with one dangling call and one result interleaved behind the human's
words, and asserts every batch in the summarize request is answered, in id
order, immediately after the message that asked; on a tree where `adopted` does
not repair, it fails with the request's next message reading `user` where it
must read `tool`.

**And the other half of the pair (`b91d836`, `13a337d`).** A pre-`1b70096`
session holds `tool_call_id: ""` beside a call the deserializer has since
renamed `call_0`, so answering the call alone would leave a `tool` message that
answers nothing — the same 400, one message along. `repair_tool_pairs` now
rebuilds each block instead of only re-ordering one: a result whose call is not
in the batch above it is dropped (a duplicate answer is the same shape, and each
accepted result consumes its call), while a result whose id names no call
*anywhere* in the transcript is the legacy shape of the same pair and is
re-pointed at the call in its block still waiting for one. Nothing can be
explained to a model about a call it cannot see — but the first shape alone,
which is where this wave started, threw away the model's real output and
answered the call with a made-up `error: no result was recorded for this call`:
a restored old session would have read a lie where the file held `test result:
ok`. Position is the only evidence a session without ids has, and the block is
the position. The two halves are pinned by the direction each can fail in: with
the adoption pass off, the legacy test reads the synthetic error; with its guard
off (any unanswered call adopting any leftover result), the misplaced-result
test hands a late result to the wrong call.

**A locked-out subagent had no way to wait, and the refusal said so
(`5eba64a`).** The sentence a sibling read was `#3 holds the machine with an
exclusive command (…); this call queued and the lock was still held — do not
retry in a loop; do other work and try once after it finishes (no tool can wait
on another agent's job)`. H13's third fix added the bounded queue and the honest
refusal, but honesty was all it was: "do other work" is not an instruction a
model can follow when every command it has needs the shell, and "(no tool can
wait…)" names the absence rather than a way round it. The models said as much:
*my only way to wait is by executing tools and I can't wait.*

`wait` is now that wait. For every agent but the root — which is exempt from the
lock and works beside a holder, so blocking the orchestrator would be H13's
blindness again — `wait` blocks while another agent holds the machine:
`machine_wait` (a sibling's `Held`) is what keeps it waiting, and the release is
answered with `the machine is free now — #3's exclusive command (cargo bench)
ended; the lock is free for your next command` — the fact, without presupposing
a refusal the asker may never have made. A hold that outlasts the 10-minute wait
names the holder and what is left of the call: `control stop #N` when the holder
is the asker's own child (which is why that branch exists at all), and otherwise
waiting again — a hold can outlive many waits — beside the two moves for when it
cannot: work that needs no shell, or finish the run and say you are blocked.

The rule's first shape asked what the agent *owned*, and got the sharp edge
wrong twice (`602515c`): a model that had read everything it owned could never
reach the machine wait at all — one finished job locked it out for good, which
its own test only exposed by marking a child's result delivered — and a result
nobody had read would have been parked behind a sibling's benchmark. So an
unread result is handed over first, whatever the machine is doing (a wait is
what a model does when it wants a result), and everything else a digest carries
— an already-read body, a job's line — is a recap the transcript already holds,
which is not a reason to refuse to wait. When the hold does outlive the result,
it is named beside it (`machine_held`), so the next call is not a blind retry. A
machine timeout rides along with whatever digest the call was holding — a job's
line, and never an unread child result, because a wait with one hands that over
one branch earlier — and a recap the call was holding rides along with the
`machine is free` sentence.

The surfaces that say it cannot disagree: `MACHINE` owns how to work (the
refusal has a road back; never retry a refused call in a loop), `wait`'s schema
owns what the call covers (an unread result first, the machine, the 10-minute
cap, the early release), and `jobs.rs` owns the refusal — which now sends the
model to `wait` instead of to a retry. `SCHEMA_TOKENS` moved 1200 → 1300 for the
contract the schema grew, by the constant's own documented rule rather than as a
silent overshoot; that also closes audit item 2 (H20).

**What the read-only audit found, which the patch did not (`4aad8a7`).** A
child read the landing against the code and found the new road described by five
sentences that said more than it does — H20's class exactly, and the reason a
wave that *adds* a road owes a read of every sentence that names it:

- the machine timeout told every holder "nothing you can call ends it", which is
  false of exactly one holder: this agent's own child. `control stop` on a child
  lands as `kill_owned`, which kills the job holding the lock, so the sentence
  names that call when the holder is a child, and keeps the two remaining moves
  (work without the shell, or end the run and say you are blocked) for the holds
  no call reaches;
- the `MACHINE` block promised a blocking wait to every reader, and the
  *root*'s never blocks — it is exempt from a lock it did not take and works
  beside the holder. The clause is scoped to a subagent, and the exemption
  sentence now says the wait with it;
- the refusal's road back was stated as one wait, and an agent with a result
  nobody has read gets that result *first*, with the lock still held
  (`machine_held`), so the second wait is the one that waits the hold out — the
  refusal and the block both say so, while what a wait hands over, and in which
  order, stays the schema's fact rather than becoming a second home for it;
- `wait`'s schema said "Returns at once when there is nothing to wait for", which
  the agent the refusal just sent there reads as "…and I own no children and no
  jobs, so it comes straight back". The phrase now names the lock too, and the
  second read below tightened its first half: `no children, no jobs` was not true
  of a call that can reach its answer with a child on the books;
- the interrupted-wait sentence said "your work is still running" of a wait the
  machine alone was keeping alive; it now names what the wait was for, the work
  when there is any and the holder otherwise.

Each of the five is pinned by a test that fails with its branch removed. The
rest of the audit's answers were clean: no production path emits a result with no
batch above it, and the three adoption doors are the complete set. The one
wording item left standing is `exclusive`'s "Siblings are refused, not
interleaved": true of every subagent's command, merely silent about the root's
exemption — incomplete rather than false, and recorded in H20 rather than paid
for out of the schema's byte budget (3,894 of 3,900 at the final landing).

**The second read, and the trap in the wave's own advice (`fd0e615`).** A second
read-only child went over the same six commits, and the worst thing it found was
in the fix itself: the road back the refusal names — *wait again* — could get the
model's run killed. The loop guard counts identical consecutive tool batches, and
a batch containing `wait` is identical every time; a job can hold the machine for
`JOB_MAX_AGE` = 4 h against a wait's 600 s, so waiting again is the *ordinary*
road, and the fifth wait stopped the run as a loop — H13's trap in the new road's
clothes. `count_round` now takes whether the round did nothing, with two such
rounds: a batch refused before it ran (already exempt) and a batch whose `wait`
slept (`state.waited`, taken by the batch loop). A wait that came straight back —
*nothing to wait for* — is not exempt, so a model spinning on an empty wait is
still stopped, and `a_repeated_blocking_wait_is_not_a_loop` pins both halves by
running eight waits at a held machine's fake clock (and the same eight at an
empty one, which must still end as a loop).

Its other four findings were sentences that named a state the agent might not be
in: the machine timeout withdrew the road back (it now names waiting again beside
the two moves for when the model cannot), `machine_free` presupposed a refusal
that may never have happened (nothing ties a wait to a refusal, and a model told
about one it never made may re-issue a command it had given up on),
`nothing to wait for` claimed "no children and no jobs" of a call that can reach
its answer with a child on the books whose result is already read, and
`Refused::Budget`'s two moves were not the asker's to make when the eight jobs
were a sibling's (the budget is machine-wide; a sibling's job is neither its to
stop nor its to wait for). Each is fixed at the sentence; the compact door test
was also strengthened, because the first read showed it carried the human's
words *between two batches* rather than between a call and its result — the shape
the live bug had — so the repair it pins is now the move as well as the answer.

What that read could not settle is recorded rather than guessed at: H29 (a
`Refused::message` doc comment naming a road into `launch` that looks like none)
and H30 (a restored child whose saved phase is not an ending reads `◐ running`
while `wait` says nothing of its own is running). Three more of its findings were
about this write-up and the test it names — a claim about what rides along with a
machine timeout that the branch order makes unreachable, a door test whose
fixture was not the shape described (now it is), and a clause naming a symbol
that does not exist — and are corrected in place. Its clean answers are worth as
much as its findings: `repair_tool_pairs`/`drop_orphan_results` survived a port
plus a 600k-shape fuzz (idempotent on every shape production can build,
terminating, never adopting another call's result), `control stop #N` on a child
frees both kinds of hold, the root's exemption and its immediate refusal hold,
and the counts below re-derived exactly.

**Verification.** Every fix was checked by breaking it, the way the rows in this
file demand. With `adopted` not repairing, the new compact test fails with the
request's fourth message reading `user` where it must read `tool`; with the
adoption pass in `repair_tool_pairs` off, the legacy test reads `error: no result
was recorded` where the file holds `test result: ok`; with the guard on that pass
off, the misplaced-result test hands a late result to the wrong call; with
`machine_wait` answering `None`, seven fail — every test whose road needs a
holder: the release line, both machine timeouts, the unread result's order, the
recap rule, the interrupted wait's holder, and the guard's held-clock case. Four
limits are pinned in their own tests, unchanged by the wave:
the root's wait does not block on a sibling's lock, an unread result is handed
over before the machine is waited out, a read recap does not take the wait away
from the machine, and the holder's own second exclusive claim still does not
queue. Two of them fail in the opposite direction, which is the point of them:
with `machine_wait` answering `None` the *root* test still passes, and it is the
one test that fails when the exemption is taken out of `machine_wait` (a
sibling's `Held` for agent 0 too) — the exemption is the road that test exists
for, not the absence of the wait. The audit's two branches fail their own tests
when removed: `mine` false loses the `control stop` sentence, and the old `still`
clause claims running work for a wait the machine was keeping alive. The second
read's own two branches fail their tests the same way: `a_repeated_blocking_wait…`
fails with the guard reading only refusals, and again with `wait_tool` not
setting the flag it reads. `cargo test` 575 + 136, `fmt` and `clippy` clean.

**Census** at `fd0e615` (`scripts/census.py`, method in §8.5), against the merge
§8.34 closed on (`a954358`: total 51,967 · **prod 12,874** · tests 22,506 ·
comments 13,326): total 52,926 · **prod 13,035** · tests 22,988 · comments
13,604 — the wave is 959 lines, 161 of production, 482 of test code, 278 of
comment and 38 blank. The production lines are the three adoption doors, the
re-point pass in `repair_tool_pairs`, the machine wait and the sentences around
it (`machine_wait`, `holding`, `free`, `held`, `timed_out` and its two branches),
the loop guard's second exemption and the flag it reads, the two refusals' new
words, and the `MACHINE` block and the `wait` schema — which is why
`SCHEMA_TOKENS` moved 1200 → 1300, by the constant's own documented rule rather
than as a silent overshoot. The wave's first landing (`602515c`) was 535 lines,
the first audit 280 and the second read 144 — and that last 144 took back a road
back that could have killed the runs it was written for.

---

## 8.36 The tools that came back, and the picture a shell cannot carry (`1452477`..`bfa0b12`, `mush/3`, `mush/4`)

This wave opened with a decision rather than a report: the human watched agents
work and asked for the file tools back, with the shape spelled out — `read_file`
that can carry an image, `list_files` and a `search` with short descriptions,
`edit_file` with **one** shape, no line numbers in a read, and search now rather
than later. The reversal is H31, and it rests on two facts the six-tool cut's
premise could not survive: the machine lock refuses *every* `run_command` while a
sibling holds it, so "the shell can read it" is false exactly when an agent is
blind (H13's own state), and a shell cannot carry an image, because bytes that
are not text have no road through a tool result that is a string.

**What came back.** `read_file` reads a window of lines with no line numbers (a
numbered line is a string that cannot match `edit_file`'s `old_string`) and one
trailing sentence saying what it left; `write_file` creates or replaces a whole
file and answers in one line naming what it replaced; `list_files` and `search`
share one walker, so "what is a workspace file" has one answer — build and VCS
directories skipped, hidden files not, symlinked directories never followed, each
directory read in name order so a capped walk stops somewhere deterministic.
`search` is literal on purpose (a regex engine is a dependency, and `rg` is the
shell's) and prints `path:line: text`. `edit_file` lost its second, top-level
`old_string`/`new_string` pair: `edits` is always a list, a lone edit sent as a
bare object is read as the list of one it means, `replace_all` lives on the
entry, and the shape the schema declares is the shape the parser reads (H20 item
3's two homes are one). `SCHEMA_TOKENS` moved 1300 → 1900 by the constant's own
documented rule, and the headline regression — `the_file_tools_work_while_the_machine_is_held`
— asserts both halves: the five file tools answer while a sibling's lock is held,
and the same moment refuses `run_command` by name.

**The audit (`mush/3`), and the two live reports it settled.** A read-only pass
over every sentence a model receives concluded that the surfaces were sound and
then found three defects and a falsehood. The defects: a `search` that skipped a
file (binary, or past `SEARCH_FILE_CAP`) answered **"no match"** — a negative the
tool cannot know and a model will act on, so `Workspace::search` now returns
`Matches { matches, more, skipped }` and the count rides back with the answer; a
path that does not exist read as an **empty** one (`no files`, `no match`), and
a `path` that was not a string silently listed the root, so both are refused
(`no such path: …`, and `tools::arg_path` for every optional path); and
`write_file` said **"(new)"** over a file it replaced whenever `read_file`
refused the old bytes, which is a false history fact a transcript keeps — it
answers from existence now, with `(replaced a file that is not text)` in between.
The falsehood sat beside H29: an *exclusive* sibling claim that loses the race
between the lock check and `take_machine` never queued, yet read the queued
road's "this call queued and the lock was still held". `Refused::unqueued_message`
owns that sentence now, the shared road back has one home (`Refused::lock_road`),
and `a_sibling_refusal_never_claims_a_queue_it_did_not_join` pins all three
roads. H29 itself the audit settled **by reading**: no road reaches `launch`'s
`Machine` arm with a sibling holding the lock, because every exclusive caller
goes through `take_machine`, which refuses any existing holder — so the arm is
described as a guard rather than a road, and the doc comment that named an
impossible one is gone. Four more sentences were repaired in the same pass:
`edit_file`'s description no longer offers `replace_all` as the fix for a
*missing* match, `run_command`'s stops repeating the truncation marker's own
sentence (−46 bytes, which paid for the rest), `status` describes the listing it
answers (a branch, not a title it does not print; and what `✉` means), and
`MACHINE`'s quoted detach line is now a prefix of the line `run_command` really
answers. `read_file`'s 32 MB refusal says a window cannot get past it, and a line
longer than the cap names `sed -n '{offset}p'` for its rest.

**The two the human asked about.** Agents were writing `cd /home/rubend/p/mush &&
…` although every command already runs with its cwd at the agent's *own* root —
for an isolated child, its worktree — and both prompts said so. The audit's
verdict: habit, with one real gap, and **not** a schema problem; what was missing
was not the fact but the *price of leaving it*, which only a worktree child can
pay, because the absolute path a `cd` names is usually the parent's checkout
(named in the brief or the task text) — the work then lands in the shared tree
while the child's branch stays at its base, which is the lie M2.6 exists to
prevent. So the isolated child's sentence now carries the cost and `RULES` grew
nothing (H32). Agents were also `sleep 50`/`sleep 55` to wait, and the audit
found no state in which that is rational: `wait` blocks on the thing itself,
polling its mailbox every 50 ms, so a completion *ends* the wait with the result
in the same answer; a completion also folds in on its own and wakes a napping
agent, so a sleeping agent learns nothing sooner (`wait_bounded` drains signals
but folds nothing) and cannot be interrupted at all — while five identical rounds
stop the run as a loop, which a repeated `wait` is exempt from and a repeated
`sleep` is not. `DELEGATION`'s last bullet now says so (H33), and the 50–55 s
shape is read as a dodge of the identical-batch guard rather than a justification
— the one state that still lets the screen and `wait` disagree is H30, which a
sleep cannot fix either. Both rows carry a reserve: if the `cd` habit survives
the sentence, the cheap non-hostile move is a note on that call's own result, and
nothing was added to the schema, which is where the reviewer proved the fact
already was.

**The image half (`mush/4`, then `bfa0b12`).** An `Image` lives *in* the message —
`path` (workspace-relative), `mime`, `bytes` — because mush has no server to host
one: the request is the only thing that leaves this machine. It is never a wire
field of its own: with images, `content` becomes the spec's content array (the
text part first, then one `image_url` part per image holding a `data:` URL), and
with none it is byte for byte the string it always was. That shape is why
`Message` gained a hand-written serializer (a field's `serialize_with` cannot see
its sibling) and a private `ContentPart`, and why `ChatRequest` still takes
`&[Message]` and never learns images exist. Base64 is hand-rolled: thirty lines
of table lookup against §7's dependency budget, tested against the RFC 4648
vectors and both tails. An image is counted by `Message::weight` as its bytes
plus its path and mime — base64's 4/3 inflation deliberately not modeled, because
the bytes-per-token heuristic was measured on text and an image's real cost is
its pixels, so the estimate errs toward "too big", which is the safe direction.
An image leaves a transcript the same way it would leave the budget:
`Message::drop_images` replaces it, in place, with one line naming the path and
format, and **trimming calls it before it drops any turn** — an image is what an
over-budget transcript is usually made of and the cheapest thing to lose, since
the placeholder still says where the file is — while `Session::save` calls it
before serializing, so a multi-megabyte screenshot never lands in
`.mush/session.json`. The drop is idempotent, which is what keeps a session
saved, loaded and saved again from stacking placeholder on placeholder (the test
saves twice and compares). The placeholder's words were tightened to be true in
*both* places it is used: `[image: shots/a.png (png) — bytes dropped to save
room; read the file again if you need them]`.

**The producer.** `read_file` answers with the image itself when the file is one,
sniffed from its own first bytes and never its name (png, jpeg, gif, webp — the
four vision endpoints document), through a `ToolOutput` that lets one tool's
result be more than text while every other call site still reads as it did
(`Deref<Target = str>`, `Display` and two `PartialEq` impls exist for exactly
that), and `Message::tool_with_images` is the one constructor for a result that
carries both. Two refusals, each naming a move that exists and neither borrowing
the other's: an image the run's model is not documented to see is refused
*before* it is sent — the road is the run's own end, saying what the picture was
needed for — and an image past the 2 MB cap is refused with the one thing that
makes it readable (downscale it with `run_command`; `offset`/`limit` cannot help
because an image has no lines, and the sentence says so). Vision is a per-model
fact in the provider table (`ModelSpec::vision`, `deepseek-flash` the one row
that states it) and **off for everything the table does not name**, because the
two ways of being wrong are not symmetric: a false "on" sends bytes an endpoint
may reject, which costs the whole turn and the human's money, where a false "off"
costs an image a human can still ask about by other means.

**Verification.** Each branch was checked by removing it. With `vision_capable`
answering `true` for everything, `an_image_that_cannot_travel_is_refused_with_the_move_that_can`
fails on the model's name; with the sniff replaced by an extension check, a png
named `lies.txt` reads as text and the test fails; with `drop_images` not
idempotent, the double-save test counts two placeholders. The audit's three
defects have tests that fail without them (a skipped-file search answers "no
match"; a missing path answers `no files`; a replaced blob answers "(new)"), and
the unqueued refusal's test fails on either road being swapped in. What the audit
verified and found sound is worth as much as its findings: the lock's four
sentences agree and the road back is findable from the refusal alone, the
loop-stop message and `count_round` read honestly, the capped-note words name
real moves, and nothing now claims a road the file tools made obsolete.
`cargo test` 586 + 147, `fmt` and `clippy` clean; root schemas 5 620 bytes against
`SCHEMA_TOKENS` 1 900, which is 80 bytes of headroom rather than a comfortable
margin — recorded so the next sentence that needs bytes arrives with a decision
attached.

**Census** at `bfa0b12` (`scripts/census.py`, method in §8.5), against §8.35's
landing (`fd0e615`: total 52,926 · **prod 13,035** · tests 22,988 · comments
13,604): total 55,072 · **prod 13,773** · tests 23,795 · comments 14,087 — the
wave is 2,146 lines, 738 of production, 807 of test code, 483 of comment and 118
blank. The production lines are the file tools and the walker they share, the one
shape `edit_file` parses, the audit's four sentence repairs and three defects
(`Matches::skipped`, the path checks, the existence answer), the unqueued
refusal and the road both sibling sentences now share, the image layer
(`Image`, the hand-written message serializer, the base64 encoder, `drop_images`,
the vision lookup), and the producer's sniff, cap and two refusals — which is why
`SCHEMA_TOKENS` moved once, by the constant's own documented rule. Two agents
built it: the tools half in this workspace, the image layer in `mush/4`, whose
brief had to carry every fact about a message layer neither agent could see from
the other's side — and it came back with three corrections to that brief (a
field's `serialize_with` cannot see its sibling; the test counts grew the second
number; `skip_deserializing` is what keeps a session from resurrecting payloads),
which is the delegation loop working as designed.

---

## 8.37 The wait that can name one thing (`0291027`, `076e164`, `mush/5`)

H34 opened with the human's own root agent explaining a `sleep` it should not
have written: "`wait` is exactly the tool for it … I reached for `sleep` because
`wait` also blocks on the two subagents that are still running, and I wanted a
peek at the job only." An investigation (`mush/5`, read-only, forked from
`28f1c84`) verified the books and measured what a repeated `wait` was actually
answering, and the human's next message set the new shape's one hard rule: it
must yield on anything that needs attention — a child or job error, a message,
"really anything" — and never sit on news the way a `sleep` does.

**The defect under it** (`0291027`): a job's report was delivered once and then
recapped *bare* on every later `wait` — `wait_digest`'s job loop fell back to
`report.line` when `record_job` answered `None` — and `done_jobs` is never
pruned, so the recap grew with every job the session had ever run. Measured on a
fabricated state: one read job answered the identical unread-looking line, three
read jobs 151 B, 32 read jobs 1 942 B, where the truth is 58 B; a wait whose
only book entry was a read job answered that recap instead of
`NOTHING_TO_WAIT_FOR`. Now a job's line travels only when `record_job` says it
is fresh, and the wait's entry guard counts only *unread* job reports — a read
child still keeps its marked digest and still holds the wait, because it can run
again. Zero schema bytes; `a_wait_hands_a_job_report_over_once_and_never_recaps_it`
and a read job added to the timeout test fail before the fix.

**The shape** (`076e164`): `wait` gained one optional `on` — a single child
(`2`) or job (`c2`), named as `status` prints it, with the bare call unchanged
and a wrong type refused rather than read as "everything". It blocks on the
target alone: the rest of the books run on, and the machine lock is never this
call's business (bare `wait` stays the one road back the refusal names). What it
does not narrow is its attention: a cancellation, the human's or a parent's
words, and any result nobody has read — a child that failed, a job that ended —
all end the wait and are handed over, with the target named beside them. A
target that is neither running nor holding a result (a seeded child, H30) is
named for what it is rather than slept on for ten minutes. Both waits now share
`wait_tick` (drain, cancellation, parked words) and differ only in what they
block on; `Target` gained the lookups a wait needs, and `jobs::unknown_job` is
the one home of the sentence `control` and `wait` both give.

**Against H15.** The trap was three shapes (`ids`/`all`/`timeout`) and "no ids
= first finish". `on` is one shape with one meaning, the default is the call it
always was, and a wrong argument is refused: "`on` must be one target as status
names it (`2` a child, `c2` a job)". The worst wrong call lands on one named
target, is answered with that name, and a repeat on a read target returns at
once — so the loop guard still stops it.

**The price, stated plainly.** `on` cost 309 schema bytes (the investigation's
sketch measured 279; the landed sentences are longer by the yield rule), so
`SCHEMA_TOKENS` moved 1 900 → 2 000 by the constant's own documented rule:
schemas 5 620 → 5 929 against 6 000, and the 128k history budget 315 300 →
315 000. The 8k default is unchanged (12 288 bytes) because its reserve is
window-capped at half the window. The alternative — a sentence in `wait`'s
schema restating `DELEGATION`'s "a finish arrives on its own" — was not taken:
bytes spent saying what every root already reads.

**Verification by removal.** Reverting the recap fix fails both of its
assertions with the recap printed in the failure; removing the news yield makes
`an_optional_target_waits_for_one_thing_and_lets_the_rest_run` time out instead
of handing over the failed child and job. The plain wait's own tests (the human
message, the parent's steering, the machine roads) are unchanged and green
through the `wait_tick` extraction. `cargo test` 588 + 147, `fmt` and `clippy`
clean. One full-suite flake was seen once and not again
(`the_turn_limit_ends_with_a_summary` did not end within its `WAIT`; it passes
alone and the next full run was green) — the H28 class, recorded there.

**Census** at `076e164` (`scripts/census.py`), against §8.36's landing
(`bfa0b12`: total 55,072 · prod 13,773 · tests 23,795 · comments 14,087): total
55,548 · **prod 13,915** · tests 24,003 · comments 14,187 — 476 lines: 142
production, 208 test, 100 comment, 26 blank. The production lines are the target
plumbing (`wait_target`, `Target`'s six lookups), the shared tick, the recap
fix, the one schema argument, and `jobs::unknown_job`'s new home.

## 8.38 The icon that said working (`⧗`)

Observed live, in the session running this repository: the root was parked in a
`wait` on a child, so its row read `◐ #0 ⏸1 root  waiting on results 3s` — the
words U7's fix had made true, over an icon that still said *working*. The foot
showed no spinner and the bar said `waiting on 1 subagent(s) — the root resumes
as they finish`; the glyph and the words disagreed inside one row. The human
read the row and asked for the surface the finding had missed: "when an agent is
waiting it still shows the working icon, can we make sure the icon status
reflect what the agents are actually doing."

**What U7 left behind.** The finding's own words were "the hourglass the human
asked for", and its closure taught `Phase::waiting` to the row's words, the
transcript's foot and the row's footer. Three readers did not ask: the glyph
(`phase_glyph`'s `Phase::Activity(_) => "◐"` arm, reached by a `wait` label like
any other tool), `Phase::label`'s machine name for the attach roster
(`working`), and `AgentTree::roster`'s buckets (every `is_busy` phase counted as
working, and a parked run is busy). One fact — this agent is not computing, it
is waiting for somebody else's result — with four spellings, three of them
wrong: the §8 class, found in the surface a glance reads.

**The fix is one derivation, four readers.** `Phase::waiting()` now reaches all
of them: the glyph is `⧗`, the label is `waiting`, and the title counts a parked
run in `M waiting` beside the napping parents it already counted there. The one
hourglass that fits is the one `unicode-width` calls a single column: `⌛`
(U+231B) is emoji-presentation and measures 2, which `every_row_mark_is_one_
column` now pins for every mark a row carries, so the next glyph cannot be
chosen by eye. What deliberately did *not* change is `is_busy`/`busy_counts`:
"a run is in flight" is the fact the `⏸N` mark and the bar's promise read, and a
parked run *is* in flight — it will finish and report. The title's `N working`
is the other question, who is computing.

**The `/compact` half of the same message, answered and left alone.** "I typed
`/compact` while waiting, it said `folding at the next step` — is that intended,
I expected immediate compaction." Intended, and it is the behaviour the
budget chapter documents: a fold never lands between an assistant's tool calls
and their results, so a request that arrives mid-run parks like a nudge and is
honoured at the next message boundary (`docs/mush.md`, "the context budget").
In this session it landed exactly there — the human's next words woke the wait,
the boundary folded the conversation, and the run after it began with the
summary (which is why the session's history starts with the compaction message).
What the screen owes is that the wait be *visible*, and it is: the row's
`≡ folding at the next step… 12s` ages, and the bar says `keep typing — your
message is answered after the fold`. Recorded rather than changed. The shape
the question suggests — let a fold request wake a parked `wait` the way the
human's own words do, so the fold lands one boundary later instead of when the
wait ends — is a rule change in the wait's yield list, not a bug fix.

**Verification by removal.** Reverting the glyph arm fails `glyphs_are_truthful`
with `◐` printed against the name it now answers, and
`a_waiting_agent_is_not_drawn_working` on `⧗ #0`; reverting `roster` alone fails
that same test's `1 waiting`/`1 working` pair, which used to read `2 working`
for one napping parent, one parked run and one working child. Three tests were
updated rather than worked around: those two — the app test's row and title
assertions are now the parked run's own — and
`a_phase_has_one_name_for_every_reader` gained the wait case, where the machine
name and the bar's word differ by design (`waiting` and the tool's own `wait`). `cargo test` 608 + 153, `fmt`, `clippy -D warnings` clean.

**Census** at this landing (against §8.37's `076e164`: total 55,548 · prod
13,915 · tests 24,003 · comments 14,187; the image-paste wave sits between
them): total 57,285 · **prod 14,397** · tests 24,582 · comments 14,749. The wave
itself is 87 lines: 4 production (the glyph arm, the label arm, the roster
branch, the `waiting` field doc), 34 test, 48 comment, 1 blank. (The first
written numbers here were 7 short — they were measured before the last two test
extensions of the same wave landed; corrected against the commit itself, which
is what a census is for.)

## 8.39 A result that reached nobody (H35, `b50a4ef`, `258ffb5`)

**The defect.** A child agent restored from a stored session was revived with a
dead parent channel, and nothing in the process could repair it.
`App::restore_agents` passed `parent: None` into `agent::revive`, which then
built `let (dead_tx, dead_rx) = unbounded::<AgentMsg>(); drop(dead_rx);` and
handed the child `parent_tx: parent.unwrap_or(dead_tx)`. Every send into that
channel was a `let _ =`: the run's start (`ChildRunning`), its `Work`, its
completion (`ChildDone`), and the thread-start-failure report in `start`. It
had no reader, and no report through it could be heard.

**Why nothing repaired it.** The one road that would wire a parent,
`App::deliver_to_actor`, is reached only after `tx.send(command)` has already
*failed* (`Ok(()) => return true` returns early on success). A restored child's
actor is alive (`agent::revive` ends in `start(actor, transcript, false)`), so
the human's nudge succeeded, the early return happened, and `parent_tx` stayed
dead for the life of the process — unreachable by construction, not merely
absent. And the books were still marked: `App::deliver` sends
`tell_parent_running` (`ChildRunning`) to the parent from the UI, and the
completion that would clear it went to the dead channel, so the books were
marked and never settled.

**What the human saw.** The root naps, the child's row turns `✓` or `✗`, and
the bar still says "waiting on 1 subagent(s) — the root resumes as they finish";
the root never runs again, its books keep the child running
(`state.running[child]`), a `wait` blocks the full 600 s and then answers
"still running", and only the human typing wakes it. Three surfaces promise the
wake — `App::tree_line`'s sentence, the `✉` result-unread mark, and
`docs/mush.md`'s "a result is never lost just because nobody called `wait` in
time" — and `ReviveSpec.parent`'s own doc claimed the re-wiring happened for a
child woken by the human's message. It is long-standing: `8f75cb2` introduced
restore deliberately ("Its completion goes to a dead channel: the human owns a
revived agent, not the root").

**The restore door passes the parent's live mailbox.** `ReviveSpec.parent` is
now `parent.and_then(|parent| self.tree.agent_tx.get(&parent).cloned())`: the
tree's live mailbox for the parent, read at the revive. The order question —
does a nested child's parent already have an actor? — is answered and pinned:
the file is in spawn order (`App::session_snapshot` walks `tree.agents`, and a
parent's node is pushed before any child it spawns) and `AgentTree::register`
inserts each revived actor's sender immediately, so a *nested* child's parent is
already in `tree.agent_tx` when the child is revived — the whole tree is wired,
not only the root's children. A parent that has no actor of its own (a worktree
found on disk) has no sender to give: the child gets a dead mailbox, and
`dead_mailbox()` is now the one home of that ("a send into it fails, and the
failure is the fact the UI reads").

**The hazard the wiring opens.** A root actor is built with no transcript at
all (`agent::spawn` starts it with an empty `Vec`; the conversation lives in the
UI and travels with the first `AgentMsg::Run`, whose arm does
`*transcript = adopted(messages)`). A completion reaching the root's live actor
therefore folded `Fold::Run` over nothing, and the run's whole request was a
bare `#2 done: …` user message — no system message, no history. The commit
closes this in two halves.

**`AgentMsg::Adopt`** hands the UI's conversation over at the restore door, at
the *end* of `App::restore_agents` so it is the conversation the human is
looking at (cut-off lines included), repaired through `adopted` like every
hand-over. It does not run — the guard is "an actor that already has a
conversation keeps it", which is why `drain_mailbox`'s arm drops it mid-run —
and the next `Run` still wins.

**`AgentEvent::ParentAsleep`** covers a parent-directed send that really fails.
`tell_parent` no longer drops it: the command goes to the UI, and
`App::hand_to_parent` delivers it to the parent the *tree* names through the
same door a human's message uses (`App::deliver_to_actor`, reviving a parked
parent and starting nothing for one that is really gone). This is the class road
— every silently dropped `parent_tx` send, not only the restore case — and the
mirror of `AgentEvent::ChildAsleep` (finding H18) in the other direction. The
root is the one agent that reports nothing: `parent_tx: None` (not a dead
mailbox, so "has a parent" is a fact in the actor), and `hand_to_parent` refuses
it once more by the tree (no parent row) — the root must never absorb its own
completion, a wake-up that could never end.

**Tests, and what fails when each is reverted.** Six were added, each verified
by removal, and the first is the end-to-end the suite lacked:
`a_restored_childs_completion_wakes_the_root` stores a session with one child,
restores it, nudges the child through
`App::deliver`, lets its run finish — a loopback `say_endpoint` answers the
revived actor, which uses the cell's own `HttpModel`, hence the new
`app_with_scripted_root_at` harness — and reads the root's request. Reverting
the restore wiring fails it with no request at all; removing only the hand-over
fails it with the hazard printed whole, `["#2 done: ported the parser"]` as the
entire request. Nothing in it drains the UI's channel, so it pins the *direct*
road.

**Five more, each verified by removal.**
`a_wait_on_a_restored_child_answers_the_result_not_the_deadline` reads the
parent's books: the root's first run calls `wait`, and the answer
(`#2 ✓ ported the parser (already read — no new run since)`) arrives in the next
request — a ghost running child would block the full 600 s and that request
would never come. `a_restored_grandchilds_completion_walks_the_tree` pins the
revive order: root → #2 → #3, the grandchild's result wakes its parent and the
parent's wakes the root, and `#3 done:` never appears in the root's request — a
report that skipped its own parent.
`a_report_with_no_actor_behind_the_parent_goes_to_the_ui` (agent.rs) is the
actor's half: a child with a dead mailbox emits `ParentAsleep` carrying
`ChildRunning` then `ChildDone` in order, and the actor with `parent_tx: None`
emits none. `a_childs_report_reaches_the_parent_the_tree_names` (app/mod.rs) is
the UI's half: a parked parent with a child row is revived and runs on the
completion it could not have been handed — reverting the UI's delivery (the arm
dropping the command) fails it. The sixth,
`the_roots_own_completion_is_not_filed_back_into_it`, pins the wake-up that
could never end: a `ParentAsleep` about the root delivers nothing.

**The shape the human reported, end to end** (`258ffb5`). The six above each
hold one half; `a_restored_root_is_woken_by_the_children_it_continued` drives
the session the report came from through the *app's* own loop: a stored session
whose two children have no worktree or branch left, the human's nudge at the
root, the root's own `control message` continuing both (*continued*, not
spawned), their runs answered by the loopback endpoint, and the parent's books
read back. Three things are asserted, and they are the three that were wrong:
the root's *next* request carries both `#N done: …` lines in a well-formed
conversation (the system prompt and the human's own words are in it, so it is
not the bare completion line), `status` answers each child's outcome instead of
`◐ running`, and the `wait` is answered out of the books — both results, marked
`(already read — no new run since)` — instead of parking for its whole cap.
The order is the test's and not the scheduler's: the root's second turn is a
`Scripted::held` reply, so both children finish and their completions sit in the
parent's mailbox before that turn's answer lands and the boundary at its end
folds them. A child tells its parent *before* it tells the UI, so the pumps
wait on the tree's `✓` for a fact the parent already holds. Ten consecutive
runs pass, and the first draft of this test is worth recording: it waited for
the root to *nap* while the children finished, which the fix makes impossible —
the root is awake, answering the news — and it failed at its 20 s deadline
until the premise was dropped. Its removal evidence is the pair, not the wiring
alone: reverting `ReviveSpec.parent` to `None` still passes, because the dead
mailbox then hands the completion to the UI (`ParentAsleep`) and
`hand_to_parent` delivers it to the root anyway — that is what the class road is
for. With the wiring reverted *and* the `ParentAsleep` arm dropped, the woken
request never comes and the test fails at its deadline.

**Recorded rather than changed.** `App::deliver_to_actor`'s own `ChildMailbox`
send and `App::tell_parent_running` are still `let _ =` — a failed one of those
is the H22 shape, and the next `control` takes the wake path again — and the
`readers` rule in `AgentTree::may_park` is what keeps the
reachable-because-stale case rare today, so the UI's half is a class road
rather than a road a test in this wave drives end to end through production.

**The `✉` re-arm, recorded rather than fixed.**
`AgentTree::{finish,fail,stopped}` set `result_unread = parent.is_some()`
unconditionally, and that arm can land *after* the parent's read: `finish_run`
sends the parent its `ChildDone` before it emits the run's `Done` to the UI, so
the parent can be scheduled in between, fold the result and have its
`ResultRead` applied to the tree first — the row then wears `✉` over a result
its parent *has* read, and nothing clears it (the books hold that run as read,
so no second `ResultRead` comes). The end-to-end test asserts the books
(`status`'s listing, the `wait`'s answer) and never the tree's marks, for
exactly this reason. The arm and the read are on different threads, so the
order is the scheduler's; a fix needs the run number on both events, which is
what would make the two comparable.

**Why the tree was right and the actor was wrong.** The human asked the sharp
version of the question — "the UI was correct in showing who was supposed to
receive it; why is the UI's resolution and the actual resolution different?" —
and the answer is the shape of the two references, not a slip in either.

*The tree resolves by name, every time.* A node carries its parent's `AgentId`,
and every use re-resolves it against a live map: `hand_to_parent` reads
`node.parent` and looks the mailbox up in `AgentTree::agent_tx`, which the UI
swaps in whenever an actor is revived (`ChildMailbox`). An id survives its
actor — it is what a row is *for* — so the tree can name a recipient that has no
thread at all, which is exactly what `✉` and `✉2` did while the orchestrator was
asleep. *The actor holds a capability, once.* `Actor.parent_tx` is a
`Sender<AgentMsg>` cloned when the actor was built: one receiver, for that
actor's whole life, with no setter and no message that carries a new one. A name
can be re-resolved; a capability cannot be re-pointed — that is the entire
difference, and it is why the actor's side went silent while the tree's side
stayed right.

*And the fact had two owners.* `result_unread` is the tree's claim, set by the
child's own end (`finish`/`fail`/`stopped`, from the tree's structure alone) and
cleared only by the parent actor's `ResultRead` event — or by the child starting
another run, which supersedes the result; the books
(`ActorState::completed`/`running`) are the actor's claim, written only by a
delivery on the wire. So "2 news waiting" was true about the tree and false
about the books at the same moment — one fact, two owners, one of them fed.
That is the §8 class again, this time between a capability and a lookup; the fix
makes the wire report its own failure (`ParentAsleep`) so the UI's id-based road
is what carries it, rather than giving the actor a second, shared copy of the
tree's map.

The UI's road is not infallible either, and the same distinction says why:
`agent_tx` is a *mailbox* registry, not an *actor* registry, so a parked child's
mailbox is a name with nobody behind it — which is what the two `let _ =` sends
recorded above (the H22 shape) cost. Name-based resolution settles *who*, never
*whether anyone is listening*.

**Census** at this landing on the rebased tree (`scripts/census.py`), against
§8.41's (total 57,994 · prod 14,459 · tests 24,963 · comments 14,980): total
58,917 · **prod 14,500** · tests 25,461 · comments 15,330 — 923 lines: 41
production, 498 test, 350 comment, 34 blank. The production lines are the
restore door's parent lookup, `Adopt` and its three arms, `parent_tx`'s
`Option`, `dead_mailbox`, `tell_parent`'s two homes, `ParentAsleep`, and
`hand_to_parent`; the test lines are the seven tests and their harnesses
(`say_endpoint`, `pump`, two stored-session fixtures); the comment lines are the
docs on those shapes plus the comments that asserted the old behaviour, flipped
rather than left standing. `cargo test` 621 + 153, `fmt` and `clippy -D warnings`
clean at `258ffb5`.

## 8.40 Four holes of one class: a report, a sweep, a sentence, a tick (`3a3f474`, `5e93f5b`, `d482ab9`, `cbed322`)

A read-only audit at `faa658d` confirmed one bug — a restored child's dead
parent channel, fixed in §8.39 (`b50a4ef`, `258ffb5`) just above — and listed
four more of the same class: a fact one hand knows and another drops, mis-times
or mis-words. They are independent, and each is its own commit below, with its
test and what fails when the fix is reverted. This is §8.40 and not §8.39
because the parental half of the same audit — a restored child's completion
reaching its parent, and the root that wakes to it — was written in parallel as
§8.39, which is the section above; the two branches met here. The audit's two
residual ordering notes are recorded at the end, not changed.

**1 — a cut-off report dropped into a parked parent** (`3a3f474`).
`App::report_cut_off` (`app/mod.rs:3481`) sent `ChildDone { run: CUT_OFF_RUN,
outcome: CutOff }` with a raw `let _ = tx.send`, so when the parent's actor was
parked — a node whose mailbox has no thread behind it (`App::park_history`) —
the one report of a run that never ended was dropped, although the message's own
doc says it wakes a napping parent. The send now goes through
`App::deliver_to_actor`, the door `AgentEvent::ChildAsleep` already uses: it
rebuilds the parent's actor and hands it the completion. A parent whose node is
gone, and the root — nobody's child, never revived — still have nobody to tell;
their row and pane still say it.
`a_cut_off_child_wakes_a_parked_parent_through_the_ui` (`app/mod.rs:11546`)
parks the parent by dropping its receiver, has a dead child mid-run, and asserts
that `stop_one` leaves the tree holding a mailbox with an actor behind it — the
parent begins the run that folds the report. Reverting the one line fails it at
"the parked parent has an actor again".

**2 — the sweep that ran after the actor was built** (`5e93f5b`). `App::new`
restored the stored agents first and ran `discover_worktrees` after, so a
restored child's actor was built on `.mush/wt/<id>` (`agent::revive`) and the
sweep deleted the directory only then. A restored agent has no stored fork
revision, so a branch with no commit of its own reads as `Landed::Merged` — the
H21 lie — and the node was frozen as merged by `App::worktree_gone` while its
actor still pointed at the dead path. The startup reclaim is now its own method,
`App::reclaim_isolated` (`app/mod.rs:1064`), called once *before*
`restore_agents` and again by `discover_worktrees` for the repository as it
stands; the restored agent derives its branch through `agent::live_branch` after
the sweep, gets no branch and no landing it never had, and is revived in the
root. `a_restored_actor_is_built_after_the_sweep_that_takes_its_worktree`
(`app/mod.rs:5621`) restores a stored child at a real zero-commit `mush/2`
worktree, then asserts the row claims no merge, nothing refuses a nudge, and the
actor's first request — read off a recording loopback endpoint — names the root,
not `.mush/wt/2`. Dropping the pre-restore call fails it at
`landed == Some(Merged)`.

**3 — a leftover answering "agent #N is gone"** (`d482ab9`).
`discover_worktrees` registers a leftover worktree with `parent: None, tx: None`,
and `deliver_to_actor` answers `false` for a missing mailbox by design — but
`App::deliver` then said `agent::gone(id)` (`agent #7 is gone`) about a row on
screen whose worktree and branch exist. `App::no_actor_line` (`app/mod.rs:2219`)
is now the one place a refused message picks its words: a mailbox-less leftover
says it was found on disk and never given an actor, and names the two roads that
work; every other absent actor keeps `agent::gone`. The refusal itself is
unchanged. `a_message_to_a_leftover_says_it_was_never_given_an_actor`
(`app/mod.rs:4366`) points at a real unmerged `mush/7` leftover on disk, sends it
a message, and asserts the bar's line says which absence this is and not "is
gone"; reverting to `agent::gone` fails it with the old sentence printed.

**4 — the tick that could park a child a `control` just resumed** (`cbed322`).
A parent's `control message` sends into the child's mailbox without touching the
tree, and the tree hears the run only from the child's own `Running` event — a
moment behind. A `tick` in that window ran `park_history`, whose `Shutdown`
cancelled the run the words just started; the child then reported `Stopped`
about a run nobody stopped, while the parent's books already said running.
`AgentEvent::ChildResumed` (`agent.rs:677`) now travels with the send:
`message_agent` emits it when a live mailbox took words that resume a child it
found at rest (`agent.rs:4229`), and the UI marks the row with the same
optimistic `AgentTree::nudge` the human's own message sets (`app/mod.rs:1488`).
The parked half takes it too: the `ChildAsleep` road, where the UI rebuilds the
actor, marks the child when the command it just delivered is a `Steer`
(`app/mod.rs:1477`). `may_park` was left alone — a pending field there is a
second place to get out of step with the phase it describes, while the nudge
mark already means "words are on their way" everywhere the human's own message
goes. Three tests: `a_control_message_keeps_a_child_out_of_the_parking_tick`
(`app/mod.rs:4181`) drives a real root, whose stub endpoint answers the first
request with a `control` tool call, into a child past `WARM_CHILDREN`, applies
the UI's events and takes one `tick` — the child's mailbox must hold the words
and no `Shutdown`; `a_woken_child_is_not_parked_before_its_own_running_lands`
(`app/mod.rs:4277`) is the parked half through `ChildAsleep`;
`resuming_a_child_reports_the_mark_the_tree_needs` (`agent.rs:5748`) is the
actor-side emission. Removing the emit fails the first at the Shutdown; skipping
the UI nudge fails both app tests.

**Recorded, not changed.** The audit's two residual ordering notes stand.
(a) `note_completion` (`agent.rs:2874`) has no run-monotonic guard, so a
`ChildParked` applied on the UI thread can reach a parent before the child's own
`ChildDone` and overwrite a newer record. (b) A completion drained by a run that
is then cancelled in the same batch waits for a later boundary. Both are the B24
delivery questions, not these four holes.

**Census** on this landing (`scripts/census.py`), against §8.38's (its own
landing, `19b0810` plus the sweep-test follow-up `8dd66ad`: total 57,302 ·
prod 14,397 · tests 24,594 · comments 14,753): total 57,814 · **prod 14,427** ·
tests 24,894 · comments 14,905 — 512 lines: 30 production, 300 test, 152
comment, 30 blank. The production lines are `reclaim_isolated`,
`no_actor_line`, the `ChildResumed` variant and its two marks, and
`report_cut_off`'s one road.

## 8.41 A stop is news, and it says who asked

The rule this changes was written down as deliberate: "a *stopped* child does not
wake its parent (`Outcome::is_news` — the human's stop is not news, and the line
is in the transcript for the next run)". The human who hit it in a live session
ruled the other way, and the argument is the one the code already makes for a
*cut-off* run — "the parent is waiting for a result that will never come … so it
has to be told rather than left to assume":

> even if a parent is depending on that child for information (which they usually
> are) regardless of whether it's a human that stopped them it SHOULD be news no?
> perhaps a hint that the human was the one that stopped the child no?

Both halves landed. `AgentMsg::Stop` now carries a `Stop` — `Human` (the tree's
`Ctrl-C`), `Parent` (`control stop`), `Reclaimed` (mush's own doing: a park, or
`Ctrl-N` taking the tree down), and `Unrecorded` for a stop that came back from a
stored session — and the outcome the parent is told carries the same value
(`Outcome::Stopped(Stop)`), so the line can name the hand: `#30 stopped: the human
stopped the run before it finished, so no result is coming — this agent is idle,
not done; control message resumes it`. `Parent` reads as "you stopped", because
the line is read by the agent that did it, and `Unrecorded` names no hand rather
than guessing one.

`Outcome::is_news` is now false for exactly one ending: a park. Parking is mush
reclaiming a thread the window is not using — the agent is at rest and resumable,
the run was not lost, and waking a parent into a fresh (paid) run for memory
management would be waking it for nothing. Every other ending is news, so a
napping parent is woken when its child stops, fails, is cut off, or finishes.

Two details the implementation had to get right, both documented where they live.
The cancel flag has two roads: the human's `Ctrl-C` sets the flag *and* sends the
message, and a run can end between the two — so `ActorState.stop` is an
`Option<Stop>` and `None` resolves to `Human`, the only cause that reaches the
flag alone. And a stop that arrives while the actor is idle records nothing: it
cancels work that is not running, and a cause stored there would name the wrong
hand at the *next* cancellation.

**Verification.** `a_stop_wakes_a_napping_parent_but_a_park_does_not` — the test
that used to be `a_stop_does_not_wake_a_napping_parent_but_a_finish_does` — walks
all four causes through `absorb`: `Human` and `Parent` fold `Fold::Run` and their
lines name the hand, `Reclaimed` folds `Fold::Idle` with the parked sentence, and
a finish still wakes. `only_a_finished_run_reports_itself_as_done` asserts each
line's words. Reverting `is_news` to `!matches!(self, Outcome::Stopped(_))` fails
the first; collapsing the payload to one cause fails the words. The mechanical
rewrite of the stop road was caught by a test doing its job:
`a_stop_aimed_at_a_parked_child_is_handed_over_rather_than_called_gone` asserts
that a *parent's* stop arrives as `Stop::Parent`, and failed when the rewrite
said `Human`.

**Consequences, stated.** A human stopping a child now wakes a napping parent
into a run — one run for a batch, since the first completion starts it and the
rest fold in at its boundaries. `Ctrl-N` still wakes nobody: every actor that
ends under it ends with `shutdown` set, which is `Reclaimed`. A park that lands
*mid-run* (the window's race, §8.40 item 4) is the one road that produces a
`Reclaimed` line for a parent, and it reads as what it is — "it is parked, not
ended". `prompt.rs`'s DELEGATION still says only that "a finish wakes you": true,
and now incomplete; the human owns that file, so it is recorded rather than
edited.

**Census** at this landing (`scripts/census.py`), against §8.40's (total 57,814 ·
prod 14,427 · tests 24,894 · comments 14,905): total 57,994 · **prod 14,459** ·
tests 24,963 · comments 14,980 — 180 lines: 32 production, 69 test, 75 comment,
4 blank. The wave is comment-heavy on purpose: the ruling, the four hands and
the two races are what a later reader has to be told, and the code that carries
them is a dozen lines. Twenty-two of the test lines are `cargo fmt`'s reflow of
calls that grew a payload, not assertions.

---

## 8.42 Four asks in one sitting: a meter that lied about images, a box with no way back, dots, and the root's job

The human pasted a screenshot, read the meter, and asked two questions in one
message — "how are we counting ctx for images" and "how are we measuring image
sizing to drop images" — then, while the wave that answered them was in flight,
three more things: remove the spinner the working line turned over, make a
pasted picture removable and a typed draft clearable, and make the root's system
message say its job is to orchestrate rather than to work. Three of the four
were defects this file should have caught; one is a policy the human owns.

**The meter lied about images (`be26cdb`, `8dd29e5`; H36).** `Message::weight`
priced an image by its **file bytes** — `bytes.len() + path + mime` — and
`BYTES_PER_TOKEN` (3) turned that into "tokens", so the 741,396-byte png the
human pasted read as **247,132 tokens**. A 1920×1080 picture is 2,073,600
pixels, and at the vision endpoints' own rule (≈ pixels/750; OpenAI's tiling
works out near 1,500 px per token) it costs **≈2,765** — the estimate was about
90× over, and in the one direction that does damage. The trimmer sheds image
payloads *before* it drops a turn, so on a 500k-token window (budget
`(500,000 − 69,500) × 3 = 1,291,500` bytes of weight: `SCHEMA_TOKENS` 2,000 +
the reply cap's 62,500 + the 5,000 margin) a ~300k-token conversation left
≈390 KB of room, the picture claimed 741 KB, and **the newest image — the one
just attached — was stripped before the model ever looked at it**: the model
received the placeholder and nothing else. Their meter reading is that same
arithmetic seen from the other side: 598.7k − 247.1k = 351.6k, so the
conversation was ≈350k tokens, not the 300k they rounded to, and one picture
accounted for 247k of the 298.7k jump. (The brief that opened this wave said the
budget was ≈1.44 MB — wrong, because it read `request_reserve` as an eighth of
the window rather than `SCHEMA_TOKENS + reply_cap + MARGIN`; the defect is the
same either way. §8.30's own numbers are the record.)

The fix prices a picture by what it *is*: `Image` gained
`pixels: Option<(u32, u32)>`, filled by a new `Workspace::image_dimensions` at
both doors that read bytes from disk (png's `IHDR`, a jpeg `SOFn` found by
walking marker segments, gif's logical screen descriptor, webp's `VP8 `/`VP8L`/
`VP8X`) and left `None` for anything whose header cannot be read — and then
`Image::weight` charges `tokens(pixels) × BYTES_PER_TOKEN` with the byte count
as the fallback, which *over*counts, the safe direction. `PIXELS_PER_TOKEN`
(750, `tokens_for_pixels` rounding up) is one constant with its reasoning, and
the caveat it does not model is stated there: an endpoint that tokenized the
`data:` URL's base64 as text would pay for the spelling too. Three sizes now
have three names instead of one: **pixels** price the context, **file bytes**
are the `IMAGE_FILE_CAP` transport gate (2 MB, unchanged), and the room *left*
(`history_budget() − used_weight_for`, the meter's own sum, split out so the two
cannot drift) is what the attach line warns about — with `/compact` and a
downscale as the two roads, and equality counting as fitting, the same
`total <= budget` the trimmer stops on.

**What the widened test wave found (three real defects, all in the parsers and
the arithmetic it was told to be robust about).** A jpeg cut off *inside* its
`SOFn` segment still answered `Some` when the walk only bounds-checked its
reads: the segment's declared length is now checked (≥ 8, and the bytes it
names must be there), which the every-prefix test caught. A webp chunk whose
length field lied (`0xFFFFFFFF`, or fewer bytes than the shape reads) still
parsed: the declared size is validated against the buffer. And a header
claiming `u32::MAX × u32::MAX` pixels could overflow the attach path's
`cost + pending`, the image's own `payload + path + mime`, and the trimmer's
total: every one of those sums is saturating now, with the exact 64-bit weight
(73,786,976,260,478,491, checked against a `u128` expectation) pinned by a test
and 260 such headers still reading as over budget. `cargo test` at the tip is
647 + 172 (from 621 + 153 at §8.41's landing), and the image tests are
load-bearing: forcing `Image::weight` back to the byte count fails six of them,
including both of the human's.

The parser's own probe was scratch and stays out of the suite, because it needs
real files: seven ImageMagick-written images — png, baseline and progressive
jpeg, gif, and lossy, lossless and alpha webp — all named their size, and
**every prefix** of all seven (1,239,137 truncations, down to the empty slice)
came back `None` or a size without a single panic. The hand-built byte arrays
that *are* in the suite carry the same shapes, including a large APP1 whose
payload is marker-shaped bytes (the walk steps over segments by their declared
length, so it cannot read a fake frame out of EXIF).

**The box's losses (`3aaf284`..`7013015`; H37).** The human's words: "once an
image is pasted into the input there's really no way to remove it... and really
no easy way to clear what you've typed into the input ... so how do we solve
this?" `Backspace` popped an attachment only on an *empty* box, so with a draft
in progress the one key that removed a picture was `Esc` — which took the words
with it and remembered nothing. The ruling is three keys and one rule:
`Backspace` **at the very start of the box** (index zero, not the start of the
wrapped line) pops the newest attachment, so the empty box is that same rule
with nothing above the cursor rather than a second rule; `Ctrl-U` clears the
words and keeps the images (readline's habit, the whole draft rather than the
visual line, because the box soft-wraps); `Ctrl-Z` puts back what the box last
lost — a pop restores the image, `Ctrl-U` the words, `Esc` the words and every
image — one slot, not a history. `Esc` now says what it took and names the road
back (`cleared the box and 2 images · Ctrl-Z puts it back`), which is the one
loss a human cannot retype.

Two decisions the implementation made explicit. The slot holds **what the loss
took**, not a snapshot of the whole box: a snapshot would clone every image's
bytes on a pop, and restoring one wholesale would delete whatever was typed or
attached *after* the loss while spending the road back — so `Ctrl-Z` adds the
lost words and images back and never removes anything newer. And a **send spends
the slot**, so a message that has been sent cannot be brought back by a
keystroke; it is spent in `send_message` rather than where the box is emptied,
because an `Enter` that sends nothing (an empty box) must not spend it. Found
and left alone: `Chat::clear`'s doc says "the box and the scrollback with it",
but `Ctrl-N` in fact keeps whatever draft is in the box — the slot is dropped
there, since a draft from the dead conversation should not come back, and
clearing the box on `Ctrl-N` would be a one-key draft loss nobody asked for.

**The working line (`0e7614f`).** "remove the working spinning animation and
instead just do `working.` -> `working..` -> `working...` looping with 1s
between dots." The spinner was ten braille frames advanced by `App::tick` — the
event loop's 30 ms poll, so thirty turns a second: a flicker, not a pulse, and a
repaint a frame for a decoration. `App::spin` is now a *beat*, advanced at most
once a second (`DOT_PERIOD`, `spin_at` the clock), and the word carries its own
dots (`chat::working_dots`, one, two, three, looping). A fold still says its own
words on that row and a run parked in a `wait` still paints no activity line at
all (U7). The ramification was in the tests, not the code: nine assertions
needed the needle `working.` instead of `working…` — and *not* the bar's own
`agents · 1 working`, which is a different fact and must not satisfy them.

**The root's job (`5bb809d`).** The human's ruling: the root's system message
should say "their job is mainly to orchestrate subagents (most of the work
should be done by subagents, they should refrain as much as possible from doing
editing, their sole job is maintaining a high level overview AND interacting
with the human/user)". `system_prompt` now opens with `ROOT_ROLE` — hold the
overview, decide what happens next, talk to the human; the work belongs to
subagents, and an edit the root makes itself lands in this checkout with no
brief, no branch and no second reader, at the cost of the picture it was
holding — and it is the one block the subagent prompt does not read, because a
child is handed a brief rather than a role. The policy bullet that said the
opposite ("do single edits and lookups yourself") was rewritten rather than left
standing beside it: delegate the work itself and keep the overview — lookups a
single call answers, the briefs, the decisions. One test came out of it
(`the_root_is_told_its_job_is_the_overview_and_the_human`), and one test had to
be repaired: `the_context_meter_shows_used_over_window` was passing on a
rounding boundary — its 43-byte question moved the 0.1k label only because the
system prompt's weight happened to sit just under one — and ~100 tokens of new
prompt moved the boundary, so the question is now long enough that its weight
must show at the label's own granularity.

**The harness's own ruling, recorded here because this file keeps the harness's
findings too.** The human's first reaction to this wave was that the dots change
should have been a subagent's, then the rule: "that's ok when the work is
delicate and minimal (specifically from a user's ask they could do the work
themselves BUT there's always the question of... will the edit have
ramifications... and IF it likely will [not likely in this case] then it should
be spawned)". So the dots, the prompt ruling and the box work were the
orchestrator's only where the edit was small and its blast radius could be
named — and the two that *were* spawned (the image accounting, the box's keys)
came back with branches, commits and tests of their own, which is the shape the
ruling is for.

**Census** at this landing (`scripts/census.py`), against §8.41's (total 57,994 ·
prod 14,459 · tests 24,963 · comments 14,980): total 60,841 · **prod 14,770** ·
tests 26,447 · comments 15,856 — 2,847 lines: 311 production, 1,484 test, 876
comment, 176 blank. Half the wave is tests on purpose: the ruling was "enough
tests to make sure we're handling images the most robust way possible", and the
three defects above were found by exactly those tests rather than by the feature.

---

## 8.43 The paste roads, the trimmer's trigger, and the foot that said nothing

The human ran what §8.42 had just landed and found three defects inside the hour,
then ruled on a fourth design. Two of the three were in code this file had
written that day; the third had been there since images existed.

**A picture pasted by path could not survive a restart (`3b37d38`; H38).** They
sent two test pastes in a row — a 724 KB screenshot, then a 170 KB crop — both
by *path*, both from `/home/rubend/screens/`, which is outside the workspace. The
bytes reached the model (that was §8.42's fix working), but the placeholder the
session file leaves behind names the human's path, and `Workspace::resolve`
refuses absolute paths by design: after a restart the model cannot read it, a
shell cannot carry an image back, and the line's "read the file again" is a road
with a wall across it. The second paste also showed the model could not even
*measure* the file it had just been handed, which is how the wrinkle was found.
The ruling — "we do need pastes from ANY location to work after restarts" — is
now a rule with one home: a picture whose file resolves outside the root is
copied into `.mush/paste/` as it attaches (the same writer, naming rule
`pasted-<millis>.<ext>` and 2 MB cap the clipboard road already used), and the
copy is the path the `Image` carries. The human's privilege to name any path does
not extend to the model's tools; what mush promises to keep is the copy, and the
original is read, never moved. The test is the restart road itself: attach →
`Session::save` (payload shed to the placeholder) → `Session::load` → the path
parsed out of the placeholder line resolves and reads back byte-identical, with
its pixels. Every `Image` mush can hand a model now carries a path the model's
own tools resolve, which is an invariant the section above can state.

**Four paths in one paste attached none of them (`3e184a3`, `9dfd20e`; H39).**
"what about pasting MULTIPLE images it doesn't seems to be handled correctly...
example I'm pasting 4 here right now" — and the four paths arrived at the model as
*text*, because `pasted_name` accepts exactly one name and answers `None` at the
first bare space (that is how it tells a path from prose). Dragging four files out
of a file manager is one paste, so the rule generalises: `Workspace::pasted_images`
splits the paste into words — quoted spans, `\ ` escapes and `file://`'s `%20`s
kept whole, whitespace and newlines as the separators — and attaches them all, in
paste order, when **every** word names an image file. One non-image word (prose, a
directory, a missing path, a `..`) makes the whole paste text, exactly as one
path's typo did: no half-taken gesture, no hijacked paragraph. The batch says one
line (`attached 4 images — Enter sends them with the message`), asks the
model-level refusals once (nothing attaches, the words land in the box), and says
the room warning once with the count at stake. Three smaller facts came out of
the tests: four copies written in the same millisecond used to take the same
`pasted-<millis>` name and overwrite one another (so an attached picture could
point at another's bytes — `create_new` plus a `-2` suffix now, pinned by a
test); a whole list wrapped in *one* pair of quotes is one name under the old
quoted-paste rule and stays text (each path must be quoted separately); and a
paste that turns out to be text may have copied the outside names read before
the word that disqualified it — files in a gitignored directory, stated rather
than swept up. Nothing caps a batch: the room left, the window and the 2 MB
per-file cap are what bound it.

**The trimmer cut to the brim (`3dfcd86`, `13a0a68`, `71857d8`, `2b9c860`,
`ca5c26b`; H40).** The human's question about `trim_history` — "it just sounds
like a function that would invalidate ctx cache on every subsequent message...
making it downright bad actually" — was right about the shape if not the
frequency: the trim is a byte-identical no-op while the transcript fits, but it
cut back to *exactly* the budget, so a conversation sitting at the ceiling could
be cut again on the very next request, and every cut rewrites the front of the
prompt, the prefix an endpoint's cache had warmed. Their ruling — "why don't we
make trim go to 80% so then fold tries kicking in again at 90%" — needs its two
numbers read as a *pair*, which the first implementation got wrong and its own
measurement caught: "trim whenever it is over four fifths" parks the transcript
at the watermark, so it never reaches nine tenths, and five thousand quiet turns
folded **zero** times while the trimmer cut **3,932** times. The hysteresis shape
is the one that means anything — trigger at the ceiling, cut to four fifths — and
the same simulation then folds once (at turn 1,193, when the total crosses nine
tenths) and cuts zero times. So `trim_history` takes the window's budget, cuts
only when the transcript is over it, and stops at `trim_target` (4/5); the fold's
trigger stays `compaction_trigger` (9/10), and after a cut the conversation has a
tenth of room before the fold and three tenths before the window. The same
ruling deleted the trimmer's **image-payload-first** pass: shedding pictures
before turns was a workaround for the byte-priced image (H36), and with pictures
priced by their pixels it identifies nothing, so a picture goes with its turn
like any other words and `Message::drop_images` has exactly one caller left (the
session writer, which keeps `.mush/session.json` small). The attach gate's lines
were rewritten with it: a picture that does not fit the room now costs the
*oldest turns*, and the line says so and names `/compact`; a picture too big for
the window even with every older turn gone is the one case whose only road is a
downscale, because the endpoint would refuse the request outright.

**The foot names what the run is doing (`235c3bc`..`a185aa7`; U15).** "when we're
waiting on a tool call the screen still shows `working` VS when waiting for
`wait` which just shows `<gear> wait` I reckon that's an inconsistency" — the
foot was indeed the one surface that did not say what the run was doing: a
twenty-minute `cargo test` wore the same `working.` as a model call, and a run
parked in a `wait`, which the row beside it names, painted nothing at all (U7's
fix chose silence; it kept U7's point and cost the fact). `Phase::words` is now
one derivation (`thinking`, the actor's own tool label, `waiting on {noun}`, the
fold's sentence, `cancelling`, `None` at rest); the row builds on it plus the
age, and the foot paints it plus the dot beat, so the two cannot disagree — one
test asserts exactly that for the same node. The parked `wait` paints
`waiting on results.`, which supersedes U7's mechanism and keeps its point, and
`Phase::doing` answers `waiting` rather than `wait` so the roster, the quit
warning and the foot share one word. `Pane.busy` and `Pane.compacting` — both
read only by the foot line — collapsed into one `Pane.words`, which is what makes
the drift impossible rather than merely tested.

**Recorded, not changed.** A parked fold still reads `folding at the next step`
on the row, the foot and the bar while `Phase::label`/`doing` say `compacting`
for it — the same class as U15 one phase over, left because the label is the
attach protocol's contract. And the row's own activity no longer carries the `…`
that `summarize_args` puts on a truncated label (`run_command cargo …` paints as
`run_command cargo`), because the dots are now the only ellipsis on those rows;
if the mark is wanted back, `Phase::words` is the one place to put it.

**Census** at this landing (`scripts/census.py`), against §8.42's (total 60,841 ·
prod 14,770 · tests 26,447 · comments 15,856): total 62,372 · **prod 14,937** ·
tests 27,253 · comments 16,339 — 1,531 lines: 167 production, 806 test, 483
comment, 75 blank. `cargo test` 652 + 189 (four ignored), clippy and fmt clean.

---

## 8.44 The image road, audited: a reader that opens nothing first, and a gate that spoke for another agent (`c0f8973`..`00096c6`, `700fbdc`, `3a7cca0`..`edac88f`, `c78695b`)

A blind audit of this file's own image road — ten findings, A1 to A10, read from
the source rather than from a live run — became three branches and three
parallel children: the reader's edges (`mush/15`, merged `700fbdc`), the attach
gate with its carry and its bounds (`mush/16`, merged `c78695b`), and the pane's
`▣` row (`5e74f8d`, merged `b6eaf91`). §8.42 and §8.43 built the road and
measured it through the human's own pastes; what the audit found is what the road
did while nobody was looking — it opened a path to ask what it was, read a file
whole to learn it was too big, and took a reader killed at the deadline for an
empty clipboard. H41 and H42 are its rows.

**The reader looked at a path by opening it (`c0f8973`; H41).** `image_at` — the
one reader behind the human's paste and the model's `read_file` — did
`File::open`, sniffed a sixteen-byte head, then read the file *whole* before
comparing its length to `IMAGE_FILE_CAP`. Two facts followed from that order. A
FIFO named like an image **blocked the open** until a writer appeared, and this
runs on the UI thread (the human's paste) and in an actor (the model's read): a
probe was still blocked after three seconds, so a paste of `x.png` could freeze
a pane or park an agent forever. And an over-cap file was read to its end to say
what the stat already said — a 128 MiB sparse png drove the probe's peak RSS
from 3,172 kB to 134,136 kB (after: 3,224 kB → 3,228 kB, still naming the size
the stat saw). The order is now: `fs::metadata` first, and only a regular file
can be an image, so anything else is `Ok(None)` **before any open**; the head is
then sniffed, and an over-cap *non-image* is still text, because the head
decides image-or-text and the cap must not turn prose into a refusal; only an
image is weighed against the cap, from the metadata length, before any whole
read; the read itself is bounded to `IMAGE_FILE_CAP + 1` bytes with `take`,
because a file can grow between the stat and the read; and a read that hits the
bound is refused **without a size named** — the length in hand is the buffer's,
not the picture's, which is why `image_too_big` takes an `Option<u64>` and the
exact-size sentence stays for the sizes that are known. The same `fs::read`
waited one call later on the model's text road, so `read_file` carries the same
regular-file guard: fixing only the image half would have left the actor parked.
`00096c6` then pinned the corners the audit named as untested — `image_mime`'s
floor (an empty slice, `RIFF`, `GIF87`, a bare png signature: no signature, no
panic), a real over-cap image refused with the stat's exact number, a sparse
32 MiB log refused by `read_window`'s whole-read cap before anything is read,
and the `None`-size sentence itself. That last pin is the one the growth race
cannot earn on its own: the stat and the read are one function, so the race
cannot be staged, and the sentence is pinned directly where it lives.

**The clipboard lied twice about the same buffer (`827ad1a`, `ee4db8b`; H41).**
The reader's stdout is drained to `READ_CAP = IMAGE_FILE_CAP + 1` bytes so a
runaway stays bounded — and `drain` said nothing about having dropped the rest,
so a 4,194,314-byte png arrived at `save_pasted_image` as a 2,097,153-byte
buffer and the refusal read the buffer's length aloud as the human's picture: "a
png of 2097153 bytes". The picture could be of any size; the number was always
the cap's. `Drained { bytes, filled }` now carries the truncation,
`save_pasted_image` takes the caller's own fact (`cut_at_the_cap`), and a
picture cut at the cap is refused as past the cap **with its true size unknown**
— one sentence sharing every word but one clause with the whole-picture sentence
(`clipboard_image_too_big`, another `Option<u64>`) — while a whole picture keeps
its exact number. The other lie was `Answer::Nothing`, which covered three
different facts — no such type on the clipboard, a reader that exited non-zero,
and a reader killed at the shared 2 s `DEADLINE` — and `read_image` turned all
three into `Ok(None)`, so the human read "the clipboard holds no image" when the
picture may have been on the clipboard and only the reader was stuck (a probe:
`sh -c 'sleep 30'`, killed at the deadline, answered `Nothing`).
`Answer::TimedOut` is now its own answer with its own sentence — it names the
program, the 2 s wait, and the road that always works (save the picture to a file
and paste its path) — while a failed reader keeps "no image", whose prose now
says why: it can serve no picture either way, and the human's move is the one an
empty clipboard asks for.

**A path with a newline put a line of its own into the model's view
(`73bfc14`; H41).** `Message::drop_images`'s `placeholder` formatted
`image.path` raw into the one-line `[image: …]` stand-in, and a newline is legal
in a Linux path: a dropped image whose path was `shots/a\nb.png` came back as
three lines where the placeholder's whole shape is one — a line the model could
read as a message of its own, in the transcript the model is the reader of. The
path now goes through the repo's one home for untrusted text (`text::sanitize`,
which removes escape sequences and control characters whole), and the one break
a sanitized path can still hold is escaped as the two characters `\n` rather
than dropped, so the path stays nameable again; a lone `\r` sanitize already
marks as `␍`. Two tests pin it: the newline stays one line, and a path holding
an escape sequence leaves no command behind.

**The attach gate spoke for the wrong agent (`3a7cca0`, `bf9b897`, `9b29e80`,
`7c8bd98`, `edac88f`; H42).** A picture attached while a **child** was focused
was resolved and copied by the *root's* workspace, and a child works in its own
worktree (`.mush/wt/<id>`), whose tools resolve the same relative path against
that worktree: the placeholder's "read the file again" named a file the child
could not read (`an_image_attached_to_a_child_is_readable_from_the_childs_own_workspace`
failed on exactly that, and passes after). The room arithmetic was the same
mistake in numbers: `used_weight_for(AgentId::ROOT)` whichever agent was
focused, so a child's own budget was weighed as the root's — the test that
caught it had expected the warning and read the plain line `attached
shots/big.png (png · 1.3 MB)` instead. `App::agent_root(id)` is now the one
answer to where an agent's tools resolve — its worktree while it has one on
disk, else the shared checkout, the same rule `agent::revive` gives the actor —
and `attach_worktree` reads it too rather than repeating it (`bf9b897`).
`App::carry_images(id, images)` re-copies the picture into *that* agent's
`.mush/paste/`, through the one writer (`save_pasted_image`), unless the
receiving workspace reads the bytes back identically — bytes, not the path's
existence, because the placeholder promises the *same* picture and a worktree
can hold an older commit. Both doors carry for the focused agent, and `deliver`
asks again for the agent that actually receives the message, because focus can
move between the attach and the `Enter`. `mush/15`'s signature change met
`mush/16`'s new call at the merge and did not compile until the call site
declared `false` for `cut_at_the_cap` (`9b29e80` — the carry holds whole bytes,
since an `Image` only exists when a road read the picture to its end within the
cap), and the paste writer's doc now names the carry as its third road
(`edac88f`). The carry also carried the audit's own defect for a moment: its
"does the receiver already hold this picture?" was a raw `fs::read`, which
re-spelled the open-before-stat hazard `mush/15` had just fixed — a FIFO named
like the picture would have blocked the UI thread at every attach and every
send. It now asks the receiving workspace's own reader (`read_image`), which
stats first and bounds its read by the transport cap whatever the file claims or
grows to; with the raw read restored, the new watchdog test fails after 5 s ("a
FIFO must answer, not hold an open until a writer appears: Timeout"). Six
existing fixtures that attached a hand-built `Image` whose path named no file
now write the file holding those bytes, which is what every real road already
left on disk.

**Nothing bounded a batch (`7ef2301`, `31cd9d0`; H42).** The gate's own prose
said "Nothing here caps the count", and `docs/mush.md` claimed the room and the
window bounded it — but the room is a warning, and a picture is priced by its
**pixels**, so a 2 MB file of a small png weighs almost nothing and passes every
token bound the window has. A paste of eight pictures at the transport's 2 MB
cap put 16 MB in the box and 22 MB into the request JSON. Two bounds now, asked
at both doors (the single attach and the batch paste) and both refusing —
attaching a request the endpoint will refuse only spends a turn discovering it.
The window's bound: the box's pending pictures, this arrival with them, may not
pass the whole history budget; a picture or batch that would is refused (the
batch says how many of how many are at stake, where the single picture's line
names the picture), and this *changed* the single road, which used to attach
with a `fail` line. The line's road is a downscale, and `/compact` is named
only as the thing it is not — a fold of the *conversation*; the single
picture's line names it when the pictures already in the box are part of the
sum, because the refused weight is `pending + cost` and no fold touches the box. The box's own bound, in bytes:
`BOX_IMAGE_BYTES = IMAGE_FILE_CAP × 8` (16.8 MB, eight of the files the
transport already caps one at), because tokens cannot be this bound — a 2 MB
file of a 100×100 png weighs fourteen tokens — and the box is bytes while the
pictures wait; the refusal names which bound it was. The room warning, where a
fold does help, keeps naming `/compact`, warns, and attaches: the human decides
what to send. The batch's room warning also stopped selling a mechanism that no
longer exists — "`trim_history` sheds an image's bytes before it drops a turn"
was true while a picture was priced by its bytes, and H40 deleted the pass; the
batch line now says what the single-picture line says, about them all
(`31cd9d0`). Measured with both bounds disabled in turn:
`a_picture_that_would_push_the_box_past_the_budget_is_refused` failed at "the
sum is past the budget: refused";
`a_batch_that_would_push_the_box_past_the_budget_attaches_nothing` at "the words
land as text" (the attach had swallowed the four paths, leaving `""` where the
paths should be); `a_picture_past_the_boxes_own_byte_bound_is_refused` and
`a_batch_past_the_boxes_own_byte_bound_attaches_nothing` at "past the box's
bound: refused"; all four pass with the bounds in.

**A tool result's picture had no row (`5e74f8d`, merged `b6eaf91`; H42).** The
dim `▣ path (format · size)` rows were painted inside the `"user"` arm of
`render_message` alone, so a picture the model read — `read_file` hands a png
back *inside the tool result* — left no row at all: the pane read exactly like a
turn where the model had not looked. One helper, `image_rows`, now serves the
user, assistant and tool arms, each before the trailing blank — one reading,
because the human's own attachment and the picture a model read are one fact
about a message, and a second copy of the loop would go its own way the first
time either changed. The row is the reading of the bytes a message still holds;
where they are gone (the session writer's `drop_images` leaves its placeholder
in the text instead), the placeholder is what reads. The pin,
`a_tool_result_carrying_a_picture_paints_its_row`, also pushes a
`Message::user_with_images("look", …)` beside the tool result and asserts the
user road is still painted.

**What this supersedes of §8.43.** Two sentences of §8.43 are history now and
are not rewritten there. "Nothing caps a batch: the room left, the window and
the 2 MB per-file cap are what bound it" — the room left warns, and the window
and the box's byte bound refuse, at both doors. "asks the model-level refusals
once …, and says the room warning once" — the batch now asks four facts once —
no model, a model that cannot see, the window's bound, the box's byte bound —
and two of them stop the whole gesture. And §8.43's closing sentence of H40 — "a
picture too big for the window even with every older turn gone is the one case
whose only road is a downscale, because the endpoint would refuse the request
outright" — still names the only road, but the gate refuses such a picture
*before* the wire now; the endpoint is no longer the wall it hits.

**Recorded, not changed.** Four things the wave saw and left. The growth race
between `image_at`'s stat and its bounded read cannot be staged — the two are
one function — so `image_too_big`'s no-size sentence is pinned directly instead
of by a test that drives the race. `read_window` (and `read_file` under it) now
stats twice per read: once for the whole-read cap and once inside `read_file`'s
own regular-file guard; one extra `fs::metadata` beside the read it guards is
cheaper than threading a `Metadata` through two functions so one caller authors
the other's decision. `Workspace::save_pasted_image` gained `cut_at_the_cap` — a
signature change with one in-tree caller (the carry, which passes `false`) — and
its doc says why the carry cannot be the cut side. And a refused attach may
already have carried a copy into the receiving agent's `.mush/paste/` before the
bound says no: the same shape §8.43 recorded for "a paste that turns out to be
text may have copied the outside names read before the word that disqualified
it" — the copy is gitignored scratch, and the order is deliberate: the weights
and the lines are about the pictures the box will hold, so the carry has to
happen before the bounds are read.

---

## 8.45 What goes on the wire, audited: the fifth a cut leaves, and the fold's own request (`e34e49a`..`991be92`, `mush/14`)

A second blind audit — the trimmer and the budget, T1 to T4 — found every bound
asked in the wrong place: the command cap was a fraction the trim does not
leave, a turn's results were bounded one at a time but not together, the request
itself was never weighed, the fold's reply cap was a constant no window touched,
and a request could replay pictures to a model the provider table calls blind.
The six fixes are one line (`e34e49a`..`991be92`, merged `48d6934`); H43 is their
row.

**`Config::cmd_cap` overshot the fold (`e34e49a`; H43).** The one cap on a tool
result's text was a quarter of the history budget, while `trim_history` stops at
four fifths of it: the fifth between that stopping point and the ceiling is the
room a result may take, and a quarter overshoots it by a twentieth, so a
full-size result landing on a just-cut transcript pushed the next request back
over the ceiling and the next turn cut again — one cut a turn, the prompt's
front rewritten every turn, the shape §8.43 measured and rejected. Measured at
the 8k default through the real functions: a transcript the trim had cut to
9,593 bytes plus a full-size result (a 3,076-byte tool message at the old cap of
3,072) landed at 12,669 against the 12,288-byte budget — over the ceiling and
too big to fold; with the cap at `budget − trim_target(budget)` (2,458) the same
cut lands at 12,055: inside the ceiling, past nine tenths, which is the fold's
road. The cap is now that relation and not a number of its own — expressed with
`transcript::trim_target`, so the two cannot drift apart — with the 512-byte
floor and `CMD_CAP` untouched. A prose correction came with it: the last tenth
before the ceiling is not "what the next growth crosses" — a turn's growth is
the whole fifth the cap bounds, and the fold's trigger sits inside that room.
`the_window_and_the_caps_scale_together` had pinned the quarter and is rewritten
to the relation; `a_full_result_after_a_cut_lands_on_the_ceiling` is the new
pin, over 2k/8k/24k/40k/120k windows plus the measured 8k numbers spelled out.

**One turn's results, together, were unbounded (`152ad7a`; H43).** `cmd_cap`
bounds one result, but a `run_command` batch is unbounded in count: four results
each answering to the cap add four fifths of the budget to a transcript with a
fifth of room, and nothing capped their sum. Measured through `run_loop` on the
shape the audit used — a first turn, so one user line and no older turn a trim
can drop, with the real 3,247-byte system prompt, under the 8k default — one
turn asking four `run_command`s left the next request carrying 13,768 bytes
against a 12,288-byte budget: no cut, no note and no fold, because a transcript
with one user line has no older turn to drop and a transcript over the budget
cannot fold. `ActorState::turn_room` is that sum's bound: set to
`budget − trim_target` when a batch starts, spent by each result's own weight as
it is stored, and `None` outside a batch; `result_cap` now answers
`min(Config::cmd_cap, what is left)`, so the first result takes its share of the
fifth and every later one is answered with what remains — mush's own cut note,
never output the cap would have let through. The content mush stores is what is
bounded, in the one place each tool already reads its cap; no history is
rewritten after the fact, and a call is still answered whatever the room is, so
the batch keeps the shape a strict server validates. The pin,
`one_turns_results_share_the_room_under_the_ceiling`, asserts the request
carrying the four results fits the budget, that the first result took the room,
and that every later one says `truncated at 0 bytes`.

**Nothing checked the request against the window (`d021dad`; H43).**
`trim_history` runs once per turn and cannot cut two shapes — a transcript with
fewer than three user lines, and the newest turn itself — and nothing after it
compared the request to the window, so an over-window request went out and the
endpoint answered a 400 with the money already spent. Measured through
`run_loop` at the 8k default with the invariant check disabled: one 2,560×1,440
png (3,686,400 px at 750 px/token) made a first turn's request weigh 18,041
bytes against the 12,288-byte budget, and a restored transcript whose newest turn
holds one unbounded result sent 23,318 bytes against the same budget — with no
cut, no note and no fold. The invariant now lives where the request is assembled,
on the messages that go out (a wrap-up instruction included): over the budget,
one road stays open first — the newest turn's **own tool results**, mush's bytes
and not the human's, go largest-first until the request fits, each replaced by
`SHED_RESULT_NOTE` (the call is not lost; the same output is one narrower call
away), with one `Notice` counting them for the human, and never a picture,
because a picture goes with its turn. A shape that still does not fit is refused: one line naming
what does not fit and the three roads that change it — downscale an attached
picture, `/compact` the conversation, or read less — with no model call, and the
actor stays alive with the transcript intact, so the next message meets whatever
the human changed. The tests drive every shape a trim cannot cut and assert the
invariant on each request that reached the model:
`no_request_the_trim_cannot_cut_goes_over_the_window` (the audit's blind spot —
no test compared a request to the window at all),
`the_window_takes_back_the_newest_turns_results_and_says_so` (the shed stops as
soon as it fits, a kept result is whole, and the human is told),
`a_picture_the_window_cannot_hold_is_refused_before_the_wire`, and
`a_transcript_that_cannot_fit_is_refused_with_one_line`; all four fail with the
check disabled.

**The fold's own request never fit a small window (`b76dbc8`; H43).** The fold's
reply cap was the constant `COMPACT_REPLY_TOKENS` (10,240), which no window
touched, and neither the automatic arm nor an asked `/compact` compared its
request to the window at all. Measured with the fit test disabled: at a
16,000-token window a fold at the trigger carried a 7,355-token prompt (the
history, the instruction, and the 2,000-token schema bound) and asked for
10,240 more — 19,595 tokens against 16,000, which a strict endpoint refuses —
and an idle `/compact` over a 37,210-byte transcript sent a 40,833-byte request
(13,611 tokens) with the same cap: 25,851 tokens against an 8,192-token window,
one wasted request and a red line, while the automatic arm printed nothing and
retried the same unchanging shape every turn in silence. The cap is now a
function of the window: `compaction_reply_cap` asks for the summary's ceiling
and no more than the window leaves under the whole prompt — the tool schemas
that head it, then the history and the instruction — floored at the same 1,024
tokens `reply_cap` is floored at, and `fold_request_fits` weighs the whole
request (schemas + prompt + cap). A fold that does not fit is not attempted: no
wire call, one line naming the three parts and the roads (`/context N`, Ctrl-N),
said once per state on the automatic arm — the same transcript retried every
turn is not news — and every time a human typed `/compact`, because a human
typed a command; the transcript is left untouched, because a refused fold is not
a trim. At the 4,000-token window the pin is 5,009 against 4,000 (1,985 of
history and instruction + 2,000 schemas + 1,024 summary). `needs_compaction`'s
inclusive upper bound, documented as inclusive and pinned by nothing, is pinned
in `a_transcript_past_the_whole_budget_does_not_fold`, and its prose now says
the whole-request fit is the caller's test — where the old sentence ("only fire
while it still fits; beyond that, trimming stays the last resort") said a fact
about the history where the fact is about the request.

**A request could replay pictures to a model the table calls blind
(`1e8e4f5`; H43).** `vision_capable` was asked at the attach gate, at the deliver
gate and by `read_file` — everywhere an image *enters* a transcript — and
nowhere a request *replays* one. A mid-run `Ctrl-P` points a conversation that
already holds pictures at a model the table says is blind, and the request built
for it carried the `image_url` parts anyway; the fold's request, built from the
same transcript, did too. Measured with the gate disabled: requests carried
`[0, 1]` image parts — one in the human's turn, one in the fold's prompt. The
gate is now asked where the request's parts are assembled: `for_the_model`
returns the transcript the actor holds, or — for a model `vision_capable` says
cannot see — a copy whose image parts are shed by `Message::drop_images`, so each
message's own placeholder stands where its pictures were, and one line tells the
human what was dropped and that `/model` is the road. The actor's transcript
keeps the picture whole (a picture goes with the turn it arrived in, and only
the request is a copy), the window invariant and the fold's fit test now measure
what actually goes out, and `read_file`'s own refusal for a blind model stays as
the honest earlier answer rather than the thing standing between an image and a
rejected request. Both tests fail with the gate disabled:
`a_blind_model_is_never_sent_an_image_part` and
`the_folds_request_is_stripped_too_for_a_blind_model`.

**The reserve's identity was said flatly (`991be92`).** Prose only: `history_budget`'s
doc claimed `history + schemas + reply + margin == window` flatly — true while
the reserve is those three numbers, false under the reserve's own half-window
cap, which binds below ~18.7k tokens (the 8k default's reserve is 4,096, half the
window). The sentence now says which side of the cap it is on and where the cap
is, instead of asserting an identity the small windows do not have.

**Recorded, not changed.** The wave touched no number §8.43's ruling set:
`trim_target` (four fifths) and `compaction_trigger` (nine tenths) keep their
pair, and `COMPACT_REPLY_TOKENS` stays the summary's ceiling — a window clamps it
now but does not replace it, and the constant's own number is the human's. The
automatic arm's refused fold is still said once per state (the same unchanging
transcript retried every turn is not news), while a typed `/compact` is answered
every time, because a human typed a command. And a refused fold leaves the
transcript exactly as it was: a fold that does not fit is not a trim.

**Census** at this landing (`scripts/census.py`), against §8.43's (total 62,372 ·
prod 14,937 · tests 27,253 · comments 16,339): total 64,807 · **prod 15,279** ·
tests 28,526 · comments 17,066 — 2,435 lines: 342 production, 1,273 test, 727
comment, 93 blank. `cargo test --workspace`: 674 + 4 ignored in the mush bin,
199 in mush-core; clippy and fmt clean.

---

## 8.46 The number the human reads, audited: a sentence nobody forwarded, a prompt that was every agent's, and a mark the run could not reach (`70d1d9c`..`9738a37`, merged `1608de9`)

The blind audit whose trimmer-and-budget findings became §8.45 had one left
(T5): the number the human reads and the number the run acts on were not always
the same number, and one sentence the model was given had never been said to the
human at all. Six commits landed it — three fixes, one dump, and two that are
prose and test clarity (`b435331`, `556498e`) — merged by `1608de9`; H44 is
their row.

**The sentence a cut gives the model reached the pane and nobody else
(`70d1d9c`; H44).** `trim_history` inserted `DROPPED_TURNS_NOTE` into the
actor's own message list, and `AgentEvent::Message` is the only road a line
takes into the UI's copy — nothing emitted it. So after any cut the pane showed
a transcript the model was never sent, `.mush/session.json` stored one, and the
number the human reads (`Chat::used_weight_for`, which the meter and the attach
gate both read) was short of the request by the note: 205 B, 4 of `user` plus
201 of text, 68 tokens. Now the call that first adds the note returns it (`Some`
once, `None` on every later call, `#[must_use]`) and the actor emits it as a
`Message`; the UI appends what it is told, and `place_dropped_note` — called
from `adopted` — moves a carried note back to its place (after the opening task,
before the oldest turn kept) instead of leaving it where the append put it,
because a note after the newest message reads as the newest thing said rather
than as a statement about the front of the transcript. `DROPPED_TURNS_NOTE` and
`is_dropped_note` are public, and the pane paints the line in mush's own voice
(`· …`), not the human's. Pinned by `a_trim_emits_the_note_the_model_was_given`
(the request opens with the note at index 2 and exactly one `Message` event
carries it; with the emit removed it was the bin suite's only failure),
`a_trimmed_request_says_the_oldest_turns_were_dropped` (the return: `Some` on
the call that adds the note, `None` after),
`a_note_that_came_back_is_put_in_its_place`,
`place_dropped_note_puts_it_after_the_opening_task`,
`the_dropped_turns_note_reads_as_mushs_line` (the pane's `· …`) and
`the_dropped_turns_note_reaches_the_pane_and_the_session` (the pane and
`session_snapshot` both hold it, so a restart resumes with it).

**A child was weighed with the root's prompt (`38f07cc`; H44).**
`Chat::used_weight_for` added `self.system` — the *root's* system prompt — for
every agent id: right for the root, whose actor is handed the conversation's own
prompt with every run, and wrong for a child, whose history never carries it.
Measured on a probe root: the root's prompt is 3,260 B, a depth-1 isolated
delegating child's 3,139 B, a depth-1 shared child's 2,912 B and a depth-2/3
leaf's 1,647 B — so a focused leaf's meter, and the attach gate's room,
over-reported by 1,613 B ≈ 537 tokens. The audit's own numbers (3,247 / 3,126 /
1,634) no longer reproduce byte-for-byte, because the prompts have moved since;
the 1,613 B over-report is the one it no longer makes. The actor now publishes
the prompt its history opens with: `start` emits `AgentEvent::SystemPrompt`
before the thread runs, `Chat::systems`/`learn_system` keep it, and
`used_weight_for` reads `system_for(id)`. An agent whose actor has not said yet
weighs no prompt — the number is the actor's fact, not a rebuild in the UI: a
child's prompt names the workspace its own tools resolve paths in, its depth and
whether it is isolated, all decided where the child is built. Pinned by
`an_agents_weight_is_its_own_prompt_plus_its_transcript` (for the root, the
conversation the next run hands the actor; for a child, its published prompt
plus its transcript — the audit's blind spot was that a 4× prompt for every
non-root agent passed all five meter tests) and
`a_child_publishes_the_prompt_its_own_history_opens_with`;
`a_restored_agent_comes_back_at_rest` was updated because a restored agent now
says exactly one thing at startup — its own prompt.

**The meter marked a line the run does not cut at (`6e3ba79`; H44).**
`App::context_meter` compared the used weight to `cfg().context_tokens`, while
every decision — the trim, the fold, the refused request, the attach gate's room
— uses `history_budget()`. On the 8 K default the budget is 12,288 B = 4,096
tokens and the fold fires at 11,059 B = 3,686, so the fold happened when the
meter read `3.7k/8.2k` = 45 %, and `full`/`over` needed 8,192 tokens = 24,576 B
— past the point the trimmer cuts, so the marks were unreachable in normal
operation and the human had no way to see a fold or a cut coming. The line is
now `ctx {used}/{budget}{ full| over} (fold {trigger}) {~}{window}`: what the
conversation weighs against the history budget, the fold's own trigger beside it
(from `transcript::compaction_trigger`, the number the run compares) and the
window last with the `~` that says it is the assumed one. `full` is at the
budget and `over` one byte past it, compared on the weights — the unit the
budget is stated in, so the boundary is the byte the trimmer cuts on and not a
floor-divided token; on the default the whole line reads
`ctx 1.1k/4.1k (fold 3.7k) ~8.2k`. Pinned by
`the_context_meter_says_full_and_over_at_the_budget` (at the budget `full`, one
byte past `over`, and that state is still well inside the window — the old
comparison read it as ordinary; with the comparison put back on the window the
test is the bin suite's only failure) and
`the_context_meter_shows_the_budget_the_fold_and_the_window`; the two token
spellings (`App::context_used_tokens`, `Chat::used_tokens_for`) are test-only
now, and `screen.rs`'s facts-line example carries the real spelling.

**`--print-config` prints the reserve's other numbers (`9738a37`; H44).** The
manual's words are that the dump prints what the constants resolve to for the
window in front of you; it printed the reply cap but not the tool schemas every
request reserves nor the history budget those leave — the two numbers a human
comparing windows (or reading a `cannot fold` line) had to re-derive by hand.
Now `schemas` is `config::SCHEMA_TOKENS` (the constant `request_reserve` sums)
and `history budget` is `Config::history_budget()` (the function the trimmer is
handed), in the budget's own bytes and the tokens they divide into; on the
shipped DeepSeek preset that reads `schemas  2000 tokens` and
`history budget  1291500 bytes (430500 tokens)`. The name column widened 13 →
15, because `history budget` is fourteen characters and a name that overflows
its padding runs into its value (`history budget1291500 bytes`), and `--help`'s
enumeration names the two rows, so it is not a list of ten facts with eleven in
the output. Pinned by `describe_reports_the_request_not_the_wishes`: the schema
row is the constant, and the budget row is `history_budget()` and its
bytes/tokens division.

**What this supersedes.** Two sentences of the record are history now and are
not rewritten there. §8.42's parenthetical — the room left is
`history_budget() − used_weight_for`, "the meter's own sum, split out so the two
cannot drift" — was half right: the sum *was* one, but it added the root's
prompt for every agent, so the meter and the attach gate could not drift apart
from each other while both were wrong for a child; the sum stays one and is now
the agent's own (`system_for(id)`). And §8.29's item-10 paragraph (`f34c4de`) —
"It cannot accumulate in `session.json`: the vec is the actor's working copy,
while the stored copy is the UI's, fed only by emitted events" — is now the
opposite of true, as is the `DROPPED_TURNS_NOTE` doc it came from: the note is
emitted, so it is in the UI's copy and in the stored session, and its 205 B is
back in every number the meter and the attach gate read.

**Recorded, not changed.** A one-off `lock::tests::*` failure was seen on a
stale `/tmp` lock left by a terminated earlier run; it passed in isolation and
on re-run, the suite's known wall-clock shape (H28), and this wave touched no
lock. And the meter's line is longer — the budget's and the fold's segments are
about twenty columns — so where it is painted is worth naming: the bar's second
row (`facts_line`, from 24 rows up), where `model @ endpoint · meter` is one
cell of a line elided whole from the right, with the `⌂` cell as its floor. The
change does not crowd a narrow terminal — a cell goes whole, never cut
mid-number — but it moves the width at which that cell is given up: the
manual's own example line (`⌂ ~/p/mush │ …`) fitted 80 columns with the old
meter and no longer does, so at the ubiquitous 80×24 the model and the meter
are dropped where they used to read. The repository cells survive, which is what
the elide order is for and what `the_facts_line_survives_at_80x24` pins.

**Census** at this landing (`scripts/census.py`), against §8.45's (total 64,807 ·
prod 15,279 · tests 28,526 · comments 17,066): total 65,378 · **prod 15,334** ·
tests 28,804 · comments 17,281 — 571 lines: 55 production, 278 test, 215
comment, 23 blank. `cargo test --workspace`: 680 + 4 ignored in the mush bin,
201 in mush-core; clippy and fmt clean.

---

## 8.47 Nothing counts turns: the runaway guard goes entirely (`7141353`..`da6a59b`)

> "i just want to remove the hard cap on tool calls I believe its 200 maybe 800
> or 1000 is more reasonable / or just remove the hard cap mechanic completely"

The human's ruling was the second half — remove the mechanic, don't raise the
number — and it was measured before it was made. `RUNAWAY_TURNS = 200` had
fired on real work: a subagent's read-only docs scan in this very workspace was
cut off with `stopped after 200 turns without finishing (runaway guard)`. The
constant's own doc called 200 "past any real task"; the run it truncated was a
real one, and any fixed number has the same failure mode, so 800 or 1000 would
only move the truncation. Four commits landed it: the removal and its pin
(`7141353`, `da6a59b`), the hand-driven script (`8398525`) and the living docs
(`b1f1572`); H45 is their row.

**What was removed.** `RUNAWAY_TURNS` and its doc; the run loop's
`for turn in 0..RUNAWAY_TURNS` is a `loop` (`turn` was read by nothing else);
the `wrap_up` flag and its `Notice` ("runaway guard reached (200 turns) —
asking the model to wrap up"); the request shape that appended
`WRAP_UP_INSTRUCTION` to a copy of the transcript and sent `tool_choice:
"none"`; the arm that answered the model's tool calls with `the run hit its
200-turn runaway guard; tools are no longer available` and returned either the
summary or the bare `stopped after 200 turns without finishing (runaway
guard)`; the trailing `Err(…)` after the loop; and `WRAP_UP_INSTRUCTION`.
`request()`'s `tool_choice` argument went with them: both remaining callers —
the run and the fold — passed `"auto"`, and the wrap-up was the only thing that
ever passed `"none"`, so the knob had one setting left. Two prose sites outside
`agent.rs` carried the same mechanic and are re-pointed at the stop that
remains: `app/mod.rs`'s `Error` arm said "a guard-stop is this same event (the
runaway guard's `stopped after N turns…`)", and
`a_failure_reaches_the_bar_like_a_stop_does` used the guard's string as its
example failure — both now use the loop stop, the run-error that still exists.
`scripts/mock_llm.py`'s `TURNS` scenario waited for the wrap-up instruction's
phrase to answer with a summary; it now repeats the *same* `run_command`, so
the run it drives is ended by the loop guard — checked on a real pty: six
identical `run_command true` rounds, then `! error: this call was not run — the
run was stopped as a loop` and the guard's notice in the pane.

**What bounds a run now.** The model's own stop: a reply with no tool calls is
the run finished and its text is the result — the prompt's own promise ("a
subagent runs until it stops calling tools, so a brief is bounded by the work,
not a turn count") now has nothing behind it. `LOOP_ROUNDS` still ends a run
early when it stops making progress: the same batch five rounds over with
nothing changed in between, with `state.loop_stop` and the H14 resume road
untouched, and with the two rounds that did nothing — a refusal before anything
ran (H13), and a `wait` that slept — still exempt. The human's Stop (`Ctrl-C`,
`Ctrl-X`) is the only outer bound. The context budget does not end a run: the
fold rewrites history into a summary, `trim_history` drops the oldest turns and
the newest turn's own results are shed to fit, and the run carries on — which
is why the ceiling had been the only outer bound, and why the human is now that
bound. The one refusal the budget can still produce is a request that does not fit
after all of that (`over_window_line`), and it names the shape the human
changes; the other ends are the run's own error arms — a reply cut off at the
token cap `TRUNCATION_ROUNDS` times in a row, a refused or unreadable answer, a
dead endpoint — none of them a count of turns.

**The test that pins it.**
`a_run_past_200_turns_ends_when_the_model_stops_calling_tools`: 220 turns of
real `write_file` work, the content differing each turn so the loop guard never
trips, then a scripted reply. The run ends as `Done` with that reply as its
result, 221 requests, no notice containing "runaway", "wrap" or "turns", the
last request's schemas and `tool_choice: "auto"` identical to the first's, no
request carrying a guard instruction, and `notes.txt` holding the last turn's
write. The probe, with `for _turn in 0..200` and a trailing guard error patched
back in, fails on the fact it exists for —
`errors: ["stopped after 200 turns without finishing (runaway guard)"]` — in
0.16 s. Its first shape used `run_command`, one shell a turn, and under a
loaded machine (load average 15 on 14 cores) 220 spawns outran its 5 s wait:
the proof is the request count, not what the turn asks for, so `da6a59b` moved
the turn's work to `write_file`. The suite's totals are unchanged (one test
removed, one added): 680 + 4 ignored in the mush bin, 202 in mush-core.

**The honest cost.** A model that keeps making *different* pointless calls has
nothing inside the run to stop it: it runs until the human stops it, where a
count stopped it before. That is the trade the human chose, and the loss it
buys away is the worse one — a real task truncated at 200 turns is a silent,
certain loss the human reads as a failure, while a model that will not stop is
visible (the row's activity, the pane, the meter's growth) and has a key. The
loop guard still ends the shape that is pointless in itself.

**What this supersedes.** N1's row in `docs/refactor.md` §11 was fixed by
`RUNAWAY_TURNS` + `LOOP_ROUNDS`; the ceiling half is gone and the row says so.
The wrap-up's finding is **obsolete, not violated**: N1's point was that a long
task must not end as a bare `stopped after N turns`, and there is no longer any
bare `stopped after N turns` to soften — the only `stopped after` left is a
job's own line (`jobs.rs`'s `#c2 stopped after 4s`). S8(iv) — the `TURNS`
scenario waiting for "runaway guard", which `f70374f` had re-pointed at the
wrap-up instruction — is superseded by the scenario's second re-pointing, at
the loop guard. §8.11's "deliberately left" clause ("`RUNAWAY_TURNS`'s wrap-up
turn explains itself when it fires") has no turn to explain; §8.27's "Left
standing, on purpose" pair — the prompt's "runs until it stops calling tools"
beside a code ceiling — is settled by removing the ceiling, not by rewording
the prompt; and `docs/refactor.md`'s Stage 2.2 list ("a wrap-up summary rather
than a bare failure") and its "turn-limit scenario" now name the long-run pin.

**Recorded, not changed.** `crates/mush/src/app/chat.rs`'s notice fixture
`"the lexer subagent hit its turn limit"` is a hand-written line for the
notice-scoping test (`a_notice_belongs_to_one_agent_only`), not the guard's
error string and no longer a message the code produces; the test asserts
scoping, so the fixture stays. And `TRUNCATION_ROUNDS` still ends a run early
on consecutive cut-off replies — a count of replies the endpoint truncated, not
of turns.

**Census** at this landing (`scripts/census.py`), against §8.46's (total 65,378
· prod 15,334 · tests 28,804 · comments 17,281 at `1608de9`) and measured at
the base this branch forked from (`5f24f95`: total 65,419 · prod 15,334 · tests
28,833 · comments 17,290): total 65,352 · **prod 15,280** · tests 28,837 ·
comments 17,276 — this wave is 67 lines fewer: 54 production, 14 comment and 3
blank lines gone, 4 test lines more (one test replaced by one). `cargo test
--workspace`: 680 + 4 ignored in the mush bin, 202 in mush-core; clippy and fmt
clean.

---

## 8.48 A write is not a result: the content cap goes (`c69e4d8`)

> "Can we also remove the 16000 byte cap for writes... its seems odd that we
> accept the bytes from the request/llm but refuse to write it... :/"

The human's ruling was the removal. `write_tool` refused a `write_file` whose
`content` was longer than `result_cap(actor, state)` — on a big window the
fixed `CMD_CAP = 16_000` bytes — with one line: "content is N bytes — over the
cap on one write; write the first part, then extend it with edit_file". It was
the only place in the tree where `result_cap` bounded an *input*: every other
caller — `read_window`, a command's output, a listing, a search — bounds what
the model reads back, which is the right shape for a result cap. A write's
content is not a result: the bytes already travelled in the tool call, so they
are in the transcript and in the request for that same turn, and the check
saved the conversation nothing while costing a turn and the model's work. The
check had no written rationale anywhere — the tool's doc said only "the answer
is one line naming what changed", the schema said only "The complete new
content", and no prompt line promised a size limit on a write — so the doc
comment that replaces it is the first written account of what bounds a write.

**The site.** `write_tool` loses the `content.len() > cap` check and the
`state` parameter it only existed for (`exec_tool`'s arm is
`write_tool(actor, args)` now, as `edit_tool`'s is), and with them the
refusal sentence and its "write the first part" road into `edit_file`. The two
tests that pinned the refusal — `write_file_creates_and_says_what_it_replaced`'s
`cap + 1` write and `write_file_does_not_call_a_replaced_blob_new`'s "write the
first part" assertion — lose it, and two new tests pin the new fact; no test
asserts a refusal that no longer exists. The parallel road was checked and had
none:
`edit_file`'s replacement text is read by `tools::edits_arg` and written with
no cap anywhere, and `tools::arg_string` has no size bound, so this check was
the tree's only cap on a tool's input.

**What bounds a write now** (written into `write_tool`'s doc comment, because
the decision is being written for the first time):

- the tool's own **answer** is one line, so a write cannot inflate a result —
  which is what `result_cap` is for, and it stays for every real result road;
- the conversation's own invariant at the wire bounds the **request**: the
  assembled request is weighed before it is sent (`over_window_line` in
  `run_loop`'s assembly), and a write big enough to push the request past the
  window ends **that turn** with that line. The write ran first, so the bytes
  are on disk and the work is not lost; and the turn cannot be cut while it is
  the newest, so the window can be broken for a while but not for good — the
  next messages make the turn an older one, which the trim then sheds like any
  other;
- the human's Stop, as everywhere.

**The tests that pin it.** `a_write_past_the_old_cap_lands_whole`: one
`write_file` call carrying 64 KB (over 26 times the 2,458-byte `cmd_cap` the
test's 8k window gives, and more than four times the 16,000-byte ceiling any
bigger window keeps) answers "wrote big.txt — 1 line (new)" and leaves every
byte on disk.
`a_write_over_the_window_ends_the_turn_and_leaves_the_file`: through
`run_loop` at the 8k default's 12,288-byte budget, a scripted model calls
`write_file` with 40,000 bytes. The turn that asked goes out (one request), the
request carrying the call back is refused with "cannot send this request: the
transcript weighs … against the 12,288-byte budget", and `huge.txt` holds all
40,000 bytes — the honest consequence, pinned rather than assumed. The test
then measures the recovery instead of promising one: one more message still
refuses (the turn is no longer newest, but the trim has two user lines and the
shed road has nothing to take), the third user line is the trim's cutting
point, and the trimmed request goes out and is answered — with the oversized
call gone from both the request and the actor's transcript.

**Recorded, not changed.** No schema description or prompt line promised a
size limit on a write, so nothing in `prompt.rs` or `tools.rs` changed — and
`TRUNCATION_INSTRUCTION`'s "create a file with a heredoc … then extend it with
`edit_file`" stays: it is about a *reply* cut off at the token cap, not about a
write's size. `result_cap`'s doc ("Every big-text road uses it — a command's
output, a file read, a listing, a search") is still the whole set of result
roads, and `README.md`'s command-cap paragraph is still true of a command's
output. `over_window_line`'s three roads ("Downscale an attached picture,
`/compact` the conversation, or read less") do not name the road that actually
recovers a writer's case — the trim's own three-user-line rule — but that is
the pre-existing over-window shape (H43's picture refusal has it too), not
something this removal created; recorded here rather than reworded. The grep
for an older section or row that called the write cap a good thing found none,
so nothing is superseded by this section.

**Census** at this landing (`scripts/census.py`), against §8.47's (total
65,352 · prod 15,280 · tests 28,837 · comments 17,276; the base this branch
forked from, `09cea90`, holds the same tree as that landing's `b6b6d56`):
total 65,469 · **prod 15,272** · tests 28,915 · comments 17,316 — 117 lines
more: 8 production lines gone (the check and the `state` parameter), 40
comment lines more (the `write_tool` doc comment the removal owes, and the new
tests' own), 78 test lines more (two new tests), 7 blanks. `cargo test --workspace`: 682 + 4 ignored
in the mush bin, 202 in mush-core; clippy and fmt clean; both endpoint-free pty
scenarios (`--resize`, `--cancel`) pass.

---

## 8.49 A drag is a rectangle: zen, and a transcript copied as its own lines (`f954cfb`..`7bd7f9b`, merged `5e296b1`)

> "selection of text/paragraphs that are scoped to conversation output or to
> whatever pane is currently rendering (since now I try selecting and I get all
> the lines from agents pane along with whatever paragraph I want from the right
> conversation pane)"

The terminal's own selection is what the human was using, and the reason it
cannot be scoped is a rule mush had already decided: it never captures the mouse
(finding K3), so a drag is the terminal's rectangle of screen cells. At 80
columns the chat pane shares the frame with the tree, so a paragraph dragged out
of the conversation arrives with the agents pane's lines in front of it — and,
inside the pane, soft-wrapped at the terminal's width rather than at the line the
model wrote. Three commits answered it: the write half of the clipboard
(`f954cfb`), the zen view (`ccd9766`) and the select mode (`7bd7f9b`), merged by
`5e296b1`; H47 is their row.

**The clipboard could only be read (`f954cfb`).** `clipboard.rs` had the whole
read road — `wl-paste`, `xclip`, `pngpaste`, a 2 s `DEADLINE`, one `POLL` — and
no way at all to put text on the clipboard: the write half of the same machine
facility had no road, so a path, a message or a selection a human meant to paste
elsewhere had nowhere to go. `write_text` adds it in the readers' own shape: the
programs a human would use, tried in one order — wayland's `wl-copy`, X11's
`xclip -selection clipboard -i`, macOS's `pbcopy` — first success wins, with one
shared deadline for the whole sequence (three writers cannot each spend two
seconds of a frozen keyboard) and the bodies in `run_writers`, so a test hands
in a writer instead of installing one on the machine's `PATH`. The text goes in
exactly: multi-line, tabs, a trailing newline neither added nor lost. That is why
`wl-copy` gets no `-n`: on the write side `-n` is `--trim-newline`, the opposite
of the rule, while the reader's `--no-newline` is `wl-paste`'s flag against the
newline *it* appends. The write itself runs on a thread of its own — the mirror
of the readers' drain thread — because a program that stops reading fills the
pipe and a caller who wrote the text itself would block in `write`; the result
travels back over a channel, because an exit status cannot say whether the text
landed (a program can exit 0 with the pipe closed under it). A writer still
running at the deadline is killed and reaped and earns its own sentence
(`xclip did not take the text within 2s — it may be waiting on a clipboard owner
that never speaks`); a writer that exits without the text is passed over for the
next; and no writer at all is a third fact, naming what to install the way the
readers' sentence does. It landed `pub` and uncalled — measured, the binary
target reports five dead items without an attribute, the whole road hanging off
it — so it carried one `allow(dead_code)`, with the comment saying it leaves with
the key that will call it; that key is the select mode, the last commit of the
landing. Pinned by `the_text_arrives_byte_for_byte` (both trailing-newline cases,
the tabbed multi-line text), `the_first_writer_that_takes_the_text_wins`,
`a_writer_that_exits_without_taking_the_text_is_not_a_success` (1 MiB into
`sh -c 'exit 0'`: the write cannot land and the road falls through),
`a_text_no_writer_took_did_not_reach_the_clipboard`,
`no_writer_at_all_names_what_to_install`,
`a_writer_that_never_takes_the_text_is_killed_at_the_deadline` (1 MiB into a
`sh -c 'echo $$ > …; sleep 30'` at a 50 ms deadline: the caller returns in well
under a second, and on Linux the writer's own `$$` has no `/proc` entry — killed
and reaped, not a zombie) and
`the_writers_are_the_three_programs_in_the_documented_order`.

**Zen: the focused pane takes the screen (`ccd9766`).** `Ctrl-F` toggles a view
where the focused pane covers the frame. Chat focused, the chat takes the rows
above the bar whole and the agents pane is a zero rect, so nothing can paint in
it; agents focused, the message box and the bar keep their rows and the tree
takes everything above the box, leaving the chat's transcript no rows at all
(`ChatPane::transcript` is `None`). The layout is derived *after* the two-pane
one and hands the box the very rect that layout gave it, so the view moves the
frame and not the conversation; and because `Tab` already cycles the focus and
the layout reads it, `Tab` is what switches which pane is full-screen. It is a
view like `Ctrl-T`: not said, not stored — `dirty_screen` is the whole record —
and `Ctrl-N` leaves it as the human set it, because the view is the human's and
not the conversation's. A hidden pane's fact does not vanish with it: while the
agents pane is a zero rect, its `N working` / `N jobs` / `N waiting` clauses are
appended to the conversation pane's title, from the same `agent_count_cells` the
pane's own title is built from, dropped whole to the columns the pane has. The
`▲N`/`▼N` hidden-row counts stay behind — and, by the same rule, the chat's own
`+N more lines` goes with the transcript when the tree is the full-screen pane —
because they are arithmetic about a list this view does not paint. The key is one
row in `KEYS` (both help surfaces render it) and one arm in the app-wide `Ctrl-`
block, so it works from either pane. Pinned by
`zen_gives_the_focused_pane_the_two_panes_width_at_every_size` (the layout at
80×24 and 200×40 in both focuses: the focused pane's width, the box's and the
bar's rows, `assert_shape` over the frame),
`zen_keeps_the_agents_counts_in_the_conversation_panes_title` (at 80×24, where
the agents pane's own title had dropped `1 waiting`),
`zen_tabs_between_the_full_screen_panes`, `toggling_zen_back_restores_the_two_panes`,
`ctrl_f_toggles_zen_from_both_panes` (and across `Ctrl-N`) and `zen_title`'s
whole-clause elision.

**The select mode: a transcript is copied as its source lines (`7bd7f9b`).**
`Ctrl-Y` opens a modal cursor over the focused conversation's *source* lines —
the lines `Message::text()` has — and `Enter` copies them to the system
clipboard. The keys are the pane's own reading keys, moved onto the cursor:
`↑`/`↓` one source line, `Shift-↑`/`Shift-↓` the same step with the selection
kept (the anchor planted where the first extended step began), `PgUp`/`PgDn`
ten, `Home`/`End` the oldest and newest, `Enter` copies and leaves, `Esc` leaves
without copying. The mode is modal the way a picker is — `keys::key` asks the
caller for `Chat::selecting()` and routes the whole keyboard to `fn select`
before either pane — so a letter is not typing, `Esc` is not the box's clear,
and the mode is safe to open with a draft in the box. The app-wide `Ctrl-` block
is decided above it, so `Ctrl-Q`/`Ctrl-C`/`Ctrl-N` still work and `Ctrl-Y` is a
no-op while the mode is on; `Tab` (the pane cycle) leaves the mode rather than
being dropped behind it. A pane with no source line says so in the bar instead
of opening a cursor over nothing, and a fold that takes the cursor's line drops
the mode rather than leaving it over a line that is gone.

What lands on the clipboard is **`Message::text()`, exactly**: the selected
source lines joined with the `\n`s they have between them. A whole message is
byte for byte; a soft wrap at the pane's width never becomes a newline; a tab is
a tab; a tool result is copied *whole* even past the eight rows the pane paints
of it (the ninth row is the `…`, and a line the cap hides still has that row to
stand on); and a picture whose bytes the session file shed copies as its
placeholder, because the placeholder is what that message's text says. A selection
over two messages joins them at the lines it starts and ends on, and the line the
bar then says is built with the copy, because only there are the counts —
`copied 12 lines from #1's reply — 1,284 bytes`, with the thousands grouped, or
`your message` / `#N's tool result` / `N messages` for *what* was copied. The
pane's window follows the cursor, not the bottom: the mode carries the window's
top (a `Cell`, because only the frame knows the pane's measure) and the frame
places the cursor's line at the top when it is above the window, at the bottom
when below, with the transcript's own blank-trimming rule kept. `Enter`'s write
is subprocesses with a deadline, so it happens on a thread of its own and the
answer comes back as `Msg::Copied`, stamped with the conversation that asked for
it the way `Msg::Clipboard` is: a copy that outlives a `Ctrl-N` reports nothing
into the new chat, and the bar says the line only when the clipboard took the
text. Paint: the mode hands the frame *which* rows (`Painted.select`) and `ui.rs`
says what they wear — the selection is the theme's hue as a band behind the
text, `Color::Black` on the accent, patched onto the row's spans and never onto
the line's own style, so the cells past a line's text and a blank row inside a
selection stay the pane's background, and a dense selection reads as bands under
its lines; that is the same pair the bar's badge and the agents pane's selected
row wear, because mush has exactly one way of putting a colour behind text,
though that row is a place in the tree and this is a band under a range of
lines. The cursor is that band's inverse, the hue's characters on
`Color::Black`, span-only too, so a cursor-only row is a dark band rather than a
block and a row that is both reads as the cursor. `99d9305` gave the old mark's
"a dim result and a green reply stay themselves inside the band" up on purpose:
the hue is chosen for `Black` text on it, and a light band under the
transcript's own light ink is mud.

**The seam, and why it exists.** The write road is a value `App` holds —
`App::write_clipboard`, a `Fn(&str) -> Result<(), String>` defaulting to the real
`wl-copy`/`xclip`/`pbcopy` sequence — so the key can be pressed in a test without
writing the human's real clipboard or depending on which programs the machine
happens to have on `PATH`. The seam is not symmetry for its own sake: `Enter`
is a *key*, and a key mush can press is a key a test can press, while the read
road needs no such seam because its answer is a message a test hands in directly
(`Msg::Clipboard`). Pinned by
`enter_in_the_select_mode_hands_the_text_to_the_writer_and_says_what_copied`
(the text the writer was handed is the source lines joined, and the line is
`copied 2 lines from #0's reply — 12 bytes`),
`ctrl_y_opens_the_select_mode_and_a_letter_is_not_typing`,
`a_copy_answer_from_a_chat_that_is_gone_is_dropped`,
`the_select_mode_takes_the_keyboard_from_both_panes_and_not_the_box`,
`ctrl_y_is_the_same_key_while_the_mode_is_on`,
`a_reply_is_copied_as_the_message_wrote_it`,
`a_tool_result_is_copied_byte_exact`,
`the_humans_own_message_is_copied_as_it_was_typed`,
`a_message_that_dropped_its_images_carries_its_placeholder`,
`a_selection_spanning_two_messages_joins_them_at_their_own_lines`,
`the_copied_line_marks_the_thousands_of_a_big_number`,
`moving_the_cursor_past_either_end_does_not_panic`,
`esc_leaves_the_mode_without_copying_and_the_box_alone`,
`the_cursor_and_the_selection_are_painted_on_their_own_lines`,
`the_panes_window_follows_the_cursor_and_not_the_bottom`,
`a_line_behind_a_tool_results_cap_stands_on_the_ellipsis`,
`a_new_chat_leaves_the_select_mode_behind` and
`the_select_mode_paints_its_cursor_and_its_selection_on_their_own_cells`.

**What this supersedes.** Nothing in this record called the clipboard read-only
or named the terminal's own selection as mush's selection, so no older section
is rewritten; the manual's key table, its file map and its transcript paragraph
are living prose and are repaired by the drift commit that follows this record,
not superseded by it.

**Recorded, not changed.** Three boundaries of the mode, stated rather than
built: it selects inside the one conversation the chat pane shows, so a selection
never spans agents (the tree is not a text); it copies *text*, so a message whose
picture is still attached copies its words and not the bytes (a picture that was
dropped copies as its placeholder, which is the transcript's own line); and it
holds one selection, not several. The mouse road was not taken: mush never
captures the mouse (K3) — that is what leaves the terminal's own selection and
its scroll wheel to the terminal — so this mode is the road that copies the
source instead of a drag, and the rectangle a drag takes remains the terminal's.
And one piece of prose inside `crates/`: `7bd7f9b` removed the `allow(dead_code)`
attribute over `clipboard::write_text` but left the comment that describes it
("The write road's primitive, waiting for the key that will call it…") — the key
has called it since, so the comment describes a state that is gone. It is left
as it is: the comment lives under `crates/`, and a prose repair belongs with the
code it describes rather than with this record.

---

## 8.50 The reply is read, not painted: a line-local markdown view (`4cfab77`..`a617686`, merged `a5b327f`)

> "what about MD rendering on TUI (ik this is a rabbit hole so the simplest way
> we could implement it, is it even worth it?)"

The human's own framing was the ruling: the smallest honest version, and no more.
The pane painted a model's markdown as the sentence — `**important**` read as
asterisks, a `## Section` as two hashes, a link as its own punctuation — and the
reply is the one piece of prose in the pane, the words a model wrote *for the
human*. Three commits landed it: the wrap bug found on the way (`4cfab77`), the
parser (`f3e55c2`) and the pane's one call site (`a617686`), merged by `a5b327f`;
H48 is their row. The branch that became `5e296b1` carried the two content
commits again as `2114efd`/`482e2db` on top of this merge — the same patches,
with the wrap fix below them already in place.

**A wrapped row could paint wider than the width it was given (`4cfab77`).**
Found while building the view, and it is a bug in the panes' own past rather than
in the new code: the break that ends a row at its last space appended the
character that had not fit to the tail, unchecked. When the row had ended exactly
at the width, that tail plus one more thing could be wider than the pane —
`wrap_text(" bcd日", 4)` painted `bcd日`, five columns in a four-column body,
and a tab, four columns at once, did it too (`wrap_text("ab c\t", 4)`) — and the
terminal cuts what a row paints past its edge, so the last glyph of the line was
silently gone. The break is a loop now: while the tail itself is too full for the
character, the tail is a row of its own. A row that already fit is untouched, so
no caller's rows moved; the only rows that change are the ones that were
over-wide. Pinned by `a_wrapped_row_never_outgrows_its_width` (`" bcd日"`,
`"ab c\t"`, `"日本語 日本語"`, a tabbed line and a CJK sentence among them, at
4..=12 columns).

**The parser (`f3e55c2`).** `mush_core::text` gained `markdown_rows`, `Run` and
`RunStyle`: plain data — a piece of the source's text and one name from a small
vocabulary, no ratatui and no colour — because the parser must never learn what
an accent is and a new surface must never learn the parser's words. It is
**line-local**: every source line is read on its own, so nothing here can reflow
a paragraph, join two lines, re-indent a list or turn `- a\n- b` into a layout
the source did not have. That boundary is the point — the human called the full
version a rabbit hole, and a chat reply needs a reading, not a document
renderer. It is **additive** too: the only text a rule removes is scaffolding a
human does not read in a view — the `#`s of a heading and the two fence lines of
a code block. Every word is kept; a list keeps its marker and only styles it,
because the marker is information; a link always shows its URL beside its text
(`text (url)`, the URL's own parentheses counted so a wiki link keeps its tail),
because a dropped URL is data loss. An unterminated marker is text (`**bold` is
`**bold`, a lone `*` is a lone `*`, a `[link](` with no `)` is the characters it
is), and a fence that never closes runs to the end of the message: an
unterminated block is still a block, and the code in it is still code.

The rules, in the whole: `**strong**`; `*emphasis*` and `_emphasis_` — an `_` at
a word boundary and alone, so `snake_case_name` survives and `__strong__`, which
is not a rule here, is not half-read; `` `code` ``; `~~strike~~`; one, two or
three `#`s and a space for a heading, with the `#`s and the one space not painted
because the style says what they said (a fourth `#`, or a `#` with no space, is
text); `- `, `* `, `+ ` and `1. `–`99. ` list markers, kept and styled — a marker
needs its space, an ordered one is at most two digits because `1998. It was a
good year` opens a sentence, and an indented marker is not a marker because
there is no nested-list layout; fenced blocks, where three backticks open and
close, the fence lines are not rows, and the body is one style with no inline
parsing, so `**` in code stays code; and links. The view's wrap mirrors
`wrap_text`'s arithmetic — per source line, explicit newlines honoured, a tab
four columns, every line `sanitize`d, a word broken only when it cannot fit a row
by itself — with the styles attached, and
`a_plain_message_wraps_exactly_like_wrap_text` pins the two against each other
for text that is not markdown at every width, so the view and the text beside it
cannot drift. Pinned by
`a_span_is_read_and_an_unterminated_marker_is_text`,
`an_underscore_inside_a_word_is_not_emphasis`,
`a_heading_is_its_text_and_only_one_to_three_hashes_are_headings`,
`a_list_marker_stays_as_its_text_and_only_a_marker_is_styled`,
`a_fence_hides_its_lines_and_marks_the_code_between_them`,
`a_link_keeps_its_text_and_its_url`, `a_line_of_only_markers_is_text`,
`a_block_line_never_reflows_the_lines_around_it`,
`an_empty_message_is_one_empty_row`, `a_span_that_wraps_keeps_its_style` and
`every_row_of_the_view_fits_its_width`.

**One call site, and the boundary (`a617686`).** `chat.rs`'s `marked` gained
the reply arm: the mark it is handed decides the view, and `mush › ` is the one
mark that reaches the parser. Every other caller keeps the plain path byte for
byte — the human's own message, a brief, a parent's steering, mush's notices and
footnotes — and three more kinds of text never reach the function at all: a tool
result and a `run_command` transcript (a diff, a test log, a shell session,
where a `#` is a comment, an `*` a glob and backticks quoting), a `Ctrl-T`
reasoning row (a working note and not prose), and a tool-call label (the call's
own JSON). Only a reply is a document. The view wraps inside the columns the mark
leaves, exactly as the plain path does, so no row of it can outgrow the pane (the
mark itself is dropped when the pane cannot afford the mark and a few words),
and nothing is written back: `markdown_rows` reads the reply's own bytes, so what
the human copies — with §8.49's select mode, or out of `.mush/session.json` — is
still the model's text, markers and all. The parser's vocabulary is spent in this
function and `reply_style` beside it: bold, italic, strike, a dim code/fence/URL,
the reply's green accent for a heading and a bullet's marker, a link underlined.
Pinned by `a_reply_is_read_as_markdown_and_its_bytes_are_left_alone` (the rows
and the palette, and `message.text()` still the source),
`a_tool_result_is_painted_byte_for_byte_as_data` (the boundary: the same markers
in a result paint exactly the rows the plain wrapper painted before there was a
view), `only_the_reply_is_read_as_markdown` (a human's line, a brief and a notice
all keep their `#`s and `*`s) and `a_markdown_reply_never_paints_past_the_pane`
(seven widths, a CJK heading, an unbreakable URL and a fence: no row wider than
the pane, and no `**` or `` ``` `` surviving it). No existing fixture moved: the
rows the plain wrapper made for text that is not markdown are the rows the view
makes.

**What this supersedes.** The grep for an older section or row that described a
reply as painted raw — or that promised a markdown renderer — found none: the
view is the manual's first account of what a reply's rows are, so it repairs
living prose rather than superseding a recorded claim. The wrap fix supersedes
nothing either: it changes only rows that were already painting past their
width.

**Recorded, not changed.** What the view refuses is the boundary, not a backlog:
reflow, tables, block quotes, setext headings, reference and auto links, HTML,
nested lists, indented code blocks, task checkboxes, thematic breaks (`---` and
`***` stay the characters they are), escapes (a backslash protects no marker) and
four or more `#`s are all simply the text they are. Adding any one of them is
what turns a reading into a document renderer — the version the human named as
the rabbit hole — and the deliberate seam is visible in the heading rule: a
marker inside a heading *is* read (`## **Title**` does not paint its asterisks)
but the heading's style is the only one it wears. One corner inside the accepted
rules was half-read rather than refused: `***bold***` came out as a strong span
holding `*bold` plus a stray `*`, because the inline scanner took the first two
asterisks for the opener. That is fixed in `da50f6c`: a run of a marker is
all-or-nothing — the whole run opens and the whole run closes, a one-marker rule
pairs only with a one-marker run, and a run that is not a matched pair is text
exactly as it was typed — so `***bold***` is a strong `bold` and no row carries
a marker left over from a half-read run. The refused set above did not move,
`__strong__` and `___x___` among it. Nothing else moved: no schema, prompt,
message or session byte changed, because the view is a paint and not a rewrite —
which is exactly why §8.49's copy road is unaffected by it.

**Census** at this landing (`scripts/census.py`), against §8.48's (total 65,469
· prod 15,272 · tests 28,915 · comments 17,316): total 69,226 · **prod 16,509** ·
tests 30,248 · comments 18,345 — 3,757 lines more: 1,237 production, 1,333 test,
1,029 comment, 158 blank. Measured at `5e296b1`, where both merges are in; the
census reads only `crates/**/*.rs`, so this record's own prose moves no column.
`cargo test --workspace`: 717 + 4 ignored in the mush bin, 215 in mush-core;
clippy (`--all-targets -D warnings`) and `cargo fmt --all --check` clean; both
endpoint-free pty scenarios (`--resize`, `--cancel`) pass.

---

## 8.51 Six blind audits, one campaign: 104 findings, none fixed in the file that found them (`43a03bc`..`17a9692`, `3ca0170`, `69017db`)

Six read-only auditors took the tree apart one area at a time, each from its own
worktree, each against a stated base, and wrote what they found into
`docs/audits/`. **Blind** is their own word and it is precise: no doc comment was
read as true — every one was a claim attacked — and no auditor fixed anything.
Each file opens with its method, its base and the probes it ran, then carries its
own findings ledger, its own priority order, a *Verified sound* list (the checks
that convinced it the rest holds) and a *Blind spots in the tests* list (the
invariants its area claims with no test behind them). The files were committed as
they were written and have not been touched since; where this record and an
audit disagree, the commits are the evidence and the record says so.

| file | area | base | landed by | findings (blocker/major/minor) | verified sound | blind spots |
|---|---|---|---|---|---|---|
| `docs/audits/agent-and-wire.md` | the run loop, `AgentEvent`, the tool batch, the mailbox, the wire, and the UI readers of those facts | `4436db5` | `43a03bc`, merged `13e4dac` | 0 / 8 / 15 | 20 | 13 |
| `docs/audits/tools-and-workspace.md` | path resolution and the sandbox, reads/windows/listings/searches, writes, images and `.mush/paste/`, the attach socket | `4436db5` | `0c55c22`, merged `50c04dd` | 0 / 6 / 10 | 15 | 9 |
| `docs/audits/secrets-session-config.md` | the key and its roads, the stored session, the home config | `4436db5` | `ae5d501`, merged `5889590` | 0 / 7 / 5 | 14 | 13 |
| `docs/audits/tui.md` | the ten files behind the screen: state machine, keys, painting | `5889590` | `082a758`, merged `17a9692` | **2** / 4 / 20 | 14 | 15 |
| `docs/audits/processes-and-jobs.md` | the job registry, the shell seam, the machine lock, the session writer | `5889590` | `69017db`, merged `83b2728` | 0 / 3 / 7 | 14 | 10 |
| `docs/audits/contract-and-git.md` | the prompts and schemas, the transcript algebra, the git verbs and the delegation road | `5889590` | `3ca0170`, merged `f815104` | 0 / 6 / 11 | 25 | 12 |

A and B audited `4436db5`; C audited the same base and its own merge (`5889590`)
became the base D, E and F read — so the six are two generations, and the later
three could cite the earlier ones by finding id (`as in B4`) instead of
re-describing a mechanism. The totals are 104 findings: **2 blockers, 34 majors,
68 minors**. The two blockers are both the TUI's: a fold under the select mode
panicked the frame (`D1`), and two stored rows of one id panicked the pane on a
keypress (`D2`).

**Method.** Every doc comment was read as a claim to check. A finding is
*proven* when a probe was run against the audit's base and its numbers are quoted
— probes were throwaway tests (`cargo test -p mush --bin mush <name>`, the
package has no lib target; `cargo test` in `mush-core`), throwaway `#[cfg(test)]
mod audit_probe` modules, fake endpoints on `127.0.0.1`, the real binary driven
from a pty, scratch repositories under `/tmp`, `/proc` and `ps` readings — or a
live run staged it. Everything else is *suspected*, with what could not be
staged said plainly. All probes were deleted before the audit file was
committed; every audit states its tree was clean. The findings the audits mark
*suspected* — the rest are proven, by a probe's numbers or by reading the code
whole — are A21 and A22 (a deletion race and a restore with jobs outstanding),
B16 (the read-modify-write window), D10 and D26 (two races), E4's live trigger
(its mechanism is proven) and F6 (a dead actor thread).

The ledger was read first by every auditor — `docs/findings.md` and §8.44–§8.50
with H41–H48 — so nothing already closed is re-reported. Where a recorded item
turned out **worse than recorded**, the audit says so; those deltas are part of
the record and are re-pointed at the sections that answer them:

- **H35/§8.39** — the `✉` re-arm recorded there as "recorded rather than fixed"
  is worse: the stale mark also pins the child's thread and exempts its node
  from the history window (A5, fixed by `4702c94`, §8.66).
- **H16's residual** — "the file is bounded by the history window … only ever
  writes live tree nodes" is false on a window whose fold cannot fit (A8, fixed
  by `3a37c02`, §8.66).
- **H12** — per-agent token accounting has an instance worse than "not visible
  while spent": the endpoint's own counts are not reported at the end either,
  on four of the roads a run can end by — **✅ that instance is closed by
  `d710c2e` (§8.83):** `run_turns` reports every ending, and a fold's usage is in
  the line.
- **H34's parenthetical** — "`done_jobs` is never pruned" is now measured (A16 —
  **✅ closed by `d8ae94d`, §8.83** — and the ledger's own A16 row is §8.83's).
- **B27's class** survived one road over: a hex-parsable chunk size past the
  body cap was still diagnosed as the endpoint's refusal (A10 — **✅ closed by
  `729e8fc`, §8.79:** the claim is now `Framing`, and its words are the size
  claim and the cap), and the retry B27's row describes was narrowed to
  `Unsent` on purpose (§8.58).
- **H21** — its fix does not survive a restart: a restored agent carries no fork
  revision, and `git::landing` answers `Merged` when it cannot ask, so a
  read-only child that never committed is stored as landed (C audit, §8.52).
- **H30** — worse than recorded: the restored row is what `git status` and
  `wait` disagree with, so one screen answers "is it working?" and "is its work
  reachable?" oppositely (C audit, §8.52).
- **H27** — unchanged and still open; the C audit's additive fork revision for
  H21 would retire it too (C audit, §8.52).
- **H28** — the E audit read one presumed cause away: a stale `/tmp` lock cannot
  outlive its holder (`flock` dies with the fd, proved with a SIGKILLed holder,
  and the tests `remove_dir_all` their root first); the shapes left are
  `open`/`flock` failing before the lock (ENOSPC/EMFILE) (§8.52).
- **H7, H10/H17** — `base` is a promise about history (H7) and the reclaim rule
  never touches unmerged or dirty work (H10/H17); the F audit's F9 is the first
  class (fixed by `6f47149`, §8.65) and F1 falsifies the second for ignored
  paths (fixed by `8b029f0`, §8.59).

**Priority orders.** Each audit ranked its findings by what the human loses, and
the rankings are themselves record material. A put A1 first (an endpoint that
never sends a newline OOM-kills the process, taking every run and up to a minute
of session file — the one loss of the human's *data*), A2 second (a request the
endpoint already received is sent again, up to six sends and ~30 minutes on the
human's bill) and A3 third (a finished job's unread report parked behind a
sibling's machine hold for the full 600 s). B put data loss first: every write
dropping the file's mode (B1), an uncapped read (B5) and a non-UTF-8 file
silently rewritten (B6). C put the inherited credential first (C1), the swallowed
provider typo second (C2) and the key sent to the vendor third (C6). D put its
two blockers first — the fold panic (D1) and the duplicate-id pane panic (D2) —
then `agent.id + 1` overflowing at startup from a stored file or a git ref (D3).
E put E1 first (a `SIGTERM`/`SIGHUP` skipped every `Drop`, leaving every process
group running), E2 second (one `write_file(".mush/lock")` gives two mushes one
store) and E3 third (a trailing `&` escapes the registry, the ceiling and the
quit). F put F1 first (an ignored-only run reads "clean — nothing changed" and
is then deleted), F8 second (a run-end commit in a directory no longer a
worktree commits the human's checkout on the human's branch) and F9 third
(`base="HEAD"` resolved in the application root, not the caller's workspace).

**One fix was in flight.** The A audit was written while a "thinking" phase
signal was landing in `agent.rs`/`app/tree.rs`/`app/mod.rs`, from the human's
report that a finished tool's label sticks through the next model call; A20 is
that defect, audited as it stood and marked *at this base*. The fix (`9a4dbbf`,
merged `a2a8267`) landed before the audit file did, and §8.53 is its section.

**Where the closures live.** The audits are left exactly as found — no status
column was turned and no finding was marked fixed inside them. Every one of the
104 findings, its severity and its status is in §8.52, whose head names the
snapshot it reflects, and the sections after it are the waves that changed those
statuses. Findings the waves did not fix are not papered over: they are ⬜ in
that table with the site that still holds them at this base.

---

## 8.52 The 104 findings, and where each stands

The audits are the queue; this is its closure sheet. Every finding is listed by
its own id, in its own audit's order, with the severity the audit gave it and its
status at **`7338d81`** — the sheet was written at `38d0438` (§8.51), and every
wave since has moved the statuses it closed, with the section that closes it in
the ✅ cell. **✅** names the commit(s) that fixed it — each verified against the
commit's own body and the finding's text, and each pinned by the tests the wave
sections name. **⬜** means the defect is still in the tree at this base; the site
named is where the code still is, re-read for this record rather than taken from
the audit. **🔄** is a partial fix, with each half said. A finding the audit
marked *suspected* stays suspected here.

The counts: **91 fixed, 4 partial (A19, B12, F7, F13), 9 open** — 2 blockers and
34 majors among the 104, of which both blockers and all 34 majors are closed. The
open set is nine minors: A13, A14, A15, A18, A21 and A22 (agent-and-wire), C7
(secrets-session-config), F3 and F17 (contract-and-git). The three majors left
open at `38d0438` — A3, A6 and F6 — closed in §8.73–§8.83, with D19, A9, A10,
D7–D18, D20–D26, B13, B14, B16, C8, C10–C12, E6, E8–E10 and F2, F4, F5, F14–F16.

**Addendum, 22 September 2026 — this sheet now speaks for `df115bc`.** Since
`7338d81` the tree has grown to **87 068** (· 4 999 blank · 24 513 comment ·
38 456 tests · 19 100 prod — +445, and prod +8 of it), and D9's leftover is
closed: the row whose parent the history window reaped now wears `⚮` and no
longer passes for a root child (§8.87). The counts above remain the snapshot at
`7338d81`; no finding's status moved.

### agent-and-wire (`docs/audits/agent-and-wire.md`)

| # | severity | the defect | status at `7338d81` |
|---|---|---|---|
| A1 | major | a response header line with no newline is read without a bound; the process can be OOM-killed by its own endpoint | ✅ `0747460` (§8.58) |
| A2 | major | a request the endpoint already received is sent again, and the 600 s deadline re-arms per attempt | ✅ `190886c` + `b6a59c3` (§8.58) |
| A3 | major | a finished job's unread report is parked behind a sibling's machine hold for the full 600 s — H13's blindness, for jobs | ✅ `c86b8c0` (§8.79) |
| A4 | major | an event for an id the UI reaped re-created that agent's transcript, and a false failure survived every restart | ✅ `66dba03` (§8.66) |
| A5 | major | the `✉` re-arm also pinned the child's thread and exempted its node from the history window | ✅ `4702c94` (§8.66) |
| A6 | major | the endpoint's own token counts are reported only on a clean, tool-free end, and a fold's usage is dropped | ✅ `d710c2e` (§8.83) |
| A7 | major | `exclusive`, `detach` and `base` are silently defaulted when the JSON type is wrong | ✅ `d8a1a04` (§8.65) |
| A8 | major | the pane's copy is never trimmed, so on a window whose fold cannot fit the session file and the meter grow without bound | ✅ `3a37c02` (§8.66) |
| A9 | minor | `RunUsage` adds endpoint-supplied `u64`s with `+=`; a debug-build actor panics mid-run, a release one prints a wrong number | ✅ `330fe61` (§8.79) |
| A10 | minor | a hex-parsable chunk size past the cap is classified as the endpoint's refusal, so a wire break that would be retried kills the run — B27's class, one road over | ✅ `729e8fc` (§8.79) |
| A11 | minor | `status` is a big-text road that ignores `result_cap` and `turn_room` (8,197 bytes at an 8 K window) | ✅ `a8851e8` (§8.83) |
| A12 | minor | `write_file` read the whole file it was about to replace, to answer its line count (128 MiB → +262 MB peak RSS) | ✅ `273ada0` (§8.60) |
| A13 | minor | the pane re-parses every visible tool call's whole argument JSON on every frame (2 MB → 9.68 ms per call, per frame) | ⬜ open — `chat.rs:2708` (`tool_label`), `agent.rs:6370` (`summarize_args` parses per frame) |
| A14 | minor | the idle fold drains the mailbox like an in-run one, swallowing a command that means "start a run" | ⬜ open — `agent.rs:2117` (`wait_for_work`), `3666` (`compact_now`), `3806` (`drain_mailbox`) |
| A15 | minor | parking a child kills the jobs it started, and `park_history`'s doc says parking ends "the thread and nothing else" | ⬜ open — `app/mod.rs:3841` (`park_history`) |
| A16 | minor | `done_jobs`, `delivered_jobs` and `forgotten` grow for the life of an actor (1,000 jobs → 61,893 bytes) | ✅ `d8ae94d` (§8.83, H57) |
| A17 | minor | `LOST_POOL`'s doc says the oldest lost number is forgotten; the code drops the newest | ✅ `3483c4e` (§8.83) |
| A18 | minor | the dropped-turns note is put back in its place only for the root | ⬜ open — `agent.rs:2236` (`adopted`), `app/mod.rs:2656` (`AgentMsg::Run` reaches only the root) |
| A19 | minor | three sentences that claim more than the code does: a repeated dropped-images notice, a shed note's promise, two `null`s | 🔄 `d8a1a04` fixed the `arg_string` "missing" misdiagnosis (§8.65); the notice still repeats every request (`agent.rs:2678`), `SHED_RESULT_NOTE`'s doc still overclaims (`agent.rs:2758`), `wait({"on": null})` is still refused (`agent.rs:5292`) |
| A20 | minor | a finished tool's label stuck through the next model call | ✅ `9a4dbbf` (§8.53) |
| A21 | minor | two `expect("workspace root must exist")` sit on the UI thread (suspected) | ⬜ open — `agent.rs:1495` (`root_actor`), `1606` (`revive`) |
| A22 | minor | after a restore the job space restarts at `#c1` while the restored transcript still names old `#cN` lines (suspected) | ⬜ open — `ids.rs:122` (the counter starts at 1 every start), `app/mod.rs:961` (`restore_agents`), `mush-core/src/session.rs` (no job counter stored) |
| A23 | minor | cleanup is process-group-only; the doc claims "everything the command started" | ✅ `d4c596c` (§8.83; `jobs.rs:483`'s sentence left, §8.86) |

### tools-and-workspace (`docs/audits/tools-and-workspace.md`)

| # | severity | the defect | status at `7338d81` |
|---|---|---|---|
| B1 | major | every write rebuilds the file at mode 0600, and a 0444 file is replaced anyway | ✅ `580e1f4` (§8.55) |
| B2 | major | a symlinked file is replaced, not written through: the edit lands in the wrong place | ✅ `1e08673` (§8.55) |
| B3 | major | `write_file` renames over whatever the name is: the attach socket (or a FIFO) dies | ✅ `93bbd80` + `75d07fb` (§8.55) |
| B4 | major | a symlinked directory inside the root is followed: the file tools leave the workspace | ✅ `8ccf851` (§8.55) |
| B5 | major | `read_file` is unbounded, and `write_file` reads the file it is about to replace (512 MiB → 514 MiB peak RSS) | ✅ `273ada0` (§8.60) |
| B6 | major | a non-UTF-8 file is silently rewritten in UTF-8 when it is edited | ✅ `469853c` (§8.60) |
| B7 | minor | CRLF files cannot be edited from what the window shows | ✅ `feb5a85` (§8.69) |
| B8 | minor | `search` shows the model a line the file does not hold | ✅ `2d0fe86` (§8.69) |
| B9 | minor | `rel()` rewrites a real name's backslash into a separator: a listed path the model cannot open | ✅ `b13aeed` (§8.69) |
| B10 | minor | `edits_arg` silently defaults a wrongly-typed `replace_all` | ✅ `d8a1a04` (§8.65) |
| B11 | minor | the attach socket has no bound anywhere: line, threads, or idle time | ✅ `1bd5e2a` (§8.67) |
| B12 | minor | `Message`'s deserializer is stricter than its own doc, and a refused reply ends the run | 🔄 `fe83f8a` made the deserializer as loose as its doc, pinned by `a_loose_reply_is_still_a_reply` (§8.83); a reply that still does not parse ends the run at `agent.rs:3070` (H56) |
| B13 | minor | `.mush/paste/` is never pruned | ✅ `a534d35` (§8.78) |
| B14 | minor | the markdown view's rows are not the plain wrapper's rows for non-ASCII whitespace (8,823 divergences in a fuzz) | ✅ `7da2cdd` (§8.80) |
| B15 | minor | `sanitize` keeps the bidi marks, and the invisible set is wider than the doc says | ✅ `dd23693` (§8.69) |
| B16 | minor | the read-modify-write window (suspected): a sibling's edit between the read and the rename is silently reverted | ✅ `7106564` (§8.78) |

### secrets-session-config (`docs/audits/secrets-session-config.md`)

| # | severity | the defect | status at `7338d81` |
|---|---|---|---|
| C1 | major | every shell, git child and clipboard child inherits `MUSH_API_KEY` | ✅ `579619f` + `b6ef169` + `199858a`, merged `9e6ac54` (§8.54) |
| C2 | major | a typo in the home config's `provider` is silently ignored, and the key goes to the LAN endpoint | ✅ `072d655` (§8.64) |
| C3 | major | a home config mush cannot read is silently discarded, then overwritten | ✅ `2080512` (§8.64) |
| C4 | major | `Ctrl-N` writes an empty conversation over the human's own | ✅ `d505d9e` (§8.57) |
| C5 | major | the store's self-ignore is created only if absent, never enforced | ✅ `a9a2f09` (§8.57) |
| C6 | major | `/provider` sends the current key to the vendor's endpoint, then saves it as that vendor's | ✅ `fa28769` + `df781fd` (§8.64) |
| C7 | minor | a key with a newline injects header lines into the request | ⬜ open — `http.rs:518` writes it raw |
| C8 | minor | the attach printers emit control sequences | ✅ `11ad7dd` (§8.84) |
| C9 | major | the restore trusts the file's agent ids: `id: 0` replaces the root, a duplicate replaces a row, `u64::MAX` panics | ✅ `7d80582` (§8.57) |
| C10 | minor | the fold's refusal echoes the endpoint's whole body into a notice | ✅ `8121e78` (§8.83) |
| C11 | minor | an environment key is silently copied into the home config by an unrelated command | ✅ `d338355` (§8.84) |
| C12 | minor | `--print-config` cannot answer the image gate | ✅ `b1ee2f0` (§8.84) |

### tui (`docs/audits/tui.md`)

| # | severity | the defect | status at `7338d81` |
|---|---|---|---|
| D1 | **blocker** | a fold under the select mode panics the frame | ✅ `15e6d9e` + `35404e4` (§8.56, H49) |
| D2 | **blocker** | two rows of one id lose a painted row, and a real key then panics the pane | ✅ `6833275` (§8.56, H50) |
| D3 | major | `agent.id + 1` overflows: a startup panic from a file and from a git ref | ✅ `f7174db` + `7d80582` (§8.56) |
| D4 | major | a command road blocks the UI thread on the endpoint: measured 10.036 s | ✅ `0951b85` (§8.63) |
| D5 | major | at a short terminal the box paints its attachments with the rows the line being typed needed | ✅ `61ec689` (§8.63) |
| D6 | major | a stored session's `base_url` re-points the endpoint and the home key follows it | ✅ `fa28769` + `df781fd` (§8.64) |
| D7 | minor | a reap keeps the cursor's index, so the selected agent changes with no keystroke | ✅ `39576fd` (§8.81) |
| D8 | minor | a reclaimed worktree keeps its branch stat on the row and in the title | ✅ `7ce13c8` (§8.81) |
| D9 | minor | an orphan row is indented by its stored depth | ✅ `c884a8c` (§8.81) |
| D10 | minor | a status can erase `⊘ cancelling…` (suspected race) | ✅ `c848deb` (§8.81; the tree half — `tree.rs`'s setters refuse `Phase::Cancelling`; the actor's check-then-emit window stays suspected) |
| D11 | minor | the chat pane's title is the one title with no elision rule | ✅ `f749e71` (§8.80) |
| D12 | minor | a resize with `/notes` or `/help` open clips every row of the report | ✅ `d018a2f` (§8.80) |
| D13 | minor | zen's Chat arm re-derives the message box, and the `at_every_size` pin checks two sizes | ✅ `bc581ba` (§8.80) |
| D14 | minor | a reply of only fence lines is an invisible turn, and the select cursor has no row | ✅ `aa0245b` (§8.80) |
| D15 | minor | a held reading comes back from the dead after a fold | ✅ `942e4b5` (§8.80) |
| D16 | minor | an agent whose actor published its prompt but has no transcript weighs 0 | ✅ `3a37c02` (§8.66) closed the mechanism; the false sentence beside it by `a7c579d` (§8.80) |
| D17 | minor | the module doc's "News" row is false for a stopped run | ✅ `fa7b1f1` + `bdbb484` (§8.81) |
| D18 | minor | a focus change under the select mode makes `Enter` copy nothing, silently | ✅ `3a51fea` (§8.81) |
| D19 | minor | a control character in a URL reaches the request line | ✅ `4f7793c` (§8.79; the TUI's `/url` arm refuses without a sentence, §8.86) |
| D20 | minor | a paste that merges graphemes leaves the cursor past the end, and one `Backspace` is swallowed | ✅ `9134ad4` (§8.81) |
| D21 | minor | the config cell's doc claims the copies cannot differ; a handle learn strands the UI | ✅ `25ec885` (§8.81) |
| D22 | minor | an empty key is "set", "(none)" and an empty Bearer | ✅ `a26f165` (§8.81) |
| D23 | minor | the `/help` table's description column collapses at the floor | ✅ `1afa368` (§8.80, ledger `R60`) |
| D24 | minor | `Ctrl-Y` under zen with the tree full-screen opens a mode the frame cannot paint | ✅ `a5dbed2` (§8.80) |
| D25 | minor | the "several agents running" line names `Enter`, which never stops anything | ✅ `4bbef67` (§8.81) |
| D26 | minor | `Msg::Git` carries no conversation stamp (suspected) | ✅ `e87af91` (§8.81) |

### processes-and-jobs (`docs/audits/processes-and-jobs.md`)

| # | severity | the defect | status at `7338d81` |
|---|---|---|---|
| E1 | major | a killed mush leaves every process group running | ✅ `a68eea0` (§8.61) |
| E2 | major | the workspace lock is a file the file tools replace: two mushes on one store | ✅ `0d45ffd` + `5714052` (§8.62) |
| E3 | major | a command that backgrounds a child escapes the registry, the ceiling and the quit | ✅ `8891ec9` (§8.62) |
| E4 | minor | a panicking actor drops its hold without killing (mechanism proven, trigger suspected) | ✅ `deaf586` (§8.61) |
| E5 | minor | the scratch file is removed only by `Drop`: a mush that does not unwind leaves `/tmp/mush-cmd-*` behind, with an orphan writing into it | ✅ `e7834a2` (§8.61) |
| E6 | minor | `Running::kill` fires `kill -9 -<pgid>` unconditionally: the second call aims at a freed group, and the failure is silent | ✅ `3bb1a5b` (§8.77) |
| E7 | minor | the session writer's `flush` has no deadline and no liveness check | ✅ `bbff494` (§8.67) |
| E8 | minor | the lock's one sentence to a human can name a dead pid, and the recorded flake cannot be a stale lock | ✅ `926a12a` (§8.83, H58) |
| E9 | minor | a handed-over job's age starts at the handover | ✅ `a07834a` (§8.77) |
| E10 | minor | the global panic hook restores the terminal from any thread | ✅ `67bc041` (§8.77) |

### contract-and-git (`docs/audits/contract-and-git.md`)

| # | severity | the defect | status at `7338d81` |
|---|---|---|---|
| F1 | major | a run whose only work is in an ignored path reads "clean — nothing changed", and is then deleted | ✅ `8b029f0` (§8.59) |
| F2 | minor | `commit.gpgsign` stops every commit, and the doc says it cannot | ✅ `ce06a83` (§8.78) |
| F3 | minor | the dropped-turns note is identified by its text, so a user line that *is* it becomes the note | ⬜ open — `transcript.rs:474` (`is_dropped_note`) |
| F4 | minor | `run_command`'s schema describes a cut where the code kills the command | ✅ `c96aae1` (§8.84) |
| F5 | minor | a child's worktree has no submodule contents, and the child is not told | ✅ `414e477` (§8.78) |
| F6 | major | a dead actor thread is invisible: a row that spins, a parent that waits 600 s, a corpse handed back as "parked" (suspected) | ✅ `98d7060` + `13b2689` (§8.73) |
| F7 | minor | the spawn cap's arithmetic is not the sweep's, and its sentence claims more than it measured | 🔄 `efd5213` corrected the sentence and the doc — the cap counts against `HEAD` and says so (`git.rs:852` `unlandable`, `agent.rs:4336` `too_many_worktrees`, the count at `agent.rs:4526`) (§8.83); the arithmetic half is still owed to the UI tree (H54) |
| F8 | major | a run-end commit in a workspace that is no longer a worktree commits the human's own checkout | ✅ `0e506f2` + `d70c768` (§8.59) |
| F9 | major | `base="HEAD"` is resolved in the application's root, not the spawning agent's workspace | ✅ `6f47149` (§8.65, H7's class) |
| F10 | major | an agent id returns to the pool after a partial `worktree add` | ✅ `ac12012` (§8.59) |
| F11 | major | an isolated spawn is refused in a workspace that is a subdirectory of a repository, with a false reason | ✅ `34b647b` (§8.59) |
| F12 | minor | a wrongly-typed `base` silently drops isolation | ✅ `d8a1a04` (§8.65) |
| F13 | minor | the one-shared-child rule is a per-parent book, so "this workspace" can hold two live writers | 🔄 `d7f12a2` counts every live writer of the directory (`WriterGuard`, the count keyed by canonical root) (§8.83); the sentence at `mush-core/src/prompt.rs:58` and `agent.rs:1806` still claims the per-parent rule (H55) |
| F14 | minor | `title` is required by the schema, optional in the code, and not bounded to one line | ✅ `09c9446` made the schema optional and one-line (§8.78) + `74ad1de` folds the title in `spawn_tool` (§8.83) |
| F15 | minor | `parse_commit_subject` is not the inverse when the failure text contains `"): "` | ✅ `02b002f` (§8.83) |
| F16 | minor | `has_commits`'s "no commits yet" refusal is unreachable from the spawn road | ✅ `7a01569` (the git door, §8.78) + `c71be60` (the spawn road, §8.83) |
| F17 | minor | a failed commit is a row tail the next run clears, not the transcript line its doc claims | ⬜ open — `agent.rs:295` (`Work::status_line`), `app/mod.rs:1914` (`AgentEvent::Status` arm) |

---

## 8.53 A finished tool's label ends when the model is asked (A20, `9a4dbbf`, merged `a2a8267`)

The campaign's first landing, and it predates the audit that found it: the A audit
read `9a4dbbf` as a fix in flight and marked A20 *at this base*.

**What was true.** The actor announced what a tool was *about to do*
(`AgentEvent::Status`, before `exec_tool`) and nothing said it was over, so the
row and the pane's foot kept `run_command printf hello > note.txt` while the
model was asked the question that result answered — the label stuck through the
whole model call.

**The measurement.** The commit's scripted run emits exactly two `Thinking`
events, one before each ask; with the emit removed it emits 0, and the UI reads
`Activity("run_command printf hello > note.txt")` — the reported defect.

**The fix.** `AgentEvent::Thinking` is emitted at the one place a request is
asked — after the fold, the trim and the `over_window_line` fit test,
immediately before the ask — and `AgentTree::thinking` keeps `activity`'s guards
and restarts `since`. The doc comment says why there: the request fits the
window and is about to go on the wire, so nothing is running locally any more;
emitted after the fit test so a request the window refuses paints no phase.

Pinned by `a_run_says_it_is_thinking_before_the_request_that_follows_a_tool`,
`a_finished_tool_does_not_hold_the_row_while_the_model_is_asked_again` and
`a_thinking_event_moves_a_running_row_and_nothing_else`.

**Recorded, not changed.** A tool that blocks locally — a parked `wait`, a long
`run_command` — emits none, because its own label is the truth while it runs;
the fold's phase, the run endings and `words`/`doing` are untouched. The audit's
own not-verified item stands: the live-terminal paint was not driven frame by
frame.

---

## 8.54 The key is mush's, and its children do not inherit it (C1, `579619f` + `b6ef169` + `199858a`, merged `9e6ac54`)

**What was true.** `Shell::spawn` built `sh -c` from the inherited environment
and never removed `MUSH_API_KEY`, so every `run_command` the root or a child
issued could read the credential — and its output is stored verbatim in
`.mush/session.json` and handed to any local user through the attach socket.
The git road had the same hole for no reason at all (a hook is a child of git,
and git does not talk to the provider), and both clipboard doors — the readers
and the writers — spawned their programs from the same environment.

**The measurements.** With `MUSH_API_KEY` set, a command's `printenv` wrote
`sk-probe-inheritance-0123456789` to a file and exited 0; after, the file is
empty and the status is 1. A `pre-commit` hook writing `printenv MUSH_API_KEY`
(its trailing `true` keeps the probe from deciding the commit) held the key
before and nothing after. `sh` run through both clipboard roads held it in both
files before, neither after.

**The fix.** `mush_core::secrets::SECRET_ENV` is the one list of names and
`scrub` the one way to remove them — an `env_remove`, never an `env_clear`. The
shell a command runs in, the `kill` that ends a command's group, git's two spawn
sites and the clipboard's reader and writer all go through it. The doc says why
the list is a list: a name belongs there only when reading it back out of a
child's environment would be a credential leak, while configuration mush reads
stays inherited — `PATH`, `HOME`, `LANG`, `EDITOR` and the human's tooling ride
through untouched.

Pinned by `a_command_never_sees_mushs_key` and
`a_command_keeps_the_environment_it_needs` (`machine.rs`),
`a_git_child_never_sees_mushs_key` (`mush-core/git.rs`) and
`a_clipboard_child_never_sees_mushs_key` (`clipboard.rs`).

**The merge.** `9e6ac54` is a clean merge of the three-commit chain; the only
content it adds over the branch tip is the A audit file. A note on the source of
the mapping: the fix was recorded with the C1 finding from the secrets audit;
all three commits name C1's mechanism and none names another finding.

---

## 8.55 The write road asks what a name is (B1–B4, `580e1f4`, `1e08673`, `93bbd80`, `8ccf851`, `75d07fb`, merged `bd05cb6`)

A write in this tree is a temp file and a rename, and a rename replaces the
*name*, not the thing the name points at — while the name's type was never asked
before the temp file was made. Four defects came from that root, one audit found
them (B1–B4), and the wave answered each with one question asked earlier:
mode, target, type, root.

**A write keeps the mode it found (`580e1f4`; B1).** `tempfile` created the
scratch at 0600 and the rename carried that mode onto the target: a 0755 and a
0644 file both came back 600, a new file was made 600, and a write onto a 0444
file returned `Ok(())` and left 600 — `git diff --cached --summary` read `mode
change 100755 => 100644 run.sh` in the human's own repository. Now an existing
target's mode is copied onto the temp before the rename; a name that did not
exist is `Fresh::Box` (0o666 under the kernel's umask, "the way `>`, `vim` and
`git` make a file") or `Fresh::Private` (0600) for mush's own stores, and
`write_file` refuses a 0444 target naming the mode. Pinned by
`a_write_keeps_the_files_mode` and `a_private_store_is_made_0600`. **Recorded,
not changed:** the refusal lives at `write_file`'s door only — `session::save`
and the human's config writer do not pass through it and may still replace a
0444 file; the doc says so rather than a guard in `atomic_write` doing work its
callers did not ask for.

**An edit follows a symlink to its target (`1e08673`; B2).** After a write to a
link the link was not a symlink any more, the file it pointed at still held `a =
1\n`, and the link's path held the new bytes — while a read follows the link, so
the model read one file and wrote another. `entry_for_write` now resolves the
symlink chain to its final target before the temp file is made, so the write
lands where the link pointed and the link stays a link; a dangling link and a
link to a non-regular file are refused with one sentence naming the link, its
target and why. Pinned by `an_edit_follows_a_symlink_to_its_target`. The
accepted residual is pinned too: `a_hard_link_forks_under_the_rename` — a
hard-linked twin forks, because writing into the inode would give up the atomic
rename.

**A write will not replace a socket or a FIFO (`93bbd80`, pinned sharper by
`75d07fb`; B3).** `write_file` asked only whether the path was the root, so
`write_file(".mush/mush.sock", …)` returned `Ok(())` and the rename unlinked the
workspace's own attach socket: the path then held bytes and every `mush read`,
`agents`, `focus` and `edit` answered "no mush is running". A FIFO went the same
way. `entry_for_write` now asks `symlink_metadata` and refuses a socket, FIFO or
device with one sentence naming the type, and the guard is kept in
`atomic_write` as well — "a rename over a socket destroys it whatever door it
came through". `75d07fb` is the adversarial pass over the four fixes: three doc
sentences that had drifted were corrected (which refusal is exclusive, the
symlink chain rather than "one link deep", what the scratch's 0600 means), and
the test now connects a `UnixStream` to the path, so the fact pinned is "a
client can connect", not `is_socket`. Its body records no code and no outcome
change: `a_write_will_not_replace_a_socket_or_a_fifo` pins the sharper fact and
the four fixes' measurements stand.

**A link inside the root cannot leave it (`8ccf851`; B4).** `resolve` was a
lexical walk that refused `..` but not a link the root itself contained: with
`root/out -> <outside>`, `read_file("out/secret.txt")` returned `SEKRIT\n`,
`write_file("out/written.txt")` landed outside the root, `list_files("out")`
listed the outside directory and `search` reached files outside (`skipped=0`).
`Workspace::real_path` canonicalizes the deepest existing prefix and refuses a
result outside the root, and every filesystem road — `read_file`, `read_window`,
`read_image`, `write_file`, `list_files`, `search` — resolves through it, with
`walk`'s start asking `symlink_metadata`. Pinned by
`a_link_inside_the_root_cannot_leave_it`, whose positive twin keeps a link that
stays inside working. **Recorded, not changed:** the human's paste road does not
resolve, deliberately — it names an absolute path the human already holds.

**What this supersedes.** The contract audit's attack on the earlier audits'
proposed fixes warns that B4's remedy must not canonicalize `resolve` itself (the
human's paste road and the model's relative names both need the lexical rule);
`real_path` is a second function beside `resolve`, and the paste road stayed on
`resolve`, so the landed fix answers the attack rather than walking into it.

---

## 8.56 The frame that killed a session, and a git name that is not a child (D1–D3, `15e6d9e`, `6833275`, `f7174db`, `35404e4`, merged `525b2fa`)

The TUI audit's two blockers and its third major landed as one branch: the two
panics are one mistake about which length a cursor is bounded by, and the third
is the same arithmetic on an id.

**A fold cannot leave a cursor on a row that is gone (`15e6d9e`; D1).** The key
road clamped the select cursor; the *paint* road did not — `select_body` used
`select.cursor` raw — and a fold replaced the transcript under the mode through a
road nothing dropped the mode for. The audit's probe: two root messages,
`Ctrl-Y`, the fold as the event arm makes it (`AgentEvent::Compact` →
`replace_transcript`), one frame, and the pane indexed a row that no longer
existed — `index out of bounds: the len is 1 but the index is 7`. Now
`replace_transcript` drops a mode over that conversation, as `clear` already
drops it for a new chat, and `Chat::painted` takes the same `clamped_cursor` the
key road takes, with a mode that has no line left painting as off — no cursor,
no `Enter copies` clause. The doc says why in one sentence: "A cursor into a
transcript that no longer exists is not a cursor", and the clamp exists "for
every road that cannot know it took rows away". Pinned by
`a_fold_while_selecting_does_not_panic_the_frame` (`app/mod.rs`),
`a_fold_leaves_the_select_mode_behind` and
`the_frame_clamps_a_cursor_left_past_the_transcript` (`chat.rs`). `35404e4` is
the prose that follows: three comments still named a fold as the road that can
leave the mode pointing into a transcript that shrank, which the fix made
impossible; they now name the roads that can (a reaped conversation, a resize, a
state a caller built). The panic was on the UI thread's draw path, and the
automatic fold at nine tenths of the budget is the road that makes it reachable
in an ordinary session.

**The cursor names the row the pane painted (`6833275`; D2).** Two lengths
decided one question: `rows()` de-duplicated by id while the cursor was bounded
by the storage length, so a session holding two rows of id 2 was `agents=3
rows=2` — and the pane indexed the painted vector with the storage cursor
(`agent_footer(self, nodes[cursor], &rows[cursor], …)`). The audit's probe:
`AUDIT PROBE: agents=3 rows=2`, then `G` through the real key table, one frame,
`index out of bounds: the len is 2 but the index is 2` (release builds too). Now
`rows()` keys its walk by a node's *place* in `agents`, so a duplicate is two
rows, `row_count()` is `rows().len()` and the only cursor bound (`cursor`,
`cursor_bottom`, `move_cursor`, `repair_focus`), and `agents_pane` indexes with
`get`, so a cursor past its rows paints no footer instead of panicking. The doc
says what keying by id had done: the second row vanished from the screen while
the cursor could still be walked onto it. Pinned by
`rows_paints_every_node_even_when_two_share_an_id` (`tree.rs`) and
`the_pane_never_indexes_past_the_rows_it_painted` (`app/mod.rs`). The restore's
refusal of a duplicate id is the other door — C9's, `7d80582` — and landed in
the same wave.

**A name mush cannot hold as a child is not a child (`f7174db`; D3, git half).**
The agent counter is "one past the largest id the repository has named", and
`git::worktree_id` parsed any `mush/<digits>` with `parse::<u64>()`: an unmerged
commit on `mush/18446744073709551615` panicked the debug build at startup
(`attempt to add with overflow`, `app/mod.rs:1209`), and in release the add
wrapped to 0, so the floor was silently left unset. `worktree_id` now refuses an
id at the top of the space — every road from a branch to an id passes through
it — `discover_worktrees` refuses a refused name *by name* in the bar
("`mush/… names no agent id mush can hold — left alone`"), and every reservation
saturates. The doc says what the floor is and why the top id would pin it. Pinned
by `no_agent_id_can_overflow_the_floor` and
`a_branch_that_names_no_agent_is_refused_by_name` (`app/mod.rs`) and
`the_path_and_branch_are_one_rule` (`git.rs`). The session-file half of the same
overflow is `7d80582`'s `vet_stored_agents` (§8.57), which refuses a stored
`u64::MAX` row before `agent.id + 1` is ever asked.

**A correction this record owes the campaign's own mapping:** `35404e4` is D1's
prose, not D3's — D3's halves are the git name (`f7174db`) and the stored id
(`7d80582`); `35404e4`'s body opens "The D1 fix changed which roads can leave
the mode pointing into a transcript that shrank".

---

## 8.57 The store's doors: the copy, the ignore line and the file's own ids (C4, C5, C9, `d505d9e`, `a9a2f09`, `7d80582`, merged `f17a031`; `c6a3055`)

Three C-audit majors on the road that opens the store at startup and the key
that replaces it, plus the repair of the merge that landed them together.

**`Ctrl-N` keeps the old conversation and arms the key (`d505d9e`; C4).** The
key replaced the store with an empty one from a single keystroke, unarmed, with
no copy kept and the only warning arriving *after* the loss. The probe, a real
pty with one message typed and sent: before, the first `Ctrl-N` left
`session.json` at `"messages": []` and the words were not anywhere in `.mush`.
Now the clear is two steps, like `Ctrl-Q`: the first press says what would go and
where it is kept (`Ctrl-N again clears 1 line — kept as
.mush/session.json.previous`), the second writes that copy and only then clears;
an empty conversation is one press; the copy is written synchronously before
anything is stopped or cleared, and a copy that cannot be written refuses the
key. The doc's reason: one slot, not a numbered family, "the newest cleared
conversation is the one the human is looking for". Pinned by
`a_new_chat_keeps_the_old_conversation_and_arms_the_key`,
`an_empty_chat_is_cleared_by_one_press_unarmed` and
`another_key_takes_the_new_chat_arm_back`.

**Mush's ignore line is enforced, not suggested (`a9a2f09`; C5).**
`.mush/.gitignore` was written only when it did not exist, so a hand edit,
another tool, or a repository shipping its own file silently put the whole
conversation in front of `git add -A`. The probe: a scratch repository whose
`.mush/.gitignore` held `!*`, the real binary in a pty — before,
`git status --porcelain` answered `?? .mush/`; after, empty. `ensure_mush_dir`
now writes the one line (`*`, which ignores itself) unconditionally: one line,
idempotent, one small write per start. The doc gives the trade in one sentence —
a sticky wrong line is a leak, which is the more expensive of the two. Pinned by
`mushs_own_ignore_line_is_enforced_and_git_stays_clean`.

**A stored id cannot replace the root or panic the restore (`7d80582`; C9).**
Every stored agent was registered under its own id with no check: `id: 0` was
registered *as the root* (its transcript replaced the root's conversation, its
mailbox the root's actor), a duplicate id replaced the first row, and
`id: u64::MAX` panicked the debug build at `app/mod.rs:743` through
`agent.id + 1`. The probe: a pty whose stored root conversation was followed by
`{"id": 0, …}` — before, the frame read `you › IMPOSTOR LINE`; after, the
root's own words and the bar names the refused row. One validation pass,
`vet_stored_agents`, now decides every row before registration: the root's id,
an id already taken, a parent chain not reaching the root, and an id no floor can
be kept above are refused and reported in one line naming the file and the row;
the reservation saturates and rows after a refused one still restore. The doc
says why the file is not trusted: it is hand-editable and a repository can still
commit one — an ignore rule does not untrack a tracked file. Pinned by
`a_stored_root_id_cannot_replace_the_root`, `a_duplicate_id_keeps_the_first_row`
and `a_u64_max_id_does_not_panic_the_restore`.

**The merge's own defect (`c6a3055`).** The merge `f17a031` combined two
changes that were each right alone and wrong together: the `Ctrl-N` road wrote
the copy with `atomic_write(&to, &json)` while the write road's change
(`580e1f4`, §8.55) made that function take `fresh: Fresh`. The compiler said it
exactly — `E0061: this function takes 3 arguments but 2 arguments were supplied`
(`session.rs:76`). The repair committed on top of `f17a031` and was merged by
`6d7453e`; it makes `.mush/session.json.previous`
`Fresh::Private` (0600), the same answer `Session::save` already gives, and
`Fresh::Private`'s doc now names the copy among the store's private files:
"Private, like the store it sits beside: the copy holds the same conversation,
so it gets the same `0600` — a new chat must not be the moment the human's umask
hands it to the group." Pinned by `the_new_chat_copy_is_private_like_the_store`;
with the call changed back to `Fresh::Box` the test reads 0644, so the next merge
that touches the call cannot silently repeat the leak. An identical twin of the
commit (`b0b8243`, same parent, same tree, same message, committed 74 s apart)
exists on the parallel line; both are ancestors of this base, and the repair is
one fact.

---

## 8.58 The response head, the ask, and one call's deadline (A1, A2, `0747460`, `190886c`, `b6a59c3`, merged `6d7453e`)

**The head is bounded before the body is read (`0747460`; A1).** `read_line` —
the only reader of the status line, the headers, the chunk-size lines and the
trailers — grew a `Vec` until a newline or EOF, with no limit, so the 80 MB
body cap never had a chance to matter: it bounds a body that has already framed
itself. The probe's endpoint answered `HTTP/1.1 200 OK` plus an 85 MiB header
line with no newline, and `get_json` returned `Ok((200, 2))` while peak RSS went
**7,804 kB → 185,712 kB** (+178 MB); 35 MiB/s over loopback meant the 600 s
deadline was room for tens of GB. `MAX_HEAD_BYTES` (**64 KiB**) now bounds each
line, the head as a whole, and the chunk-size and trailer lines; a head past it
is a refusal naming the endpoint and the size — **never `Framing`**, because the
endpoint sent it — and the connection is dropped. After, the same endpoint fails
with `http://127.0.0.1:PORT/v1/models sent a response head larger than 65536
bytes`, peak RSS stays 7,680 → 11,820 kB, and the endpoint's write dies after
1,441,792 bytes (a socket buffer's worth, not 85 MiB). The doc's reason is one
sentence: one named number for all of them, because the thing being bounded is
one — the memory a reply that has not framed itself may take from mush. Pinned
by `a_header_line_past_the_bound_is_a_refusal`.

**One ask spends one call deadline (`190886c`; A2, first half).** The 600 s
deadline lived inside `http::post_json` as `CHAT_READ_TIMEOUT`, so it was
re-armed for every attempt `retrying` made. The probe: a loopback endpoint that
accepted and said nothing cost three attempts and 2.89 s against the probe's 300
ms per-attempt deadline — three connections each waiting a full deadline, half
an hour for one ask with the shipped 600 s. `retrying` now takes the deadline
once, computes the one deadline, and hands each attempt only what is left; the
backoff is spent from the same budget, and the value travels as a parameter
(`retrying` → `ModelClient::chat` → `http::post_json` → `Watch`) so a test can
pass milliseconds and prove the road a human would otherwise reach in ten
minutes. The doc says the number belongs to the *call*, not to the attempt.
Pinned by `one_ask_spends_one_call_deadline` — 1.0017 s on a 1 s deadline across
three runs, one accept at the endpoint.

**A request that went out is never sent again (`b6a59c3`; A2, second half).**
Once `write_request` flushed, every failure was still retryable, and the pool
replaced a dead connection itself. The probe: a loopback endpoint that read one
whole POST and closed logged **two identical POST bodies** for one logical call.
`Unsent` now marks the failures that mean no complete request ever arrived — a
dial that never connected, a write that did not hand the whole request over —
and `retrying` repeats exactly it; a deadline, a dropped connection, an unframed
reply and a 5xx are final, with the endpoint named. The doc's sentence is the
ruling: "No complete request ever arrived, so the endpoint has nothing to have
read, run or charged for." Pinned by
`a_request_the_endpoint_received_is_never_sent_twice` (exactly one POST, 464 ms
on the test's deadline), `a_cut_off_body_on_a_real_wire_is_final_and_asked_once`,
`a_kept_connection_that_died_after_the_write_is_final`,
`a_call_that_cannot_connect_is_retried` and
`unsent_failures_on_every_attempt_name_the_attempts`.

**Recorded, not changed.** The second half deliberately drops B23's automatic
retry on a connection reset and B27's on a broken frame — both happen *after*
the request went out, which is the ruling's cost; the run ends and the human can
ask again. B23's and B27's rows in §2.75 carry that narrowing now. The audit's
A10 was left open here: a hex-parsable chunk size past the body cap was still
diagnosed as the endpoint's refusal — closed by `729e8fc` (§8.79), which makes
the claim a `Framing` break. A2's own remaining cost — the endpoint
may have run the call even though mush never saw the answer — is the price paid
so a human is never billed twice.

---

## 8.59 The git road stops at the worktree it was given (F1, F8, F10, F11, `8b029f0`, `0e506f2`, `d70c768`, `ac12012`, `34b647b`, merged `15324e5`)

**An ignored-only worktree is kept, not swept (`8b029f0`; F1).**
`git status --porcelain` hides ignored paths, so a run whose only output matched
the repository's own `.gitignore` read as "clean — nothing changed": `commit_all`
answered `Ok(None)`, `reclaimable` said `Landable(NothingCommitted)`, and
`reclaim` removed the checkout — the run's only copy with it. The probe:
`commit_all -> Ok(None)`, `status --porcelain -> Ok("")`, `reclaim -> Removed`,
the run's only output still on disk `false`; after, `Ok(Ignored(["ignored/report.txt", "run.log"]))`,
the kept sentence names 2 ignored paths, and the output is still there. One
reading — `git status --porcelain --ignored=matching`, its `!!` lines included
(`changes`) — answers both `commit_all` and the reclaim probe;
`Commit::Ignored(paths)` / `Work::Ignored` name the paths, `Commit::Nothing`
means nothing at all, and the human's bar still counts only what they could
commit. The doc names the defect it answers. Pinned by
`an_ignored_only_worktree_is_kept_and_named` (`mush-core`) and
`an_ignored_run_is_never_nothing_changed` (the bin). **The cost is named where
the code decides:** a child that merely compiled keeps its branch and checkout,
spending one of the `MAX_WORKTREES` slots until the human discards it — "the
trade H10's reclaim rule already makes". H53 is that cost's row.

**A commit never leaves the worktree it was given (`0e506f2` + `d70c768`; F8).**
`git -C <dir>` walks up to the enclosing repository, so a `.mush/wt/<id>` that
is no longer a worktree is an ordinary directory inside the human's checkout:
`commit_all` there staged and committed the human's own modified and untracked
files, on the human's branch, under mush's subject and identity. The probe:
`F8 is a worktree (plain dir)? false`, what `commit_all` would see
`Ok("M a.txt\n?? notes.txt")`, `commit_all -> Ok(Some("ba17e95"))`, the root's
HEAD moved to `ba17e95 mush #1: the brief` with `a.txt | 2 +-` and `notes.txt | 1
+` staged; after, `Err("/tmp/…/.mush/wt/1 is no longer a worktree")` and the
human's checkout untouched. `commit_all` now requires `dir` to be the root of its
own working tree (`.git` present and `git rev-parse --show-toplevel` equal to
it) and refuses otherwise — an `Uncommitted`-shaped failure the run's row
carries, never a commit somewhere up the tree. The second half is the guard
beside it: `reclaim_isolated` swept every `mush/<id>` branch on the human's key
with no reference to the tree, after a `stop_all` that only *sends* `Shutdown`,
so a node still running in its worktree lost the directory — and the next write
recreated it as a plain path inside the human's checkout, the state the commit
guard now refuses. The pass asks the tree first (`worktree_in_use`): a node in
flight, or one a busy child or unread result will wake, holds its checkout — the
same guard `refresh_git`'s sweep already uses — while a leftover with no node
holds nothing and is still reclaimed. Pinned by `a_commit_never_leaves_the_worktree`
and `ctrl_n_never_reclaims_a_worktree_a_live_node_holds` (both halves: a
thinking node and a resting parent with a running child).

**A failed `worktree add` is asked about, not inferred (`ac12012`; F10).**
Every error from `worktree_add` was read as "git made nothing". With a failing
`post-checkout` hook git had already created the branch and the checkout,
`lose_agent` handed the id back, and the next isolated spawn drew the same id
and died on `fatal: a branch named 'mush/1' already exists` — for the rest of
the conversation. The probe: `worktree_add(1) -> Err("Preparing worktree (new
branch 'mush/1')")`, "did git make the branch? true", "is the checkout there?
true", the next add the branch-exists error. The spawn now asks git what it made
(`git::resolve(root, mush/<id>)` and `worktree_path(root, id).exists()`) and
hands the number back only when both say nothing was created; anything found
reserves the numbers above it. The non-UTF-8 path once passed as `""` is refused
before anything runs. Pinned by `an_id_comes_back_only_when_git_created_nothing`;
a taken path is *something*, so the consecutive-id test now uses a failure git
has before it creates anything (a ref that cannot be locked).

**An isolated spawn works below the repository root (`34b647b`; F11).**
`worktree_add` refused any workspace without a `.git` entry while the base was
resolved one call earlier with `git -C dir rev-parse`, so in `repo/sub` the whole
isolated road was unavailable and the sentence the model read — "not a git
repository" — was false about the repository the human had opened mush inside.
The probe: resolve in the subdirectory `Some("74a4df5…")`, `.git` there `false`,
`worktree_add -> Err("not a git repository")`, `would git itself have made it?
Ok("yes")`; after, `Ok(("…/sub/.mush/wt/1", "mush/1"))` forked at the base. The
gate is git's own question (`rev-parse --git-dir`), and the refusals kept are the
ones a human can act on — not a repository, no commits, git's own message. The
doc says `dir` "may be any directory *inside* a repository". Pinned by
`an_isolated_spawn_works_below_the_repository_root`.

---

## 8.60 The read road costs what it says (B5, B6, A12, `273ada0`, `469853c`, merged `43bfff2`)

**The whole read is capped before anything is read (`273ada0`; B5, and A12
with it).** `read_file` read first and decided after — `READ_FILE_CAP` bounded
only `read_window` — so a 33 MiB text file came back whole (34,603,008 bytes,
peak RSS **36,344 → 70,008 KiB**) and a 512 MiB sparse blob was read whole before
"looks like a binary file" (peak RSS **2,636 → 526,752 KiB**). The same commit
closed the agent-and-wire half, A12: `write_file` called that uncapped read and
`lines().count()` on every overwrite to print `N → M lines` (over a 34 MB file:
37,352 → 72,184 KiB to answer `17825792 → 1 lines`). One `whole_read` now stats
first — a non-regular file and a file past the 32 MB cap are refused **before
anything is opened**, with `read_window`'s own sentence naming `run_command` —
and reads at most cap + 1 bytes, because a file can grow between the two;
`line_count` is bounded the same way and past the cap answers `More`, so a
partial number is never printed. After: the 512 MiB blob refuses at 2,536 →
2,544 KiB peak RSS, and the 34 MB file answers `More` with RSS unchanged (37,308
→ 37,308 KiB). The docs give both reasons: past the cap the memory a read costs
is the agent's problem rather than the file's, and how many lines is not knowable
without the whole read the cap refuses — a partial count would be a wrong one.
Pinned by `a_file_bigger_than_the_read_cap_is_refused_before_any_read` and
`a_whole_read_is_capped_and_says_where_to_go`. The contract audit's attack on
B5's proposed fix — a capped read must not invent the `before` count — is
answered by `LineCount::More` itself.

**A non-UTF-8 file is not edited through a lossy read (`469853c`; B6).**
`read_file` decoded with `from_utf8_lossy` into the same string `edit_file`
edits and writes back, so a Latin-1 `caf\xe9 = 1\nna\xefve = 2\n` came back with
two U+FFFD (`239,191,189`): bytes lost in the lines the model never touched. The
defect's second half was a definition: "binary" meant "contains a NUL", not "is
not valid UTF-8". The whole read now takes a `Decoding`, and `read_file` and
`line_count` are strict (`str::from_utf8`), refused naming the file, the first
undecodable offset, the encoding problem and the roads that can still change it
(`iconv` through `run_command`). `read_window` and `search` stay lossy **on
purpose**, each saying so in its own doc: a show-only road shows what it reads
and never writes it back, and a refusal there would hide the file from the only
tools that can diagnose it. Pinned by
`a_non_utf8_file_is_not_edited_through_a_lossy_read` and
`a_non_utf8_file_is_refused_whole_and_shown_in_a_window`; the positive twin — a
valid UTF-8 file with multi-byte characters still edits — passes in the same
test. "950 tests pass" is the body's own count at the landing.

---

## 8.61 A signal takes the quit road, and the ghosts are reaped (E1, E4, E5, `a68eea0`, `e7834a2`, `deaf586`, merged `0d033f1`)

**A killed mush leaves every process group running (`a68eea0`; E1).**
`SIGTERM`/`SIGHUP`/`SIGINT` got the kernel's default, which skips every `Drop`:
the session flush, `kill_all`, the writer join and the socket removal all never
ran. The harm S4/H9 fixed for the clean quit was left open on the road that
happens when a terminal window is closed. The probe, a real pty and `kill
-TERM`: the socket survived, an orphan wrote 31 → 51 B and was still alive after
two minutes, the scratch grew to 3,020 B. `signal_hook::flag::register` now sets
an `AtomicBool` the 30 ms event loop reads and turns into `App::signal_quit`;
the second signal re-raises the default. Pinned by
`a_signal_takes_the_quit_road_without_the_arming_press`, and the new pty
scenario drives it end to end. **Recorded:** an in-flight turn or question takes
the same road, which the doc says.

**A mush that does not unwind leaves its scratch behind (`e7834a2`; E5).**
`NamedTempFile`s are unlinked only by `Drop`, so a mush killed without unwinding
left `/tmp/mush-cmd-*` behind with an orphan still writing into it: 18 files in
`/tmp` before the probe and 20 at exit, the orphan's fd pointing at a scratch
grown to 3,020 B two minutes after mush died. The name now carries the owning
mush's pid (`mush-cmd-<pid>-<random>-<kind>`), and
`machine::reap_dead_scratch()` at start reaps pairs whose pid is not alive
(`rustix::process::test_kill_process`; `Ok` and `EPERM` both mean alive). Pinned
by `a_start_reaps_a_dead_mushs_scratch` and
`a_scratch_file_is_named_after_the_mush_that_owns_it`. **Recorded:** the files
stay in the temp directory rather than under `.mush/`, and a leftover is reaped
only at the next start. (B13's paste directory is the third `/tmp`-like path and
is still unpruned — §8.52.)

**A dropped hold kills what it held (`deaf586`; E4).** `Foreground`'s `Drop`
never killed, so a panicking actor left its group in neither map and the machine
held `Some((owner, cmd, None))` — an unreachable process group and a machine lock
nobody could clear. The probe left `[(2761487, "sleep")]` after a dropped hold
plus `kill_all`; the live trigger (an actor panic) was never driven, with C9's
overflow panic as the precedent. `Drop` now kills when `poll()` says `Ok(None)`
— not reaped means the pgid is not reusable — `handed_over` spares the job the
hold became, and `kill_owned` clears the holder when its owner is killed. Pinned
by `a_dropped_hold_kills_a_running_command_and_leaves_a_finished_one_alone`,
`a_dropped_hold_ends_a_real_process_group`,
`killing_an_owners_jobs_frees_the_machine_it_held`,
`a_handed_over_hold_does_not_kill_the_job_it_became`,
`a_cut_off_owners_job_is_killed` and `a_cut_off_owner_frees_the_machine_it_held`.
No `catch_unwind` was added: the unwinding roads run `Drop`.

---

## 8.62 What mush owns stays owned, with the panic road kept (E2, E3, `0d45ffd`, `8891ec9`, `5714052`, merged `cd15737`, landed `9b031a5`)

**A write cannot replace the workspace lock (`0d45ffd` + `5714052`; E2).** The
lock is a regular file, and `write_file` is a rename, so
`write_file(".mush/lock", …)` gave the name a fresh inode while mush's `flock`
stayed on the orphaned one: the probe's write answered `Ok(())` and a second
`acquire` answered `Ok`; two mushes ran against one store ("A alive? yes B alive?
yes", the lock naming B). `mush_core::session::STORE_FILES` and `store_file` now
refuse the store's own names by **name**, not by shape, and the `Guard`'s
`Identity::still_mine` refuses a session save when the locked inode was replaced
— the human's `mv` road — so the first mush stops writing. Pinned by
`a_write_cannot_replace_the_workspace_lock`,
`a_write_will_not_replace_the_stores_own_files` and
`a_save_refuses_a_lock_file_that_was_replaced`. The processes audit notes that
B3's shape refusal cannot reach this file — the lock is a regular file — which
is exactly why the names are refused by name.

**A finished leader takes its group with it (`8891ec9`; E3).** A job's end was
the leader's alone, so a command that backgrounds a child escaped the registry,
the 4 h ceiling, `Stop` and the quit. The probe: `poll` answered
`Some(Exited(0))` while a `sleep` lived; after `kill_all` the group still held
`[(2760919, "sleep")]`. The `Job` trait gained `end_group`: the watcher, seeing
`poll` end, asks `/proc` what the group still holds and only then sends
`kill -9 -<pgid>`; `GroupEnding` carries nothing/count/failure into the
completion line. Pinned by `a_job_takes_the_whole_group_with_it`,
`a_completion_line_says_the_group_was_stopped` and
`a_background_job_does_not_hold_the_tool_hostage`. **Recorded:** without
`/proc`, `group_members` answers empty by design — nothing is signalled on a
guess.

**The reconciliation.** `cd15737` ("what mush owns stays owned, with the panic
road kept") is the hand resolution of the E2/E3 branch against master's E4 fix
(§8.61), conflicting in `jobs.rs` and `main.rs`. Both designs stay: master's
`handed_over` + `Drop` ends a command that is *still running*, and the branch's
`left: Option<GroupEnding>` + the road that takes the group when a command ends
**by itself** — seen by `Foreground::poll` — records the count for
`left_behind()`. `main` keeps master's dead-scratch reap and whole-run signal
guard under the branch's binding name, with `Some(lock.identity())` handed to
the session writer. The rewritten `Drop` doc gives the rule in one voice.
**Recorded, not pinned:** a command that ended by itself and whose end is first
seen at `Drop` — an unwinding panic between two `wait_bounded` polls — is not
swept there, because `Drop` ends only a *running* command; E4's rule for a
reaped leader wins that corner, it predates the merge, and no test pins it. That
residual is H52.

**A correction this record owes the campaign's own mapping:** the mapping called
`cd15737` "the reconciled E2/E3+E4 merge". E4's own commit (`deaf586`) had landed
in `0d033f1` before it; what `cd15737` reconciles is the E2/E3 branch with that
landed fix, and `cd15737` is the branch merge whose content reached the master
line as `9b031a5`.

---

## 8.63 The frame never waits, and the box paints the line being typed (D4, D5, `0951b85`, `61ec689`, merged `a378e86`)

**The model list is fetched, not waited for (`0951b85`; D4).** `/url`, `/models`,
`/provider` and `Ctrl-P` called `refresh_models`, which ran `http::list_models`
on the thread that paints; against a listener that accepts and never answers the
audit measured the frame frozen for **10.036 s** (the commit's own probe:
10.034 s inside `update`). The roads now take the startup discovery's thread:
`refresh_models` paints `fetching models from …`, spawns the fetch, and the list
arrives as `Msg::Models`; a second ask for the same endpoint does not stack a
thread, and an endpoint or provider switch drops the list it no longer
describes. The doc names the measurement. Pinned by
`a_command_road_never_waits_on_the_endpoint`: each of the four roads returns in
well under 100 ms against the silent listener, each answer arrives as
`Msg::Models` when the listener answers, and the bar says `no models from …`
only when that empty list lands. This is also what made `docs/mush.md:1144`'s
"Keypress → screen | < 5 ms | update touches only UI state" true again — the row
was drift while these roads blocked, and the audit named it.

**The box gives the text its row before the attachments (`61ec689`; D5).** At
40×12 with three attachments the box asked for six rows and was granted five, so
`text_rows = field.height - attachments.len()` went to zero: three
`▣ shots/shot0.png (png · 0 B)` rows painted, the draft nowhere, and the cursor
blinking in a file name. The audit's probe at 40×12: `height: 5 inner: 3 cursor:
Some((13, 9)) text_painted=false`; the commit's own probe records `asked=6
box_height=5 inner_height=3 cursor=(23, 9) attachments=3 text_painted=false`, and
at 40×10 `box_height=3 inner_height=1 cursor=(23, 7) text_painted=false`. After:
40×10 `cursor=(23, 7) attachments=0 text_painted=true`, 40×12 `cursor=(23, 9)
attachments=2 text_painted=true`. `content_rows` now owns the painted answer —
it caps the attachment rows at one row short of the room the box really has, so
the text keeps its row before the pictures and the cursor is clamped inside the
text area, never onto an attachment row. The doc says it is "The one owner of
'how many rows the content has' when the box is painted" and that "the text's
own row comes first". Pinned by `the_box_paints_the_line_the_cursor_is_on`;
reverting the cap makes the test fail at the cursor assertion, as the old code
did. **A disputed number, recorded as disputed:** the audit's and the commit's
probes disagree on the cursor's *x* for the same 40×12 state (13 and 23); both
agree on the fact — the cursor was on an attachment row and the draft was
unpainted.

---

## 8.64 A key belongs to a host (C2, C3, C6, D6, `072d655`, `2080512`, `fa28769`, `df781fd`, merged `86a0fb1`)

**A provider typo is named, not swallowed (`072d655`; C2).** Two
`if let Some(provider) = …` arms dropped a name they could not read without a
word — the home config's and the session's. The provider stayed at `Custom`,
whose endpoint is a LAN host, and the key still in hand was sent there: the
damage `MUSH_PROVIDER`'s check exists to prevent, on the two roads it did not
cover. The probe, the real binary with a home config holding
`{"api_key": "sk-home-KEY-…", "provider": "deepsek", "model": "deepseek-flash"}`:
before, `--print-config` printed `endpoint http://rubendpc:8078`, `provider
custom`, the masked key, `EXIT=0`, and no line said the provider was ignored;
after, `mush: home config: unknown provider \`deepsek\` (try deepseek or custom)
— /tmp/…`, `EXIT=1`. `resolve`/`resolve_with` return
`Resolved { config, notices }` now; the home-config arm is an error naming the
value, the providers the table holds and the file's path, while the session arm
is a notice naming the provider and **keeping the endpoint the session stored**,
because a session is a file a workspace carries and a typo in it must not take
the TUI down. Pinned by `a_home_config_typo_names_the_value_and_the_file` and
`a_session_typo_is_reported_and_keeps_its_endpoint`.

**A home config mush cannot read is said and kept (`2080512`; C3).**
`UserConfig::load_from` fell back to defaults on *any* failure — unreadable, not
JSON, one field of the wrong type — with nothing said, no bar line and no copy
kept; and because `save_to` merges by parsing the existing file, an unreadable
file had nothing to merge, so the first `/key`, `/url` or picker replaced it with
mush's four fields and the header, the human's key inside it. The probe:
`{ "api_key": "sk-secret", ` → before, `--print-config` printed `api key
(none)`, `EXIT=0`, no line why; the same file through a pty with `/key
sk-new-probe-…` rewrote the config with `backup exists: False`. After,
`home config unreadable — could not read …c3-config.json — EOF while parsing a
value at line 1 column 26; using defaults`, and `backup exists: True` holding the
original bytes. `load_from` returns `Loaded { config, complaint }`; `main` reads
the file once and says the complaint before the first frame; `--print-config`
prints it as a `home config` row, sanitized; `save_to` moves an unmergeable file
to `.bak`, then `.bak.2`, never overwriting, and refuses the write if it cannot.
The doc: "'Missing' and 'there and unusable' are different facts about a layer",
and a save must not be the thing that loses the human's key. Pinned by
`an_unreadable_home_config_is_said_and_kept`,
`describe_reports_an_unreadable_home_config` and
`an_unreadable_home_config_travels_to_the_dump`.

**The key and the host move together (`fa28769` + `df781fd`; C6 and D6).** The
home key and the host it is sent to were decided by different layers of one
precedence chain, and the stored ones could move the host under the key: a
cloned repository's `session.json` put the machine-global key on the wire to a
host the *file* chose seconds after startup (D6), and `/provider` handed it to
the vendor while `/url` handed it to whatever host the human typed (C6). The
probes, a real binary and a loopback listener with a home config holding a probe
key and a session naming the listener's host: before, `GET /v1/models` carried
`Authorization: Bearer sk-probe-home-KEY` and no line said so; after,
`Authorization=None` and the bar carries `session: endpoint … is another host —
no api key for this endpoint; the key was not sent — /key <secret> sets one
(saved to …)`. The runtime half: `/url http://127.0.0.1:B` produced
`Authorization: Bearer sk-probe-url-KEY` at B before, none after, with the bar's
`endpoint: … · ctx ~8.2k · no api key for this endpoint — /key <secret> sets one
(saved to …)`. One function,
`Config::forget_key_if_host_changed(was)`, is the rule — the host is the URL's
authority, port included; scheme and path are not — applied at both resolution
doors (`resolve_with` when the session layer re-points the endpoint, and
`ConfigCell::edit` after every runtime edit); `switch_endpoint` and
`switch_provider` return whether the key was forgotten and the `/url`/`/provider`
arms append `no_key_hint()`, one spelling for both; and `userconfig::save_to`
states `api_key` even when it is `None`, so the old host's key is not merged
forward. The doc is the rule: "A key belongs to a host (findings C6, D6): the
request head carries it to whatever `base_url` names, so an endpoint that moved
under the key is a secret handed to a host the human never aimed it at". Pinned
by `a_stored_session_cannot_take_the_home_key_to_its_own_host`,
`the_host_of_an_endpoint_is_its_authority`,
`a_host_change_forgets_the_key_in_both_copies`,
`a_save_states_the_key_even_when_it_has_none` and
`switching_to_a_provider_forgets_the_key` (which pins the same-host twin as
well). **Recorded, not changed:** a change that only moves the path, or the same
host under another scheme, keeps the key — one destination.

---

## 8.65 The spawn's arguments are the model's only interface (A7, F12, F9, `d8a1a04`, `6f47149`, merged `bf55c3e`)

**A wrongly-typed argument is refused, not defaulted (`d8a1a04`; A7 and F12,
plus B10 and A19's third sentence).** Four fields the model sends were read with
a type test falling back to a default, and the default changed what ran:
`exclusive: "true"` ran (two benchmarks interleaved — the one thing the lock
exists to prevent); `detach: "yes"` became a foreground call (a 60 s block and,
for anything longer, the 120 s kill `detach` promises not to apply); `base: 7`
spawned a *shared* child whose edits land in the parent's checkout and whose
branch does not exist (F12); `title: 7` spawned too, silently dropping the row's
name. The edit batch's own fields followed: `replace_all` (B10) and the
`old_string`/`new_string` pair, while `arg_string` no longer calls a present
non-string "missing". `tools::arg_bool` refuses a wrong type with the sentence
it already owns; a new `arg_string_opt` treats `null` as absent and any other
non-string as refused. The docs: "'missing' is a true sentence about a field
that is not there and a false one about a field the model sent as a number"; a
`base` that reads as "no base" "drops the child's worktree and puts its edits in
the parent's checkout (finding F12)". Pinned by
`a_wrongly_typed_exclusive_is_refused_not_read_as_false`,
`a_wrongly_typed_detach_is_refused_not_read_as_foreground`,
`a_wrongly_typed_command_is_refused_not_read_as_missing`,
`a_wrongly_typed_base_is_refused_never_read_as_no_base`,
`a_wrongly_typed_title_is_refused_never_silently_dropped` and
`a_wrongly_typed_edit_field_is_refused_never_defaulted`. **Recorded:** the one
type-tested read left is `read_args`, which picks a label for the screen and
decides nothing. The positive twin is pinned: `base: null` still means absent
and spawns the shared child. A19's third sentence — `arg_string`'s "missing"
misdiagnosis — is fixed here; A19's other two sentences stay open (§8.52).

**`HEAD` is the spawning agent's `HEAD` (`6f47149`; F9, H7's class).**
`spawn_agent(base="HEAD")` resolved the name in the application root, not in the
spawning agent's workspace, so a nested child silently forked from someone
else's history while the reply named it `at 75f8ee7` as if the caller had asked
for it. The probe: a parent on `mush/1` with one commit of its own had HEAD
`df04ff9…`, the application root `75f8ee7…`, and the child forked at `75f8ee7…`.
The base now resolves in the *caller's* workspace (`actor.ws.root()`), with the
object store shared so `worktree_add` still runs from the root with the resolved
id; the base description and the delegation policy say whose `HEAD` that is; and
one spelling, `agent::fork_base`, serves both the child actor's landing base and
the UI's `App::fork_base`. The doc gives the rule in one sentence: a name is
resolved "in the workspace whose view the name was spoken in: the object store is
shared … while the word `HEAD` is not (finding F9)". Pinned by
`a_nested_base_head_forks_from_the_parents_worktree` and
`the_actor_and_the_ui_agree_on_the_base`. The second test also closes the
"merged" / "1 commit nobody merged" disagreement between the actor's name and
the UI's derivation of the base.

---

## 8.66 An agent that is gone must not come back (A4, A5, A8, D16, `66dba03`, `4702c94`, `3a37c02`, merged `0c49a8b`)

**An event for a gone id changes nothing (`66dba03`; A4).** The door in
`App::update` handed every `Msg::Agent` to `on_agent` without asking whether the
tree still held the id it named. The window is one frame wide and real — the
event loop drains actors without blocking and the reap tick runs after the drain
— and `Chat::push_message` writes unconditionally while `Chat::forget` is called
only by the reap for `tree.past_history()` ids, so a ghost id was unreachable by
every reaping path forever. The probe: 51 finished children and one tick, then a
late `Message`/`Notice`/`Error`/`SystemPrompt`/`Done`/`Spawned` for the reaped
id — `transcript(id)` came back `["a reply the reap was too late for"]`, two
notices, 73 B of phantom weight, the stored session +313 B, and a restart
restored the false `✗`; the `Spawned` for the gone parent added a 52nd row while
the child it named was never told to end. The guard the neighbouring doors
already have (`tree.has`) now gates this one: an event whose subject the tree has
reaped is news about nothing — no transcript, no notice, no weight, no row — and
a `Spawned` for a gone parent answers its own channel with `Shutdown` instead of
leaking a child nobody owns. Pinned by
`a_late_event_for_a_reaped_agent_changes_nothing`. **Residual, suspected:** the
same one-frame window can `Shutdown` a run just started (`park_history`, §8.21);
the audit did not stage it, and H51 is its row.

**A run's ending reaches the UI before its parent (`4702c94`; A5, and §8.39's
recorded residual closed).** The child sent its parent `ChildDone` *before* it
emitted the run's ending, so a parent scheduled in that gap folded the result
and emitted `ResultRead` (clearing `result_unread`), and the child's `Done` then
landed and re-armed the mark with nothing left able to clear it — `record_child`'s
`fresh` already spent, `delivered` already naming that run. §8.39 recorded this
as "the `✉` re-arm, recorded rather than fixed"; the audit measured it worse than
recorded: the stale mark also pinned the child's thread (`may_park`'s
`result_read`) and exempted its node from the fifty-node window — a pty run
stored 57 nodes where ≤50 should hold, with threads alive at rest, and three
n=20 runs left unread sets `{2,19}`, `{1,6,8,14}`, `{2,5,11,19}`. The fix is one
reorder: emit the run's ending before `tell_parent(ChildDone)`. Both events
travel the one UI channel, so the emit makes the order a contract instead of a
race, and the rule is written on the `ResultRead` variant and at the emit site:
"Its order against the child's own ending event is a contract, not an accident …
so a `ResultRead` can never be ordered before the `Done`/`Error`/`Stopped` it
answers." Pinned by `the_runs_end_reaches_the_ui_before_the_parent_hears_the_report`,
`a_read_that_lands_before_the_end_is_re_armed_and_pinned` and
`twenty_children_end_read_and_park` (which stores zero `result_unread: true` and
parks the twelve children outside the warm window). §8.39's alternative — a run
number on both events — was not the road taken. **Recorded, not staged:** after a
restore, a `#N done:` delivery can be delivered twice; it is code-read only, and
H35's row now carries it.

**The store and the meter read a bounded view (`3a37c02`; A8, and D16 sideways).**
The pane's record was the only copy of a conversation the UI had and nothing
trimmed it — `push_message` appends, only a `Compact` removes, the actor's
`trim_history` touches the actor's own list — so on a window whose fold cannot
fit (`fold_request_fits` refuses `SCHEMA_TOKENS + prompt + 1024 >
context_tokens`, every window below ≈5.5 k tokens) the actor kept cutting turn
after turn while the pane, the session file and the `ctx` meter grew. The probe,
24 runs of a pair through `App::update` on a 5,376-token window: the pane's copy
43,902 B, `used_weight_for(ROOT)` 47,176 against the 8,064-byte budget, the meter
`ctx 15.7k/2.7k over (fold 2.4k) 5.4k`, `session_snapshot` 45,204 B (the audit's
pty read 24,417 → 48,621 B across 12 → 24 runs and 48,999 B stored). After:
`used` 7,138 ≤ 8,064, a 4,091-byte stored row, a meter without `full`/`over` —
and the pane still holding the whole 43,902 B. The fix is `Chat::bounded_transcript`:
the system prompt, the transcript and the same `trim_history` an actor's own list
gets, with the dropped-turns note put back where the dropped turns were, read by
`used_weight_for` (meter and both attach gates) and `session_snapshot`. The doc
says why the number is about the next request: "weighing the record made the
meter say `over` forever on a window whose fold cannot fit while every request
that went out still fitted (finding A8)". Pinned by
`the_store_and_the_meter_hold_a_bounded_view`. **Recorded:** the pane keeps the
whole conversation in memory on purpose — a cut bounds the request, not the
reading — and the prose that claimed the file's bound was `CHILD_HISTORY × the
fold trigger` now says what bounds it. This closes H16's residual. The same
commit closes D16 sideways: `used_weight_for` used to early-return 0 when an id
had no transcript entry, dropping a just-published prompt (probe
`used_weight_for(#1)=0` while prompt weight was 3,006); the bounded view weighs
`system_for(id)` with nothing said, and
`an_agents_weight_is_its_own_prompt_plus_its_transcript` pins the own-prompt sum.
The audit's own pin for D16 does not exist, and the doc sentence "or that has no
transcript at all, weighs nothing" is now stale.

---

## 8.67 A thread mush owns cannot freeze it, and every road into the socket is bounded (E7, B11, `bbff494`, `1bd5e2a`, merged `0bf64db`)

**A flush cannot outlive its writer (`bbff494`; E7).** A writer with no worker —
the shape a worker that died or never started leaves — hands a snapshot over and
flushes from another thread, and the flush never returns: the caller parks its
`Sender` in `pending.waiting` and only the worker can wake it, while the caller
is the thread that reads every key, resize, `Ctrl-Q`, `Ctrl-C` and `Ctrl-X`. A
worker gone by panic, or a write wedged on an NFS mount or a full disk, froze the
whole TUI for good, and the transcript since the last write went with it
silently. The probe: `PROBE parked-writer flush returned within 5s: false (waited
5.000126974s)`; after, the same probe returns in 51.9 µs. Three things in one
file: the worker is `Builder::new().name("mush-save")` and a refused spawn is
`Writer::new`'s returned error (`main` keeps a `Writer::without_worker` carrying
the reason, so the workspace still opens and every flush reports the failure on
the status line); `Inner.alive` is cleared by the `Alive` guard on the worker's
own stack — a `Drop`, so a panic clears it — and `flush` checks it before
parking, returning "the session writer is gone — the session was not saved" at
once; and `flush`'s `recv` is bounded by `FLUSH_DEADLINE` (10 s), because "Ten
seconds is far past what a local write owes and far short of a freeze". Pinned
by `a_flush_with_a_dead_writer_returns_with_an_error`,
`the_writer_thread_is_named` and
`a_timed_out_flush_leaves_the_next_one_able_to_try`. **Recorded:** a timed-out
flush is not sticky — it marks no worker dead and takes its own waiter back out
of the queue, so a run of timeouts cannot grow it, and the write that lands late
still lands.

**Every road into the socket is bounded (`1bd5e2a`; B11).** One client wrote 32
MiB with no newline and the server read all of it into a `String` (probe:
`VmRSS: 8940 kB` → `43804 kB`, +34 MiB of heap for one line, and any amount would
go); 100 clients that connected and then said nothing bought 100 threads that
lived until they left (`Threads: 3` → `104`); any same-user process can reach
`.mush/mush.sock`. Three bounds in one place: `MAX_REQUEST_BYTES` (**64 KiB**)
with `read_line_capped` keeping at most the cap and draining to the newline
(constant memory; `bad_request` names the cap and the connection stays open so a
following good line is answered); `MAX_CONNECTIONS` (**64**), the one past it
answered `unavailable` with no thread of its own and a served connection's slot
returned by a `Drop` guard; and `IDLE_TIMEOUT` (**30 s**, the server's half of
the bound `ask` already puts on itself) through the socket's own read timeout.
The doc is the rule: "the same decision every other input road in the tree
already makes (the clipboard's `READ_CAP`, the HTTP body's `MAX_BODY_BYTES`): a
buffer whose size the client does not choose". Pinned by
`a_request_line_is_capped_and_a_client_is_reaped` and
`a_line_at_the_cap_is_kept_and_a_line_past_it_is_refused`. After: the same 32
MiB costs ~+1.8 MB of RSS, 100 idle clients cost 64 connection threads, and the
client past the cap reads
`{"error":{"kind":"unavailable","message":"the attach surface already holds 64 connections — retry when one is free"},"id":null}`.

**The merge.** `0bf64db` is the hand resolution of E7 and B11 with the lock's
identity: `Writer::new(root, lock)` and `without_worker(root, lock, reason)`
union the spawn-`Result` road with `Some(lock.identity())`, the same union in
`main.rs`'s `match` and in the `app_writing` fixture. **A correction this record
owes the campaign's own mapping:** the mapping named `948b954` as this merge.
`948b954` carries the same parents, subject and author time but is **not an
ancestor** of this base; the landed twin is `0bf64db`, and their trees differ
only by a rustfmt wrap of `without_worker`'s signature — so the record cites
`0bf64db` and names the other hash rather than pretending it landed.

---

## 8.68 An elided tail is one stop, and the fold is one value (#71/#72, `f915022` merged `f47bfdb`; `f15a809` + `59f3a9f` merged `1aceabb`, replayed `6a6db8c`)

Two branches reworked the same road in `chat.rs` — how a message's painted rows
and their caps are computed and tagged — and the merge is where they were
reconciled. Neither answers one of the 104 findings: #71 comes from the human's
own report about the select mode, measured with the TUI audit's probe, and #72
from the human's ask for a per-kind row setting; the pane-and-text duplication
pass (`b1f1642`, §8.70) landed between them and read the tree #71 left.

**The select cursor steps where the pane painted (`f915022`; #71).** The cursor
was a source line, and a tool result past the pane's cap paints one `…` row for
every line that cap hid: every hidden line was its own stop, all of them on the
same painted row. The human's words: stepping with `Ctrl-Y`, "especially inside
an elided command output, the selector sits there a while instead of treating the
`…` as one line" — and a `Shift` selection over the block took only the one line
the press landed on. Measured with the audit's probe (a 30-line result, `Ctrl-Y`,
then a `↓` per hidden line): **22 `↓` presses sat on that same `…` row** before,
**1** after. A stop is a painted row now, and the hidden tail is one row: `Stop`
is `Line(n)` or the one `Tail` the `…` stands for, the row map says which each
painted row is (`Chunk::rows`, `render_message`), and the cap's boundary comes
from the painter's own wrap walk (`capped_result` at that landing; it is
`folded_rows` after the reconciliation below), shared with the selector's
stop list (`Stops`). Which lines a cap hides is the pane's measure and only the
frame knows it, so the pane publishes the width it painted the mode's rows at
(`Selecting::measure`). A selection whose range includes the elided stop covers
the whole block — each stop covers a span (`Stops::span`), one line or every
line the cap hid — so `Shift-↓` onto the `…` and `Enter` hands the clipboard
every hidden line, in order and byte for byte, and the copy still hands out
source, never screen. The doc on `Stop::Tail`: "It names no line because *which*
line that is is the pane's measure … not the transcript's: this is the one
spelling of 'the hidden tail'." Pinned by
`the_cursor_steps_over_an_elided_tail_in_one`,
`shift_over_the_ellipsis_copies_the_whole_tail` and
`a_line_behind_a_tool_results_cap_stands_on_the_ellipsis` (updated); the mode's
other boundary pins are unchanged. **Residual:** until a pane has painted a
measure, no line is known hidden and every source line is a stop.

**The fold is one value, per kind of block (`f15a809` + `59f3a9f`; #72).** A pane
painted a multi-line block by whatever its arm happened to do: `render_message`'s
"tool" arm kept `const SHOWN: usize = 8` and painted eight wrapped rows plus a
bare `…`, while every other arm that carried a block had no cap at all — so a
new kind of block could only arrive uncapped by accident. Measured before: a
25-line tool result painted ten rows (eight, the bare `…`, the blank); after, the
same eight rows, `… +17 more lines` and the blank, while the reasoning's 25-line
block paints 26 rows either way and at a 3-row setting paints three and
`… +22 more lines`. The number is a value now — `Fold`, one row count per `Kind`
of block — and the arms that paint a block ask it; `Kind::slot` is a `match` with
no wildcard and the table is exactly `Kind::COUNT` long, so a variant added
without a number stops the crate from building rather than painting itself whole.
The `…` row is `elision`, the tree's own `more_label` words, so pane and foot say
it one way. The failure rule lives on the value too: a block that reports a
failure is never what the fold gives up, so a 0-row setting still paints a
result's own `! error: …` row and the `…` after it. The second commit is the
second kind: a child's or a job's completion lands in the parent's conversation
as a `user`-role line (`push_line`), and the `user` arm had no cap at all — a
job's line carries its whole output tail — while the `spawn_agent` result beside
it folded to eight rows. Those lines are `Kind::Mush` (a child's or a job's
report, a fold's carried summary, the dropped-turns note) and the words another
agent addressed to this pane are `Kind::Brief` (the brief a child's pane opens
with, a parent's steering after it); both go through the same `Fold`, and
`voice_kind` is the one classification of a voice, its two `None`s the
exemptions — the human's own lines, however long, and the model's reply, which is
the conversation's own text. Measured: a 25-line child's report painted 26 rows
before, eight rows and `… +17 more lines` after, with `Ctrl-Y` still copying its
25 lines byte for byte. Pinned by the tests the code carries (the bodies name
none): `every_multi_line_block_the_pane_writes_goes_through_the_fold`,
`a_failure_is_never_what_the_fold_gives_up`,
`the_humans_lines_and_the_reply_are_never_folded` and
`a_childs_report_folds_like_a_commands_result`. **Residual:** the reasoning's
slot is `usize::MAX` today — the text the human pressed `Ctrl-T` to read is shown
whole — and `Fold::with` is `#[cfg(test)]`-gated until the setting surface it
exists for lands.

**The reconciliation (`1aceabb`, replayed onto the master line as `6a6db8c`).**
The two branches conflicted in nine hunks over the same road: `render_message`,
the map it returns and the walk that reads it. The union keeps both —
`render_message` takes the `Fold` and returns the `Stop`-tagged row map, and
every arm (the human's lines, the reply, the tool result, mush's reports and the
brief, the reasoning) asks the fold for its kind's number, keeps the failure
exemption and tags each painted row. The arithmetic moved into one walk,
`folded_rows`, read by the painter (`folded_marked`) and by the select mode
(`Stops::of` through `Chat::stops_at`), so a child's report folds and steps
exactly like a command's result; `folded_block` is the one classification of
kind and head, and `step_line` became the stop-based `step_stop`. Everything
else both branches state survives: `Stops::span`, `Selecting::measure` and the
`…` copy road, the four `Kind`s and the failure exemption, `more_label` as the
one wording.

**What this supersedes.** §8.49's sentence — "a tool result is copied *whole*
even past the eight rows the pane paints of it (the ninth row is the `…`, and a
line the cap hides still has that row to stand on)" — was true of the code
§8.49 landed and is history now: a hidden line is the one `Tail` stop, and
`Enter` on it takes the whole tail; the pin §8.49 names,
`a_line_behind_a_tool_results_cap_stands_on_the_ellipsis`, was updated in place
by `f915022`. The pane-and-text duplication pass's candidate 1 — `mark_rows`
re-wrapping what `marked` wrapped — is **not** what these commits fixed: at this
base `mark_rows` and `View` still exist, no `wrap_tagged`, `markdown_tagged_rows`
or `edge_line` is in the tree, and the row-map double count the pass named is
still open in `docs/refactor.md` §11's queue (§8.70).

---

## 8.69 What the model reads back is what the file holds (B7, B8, B9, B15, `feb5a85`, `2d0fe86`, `b13aeed`, `dd23693`, merged `a1cf829`)

Four small ones from the tools-and-workspace audit, one branch, all of them the
same sentence: a read road may show the model anything it likes except a fact
the file does not hold.

**A CRLF file is read and edited consistently (`feb5a85`; B7).** `read_window`
split with `str::lines()`, which drops the `\r` of a CRLF ending, so on
`alpha\r\nbeta\r\ngamma\r\n` the window was `alpha\nbeta\ngamma` and a two-line
`old_string` copied out of it answered `old_string not found`; a one-line
`old_string` that matched inserted LF and produced
`alpha\r\nB1\nB2\r\nbeta\r\ngamma\r\n` — mixed endings the human's diff shows as
a whole-file change. The window now says its lines end CRLF (a window still shows
a line's *text*, not its bytes) and the edit road refuses an `old_string` or
`new_string` holding a line break or a `\r` in a file whose lines all end CRLF,
naming the endings and the roads that can still work (`run_command` with `sed
-i`/`perl -pi`, or `write_file`); a single-line edit lands byte for byte, and a
mixed file keeps the byte-exact rule — `is_crlf`'s doc: "A *mixed* file (one
bare LF anywhere) is not one: an edit into it can be byte-exact, and calling it
a CRLF file would refuse work the bytes allow." Pinned by
`a_crlf_file_is_read_and_edited_consistently`.

**A searched line is the file's own bytes (`2d0fe86`; B8).** `search` built its
match line with `text::truncate`, which begins with `sanitize` — the *display*
rule — plus a `trim_end`, so on `before\x1b[31mneedle\x1b[0m after\n` the model
was told `esc.txt:1: beforeneedle after` while the file, and its window, held the
escape sequence; a model copying the shown line into an `old_string` reads "not
found" against the file it was just told about. `search` now reads through
`text::file_lines` (the file's own split: a `\r` before a `\n` stays on its line)
and reports the line raw — no sanitize, no `trim_end`, CRLF ending included — and
a line past `MATCH_LINE_CAP` is cut on a character boundary with the cut marked
(`match_line`), naming `run_command` (`rg -n`) as the road that prints it whole,
because a partial line a model mistakes for the whole is a wrong fact, not a
short one. Pinned by `a_searched_line_is_the_files_own_bytes`. Painting is the
pane's job: the pane sanitizes its own copy.

**A listed path can be opened again (`b13aeed`; B9).** `rel` folded every `\`
into `/`, and on this box `\` is an ordinary byte of a file's name: `a\b.txt`
listed as `a/b.txt` and `read_file("a/b.txt")` answered `No such file or
directory`; a file named `a<LF>b.txt` came back as one entry holding a newline,
which the one-name-per-line listing read as two entries, neither of them the
file. `rel` now keeps the name the filesystem holds — the one rule left is the
one for a path outside the root, shown whole — and the model roads go through
`name_for_model`, which hands over only a name the tools can open again (valid
UTF-8, no `\n`/`\r`, and nothing `resolve`'s own trim would change), counting
the rest as `unnamed` and naming the shell (`ls -b`, `rg`) as the road that
reaches them. The dropped-image placeholder no longer invites a read that would
fail. Pinned by `a_listed_path_can_be_opened_again` and
`a_dropped_image_whose_path_holds_a_newline_still_leaves_one_line` (updated).
**Recorded:** unnameable files are counted, not handed over; a directory holding
only an unnameable name is not "no files".

**`sanitize` strips the marks that command a display order (`dd23693`; B15).**
`sanitize`'s doc said it removed the bidi embedding and isolate characters, but
`invisible` kept the bidi *marks*: LRM U+200E, RLM U+200F and ALM U+061C
survived, while LRI U+2066 and RLO U+202E were removed as documented — and the
marks are exactly the characters that pick the order a neutral run is laid out
in, so one invisible character can make a painted name read as a different path.
The three marks now go with U+202A–U+202E and U+2066–U+2069, with the reason
beside the list: "A mark is the same command spelled invisibly … and the whole
family goes (finding B15)." The doc also names what is *kept* and why: ZWJ and
ZWNJ are orthography (an emoji sequence, a Persian word), and ZWSP, BOM and SHY
reorder nothing and take no column. Pinned by
`sanitize_strips_what_its_doc_says`, which reads all seven display-order
characters through `sanitize` and `truncate` and keeps a ZWJ emoji whole.
**Recorded:** ZWSP, ZWNJ, ZWJ, BOM and SHY stay — dropping them would be the
sanitizer editing the text it was asked to make safe.

---

## 8.70 The four duplication passes: ≈530 lines, ≈3 %, and the conclusion they support (`b1f1642`, `5e782f7`, `37f2724`, `79a18e8`)

After the fix waves, four blind duplication passes read production code only and
wrote four reports; none of them changed a line of code — each is a docs commit
adding one file, landed by `a1cf829` (pane-and-text), `50e4d9e`
(store-workspace-cli), `b654717` (actor-tools-wire) and `38d0438`
(app-and-panes). Their method is the same in four areas: read the production
region end to end, find shapes repeated across sites, normalise lines (string
literals and numbers replaced) and count with `sort | uniq -c`, measure each
candidate's net lines and risk, name what *looks* duplicated and is deliberately
not, and list the live disagreements the shape has already produced. Every line
reference is against their base `f47bfdb`.

| report | area (production code lines) | net | candidates |
|---|---|---|---|
| `docs/dedup/pane-and-text.md` (`b1f1642`) | `chat.rs`, `ui.rs`, `text.rs`, `theme.rs` (2,644) | ≈ 120 removed (4.5 %) | 5 ranked + 6 below the bar |
| `docs/dedup/store-workspace-cli.md` (`5e782f7`) | 15 files: the store, the workspace and the CLI (headline ≈4,600) | ≈ 180 removed | 10 ranked |
| `docs/dedup/actor-tools-wire.md` (`37f2724`) | `agent.rs`, `http.rs`, `model.rs`, `jobs.rs`, `machine.rs`, `signals.rs`, `events.rs`, `clock.rs`, `ids.rs` (4,865) | ≈ 117 removed (2.4 %) | 10 ranked |
| `docs/dedup/app-and-panes.md` (`79a18e8`) | `app/{mod,tree,screen,keys,commands,settings}.rs`, `input.rs` (4,800) | ≈ **+112** — net-positive (2.3 %) | 18 ranked |

The four state ≈530 net lines between them (120 + 180 + 117 + 112); against the
areas' own headline figures (2,644 + 4,600 + 4,865 + 4,800 = 16,909) that is
≈3.1 %. The store report's header figure does not match its own per-file list
(9,890), and no percentage is stated there, so the share moves between ≈2.4 %
and ≈3.1 % depending on which reading is used; the campaign's own brief said
~530 of ~19,600 ≈ 2.7 %, and neither 19,600 nor 2.7 % is reproducible from the
four files — the record keeps the reports' own numbers. The largest single item
anywhere is 55 lines (pane-and-text's row-map double count); the app-and-panes
pass would *spend* 112 lines, because its candidates are consistency fixes
(three `AgentNode` constructors, four picker openers, the help page's two
columns) and several of its lower rows are net-positive by construction. The
pane-and-text report's five ranked rows sum to 111 rather than its stated ≈120;
the difference is its six below-the-bar rows, ≈11 more.

**The conclusion the four support.** Each report's own share is a few percent,
and 36 "looks duplicated and must stay separate" entries (8 + 8 + 11 + 9) argue
the remaining repetition is deliberate — a per-surface refusal sentence is not
the same fact as another surface's; the residue is policy, not deletable
duplication. What the reports ask for instead, in their own terms, is decisions
rather than line-shaving — the attach CLI's four subcommands rewritten from one
table (store report §6, "the one candidate that can make the product worse while
the tests stay green"); one delivery owner across the child and job books,
arbitrated by the B24 tests (actor report §2, "the least certain number in this
document"); and behaviour calls where the two copies genuinely disagree, like
moving the cap into the output join (actor report §4), which the report says is
"a behaviour change for the human to call, not a blind edit". Past that the road
is a surface rewritten or deleted, not extracted.

**The contradictions the passes named.** The reports name ten places where two
roads already disagree rather than merely repeat — the part of the campaign that
is a defect list, not a style list. The store report names three (two of them the
same shape): the `--context` flag does not trim where `MUSH_CONTEXT` does
(`config.rs:356` vs `main.rs:121`, "already diverged"); `write_pasted_image`'s
failure names `.mush/<name>` for a file at `.mush/paste/<name>`; and
`create_paste_file`'s failure names the same wrong path. The actor report names
two: the attempt's deadline is computed twice and three syscall timeouts ignore
the budget they were handed, so "one ask spends one deadline" is false on that
road and a Stop can land late; and the output cap is applied at two levels with
one name — `Job::output`/`tail` cap each stream while `preview` caps the joined
text — so a command writing cap bytes to each stream gives the model up to twice
the cap as a foreground result and cap as a job, while `preview`'s doc claims
"exactly as a foreground result reads" and nothing tests it. The app-and-panes
report names five: `nudge_failed` puts a phase back without its clock, so a row
that said `waiting on results 4m` says `0s`; `picker_width` multiplies `u16` by
60 and overflows above 1092 columns while `agents_columns` uses `u32`; the
reaper's `past_history` counts only droppable children while `parkable` counts
all parented ones, so one kept child is enough for the two to part company and a
child can be parked with an unread result; one 10 MB picture pasted alone and
with three others gets two explanations of one refusal; and `JOB_TITLE_COLUMNS`
(30) says it is "the same bound as an agent's title" while `tree::TITLE_COLUMNS`
is 24. The pane-and-text report names none. **None of the ten was fixed by this
campaign:** the four reports are the queue's evidence, and their per-item rows
belong in `docs/refactor.md` §11 ("The duplication queue"), the one ledger of
duplication decisions — this record points there and writes no rows.

---

## 8.71 The census at the campaign's end, and what the living docs still say

`python3 scripts/census.py` at `38d0438`:

**TOTAL 79,208 · blank 4,605 · comment 21,708 · tests 34,663 · prod 18,232.**

Against §8.50's landing at `5e296b1` (total 69,226 · prod 16,509 · tests 30,248
· comments 18,345 · blank 4,124 by subtraction): this campaign is **9,982 lines — prod +1,723,
tests +4,415, comments +3,363, blank +481**. The census reads only
`crates/**/*.rs`, so the six audit files and the four duplication reports move no
column; the delta is the fix waves — 2 blockers, 31 majors and 13 minors fixed,
plus the fold/stop wave that answers no finding. Production grew while the
campaign's own prose about removals is elsewhere (§8.47/§8.48 removed a turn cap
and a write cap, both before this range); the landings here are bounds, guards,
and the reasons beside them, and the comment growth (+3,363) is that requirement
— roughly twice the production delta, in a tree where the doc comment is the
code's other half. No `cargo test` was run for this record pass: it changes no
line under `crates/`.

**The scoreboard.** 46 of the 104 findings fixed, 1 partial (A19), 57 open;
the sheet's head moved to 91/4/9 by §8.72–§8.85, and the drift below is re-read
in §8.86;
H49–H53 are the queue's new rows, and the status moves the audits' ledger deltas
owed are on H12, H16, H21, H27, H28, H30, H34 and H35, with the narrowing of
B23/B27's retry class on those rows in §2.75. The three open majors are A3, A6
and F6; every blocker and every major of the tools, secrets, TUI, processes and
contract audits except F6 is closed.

**The rulings these landings rest on**, stated where they were decided and
re-pointed here because the waves are their evidence: a run's row and foot must
name what the run is doing, so a request's own phase is emitted before every ask
(A20, §8.53); a request that went out is never sent twice and one ask spends one
deadline, so a retry can never bill the human for a call the endpoint may have
run (A2, §8.58); a write keeps the mode it found, the target a symlink names,
the type of the name, and the root (B1–B4, §8.55); a credential belongs to the
process that talks to the provider, not to every child (C1, §8.54); and what
mush owns — the store's own names, a command's process group, a worktree a live
node holds, the scratch a dead mush leaves — is cleaned on every road out,
including the roads that never unwind (E1–E5, §8.61–§8.62). The other rulings
the campaign lives under are older sections' and are not restated: the turn cap
and the write cap were *removed*, not raised (§8.47, §8.48); a picture is priced
by its pixels, not its bytes, and the 2 MB cap is transport (§8.42); the trimmer
stops at four fifths and the fold waits in the last tenth (§8.43, §8.45);
images go with their turn (§8.43); and a paste from any location survives a
restart (§8.43).

**Recorded, not changed: the living docs the campaign made false.** The record
names them so a drift pass can repair them, and it does not edit them; §8.86
re-reads the list at `7338d81`. `README.md`
**234–245** still describes the three-attempt transport retry and the half-hour
worst case that `190886c` and `b6a59c3` (§8.58) removed: the retried class is
`Unsent` only and one ask spends one 600 s deadline. `README.md:189`,
`docs/mush.md:465`, `docs/mush.md:874` and the `.mush/` tree listing
(`README.md:282–285`, `docs/mush.md:805–809`) still describe `Ctrl-N` as one
press that clears; it is two-step and first keeps `.mush/session.json.previous`
(`d505d9e`, §8.57). `docs/refactor.md:480`'s B23 row says "transport failures
only", which the same narrowing made false. `docs/refactor.md:585`'s §11
preamble still calls itself "the one ledger of what those reviews have found"
while the four duplication reports have landed beside it (§8.70), and
`README.md`/`docs/mush.md`/`docs/refactor.md` name neither the six audits nor
the four passes anywhere. Older drift the audits named and this campaign did not
touch: `docs/mush.md:203`'s `run_command` row omits that a command past
`CMD_OUTPUT_LIMIT` (8 MiB) is killed, not cut (F4, still open);
`docs/mush.md:869/873` says the session is rewritten once a second where
`SESSION_DEBOUNCE` is 60 s; `docs/mush.md:1291` says three `#[ignore]`d tests
where there are four; `docs/refactor.md:474` says the reply cap is a quarter of
the window where it is an eighth; `docs/refactor.md:424` says the read and
listing caps "went with the file tools" where they came back (§8.36); and
`README.md:154`/`docs/mush.md:895`'s "only one shared child may run at a time"
is the per-parent book F13 is still open about. The two test-visible items of
§8.50 are repaired above in the record, not in the manual: `f915022`'s stop and
the fold (§8.68).

---

## 8.72 One number, one directory, one copy: the store report's four extractions (`1fd1933`, `ce315a2`, `0348028`, `0b42d69`, merged `1e07c2e`)

The four duplication passes landed as documents (§8.70), and the first landings
to answer one of them are the store report's cheapest rows: `docs/dedup/store-workspace-cli.md`'s
ranked extractions, carried in `docs/refactor.md` §11 as `R39`, `R44`, `R37`
and `R41` (in landing order) — each now with the landed shape and commit rather
than the proposed one. Two are the live contradictions that report named in its
own closing list (the flag and the paste path); the other two are the ranked
rows: one shape written twice, one race found by reading.

**The flag and the variable read one number (`1fd1933`; `R39`, the report's
`--context` contradiction).** `--context` trimmed nothing while `MUSH_CONTEXT`
trimmed, so the same statement was read two ways: measured, `mush --print-config
--context " 8192" ws` exited 1 with `` --context needs a token count, got
` 8192` ``, while `MUSH_CONTEXT=" 8192" mush --print-config ws` printed `window
8192 tokens (stated)`. `config::parse_context(value, road)` is the one read;
`parse_context_env` is the environment's name for it, and `main.rs`'s arm calls
it with `--context`, so both doors trim and both refuse by the same rule, each
sentence naming the road that carried the value. Pinned by
`the_context_flag_and_the_variable_read_one_number_one_way`: 8192, ` 8192`,
`8192 ` and ` 8192 ` agree from the flag and from the environment, and `8k` is
refused by both. **Recorded, not changed:** `parse_context_hint`'s `MIN..=MAX`
stays its own bound — the environment is a human's statement with a floor it can
be clamped to, while a window read out of an endpoint's complaint outside that
range is evidence of a misparse, not a window.

**A paste's refusal names the file that is there (`ce315a2`; `R44`).** The paste
directory was spelled four ways and two of them lied: an `Image` carried
`.mush/paste/<name>`, while both write failures said `cannot write .mush/<name>`
(`workspace.rs:855` and `:1741` at the report's base) for a file that is at
`.mush/paste/<name>` — a message a human is meant to act on, pointing at a file
that is not there. `workspace::PASTE_REL` is now the one spelling, with
`paste_dir(root)` and `paste_rel(name)` for the two shapes a road needs: the
directory the writer creates, the path an `Image` carries, and the refusal a
failed write gives. Pinned by
`a_paste_that_cannot_be_written_names_the_paste_directory` — with `.mush/paste`
occupied by a file, the create refusal reads `cannot create .mush/paste: …` and
the name-making refusal reads
`cannot write .mush/paste/pasted-1700000000000.png: …`. **Recorded, not
changed:** `IMAGE_FILE_CAP` and `SEARCH_FILE_CAP` both hold 2 MiB and stay two
caps — one bounds what mush puts on the wire, the other what a walk opens in
memory.

**The copy beside a file is numbered by one road (`0348028`; `R37`).**
`session::keep_unreadable` and `userconfig::keep_unparsable` each wrote the same
eight-line name search, identical character for character, and each spelled the
bound `100` — so the two could disagree about which `.bak.N` is the second
accident. `workspace::backup_name(path)` is now the one road, beside
`atomic_write`: the first free `<path>.bak`, then `.bak.2`, … up to
`workspace::BACKUP_TRIES`, one const for both callers. Each caller keeps its own
rename and its own reason for the copy (a conversation that must not be lost, a
key that must not be replaced) in its own doc. Pinned by
`a_backup_name_is_the_first_free_one_beside_the_file` (the numbering and the
bound), `keeping_a_second_unreadable_session_does_not_overwrite_the_first`,
`a_session_that_cannot_be_kept_still_names_the_file` and
`a_second_unparsable_config_does_not_overwrite_the_first`; the “every backup
name beside the file is taken” refusal now carries the full path on both roads.
**Recorded, not changed:** `session::Stored` and `userconfig::Loaded` stay two
vocabularies for one three-way fact — the session's must not be flattened (that
flatten is what overwrote a lost conversation), while the config's may be (a
missing file and an empty one are the same defaults, and only the complaint
carries news).

**Search reads under its cap like the other two readers (`0b42d69`; `R41`).**
`search` checked `SEARCH_FILE_CAP` from the stat and then called `fs::read`,
which has no bound — the one reader of the three that could load a file that
grew behind its own stat (`whole_read` takes `READ_FILE_CAP + 1`, `image_at`
takes the cap's remainder). The window is a race, so this is not a live bug, but
it is one question — how much of a file may be read — answered two ways by two
roads. `read_bounded(path, cap)` is now the read — `File::open` plus
`take(cap + 1)` — so a file that grew past the stat is caught by its length and
counted a skip. Pinned by `a_bounded_read_stops_at_the_cap`: a file of 4,096
bytes read with a cap of 1,024 comes back 1,025 bytes long, the length that says
“past the cap”, while a file inside the cap is read whole. The search tests are
unchanged.

---

## 8.73 A thread that dies files its own ending (F6, `98d7060` + `13b2689`, merged `0839fcf`)

The first of the three majors §8.71 named as still open to close — and the one
whose evidence an unwinding thread destroys, because the panicking actor is the
very thread that would have reported.

**What was true.** A panic inside an actor's body unwinds the thread, and the
unwinding skips both readers of that run's ending. The UI is a phase behind for
good — it keeps painting the last thing it was told, a row that spins forever.
The parent is never told, so a `wait` burns its whole 600 s cap on a child that
can never report and a listing says `◐ running` for the rest of the session, and
the slot the run held in `ctx.live` is never given back, so `MAX_AGENTS` refuses
a spawn with a sentence that is false and that no `wait` can clear. The audit's
probe (a model client that panics mid-reply) measured it verbatim:

```
the parent heard: [ChildRunning { id: 7 }]
the UI was told: [Running { cancel: false }, Thinking]
the run held slot(s) [1]; the tree's count is now 1
the parent's listing: #7 ◐ running on mush/7
the wait answered: "wait timed out — #7 still running" after 600s
control message: "…its actor was parked, so mush is waking one…"
```

**The fix.** `actor_main` is now a wrapper: it catches a death where it happened
and files it as the ending it is — `Outcome::CutOff`, on the road a stopped or
reaped run takes (`tell_parent`, under `CUT_OFF_RUN`, the number no actor can
report) and as one `AgentEvent::CutOff` carrying the payload, which is the only
record of what broke (the process-wide hook restores the terminal and nothing
else). A cut-off is not `Failed` — nothing the model did broke, and a failure is
a result a `wait` may hand over — and not `Stopped`: there is no actor left to
resume. The UI's half is the row (a new `AgentTree::cut_off`, the phase whose own
doc says the run never ended) plus the pane's sentence and the jobs the vanished
owner left, all of it written once in `App::note_cut_off`, shared with
`report_cut_off`, which stays the road for an actor that was already gone and
left nobody to file anything. The slot is a `LiveGuard` whose `Drop` decrements,
because the road that would have written the count down is the road the panic
skips: the ending and the slot are the two things an unwinding thread leaves
unsaid. After, the same probe: one `ChildDone { run: u64::MAX, outcome: CutOff
}`; the same listing reads `✉ #7 ⚠ cut off — the run never ended; nothing was
committed`; the same wait answers at once and costs no part of the cap; the count
is 0 and the thread's join is clean.

**A corpse is not a parked child (`13b2689`).** A mailbox with no actor behind it
is what a parked child leaves and also what a child whose thread died leaves, and
the two roads that answer a parent about that mailbox — `control message` and
`control stop` — read the send failure as a park: the audit's probe read the
words back, `its actor was parked, so mush is waking one: this resumes it`. The
failed send cannot settle it (a park ends the thread too, and the parent holds
no handle on the child's thread); what settles it is the ending the child filed
before it went: `actor_gone` reads the parent's own books, and a cut-off is the
one ending only a vanished actor reports, while `App::park_history` only ever
reclaims a thread at rest. A stop on a corpse says there is nothing left to
stop; a message says the child is gone, not parked, and that a revived child
resumes from the copy of the conversation on screen rather than from where the
dead run left it. The prose that claimed a failed send *is* a park is corrected
where it lived: `park_history`'s “the send is the whole probe” and its “Already
parked” arm, `App::deliver_to_actor`'s, and the three doc comments that read an
empty mailbox as a park alone (`AgentEvent::ChildAsleep`, `hand_to_ui`,
`agent::gone`).

**Pinned by** `an_actor_thread_that_dies_mid_run_is_reported_cut_off` (the
ending, the slot and the UI's telling, extended by `13b2689` to assert that both
control roads' answers never say “parked”),
`a_thread_that_dies_without_a_reporting_road_leaves_the_count_where_it_found_it`
(the guard alone; the root has no reporting road at all) and
`a_dead_actors_row_says_cut_off_and_the_ui_files_no_second_ending` (the row, the
notice, and that the UI adds no second filing to the parent).

**Recorded, not changed.** The cut-off is filed under `CUT_OFF_RUN` because the
dead run cannot know its own number; the payload the panic carried stays the
only account of it (the terminal hook restores the terminal and files nothing);
and a park is still the one thing a failed send can also mean, told apart by
what the dead thread reported rather than by the send.

---

## 8.74 `Ctrl-O`: the output view, and the fold's zero (`f43a1de` + `6c5b7d2`, merged `1ab5a9b`)

A key that was bound to nothing and a fold state that did not exist; the two
halves landed as one branch because the view is not paintable without the second.

**Zero is the fold's own state (`f43a1de`).** `Fold::with(Kind::Result, 0)` was
not “paint no row of this kind”: it painted `  … +3 more lines` for a three-line
success, a failed result painted its `! error: …` row *plus* `    … +2 more
lines`, and the select mode still gave the block one stop per source line,
because `Stops::of` only knew “every line” and “a tail”. Now
`Fold::without_output` is the state: the three output kinds — a tool's result,
mush's report about a child or a job, the brief a child's pane opens with — go
to no rows, and `folded_rows` paints no `…` for a kind with no rows, because the
elision row is part of showing a block and this state has no head for it to stand
behind. The one row such a block keeps is the failure row `Fold::shown` never
gives up — the result's `  ! error: …`, the report's `· #1 failed: …` first line
— painted alone, and a hidden block has no stop at all while the failure keeps
one with no tail. `Fold::with`'s `#[cfg(test)]` gate came off, because the view
is its first runtime caller, and `Chat` holds the view (`output`, `painted_fold`)
as one flip for every pane. Pinned by
`ctrl_o_hides_and_shows_command_output` (the `⚙` labels and every other row
untouched, the same rows back on the second press),
`ctrl_o_never_hides_a_failure`, `ctrl_o_keeps_the_folds_numbers_in_the_shown_state`
and `ctrl_o_leaves_no_stop_over_a_hidden_block`.

**The key is the output view (`6c5b7d2`).** `Ctrl-O` was unbound — the keymap's
`Ctrl-` block had no `o` arm, so the key fell through to `Intent::Ignore`. It is
now the output view in the family of `Ctrl-T`'s reasoning and `Ctrl-F`'s zen: an
app-wide intent, so it works from either pane and over a picker or the select
mode, and it sits above both in `key`. The view is not said and not stored:
measured at the app level, the press leaves the bar empty, the transcript at the
same length and `session_dirty_at` still `None`, and a fresh `App` on the same
directory paints the tool result again, with the pane's rows back exactly (the
same `…`, the same eight wrapped rows) on the next press. The `KEYS` row both
help surfaces read is “show or hide tool output, reports and briefs (a failure
always shows)”, and the failure exemption is the fold's, not the handler's.
Pinned by `ctrl_o_is_a_view_and_is_not_stored` (a real `Ctrl-O` through the key
table) and the `Ctrl-O` row of
`the_app_keys_work_from_every_pane_and_over_a_picker`.

**Recorded, not changed.** A hidden block still holds its bytes — the view is a
fold, not a cut — and the select mode's stops follow the painted rows, so a
hidden block offers nothing to copy.

---

## 8.75 A wire phase cannot outlive the call's deadline (`1dbea62`, merged `18f2afb`)

The first of the two divergences the actor duplication report named, and the one
whose rule sounded already settled: “the attempt's deadline is
computed twice and three syscall timeouts ignore the budget they were handed, so
‘one ask spends one deadline’ is false on that road and a Stop can land late”
(`docs/dedup/actor-tools-wire.md`, §8.70). The commit answers it without a new
name — the deadline already had a home, and the phases were the ones ignoring it.

**What was true.** `CONNECT_TIMEOUT` (5 s), `WRITE_TIMEOUT` (30 s) and
`RESOLVE_TIMEOUT` (10 s) were schedules of their own: `model.rs::retrying` hands
each attempt only what is left of the logical call's one deadline, and then the
name lookup, the connect and the write could each spend their own constant on
top. A stalled write was the shape that hurt: the request went out through
`write_all`, whose only bound was the connection's 30 s `SO_SNDTIMEO`, and a peer
that trickled its receive window kept it looping past even that; the watch was
never consulted during a write, so a Stop landed with it. Measured with a 300 ms
call deadline, a 16 MiB body, and a listener that accepts and then never reads:

```
before  90.86 s, `WouldBlock` marked `Unsent` (through `model.rs`,
        `ModelError::Unsent` — a class `retrying` asks again)
after    0.31 s, `TimedOut` "the endpoint stopped responding", unmarked
        (`model.rs`'s `transport()`: `ModelError::Transport`)
```

The connect against a route that swallows the SYN (`10.255.255.1`) read 5.05 s
`TimedOut` marked `Unsent` before and 0.30 s `TimedOut` unmarked after.

**The fix.** The rule has one home — `Watch`, the value that owns the deadline —
and one spelling: every per-phase bound is the smaller of its own ceiling and
what is left of the call (`Watch::left`). `CONNECT_TIMEOUT` is taken per address,
the write re-sets the socket's own timeout before every chunk and checks the
watch between them, and the lookup's wait ends at `min(RESOLVE_TIMEOUT, left)`; a
phase that spends the budget answers in the sentence the read deadline already
used (`Watch::spend`), and the two roads that would wrap it in `Unsent` —
`request`'s opener and `exchange`'s write — pass a spent call through, so the
layer above classifies it as the deadline it is, never as a failure to ask again
with nothing left. `ReadWrite` gained `set_write_timeout` because a kept
connection carries the ask that opened it, so the bound must be set per ask, not
per connection, and the three constants now say outright that they are ceilings,
not schedules. Pinned by `no_phase_outlives_the_calls_deadline` (lookup, connect
and write each ended at the call's deadline, the lookup through the opener's own
`Watch` on a fake clock because `connect`'s resolver has no seam for a lookup
that never answers), `a_stop_lands_while_a_write_stalls` (the Stop is the answer,
read as the write returns at the deadline, not 30 s later) and
`a_healthy_call_is_not_cut_by_the_ceilings` (a listener that reads the whole
request and answers still succeeds well inside them).

**Recorded, not changed.** The report's second divergence — the output cap
applied at two levels with one name, `Job::output`/`tail` capping each stream
while `preview` caps the joined text, so one command can hand back up to twice
the cap as a foreground result and cap as a job while `preview`'s doc claims
“exactly as a foreground result reads” — is not this commit's; it stays an open
row in `docs/refactor.md` §11.

---

## 8.76 The four blind passes enter the ledger, and the record's own paragraphs catch up (`0fe6e22`, `47e6f87`, `f79e182`, merged `8b0fc6d`; `706b64e`; `49f0619`)

A docs-only landing with its own follow-ups, recorded because the ledger it
touches is where §8.70's queue lives.

**The passes are rows now (`0fe6e22`).** The four duplication reports became the
seventh through tenth reviews in `docs/refactor.md` §11, and their ranked
findings became rows `R30`–`R73`, one row per item, in discovered order. The
rows carry the reports' estimates and risk sentences, with the premises
re-verified against the tree at `38d0438` and the stale ones corrected in the row
to say so: the pane pass read a tree before the elided-tail and fold waves; the
app pass's field and guard counts were off; the actor pass's failure sentences
are five and not four; the store pass's six-places count is trimmed by the flag
table already there. The four store rows closed by `mush/84` (merge `1e07c2e`,
§8.72) were entered with the landed shape and commit — `1fd1933`, `ce315a2`,
`0348028`, `0b42d69` — and the four app findings that spend lines rather than
save them are marked judged, with the invariant each buys. The live defects the
reports named entered as rows too: `nudge_failed` restoring a phase without its
clock, `picker_width` overflowing `u16` above 1092 columns, the parker and the
reaper counting different populations, the store's flag-versus-environment and
paste-path contradictions (both since closed), and the actor's
budget-versus-timeout divergence.

**The ledger's anchor is the census (`47e6f87`).** `scripts/census.py` at
`38d0438` — 79,208 lines, prod 18,232 — was added above the new rows as the tree
they were checked against, in the reports' own production-only currency
(2,644 / ≈4,600 / 4,865 / 4,800); the older `b8d8baa` sentence was left as the
census the reviews before these read rather than overwritten. `f79e182` is one
sentence of the same work: the fourth review named `first_backup` for the `.bak`
name's one home, and the symbol that exists since `0348028` is
`workspace::backup_name`.

**The record's own two corrections.** `706b64e` corrected the paragraph after H53
in this file, which still said `docs/refactor.md` §11 was “the ledger of a queue
closed except `R6`” — true when §8.49 was written, false since the four passes
landed; it now names the passes and the four rows the store wave closed. (The
same paragraph's pointer said the store wave was §8.70; §8.72 is the wave, and
this pass re-points it.) `49f0619` rewrote §8.70's concluding paragraph, which
had stated what extraction cannot reach as if a size goal were the measure,
while the four reports' own conclusion is about the residue — policy, not
deletable duplication — and the decisions they want instead: a table, one
delivery owner, behaviour calls.

**Recorded, not changed.** The record writes no duplication rows of its own:
§11 is their one ledger, and where this pass closed one — `R60` (
`1afa368`, §8.80), `R72` (`d56a10c`, §8.80), `R66`'s live defect
(`9f0a12c`, §8.81, its one-`enter` refactor deliberately not done) — that file
carries the closure.

---

## 8.77 The group, the age and the terminal (E6, E9, E10, `3bb1a5b`, `a07834a`, `67bc041`, merged `bb22b03`)

Three minors of the process audit that share one shape: a fact the code knew was
not the fact a reader (a signal, a clock, a panic hook) was handed.

**A group is signalled once, in process (`3bb1a5b`; E6).** `Running::kill` fired
`Command::new("kill")` with `-9 -<pgid>` on every call and discarded the
subprocess's `ExitStatus`. Measured with a `PATH` shim that logs every
invocation, around a real `sh` command holding a spinning member in its group:

```
kill -9 -3997322
kill -9 -3997322
PROBE group after two kills: [3997323]
```

The first call is safe — the leader is unreaped, so the id is still the
command's — but the second aims at a group `wait` has already freed, where the
kernel may have handed the number to a process group mush never started. With the
shim answering what a machine without a working `kill` answers (`exit 127`), the
member survived the kill silently: the one road that took a group mush started
was a fork/exec `PATH` can hide. The second call is a no-op now (`Running.killed`),
the signal is the syscall itself (`rustix::process::kill_process_group`,
`ESRCH` read as “already gone”), and anything else is owed once through
`Job::kill_failure`, which the job's own watch thread puts in the window its
owner reads — the note `watch` already carries. `end_group`'s group call goes the
same road, keeping E3's rule that only a group with a member is signalled: a
member is proof the id is still the command's. Pinned by
`a_second_kill_signals_nothing` (with the shim delegating to the real `kill`, two
`kill()` calls end the group, leave the shim's log empty and report nothing),
`a_kill_without_the_kill_program_still_ends_the_group` (the shim answers
`exit 127` and the group dies anyway) and
`a_failed_kill_leaves_one_sentence_in_the_window` (a scripted kill that cannot
end the group puts exactly one sentence in the completion and in `status`).

**A handed-over job keeps the age of its tool call (`a07834a`; E9).** `Launch`
carried no start time, so `Registry::launch` stamped `started: self.clock.now()`
and handed the watch thread a second, slightly later reading of the same clock at
the moment the command *became* a job — a command that ran for most of
`CMD_DETACH_AFTER` as a tool call was reported as new. Measured with a scripted
command and a fake clock held for 59 s before the handover, the row read
`age after a 59 s tool call: 0ns` and the completion line `#c1 done: exit 0 · 0s
· cargo build`. The instant is now stamped where the command is *held* —
`Registry::hold` already reads `self.clock`, so `Foreground` keeps `started`,
`Launch::held` carries it, and `launch` uses it for both the record and the watch
thread — and the record's own words (“the moment it was handed”) are then true,
while a command that was *started* as a job is still stamped at its one door into
the registry. Pinned by
`a_handed_over_job_keeps_the_age_of_its_tool_call`: with the job's thread held off
its handle, a 59 s hold hands over and the row's age is exactly 59 s, and the
completion line says `· 59s ·` rather than `· 0s ·` (fails on the old stamping
with `left: 0ns, right: 59s`).

**Only the terminal's own thread's panic restores the modes (`67bc041`; E10).**
`install_panic_hook` ran `restore_terminal_modes()` for every panic in the
process, and mush has a thread per agent (`mush-agent-{id}`), per job
(`mush-job-{id}`) and one for the session writer: a worker's death wrote the mode
escapes to the human's terminal while the UI thread kept painting frames into a
screen that was no longer mush's. Measured with a probe that installed the hook
and panicked on a thread named `mush-job-1`, the test process's stdout carried
`^[[?1049l^[[?2004l^[[?1006l^[[?1015l^[[?1003l^[[?1002l^[[?1000l` —
`LeaveAlternateScreen` and every mode reset, from behind the human's back. The
hook now captures the terminal thread's `ThreadId` where it is installed and
compares `thread::current().id()` in the hook, so only that thread's panic
restores; a worker's panic has its own roads and needs no terminal (a job's
thread ends its process group, the writer marks itself dead) and the panic
message still goes to stderr through the previous hook. The escapes themselves
move behind `restore_mode_sequences`, a small `Write` seam, so the test reads
them rather than relying on a screen. Pinned by
`a_worker_panic_leaves_the_terminal_alone`: with the hook installed for the test's
own thread, a panic on a `mush-job-1` worker writes no escape sequence at all,
and a panic on the owning thread writes `?1049l`/`?2004l`.

---

## 8.78 The put-away commit and five more of the tools/contract tail (F2, F16, F5, B13, F14, B16, `ce06a83`, `7a01569`, `414e477`, `a534d35`, `09c9446`, `7106564`, merged `6a8dba9`)

Six minors — five of the contract-and-git audit and one of tools-and-workspace —
landed as one branch. The merge body names F15 as left for the `agent.rs` wave;
that wave is §8.83.

**A signing configuration cannot stop the put-away commit (`ce06a83`; F2).**
`commit_all` supplies the identity and `--no-verify`, so its doc promised a commit
that “does not depend on the human's identity and never runs their commit hooks”
— but `commit.gpgsign` is neither a hook nor the identity, and a machine that
signs every commit by default stopped every isolated run's put-away commit. The
audit's probe, run here before the fix:

```
commit_all -> Err("fatal: cannot exec '/nonexistent/mush-no-gpg': No such
file or directory\nerror: gpg failed to sign the data: … fatal: failed to write
commit object")
```

with `commit.gpgsign=true` and a `gpg.program` that is not there, and the code's
own command line against `gpg.program=/bin/false` exits 128. The work is not
lost, but the parent reads an error instead of a revision, and a worktree
correctly kept for holding it is counted by `unlandable` — so a signing config
spends `MAX_WORKTREES` slots, one per child, and the human is refused spawns for
a git setting. `-c commit.gpgsign=false` is now part of the argv (`-c` outranks
both the repository's and the human's config), and the doc names signing beside
the identity and the hooks: signing is configuration, and this flag is the only
reason the promise can be kept. Pinned by
`a_signing_config_does_not_stop_the_commit` (a repository with
`commit.gpgsign=true` and no signer answers `Commit::Made`, `subject_of` reads
the subject back, and the worktree is clean — green after the flag, the `Err`
above without it).

**An unborn repository refuses a base worktree with its reason (`7a01569`; F16's
git half).** `worktree_add`'s “the repo has no commits yet — commit first or drop
isolated” fired only when `base` was `None`, and the same `match` evaluated
`has_commits(dir)` in the other case anyway and threw the answer away — one
`rev-parse` per isolated spawn whose only effect was to be discarded. Before,
with the audit's probe at the Rust level:
`worktree_add(&unborn, 9, Some("HEAD")) -> Err("fatal: invalid reference: HEAD")`;
after, the same call answers `the repo has no commits yet — commit first or drop
isolated`. The gate is now the repository's own state, asked first and once
(`can_branch_from`): an unborn repository refuses a base the same way it refuses
no base, and nothing is made before the refusal. Pinned by
`a_base_worktree_in_a_repo_without_commits_refuses_with_that_reason` (both roads,
and no `.mush/` made). **Residual, plainly:** on the production road `spawn_tool`
resolved `base` before this function was reached, so the *spawn* a human saw in a
fresh `git init` still said `` unknown base `main`: no commit, branch or tag by
that name `` — an `agent.rs` change this branch could not make; it landed as
`c71be60` (§8.83), and the audit's `a_base_spawn_in_a_repo_without_commits_refuses_with_that_reason`
is pinned there.

**A new worktree is given its submodules (`414e477`; F5).** `git worktree add`
is a checkout of refs, not a copy of the tree, and it does not populate
submodules (git 2.55 has no `--recurse-submodules` for it): a base tree that
records one leaves an empty directory in the child's checkout, and
`git status --porcelain` is empty, so nothing on any surface said the tree was
incomplete. Measured with a repository holding one local submodule, before:
the checkout had `lib/sub` with 0 entries, `git status --porcelain` read `""`
(clean), and `fs::read_to_string(path.join("lib/sub/s.txt"))` was `NotFound`;
after, `s.txt` is there at the recorded commit and the status is still clean.
`populate_submodules` runs `git submodule update --init --recursive` in the new
checkout when it carries a `.gitmodules`, so an ordinary spawn spends no process
on the question. It is deliberately best-effort: the branch and the refs are
right, and a submodule that cannot be fetched (no network, a private remote, a
protocol the human's git refuses for submodules) must not lose the worktree a
spawn is standing on. Mush does not touch `protocol.file.allow` to help — a
repository must not be able to make mush clone a local path; the test allows the
`file` transport through git's own `GIT_ALLOW_PROTOCOL`. The child's prompt now
carries the one sentence the audit asked for either way — a worktree is a
checkout of refs, not a copy of the parent's tree — with the road for a submodule
that is still empty (`git submodule update --init`), and only an isolated child
reads it. Pinned by `a_submodule_repo_gets_its_submodules_in_the_new_worktree`
and `subagent_prompt_names_an_isolated_worktree`. **Residual:** the spawn *reply*
cannot name a submodule fetch that failed — that string is built in `agent.rs` —
so the child's own prompt names the road it would run by hand.

**The paste directory is pruned as it is written (`a534d35`; B13).** Every
pasted picture — the clipboard road, the pasted-path road and the app's carry —
was written once and never removed: no `remove_file` touched `.mush/paste/`, the
audit's probe wrote 5 pastes and left 5 files, and the directory is invisible to
`git status` (`.mush/.gitignore`), so only `du` ever said so. The same probe as a
test, before the fix, read “after 4 pastes and the write road's own: 4 files in
.mush/paste”; after it: 2, the run's two live pictures and nothing older.
`Workspace` now records when it was opened (`opened`, unix millis, set in `new`),
and `write_pasted_image` — the one write door every paste road ends at — prunes
as it writes. Two facts decide a candidate, both read from the name the writer
gave the file (`paste_moment`): a paste whose moment is at or after this
workspace's own open is *this run's*, and a live transcript's images are exactly
that, so it is never a candidate however long the run has been going; a paste
older than `PASTE_MAX_AGE_MILLIS` (a day) is one an earlier run left behind, and
past this run's own pictures it is the history a prune takes. That is the
conservative form of “never remove one the live transcript still points at”: the
workspace cannot see a transcript, and the run's start is the line it can draw
without one. A file whose name is not `pasted-<moment>.<ext>` (a human's file in
the directory) and a directory named like a paste are both left alone, and the
file just written is passed to the prune so it is never a candidate even if the
clock moved backwards between the write and the prune. The age is read out of the
name rather than the mtime, because the name is the moment the writer chose and
the one every road (and a test) can read, while an mtime is a second fact that
can disagree with it. Pinned by
`the_paste_directory_is_pruned_but_not_under_a_live_image`: a run opened three
days ago, two pastes from a run that ended before it, one the run itself pasted
on its first day (older than a day, still named by its transcript), then the
write-road prune — the two old ones gone, the run's two still there, the
directory down to 2 entries.

**The spawn schema's title is optional, and one line (`09c9446`; F14's schema
half).** The spawn schema required `title` while the code treats it as optional:
`spawn_tool` reads it with `arg_string_opt`, trims it, and lets a missing or
blank one leave the row with a handle derived from the brief — so the model was
made to pay for a field the code degrades gracefully without, and the schema's
sentence (“A 3 word description of this agent's brief.”) did not say the one
thing the row's painter makes true. The schema now says what the code does:
`required` is `["brief"]`, and the description is “A 3 word, one-line
description of the brief.” Pinned by
`the_spawn_schemas_title_is_optional_and_named_as_one_line`. The wording is terse
because the schema reserve is: `tool_schemas()` was 5,945 bytes against the
`SCHEMA_TOKENS * 3` budget of 6,000, and this change is net −8 bytes. **Residual,
plainly:** the finding's proven half — a `title` holding a newline still reached
a one-line row, because `text::truncate` keeps it — is fixed in `spawn_tool`, in
`agent.rs`, which this branch could not edit; it landed as `74ad1de` (§8.83),
with `a_title_with_a_newline_cannot_reach_a_one_line_row`. The sentence this
commit made stale — `agent.rs:18071`'s “while the schema requires one” — was
named for the pass that owns `agent.rs` and is still there at this base.

**The edit road names its read-modify-write window (`7106564`; B16).**
`edit_file` reads a file, transforms the text in memory and writes the whole
result back; nothing between the two calls checks that the file is still what was
read, so a change another writer lands in the window — a sibling agent in a
shared checkout, or the human's editor — is silently gone. B16 was *suspected*
(the window was staged by hand, not driven inside one call); the staged shape is
now a test and the outcome deterministic:

```
write "line one\n" · read_file -> "line one\n" · another writer lands
"line one\nline 2\n" · edit_text_many + write_file
final bytes: "LINE ONE\n"      (the other writer's line is gone)
```

The decision is the audit's first road — accept it and say so — not the second (a
conditional write, which would take the file's stat compared across the caller's
two calls and so lives in `agent.rs`'s `edit_tool`, a file this branch could not
edit). `tools::edit_text_many`'s doc carries the reason: the `current` it
transforms is the text the caller read, the read and the write are the caller's
two calls, and a transform cannot compare-and-swap across them. The model-facing
half is `edit_file`'s description: “Applied to the file as read: a concurrent
change is lost.” It is terse because the schema reserve is: the clause took
`tool_schemas()` from 5,945 to 5,996 of 6,000, the fuller sentence does not fit,
and raising `SCHEMA_TOKENS` is not a free move — it feeds `request_reserve` and
`fold_request_fits`, and at 2,100 it stops `agent.rs`'s
`compaction_folds_overflowing_history_into_a_summary` (a 6,000-token window) from
folding at all. Pinned by
`an_edit_written_from_a_stale_read_loses_the_other_writers_change`.

---

## 8.79 A finished job's line, and three more of the wire's edges (A3, A9, A10, D19, `c86b8c0`, `330fe61`, `729e8fc`, `4f7793c`, merged `4283cdb`)

Four findings from three audits: A3, one of the three majors §8.71 named as still
open, and three minors — two of the wire (A9, A10) and one of the TUI (D19).

**A finished job's line is not parked behind a sibling's hold (`c86b8c0`; A3).**
`wait_tool`'s machine gate asked `unread_result`, which looked at children only,
on the premise its doc gave — “a job's line … was folded into the transcript when
the job ended (`note_job`), so handing it over again is a recap”. That premise is
false: `note_job` *records* the report, and the fold into the transcript happens
at a message boundary (`fold_completions`), while the mid-call poll that runs
inside a wait (`drain_signals`) records without folding. A job that ends while a
`wait` is in the same batch — exactly `[run_command{detach:true}, wait]` —
therefore leaves a line nobody has read, and the wait slept to its deadline
before handing it over. Staged with a throwaway probe (one finished job in
`done_jobs`, nobody has read its line, and a sibling's `take_machine(2, "cargo
bench")`), then `exec_tool(Wait, {})` on an `Advanceable` clock:

```
before: elapsed=600s, answer = "#c1 done: …" + "wait timed out — #2's
        exclusive command (cargo bench) still holds the machine; …"
after:  elapsed=0ns,  answer = "#c1 done: …" + "the machine is still held
        by #2's exclusive command (cargo bench) — wait again to wait it out,
        or work without the shell"
```

Up to ten minutes of the human's wall clock, and the model was handed the result
only once its run no longer had the time to use it — H13's blindness one id space
over. The identical *child* shape already answered at once; the job half was the
missing predicate, so `unread_result` now counts an undelivered job line the way
the entry guard already did (`!delivered_jobs.contains(job)`), and the two
comments that carried the false premise say what is true: a *delivered* job line
is the recap `wait_digest` refuses to repeat, an undelivered one is a result.
Pinned by `a_finished_job_is_handed_over_before_the_machine_is_waited_out` (the
line and the lock, exactly the child twin's shape, `clock.elapsed() == ZERO`, and
the delivery mark set so the line cannot travel twice; on the old predicate the
clock reads 600 s).

**An endpoint's numbers cannot overflow the run's usage line (`330fe61`; A9).**
`RunUsage::add` folded the endpoint's own `usage` fields with `+=`, and `line`
summed two of them with `+`. `Usage`'s fields are plain `u64`s parsed from the
wire, so `u64::MAX` is a value an endpoint can send. Measured with a throwaway
scripted probe (two replies, all three fields `u64::MAX`, both profiles):

```
debug: the second request died, `attempt to add with overflow` at
       crates/mush/src/agent.rs:69:9 — the audit's own line,
       `self.prompt += usage.prompt_tokens`.
release: `the endpoint counted 18446744073709.6M prompt + 18446744073709.6M
       completion tokens this run (18446744073709.6M total)`; and a second
       probe (`u64::MAX` then `1`) read `0 prompt + 0 completion … (0 total)`
       — a run of 1.8e19 tokens reported as zero, silently.
```

After the fix both profiles report the saturated line and the run carries on to
return its answer. Every sum saturates now: the three in `add` (`total` only when
the reply sent one) and the invented total in `line`. The counts are the
endpoint's own JSON; a sum of a hostile number has to read as over, never wrap to
a wrong number — the same choice `request_weight` and `Image::weight` already
make, for the same reason, written on `RunUsage` and beside the arithmetic.
Pinned by `a_reply_carrying_u64_max_saturates_the_run_usage_instead_of_panicking`
and `a_run_invents_a_saturated_total_when_a_reply_omits_one`, both in debug and
`--release`. **Recorded, not changed:** `docs/audits/agent-and-wire.md`'s blind
spot bullet still reads as open; docs were out of this change's reach.

**A chunk-size claim past the cap is framing, not a refusal (`729e8fc`; A10).**
`read_chunked` answered a chunk-size line past `MAX_BODY_BYTES` with
`body_too_large()` — `InvalidData`, and so `ModelError::Refused`, the sentence a
healthy endpoint is blamed with for an answer too big. A garbled size line that
still parses as hex (`FFFFFFFF`) lands in exactly that arm. Measured with the
audit's reply (`HTTP/1.1 200 OK` + `Transfer-Encoding: chunked` +
`FFFFFFFF\r\nhello\r\n0\r\n\r\n`) through the in-memory harness and a loopback
`HttpModel` under `retrying`:

```
before  is_framing=false  "the response body is larger than 83886080 bytes"
        error=Refused(…)                    connections=1
after   is_framing=true   "a chunk size of 4294967295 bytes would put the body
        past the 83886080-byte body cap"    error=Framing(…)  connections=1
```

What the run says changes with the class: before, “the endpoint's reply was
refused: the response body is larger than 83886080 bytes”; now, “the reply from
<url> broke before it could be read: a chunk size of 4294967295 bytes would put
the body past the 83886080-byte body cap”. The finding's “would be retried” is
not what the code does, and `connections=1` before and after is the measurement
of it: `retrying` repeats only `ModelError::Unsent` (A2's fix), so `Framing` is
as final as `Refused` was — the class change moves the blame, not the number of
asks. The cap check in `read_chunked` now raises `framing(…)` with the claimed
size and the cap in its words, because a chunk-size line *is* the framing: the
endpoint never handed over the bytes it claimed, and a garbled line and an
honest 4 GiB chunk are the same bytes to mush — it only ever has the claim, so
it reports the claim. `Content-Length`- and end-of-stream-framed bodies keep
`body_too_large()`: there the size is the answer's own, written in the endpoint's
own head, and the refusal is mush's to make. Pinned by
`a_chunk_size_claim_past_the_cap_is_framing_not_the_endpoints_refusal` and the
strengthened `an_oversized_body_is_refused` (the `Content-Length` half asserts
`!is_framing` and the refusal's own sentence, so the two roads' classes are
pinned apart). Two doc-only sentences in `model.rs`'s `Framing`/`Refused` class
docs are corrected to include the new species and to stop claiming that every
over-cap body is a `Refused`; no classification or retry code changed.
**Recorded, not changed:** a chunked body that *really* is over the cap is a
`Framing` break too, since mush cannot tell an honest 4 GiB size from a garbled
hex line, while a `Content-Length`-framed body that size stays a plain refusal.

**A URL with a control character is refused at every door (`4f7793c`; D19).**
`Config::base_url` is written into the request line raw, and no door checked what
may be in it: `normalize_url` trimmed the ends only. Staged with a throwaway
probe: before, every door stored
`http://host.test:8078/v1\r\nX-Injected-By-Url: yes` (`MUSH_URL`, `--url`, the
home config, a stored session and `set_base_url` all answered it), and a request
built from it reached the wire as a head the value had a hand in —
`POST /v1/chat/completions\r\nX-Injected-By-Url: yes HTTP/1.1\r\n…`. After, the
same value through every door: `MUSH_URL` falls back (`http://rubendpc:8078`),
`--url` refuses (`--url contains a control character (\r) — check the value`),
the home config refuses naming its path, a session is a notice with the endpoint
in force kept, and `set_base_url` leaves the endpoint unchanged. One door,
`checked_url`, does the checking, in the shape `parse_context` gives the window:
refused by name, with the offending character as its *escape* (`\r`) rather than
echoed as itself, because the refusal is a line mush prints at the human's
terminal. `MUSH_URL` reads it through `parse_url_env`, beside the other
environment readers. A session is a notice rather than a start-up failure, for
the reason C2 gives one: that file belongs to a workspace rather than to a
human's hands, and a corrupted endpoint must not take the TUI down — it is simply
not used, and the endpoint left in force is named. The home config's own header
line now says a control character in `base_url` is refused, because that file is
the one door a human opens by hand. Pinned by
`a_url_with_a_control_character_is_refused_by_every_door`, while the newline a
pasted block trails is still trimmed. **Recorded, not changed:** the TUI's
`/url` arm still accepts any non-empty text, so a human who types or pastes a URL
with a control character is refused *without a sentence* — `set_base_url` drops
it and the ack names the endpoint actually in force; and the key's own door (C7)
is still open, `MUSH_API_KEY` and `/key` accepting a control character that
`http.rs` writes raw.

---

## 8.80 The chat pane's title elides, the popup re-wraps, and zen keeps the box's rows (D11–D16, D23, D24, B14, R72, `f749e71`, `d018a2f`, `bc581ba`, `aa0245b`, `942e4b5`, `a7c579d`, `1afa368`, `a5dbed2`, `7da2cdd`, `d56a10c`, merged `bc3d83d`)

Ten commits: nine of the TUI audit's minors, one tools-and-workspace fuzz, and
one ledger row that was a real narrow defect. Each is a fact one surface computed
differently from the surface beside it.

**The chat pane's title elides like every other title (`f749e71`; D11).** The
chat's title was built by pushing clauses onto a string and handed to
`Block::title`, the one title on screen no arithmetic fitted to its pane. The
audit's probe, a 40-column terminal with `Ctrl-Y` open and six lines hidden:
`title=" agent #12 · Enter copies · Esc leaves · +6 more lines · /notes "`,
`width=64 room=38`, and `painted_border="┌ agent #12 · Enter copies · Esc
leaves┐"` — the count of hidden lines and `/notes`, the pane's own way of saying
what it hides, were cut mid-word by the border. The title is now built as whole,
space-terminated clauses and routed through `screen::elide`, the same rule the
agents pane's title and the bar's facts line already read, so a clause that does
not fit is dropped whole and the pane's own name is the floor; `elide` is
`pub(super)`, with the conversation pane named as its third reader. At the same
40×10 the frame paints ` agent #1 · +6 more lines · /notes ` (31 ≤ 38) when not
selecting and ` agent #1 · Enter copies · Esc leaves ` (37 ≤ 38) while selecting
— the count clause dropped whole rather than cut, and the mode's line (which is
first because it is the newest thing about the pane) kept in full. Pinned by
`no_pane_title_paints_past_its_pane`: a 12-agent tree, a child focused with a
select mode, a held reading and six hidden notes, every size and both focuses and
the zen view, every title asserted within its pane.

**A resize re-wraps an open popup (`d018a2f`; D12).** `/notes` and `/help` wrap
their rows to `picker_text_width(term_width)` when they open, and `set_term_size`
recorded the new size without re-wrapping: the popup kept the rows of the size it
was opened at. The audit's probe — a report opened at 200 and painted at 60 —
read forty columns cut off every row and the tail reachable only after closing
and reopening; the sweep's comment claimed a resize “opens them again for each
size”, it did not, and the sweep's `reopen` hook had already gone unused.
`App::set_term_size` now re-wraps an open popup when the *width* changed:
`/notes` re-reads `chat.notes_report` for the popup's own agent (carried on
`Picker`, so the report cannot follow a focus that moved under it), `/help`
re-derives `help_notice`, and both wrap at the new `picker_text_width`. The model
and provider lists are one label per row and are left alone; the cursor keeps its
place in the list, clamped to the list the new width made — a row index cannot
promise more across a re-wrap — and the dead `reopen` hook and its false comment
are gone from the sweep. After, at 60×17 with the popup open from 200: the tail
is painted, every row fits the 34-column list, and the rows are exactly the ones
a reopen produces. Pinned by `a_resized_popup_is_rewrapped_not_clipped`, for both
`/notes` and `/help`.

**Zen hands the message box the two-pane split's rows (`bc581ba`; D13).** The zen
Chat arm ran `chat_pane` again over the frame above the bar —
`Layout::vertical([Min(3), Length(input_rows())]).split(...)` on a taller area
than the two-pane chat column — while the zen Agents arm reused the two-pane
split's box rows. At the audit's 40×12 with a four-line draft (`input_rows` 6)
the probe measured `two.box={ y: 6, height: 5 }  zen_chat.box={ y: 5, height: 6
}` before and `two.box={ y: 6, height: 5 }  zen_chat.box={ y: 6, height: 5 }`
after — the box moved up a row and grew one when the tree gave up its rows,
against the record's promise that the view “moves the frame and not the
conversation”. At 40×10, where the six-row column cannot hold the box's ask and
the transcript's floor, the two-pane box is three rows and the re-derivation
handed it all six of the nine zen rows. `App::screen` now computes the chat
column's split once and all three arms read it; `chat_pane` is gone — it was the
second split — and the comments that claimed the Agents arm was “its own split,
not a re-derivation” now describe the one split both arms read. Pinned by
`zen_keeps_the_boxes_rows_at_every_size`: at 40×12, 40×10, 60×17 and 79×24, with
a four-line draft, `zen.input`'s y and height equal the two-pane box's in both
focuses.

**A reply of only a fence is a visible turn (`aa0245b`; D14).** `markdown_rows`
removes the fence lines of a fenced block — scaffolding a human does not read —
and it did that for a block with no body too, where the fence lines are the whole
of what the model wrote. Staged at the pre-fix code, the audit's probe read
`"```"` with `rows=[]`, `select=None` and the title still advertising `Enter
copies`; `"```\n```"` the same; and a cursor stepped onto a fence line inside a
block that has a body gave `select=None` too (`painted` returned `None` both for
“no mode” and for “mode with no visible row”, the confusion the audit names). Two
halves, one fact: `mush_core::text`'s `markdown_rows` is now one walk that also
answers `markdown_row_counts` — the per-source-line row map — so a fenced block
whose body says nothing paints its fence lines as text; and `chat`'s `mark_rows`
takes the counts from that walk instead of restating the fence rule (the
restatement is what made the two row counts drift), while `cursor_row` falls back
to the nearest painted row of the cursor's own message, so a source line the view
paints no row for still paints a cursor. The copy reads the stop, not the row, so
the fence bytes are still what `Enter` copies. After, `"```"` paints
`mush › ``` `, `start_select` finds a row, and stepping onto every fence line
keeps `select=Some([…])`. Pinned by
`a_reply_of_only_a_fence_is_not_an_invisible_turn`,
`the_select_cursor_has_a_row_on_every_line_lines_of_names` and the walk's map in
`a_fence_hides_its_lines_and_marks_the_code_between_them`.

**A fold drops the pane's reading (`942e4b5`; D15).** `Reading::Holding` names a
window by `(offset, up_to)` — up_to being the transcript's length when the human
scrolled away — and `replace_transcript` left it in place. `held` refused it for
one frame (up_to > messages), and the growth back past the old up_to resurrected
it: a window belonging to a conversation that no longer existed, which the title
then read as the pane's live position. Staged with the drop removed, the pin
reads exactly the audit's line: `after_fold=" mush "`,
`after_growth=" mush · scrolled ↑3 rows · PgDn "`. The fix is the audit's: drop
`reading` for the agent inside the one road that replaces a transcript, beside
the select mode it already drops for the same reason one step out. What is left
of `held`'s bound is the backstop under the rule — a transcript shorter than the
one the window was taken in, from a road nobody has written — and its doc now
says that instead of claiming a fold can leave a reading pointing past the end.
Pinned by `a_fold_puts_every_pane_back_at_the_bottom`, which also checks that
another conversation's reading is not the root's fold's to drop.

**A published prompt weighs with nothing said (`a7c579d`; D16).** The audit's
D16 measured the window between the two events an actor's first turn is made of,
and the number was right for the code it read: at the audit's base
`used_weight_for` returned 0 for an agent with no transcript entry, *before* the
prompt was weighed, so the meter and the attach gate read 0 while the next
request would carry the prompt (`P4 used_weight_for(#1)=0 prompt weight=3006`,
`after one line: 3012`). The bounded-view rewrite (`3a37c02`, after the audit's
base) had already closed the mechanism: the sum is taken over
`bounded_transcript`, which opens with `system_for(id)` whatever the transcript
holds. Staged the old early return in that function and the new pin fails exactly
as the audit wrote it (left 0, right 3006); with it gone, `used_weight_for(#1)`
is 3,006 for the published prompt alone and 3,012 after the first `user("hi")`
(3,006 + 6). What was still false is the sentence beside the code — “An agent
whose prompt has not been published, or that has no transcript at all, weighs
nothing” — so that is what this commit fixes, with the window named and the
lesson kept: a request that will carry a whole system prompt is never priced at
0. Pinned by
`an_agent_whose_actor_said_its_prompt_weighs_it_even_with_nothing_said`.

**One column rule for the help tables (`1afa368`; D23, and ledger row `R60`).**
The `/help` popup wraps its tables to `picker_text_width`, 34 columns at the
40-column floor, and both tables reserved `4 + longest left cell + 2` of it for
the left column whatever that left the description: the command table's column
was one column wide (`4 + 27 + 2`), so every description wrapped to one character
per row — the audit's probe read `/provider [deepseek|custom]  s` / `w`. Measured
on this tree at width 34: the whole notice was 563 lines, the command table a
column of `s` / `w` fragments. The audit's parenthetical — “as the key table
already does” — is not true of the code it read: `help_table_at` had the same
`4 + w + 2` arithmetic and the same collapse (nine columns there, one in the
commands table), so the rule is fixed for both tables, not copied from one. The
fix is one shared `mush_core::text::columns(left, left_width, description,
width)` beside `wrap_text` — the column arithmetic `R60` names, entered here
because two tables read it: the two-column shape while the description column is
at least `MIN_DESCRIPTION_COLUMNS` (16 — a readability judgement, and one number
because both tables read it), and otherwise the description hangs under its own
left cell, wrapped at the table's four-space indent and the surface's whole
width. `commands::table_at` and `keys::help_table_at` are loops over it;
`usize::MAX` (`--help`) is unaffected, since the column shape holds. After, at
width 34: 136 lines, longest 33, and the shapes the pin reads:

```
/provider [deepseek|custom]
switch provider, or pick one
from a list
Ctrl-Q
quit (a second press confirms
while work is running)
```

Pinned by `the_help_picker_keeps_a_readable_description_column`, which renders
both tables at the popup's floor width and asserts the hung shape, the absence of
one-character description rows, and that every row of the notice fits the surface
it is painted in.

**`Ctrl-Y` with the chat pane hidden says so (`a5dbed2`; D24).** `Ctrl-Y` is
app-wide and `App::select_key` asked the chat for a cursor over `tree.focused`
whatever the layout was. In the zen view with the tree full-screen the chat pane
is the message box alone — `ChatPane::transcript` is `None`, the chat's rect has
no rows — so the key opened a modal mode the frame cannot paint. The audit's
probe: a reply, `Tab` to the tree, `Ctrl-F`, `Ctrl-Y` → no row holds the reply,
`app.chat.selecting() == true`, and no line says a mode took the keyboard. The
mode owned the keyboard, painted no cursor, and a letter was swallowed with
nothing on screen saying why. The key now refuses before the chat is asked, with
the same shape as the empty-pane answer: `the chat pane is hidden — Tab shows it,
then Ctrl-Y`, written to the bar; inside the pane that does paint the transcript
the mode opens exactly as before. Pinned by `ctrl_y_with_the_chat_hidden_says_so`
(the refusal, the line, that `j` still moves the tree's cursor, and that `Tab` to
the chat pane then opens the mode).

**The markdown wrap keeps non-ASCII whitespace like the plain wrapper
(`7da2cdd`; B14).** `markdown_rows`' doc claimed its rows were “the rows the
plain wrapper would have made for the same text, with the styles attached”, but
`wrap_capped` trimmed the tail a space break leaves with `trim_start()` — which
eats *any* Unicode whitespace — while `wrap_runs` removes only literal spaces. A
break that landed before a no-break space therefore dropped the NBSP from the
plain wrapper's rows and kept it in the view's. Measured with the audit's fuzz
over the alphabet {a, b, ' ', U+00A0, U+3000, U+2028, tab}, lengths 1–5, widths
1–6, 19,607 texts: **8,823 divergences**, e.g.
`DIVERGE " \u{a0}a" @ 2: wrap=["", "a"] view=["", "\u{a0}a"]`. The plain
wrapper was the road losing text the pane could show. A break trims **the spaces
it broke at**, and only those: the tail begins at the space the row ended on, so
every other character of it is the text's own — a no-break space, an ideographic
space or a line separator is not a space to break at, so it is not one to delete.
`wrap_capped` now trims with `trim_start_matches(' ')`, the tail rule `wrap_runs`
already had: one rule, two spellings, no character dropped, and the two wrappers
cannot disagree about where a row ends or what it holds. The same fuzz measures
**0 divergences** after the fix, and `markdown_rows`' doc now says exactly what
the wrap is — the same break points, tab stop and tail on every break, a no-break
space included. Pinned by `a_plain_message_wraps_exactly_like_wrap_text`, whose
alphabet and text generation are the fuzz's.

**A share of the terminal is one integer type (`d56a10c`; R72).** `picker_width`
computed `terminal_width * 60` in `u16`, while `agents_columns` computed the same
share in `u32`. Above 1092 columns the `u16` multiply overflows: a debug build
panics on it, and a release build wraps, which the clamp then quietly turns into
the popup's floor. Measured, before, in the debug test build:
`picker_width(1093) panics at crates/mush/src/app/screen.rs:78: "attempt to
multiply with overflow"`; after, `picker_width(1093)` is 80 — 60% is 655, clamped
to `PICKER_MAX_WIDTH`. Both shares now go through one `share(whole: u16,
percent: u32, min: u16, max: u16) -> u16`, whose multiply is done in `u32` and
whose product is clamped to the same constants as before;
`agents_columns` keeps its `.min(terminal_width.saturating_sub(CHAT_MIN_COLUMNS))`
after the share's clamp, and the picker's floor and ceiling are unchanged. Pinned
by `no_share_of_a_terminal_overflows_its_integer`: from 40 to 2000 columns, both
functions must not panic and must equal the natural share computed in `u32`,
clamped.

---

## 8.81 The tree, the input and the keys — the wave's minors, and a window that says its road (D7–D10, D17, D18, D20–D22, D25, D26, R66, `39576fd`, `7ce13c8`, `c884a8c`, `c848deb`, `9f0a12c`, `fa7b1f1`, `3a51fea`, `9134ad4`, `25ec885`, `a26f165`, `4bbef67`, `e87af91`, `9430058`, `bdbb484`, merged `93f7932`)

Fourteen commits assembled into one landing through three merges — `4547faf`
for the tree's five minors, `aad2552` for the session's News row, the select
mode's pane, the many-agents line and the git stamp, `680c99e` for the window's
road and the cell's two copies — and merged into master by `93f7932`, with
`bdbb484` on top. Eleven are TUI-audit minors, one is a refactor row whose lie
the ledger had already named (`R66`), one is the human's live report about
the context window, and one (`bdbb484`) is a sentence in that report's own
section.

**A reap keeps the cursor on the agent it named (`39576fd`; D7).** `reap` held
the cursor as an index into `rows()`, and the rows it sweeps are dropped from the
oldest end — the end *above* the cursor — so the same index named a different
agent once the sweep was over. The audit's probe (51 finished children, ten `j`s,
then a reap of the past-history set) read `cursor index=10 before=Some(AgentId(10))
after=Some(AgentId(11))`: the selection moved with no keystroke. `reap` now
records `cursor_id()` before the retain and points the cursor back at it with
`point_cursor_at` afterwards, and the clamp in `repair_focus` is only the
fallback for a cursor whose own agent went with the reap. Pinned by
`reaping_keeps_the_cursor_on_the_agent_it_named`.

**A reclaimed worktree drops its branch stat (`7ce13c8`; D8).** `mark_reclaimed`
cleared the branch and `kept` but left the id's entry in `agent_stats`. The row
paints that entry beside the branch name and the pane title sums every value in
the map, so a worktree the sweep reclaimed kept a `+3−1` that described a branch
nobody has any more: the probe read the row before as
`   ✓ #1 port  mush/1 +3−1  did it     ││` with `Σ +3 −1`.
The fresh stat map is installed before the sweep decides, which is why a worktree
the same pass reclaims left its figure behind. One removal fixes both readers,
because the tree owns the map: the stat goes with the branch it described.
Pinned by `a_reclaimed_worktree_leaves_no_branch_stat`; `screen.rs` is untouched.

**An orphan row is painted at the top level (`c884a8c`; D9).** `ui.rs` indents a
row by `"  ".repeat(row.depth)`, and `screen.rs`'s `row` set that from
`node.depth` — where the agent was *spawned*, a fact the actor's system prompt
and the `MAX_DEPTH` spawn limit read and one the row must not rewrite. But
`rows()` has always ordered a node whose parent is not in the tree as a
top-level row, so the painted order and the painted indent were two spellings of
the nesting: a reaped parent left its children indented over nothing (the probe:
`     ✓ #2 2  done` with no `#1` row on screen, against ` ✓ #2 2  done` after).
`AgentTree::painted_depth` derives the indent from the same painted chain
`rows()` orders by — zero when `parent_in_tree` is none, else one more than its
painted parent's — and `screen.rs`'s `row` is the one line that reads it. The
stored depth stays what it is, so the prompt, the spawn limit and a stored
session are untouched. Pinned by
`a_row_without_a_painted_parent_is_painted_at_the_top_level`.

**A status never erases the cancelling mark (`c848deb`; D10).** `activity` and
`thinking` refused to replace a fold and nothing else, and a `Cancelling` phase
passes both existing guards (`is_busy`, “not a fold”). The actor checks the
mailbox and then emits its status, so a Stop that landed in that window was
overwritten by the label already in the actor's hand: the probe read `after
Ctrl-C: Cancelling`, then `tree.activity(id, "edit_file src/a.rs")` →
`Activity("edit_file src/a.rs")`, `tree.thinking(id)` → `Thinking`, and
`expire_cancels()` → `false`. Both setters now refuse `Phase::Cancelling` exactly
as they refuse a fold: `⊘ cancelling…` is the human's own keystroke's feedback on
the row they are watching, and erasing it also makes the stale-cancel backstop
never fire, because `expire_cancels` only retires a phase still `Cancelling`.
Pinned by `a_status_never_erases_the_cancelling_mark`. **Recorded, not changed:**
the race in `agent.rs`'s check-then-emit window itself remains suspected — this
closes the tree half the audit proved.

**A failed nudge restores the phase's clock (`9f0a12c`; R66).** `nudge` rewrote
`phase` to `Thinking` and restarted `since`, and `nudge_failed` wrote back only
`node.phase = was` — against its own doc, “Put the row back exactly as it was”. A
row that had been saying `waiting on results 4m` came back saying `0s`, because
the clock of the failed nudge stayed on it: the probe read
`age(id, 240s), nudge, nudge_failed -> since.elapsed() = 3.9µs (0s)` before and
`since.elapsed() = 4m` after. `nudge` now returns `Option<Replaced>` — the phase
it displaced *and* the instant that phase began — and `nudge_failed` writes both
back; the value is a small `pub struct` because `nudge` is `pub`, and `app/mod.rs`'s
one call site passes it opaquely through. Pinned by
`a_nudge_that_cannot_be_delivered_restores_the_previous_phase_and_its_clock`.
**Recorded, not changed:** the ledger's broader `AgentNode::enter(phase)`
refactor of the eleven setters is deliberately not done here; this closes the
defect only.

**A stopped run is news on the run, not a line the session owes (`fa7b1f1`, with
the sentence corrected by `bdbb484`; D17).** The module doc said **News** — “a
failure, a run mush stopped” — “is written to the session so a restart still says
what broke”, while `stored_notices` keeps `NoticeKind::Error` only; probed on the
base, driving the two lines one loop guard writes, `P3 kind=Some(Stopped)
stored_notices=0`. The code is right and the sentence was the defect: a stop must
not come back as a red `!` — nothing the model did broke — and the stop is not
lost across a restart either, because the session stores the row's status. The
sentence now says that: only the failure is the half written to the session,
because that is the line a restart owes; a stop is news on the run that stays on
the row, and no clock takes either away. Pinned by
`a_stopped_run_is_either_stored_as_a_stop_or_not_claimed_to_be`. `bdbb484` is
that pin's own correction: the repair's sentence said a stop is carried across a
restart by the session's stored status, which is true of a stop the human asked
for (`AgentEvent::Stopped` → `Phase::Stopped` → `StoredStatus::Stopped`) but not
of the loop-guard stop the finding is about — that one ends as the run's error
(`tree.fail` writes `Phase::Failed(guard words)`, the snapshot stores
`StoredStatus::Failed`), so the “restart still says what broke” half is false for
that half of the same arm. Two sentences, one fact: what a restart reads for a
stop is the status the run stored, and for the loop guard that status is the
error it ended with — no behaviour change, the same test.

**A focus change under the select mode says the selection is gone (`3a51fea`;
D18).** `attach::Op::Focus` reaches `attach_focus`, which moves the pane, and the
select mode was left standing over the old agent: `select_apply`'s clamp filters
on `select.agent != on` (where `on` is the *new* `Tree::focused`), so the first
key after the move — `Enter` — cleared the mode, copied nothing and said nothing.
The probe, the mode open on the root's pane and a client focusing #1, read
`copied_none=true selecting=false line="agent #1: lexer"` (before `""`) — the
selection eaten with no line, while `Ctrl-Y` was a no-op and `Enter` copied
nothing. A focus that actually moves the pane now drops the mode and says so in
the bar (“selection dropped — the pane moved to agent #1”), and `Enter` with no
line left under the mode refuses out loud too (“nothing to copy — the selection
is gone”), because that road is reachable without a focus change (a reap moves
the pane under it). `Chat::selecting_agent` is the one new small accessor: the
app has to tell “the mode already names the pane the human reads” from “the pane
moved under it”. Pinned by
`a_focus_change_under_the_select_mode_either_follows_it_or_says_it_left`.
**Recorded, not changed:** the human's own `Tab` road still cancels silently —
there the human pressed the key that moved the pane, and `attach_focus`'s doc
says why the two roads differ.

**A merged paste leaves the cursor in the box (`9134ad4`; D20).** `Input::insert`
advanced the cursor by the *paste's* grapheme count, not the result's. A skin-tone
modifier or a regional indicator joins the cluster before it, so the cursor
landed one past the end of the box and `backspace` — which computes both byte
positions past the end — deleted nothing on the first keystroke. The audit's
probe, reproduced before the fix: `cursor=2 graphemes=1 text="x🏽"`, then
`after one Backspace: text="x🏽" cursor=1` (it deleted nothing); after:
`cursor=1 graphemes=1 text="x🏽"`, `after one Backspace: text=""`. The invariant
the module's doc states is that edits land on grapheme boundaries, and the cursor
is the position the next edit lands at, so it is clamped into the result as well
as advanced across it. Pinned by
`a_paste_that_merges_with_the_grapheme_before_it_leaves_the_cursor_in_the_box`.

**A learn decision cannot leave the two copies apart (`25ec885`; D21).** The
audit's probe, on a cell whose UI copy sat at 128000 while the shared cell had
learned 16000 through a handle: `PROBE after the actor's handle learns:
ui=128000 actors=16000`; `PROBE after the 4000 complaint: ui learned=false
ui=128000 actors=4000`. The UI's learn was judged against its own frame-cached
copy, so a 4k complaint the cell in force had already taken was refused by the
side that paints the bar and the tool caps: two windows in one tree, one of them
stranded. `ConfigCell::learn_context` now judges and writes through the shared
cell — the copy every request is built from — and reads the resulting window back
into the UI's copy whatever the answer; the handle's raw write is private
(`learn_window`), and `learn_context` takes the announcement as a closure it
calls exactly when the number landed, so `AgentCtx::learn_context` (agent.rs, the
one call site) cannot learn a window the UI never hears about. After, the same
probe reads `ui=4000 actors=4000` and the acceptance reads `ui learned=true
ui=4000 actors=4000`. Pinned by `the_two_copies_agree_or_the_number_is_refused`.

**An empty key in the home file is no key (`a26f165`; D22).** A hand-edited
`api_key: ""` in the home config was copied into the resolved config as
`Some("")`, and three surfaces then disagreed about one state: the probe read
`PROBE resolved api_key = Some("")`; `/key` would say “api key set (••••…)”,
`--print-config` said “(none)”, and a request carried `Authorization: Bearer`
with nothing after it. The file's road was the only one to that state —
`MUSH_API_KEY=""` is already dropped by `env_nonempty` — so the home layer
filters the empty string where it is read, and the three surfaces agree
(`PROBE resolved api_key = None`): `/key` takes its “no api key” arm,
`--print-config` still says `(none)`, and no request carries an empty Bearer.
Pinned by `an_empty_config_key_is_no_key`.

**The several-agents line names `c`, not `Enter` (`4bbef67`; D25).** `Ctrl-C`
with several agents busy and the focused one not among them said “Enter picks one
to stop”. `Enter` is `Intent::Send` in the chat pane and `Intent::TreeFocus` in
the tree — it sends the box's draft, or makes a row's transcript visible, and
stops nothing (proven by the key table). The probe, two runs in flight and the
root focused, read `P25 line="2 agents running · Enter picks one to stop · Ctrl-X
stops them all"`. The line now names the road that really stops: `c` in the
agents pane stops the row under the tree's cursor, and `Ctrl-X` still stops every
running agent. It is 68 columns, inside the bar's one row at 80×24 once the badge
takes its seven, so nothing of it is elided. Pinned by
`the_many_agents_line_names_a_key_that_stops`.

**A git read from the old chat does not touch the new tree (`e87af91`; D26,
suspected).** `Msg::Git` was the one off-thread message with no conversation
stamp, while `Msg::Agent`, `Msg::Clipboard` and `Msg::Copied` all carry one: a
read in flight across `Ctrl-N` landed in the new tree. `adopt_git` filters the
stats by `tree.has(id)` — an id test, not an identity test, and the ids restart
with the tree — `sweep_worktrees` acts on a snapshot taken from the old tree, and
`git_in_flight` was cleared from under whatever read the new tree started.
Suspected only (mechanism read, not raced). Probed on the base, a fresh chat, an
id the new tree holds too, and the old read delivered by hand:
`P26 agent_stats={AgentId(1): Stat { files: 4, added: 40, removed: 4 }}
git=Some("the old tree's branch") git_at=true in_flight=false`. The read is stamped
in `refresh_git` with the tree's conversation, captured before the worker starts,
and `update` drops one that is not the tree's, the way it drops a stale
`Msg::Clipboard`; `adopt_git` therefore only ever adopts this tree's own read, and
its `tree.has` filter stays for the narrower case it was written for (a reap
between the read and the adoption). `new_chat` clears `git_in_flight` before
asking again: a read in flight belongs to the tree that just died, and the flag
left up would refuse this ask — and every later one — behind a read whose answer
the new tree will never adopt. Pinned by
`a_git_read_from_the_old_chat_does_not_touch_the_new_tree`.

**A window carries the road it came by (`9430058`; the human's live report).**
Two mush sessions on the same config painted `~1M` and `~500k` in the `ctx` line,
and nothing in the frame or in `--print-config` could say which road each number
had taken — the `~` only meant “the human stated none”. Three roads arrive
unstated (mush's model table, the endpoint's model list, the endpoint's refusal)
and only a stated one is ever stored, so two directories can legitimately
disagree forever. The probe, one config per road:

```
before
table:      mark "~", 8192   = "8192 tokens (assumed from the model or the provider)"
advertised: mark "~", 500000 = "500000 tokens (assumed from the model or the provider)"
complaint:  mark "~", 4000   = "4000 tokens (assumed from the model or the provider)"
after
table:      mark "~", 8192   = "8192 tokens (assumed from mush's model table)"
advertised: mark "≈", 500000 = "500000 tokens (advertised by the endpoint's model list)"
complaint:  mark "≤", 4000   = "4000 tokens (named by the endpoint in a refusal)"
stated:     mark "",  32768  = "32768 tokens (stated by the human)"
```

`WindowSource` moves into `mush-core`'s config, beside the window it describes,
with `Stated` and `Table` added; `Config` stores the source where
`context_explicit: bool` was and derives `context_explicit()` from it, so a
second flag cannot disagree. `set_context` records `Stated`,
`adopt_context(tokens, source)` records the road it was told and refuses a stated
window, and `rederive_context`/`Config::new`/`Config::from_env_layer` record
`Table`, with the session/home window application going through `set_context`.
`settings.rs` re-exports the type, so `crate::app::WindowSource`,
`crate::app::settings::WindowSource` and agent.rs's import keep their names;
`believable` keeps its policy and documents that `Stated` and `Table` are not
learnable numbers. The meter's mark is per road — `~` the table, `≈` the model
list, `≤` a refusal, none the human's own — from one `window_mark` both surfaces
paint, so each is one display column and the meter keeps its width at 80×24,
while the words live in `--print-config`. Pinned by
`each_road_a_window_came_by_is_named_in_the_meter_and_in_print_config` and
`the_meter_marks_each_road_a_window_came_by`. **Recorded, not changed:** the
files outside the commit's brief are named in its body (http.rs's ignored live
test's `Config` literal, agent.rs's one `context_explicit` reader).

---

## 8.82 `/context` is a command again, and says the road the window came by (`00f2433`, `d91586c`, `d85cfd1`, `6e7dac4`, merged `c7298ab`)

The human's ask, answered one layer down: §8.81's marks tell a reader *that* a
window took some road, but a mark is one display column and cannot be read, and
the command that once asked which road had been cut with the git wrappers
(`ad5b791`, “the meter is on screen”). Four commits bring it back, with the words
beside the roads they name.

**The words live beside the road (`00f2433`).** `--print-config` carried the four
sentences in a `match` of its own, and `/context` asks for the same four; two
copies is how two surfaces come to describe one window differently. They move to
`WindowSource::words`, beside the roads they name, and `describe` reads them.
Measured through the dump itself, one config per road: `8192 tokens (assumed from
mush's model table)`, `500000 tokens (advertised by the endpoint's model list)`,
`4000 tokens (named by the endpoint in a refusal)`, `32768 tokens (stated by the
human)` — unchanged output, now from one definition. Pinned by
`each_road_a_window_came_by_is_named_in_the_meter_and_in_print_config`, which
goes through the one definition.

**`/context` is a command again (`d91586c`).** With no argument it answers from
`WindowSource::words`: the probe, a real app on the shipped 8192-token default
window, read `/context` → `` unknown command: /context — /help lists them ``
before and `ctx ~8.2k · assumed from mush's model table` after, with the window
and its road unchanged and now sayable; `/help` and `mush --help` gained the row
`/context  say the window's size and the road it came by` (both read `COMMANDS`,
so there is no separate list to forget). The argument is read where the usage
line is written: a line with one is refused until the commits that answer it
land, so help cannot advertise a shape the parser does not take. Pinned by
`the_context_command_names_the_road_the_window_came_by`, with
`every_command_in_the_table_parses` and
`the_help_lists_exactly_the_commands_the_parser_knows` walking the new row in
both directions.

**`/context N` states a window this workspace remembers (`d85cfd1`).** The
setter half: a token count, read by the one reader `--context` and `MUSH_CONTEXT`
use (`config::parse_context`), written through the cell's one edit door so the
meter's copy and every actor's handle move in one write, and flushed before the
bar promises anything — the session is what remembers a stated window. The probe,
a real app on the shipped default: before, `/context 8192` →
`Err(Unknown("/context"))`, the bar said nothing, the session file's `context`
was absent (`None`); after, `/context 32000` made the bar read `ctx 32k (set) ·
stated by the human — this workspace will remember it`, the window became 32000
tokens, road `Stated`, in a handle taken before the command as well, the session
file carried `"context": 32000`, and the startup chain (`resolve_with`) read it
back as a stated 32000-token window. `/context 8k` is refused with ``/context
needs a token count, got `8k` `` — the sentence the flag and the variable print,
road named. Pinned by
`the_context_command_states_a_window_and_the_workspace_remembers_it` (save/load
through the real writer, then the startup resolution) and
`a_context_argument_that_is_not_a_number_is_refused_by_name`.

**`/context auto` gives the statement up (`6e7dac4`).** The road back from the
trap the human hit — a workspace that remembers a number forever. `auto` drops
this workspace's statement and derives the window again (`Config::forget_context`,
which gives the road up *before* asking `rederive_context`, because that call
leaves a window the human stated alone). The stored statement goes with it: the
snapshot writes the session's `context` only while the road is `Stated`, so the
flush after the edit is what writes the field away, and a restart reads no
statement at all. The probe: `/context 32000`, then `/context auto` made the bar
read `ctx ~8.2k · assumed from mush's model table — the statement is forgotten`,
the window became 8192 tokens, road `Table`, the session file's `context` went
from `Some(32000)` back to `None`, and the startup chain derived the table's
window instead of reading the number back as a statement. Pinned by
`context_auto_returns_the_workspace_to_the_derived_window_and_forgets_the_statement`,
with the word's letter-blind read in `arguments_parse_the_way_the_executor_uses_them`
(`/context AUTO` is the same ask).

**The two design boundaries this command rests on, stated because neither is
visible in one commit.** First, `/context auto` does not touch a home-config
statement: `auto` gives up *this workspace's* statement — the session layer's —
and the home file's window is a different layer (CLI > env > session > home) that
only a fresh resolve re-reads and that mush never edits. Second, a window stated
in the home file is machine-global: `UserConfig::context` is a statement that
applies to every workspace on the machine, which is why the session's own
statement is the more specific one and waits above it — a human cannot state one
window for one workspace through the home file.

---

## 8.83 The actor's leftovers — usage on every ending, bounded big-text roads, and the books stop growing (`d710c2e`, `a8851e8`, `8121e78`, `02b002f`, `c71be60`, `74ad1de`, `efd5213`, `d8ae94d`, `d7f12a2`, and from the grandchild the merge carried `fe83f8a`, `3483c4e`, `d4c596c`, `926a12a`; merged `b54d9fc`, the grandchild by `1995b54`)

The wave that finishes the audits' queue: A6 — the last of the three majors
§8.71 named as open — A11, A16, A17, A23, C10, E8, F7's sentence half, F13's
guard half, F14's newline half, F15, F16's spawn half and B12's deserializer
half. One branch landed it, and the `agent.rs`-owning grandchild came in through
`1995b54` and the merge `b54d9fc`.

**Every ending reports the endpoint's own token counts (`d710c2e`; A6).**
`report_usage` had exactly one call site — the clean, tool-free end of
`run_loop` — and `compact_history` read its own reply for the summary's text and
never for `usage`, though the fold re-sends the whole history and is usually the
run's largest call. Measured before: a run whose fold the endpoint counted at
9,000 prompt + 100 completion and whose final reply reported 7/5/12 had the one
line read “the endpoint counted 7 prompt + 5 completion tokens this run (12
total)” — the fold absent; a run cancelled after a call the endpoint counted at
4,040/0 had notices `[]`. After: “9k prompt + 105 completion … (9.1k total)” and
“4k prompt + 0 completion … (4k total)”; a `/compact` from rest — a fold no run
owns — reports “… for this fold (1.3k total)”, because “this run” would name a
run that does not exist. The report now lives on a `run_loop` wrapper around the
turn loop (`run_turns`), so the accumulator survives every road out of the loop,
and `compact_history` feeds the same accumulator the run's replies feed; the idle
fold reports itself with `fold_line`. Pinned by
`a_fold_and_the_final_reply_both_report_the_endpoints_counts`,
`a_cancelled_run_still_reports_what_the_endpoint_counted` and
`an_idle_fold_reports_the_endpoints_own_counts_for_this_fold`.

**`status` is a big-text road and answers to `result_cap` (`a8851e8`; A11).**
`result_cap`'s doc names every big-text road — “a command's output, a file read, a
listing, a search” — and `run_loop` spends the shared `turn_room` with each
result's weight, but `status_tool` returned the registry's own listing whole,
bounded only by `jobs::STATUS_WINDOW` plus each job's headline. Measured: an 8 K
window (budget 12,288, `cmd_cap` 2,458) with four jobs whose output windows the
registry shares out of `STATUS_WINDOW` gave **6,589 bytes** of status, 2.7× the
turn's whole room (the audit's ended-jobs probe of the same window read 8,197).
After: ≤ `result_cap` plus the `truncate_for_model` marker, which says what it
kept and how to ask narrower. Pinned by
`a_status_listing_is_bounded_by_the_turns_result_cap`.

**The fold's refusal is bounded like the run's (`8121e78`; C10).** The run's own
refusal arm truncates what the endpoint said to 600 columns
(`truncate(&body, 600)`); the fold's arm handed the whole body into an
`AgentEvent::Notice`, and a body is bounded only by `http.rs`'s `MAX_BODY_BYTES
= 80 MiB`. Measured: a `/compact` against a scripted 500 whose body is 1 MiB
emitted a notice of **1,048,622 bytes** (wrapped and painted; `/notes` re-wraps
it); after, the endpoint's status and 600 columns of its words, cut with the `…`
`truncate` adds, ~640 bytes. Pinned by `a_folds_refusal_is_bounded_like_the_runs`.

**A failed commit subject round-trips any error text (`02b002f`; F15).**
`commit_subject` closes the failure head with `"): "` and `parse_commit_subject`
split on the *first* `"): "` (`split_once`). An error that itself carries the
sequence — an endpoint's `refused (429): slow down` — puts the parser's delimiter
inside the head, so the row's brief is the error's tail: the subject is the only
record a leftover row has of its task. Measured before:
`commit_subject(7, "port the parser", Failed("the endpoint refused (429): slow
down"))` parsed back as brief `"slow down): port the parser"`; after, the brief is
`port the parser` for that error and every text in the fuzz list — a bare
`"): "`, backslashes, a text that already holds the escape's own output, a brief
that itself holds `"): "`, and a CJK error cut at 40 columns. The error is
written through `escape_subject` (every backslash doubled, then `"): "` marked by
a backslash before its `)`) and read back through the exact inverse
`unescape_subject`; the delimiter is the first `"): "` with an *even* run of
backslashes before it (`split_subject_head`), so `rsplit_once` is not needed and
a brief holding `"): "` cannot swallow the head. Pinned by
`the_commit_subject_round_trips_for_any_error_text`.

**A base spawn in an unborn repo reads that reason (`c71be60`; F16's spawn half).**
`spawn_tool` resolved the `base` name before `worktree_add` ever asked
`has_commits`, so a fresh `git init` answered `` unknown base `main`: no commit,
branch or tag by that name in this agent's workspace `` — git's word about a name
that could never resolve, sending a human looking for a typo instead of a commit.
The two states that refuse before any name matters — no repository, no commit —
are now one gate in git.rs (`can_branch_from`), shared by `worktree_add` and by
the spawn road, which asks it *before* resolving the name so the refusal costs no
resolve. Measured before, the same call on a fresh `git init -b main` returned the
“unknown base” line; after, `the repo has no commits yet — commit first or drop
isolated`, nothing created, no id drawn, and a non-repository reads `not a git
repository` — the state's own sentence. Pinned by
`a_base_spawn_in_a_repo_without_commits_refuses_with_that_reason` and
`a_base_worktree_in_a_repo_without_commits_refuses_with_that_reason`, with the
wording pinned by `a_named_base_is_resolved_before_anything_is_created`.

**A title is folded to the row's one line (`74ad1de`; F14's newline half).**
`spawn_tool` trimmed the model's `title` and took anything non-empty, and the tree
row is one line painted through `truncate`, which keeps `\n` and `\t`:
`title: "parser\nport"` reached a one-line painter. Measured before, the `Spawned`
events carried `["parser\nport the lexer", "parser\tport"]`; after,
`["parser", "parser port"]` — `first_line`, the house's one home of “a brief or
title read as one row” (refactor `R11`), collapses whitespace runs and drops a
second line. Pinned by `a_title_with_a_newline_cannot_reach_a_one_line_row`.

**The spawn cap counts against HEAD, and says so (`efd5213`; F7's sentence half).**
`unlandable` asks every worktree `probe(root, id, "HEAD", _, None)`, while the
sweep that takes worktrees asks each node against the branch its own parent holds
and with the fork it was created at. For a nested child the two differ, so a full
cap handed the model a false fact (“each holds an unmerged branch”), a remedy
already done (“merge or delete its branch”), and no way to tell the real
unlandable id from the counted landable one. Reproduced with the audit's probe
(ids 9/10 — a parent `mush/9` with a commit, a child `mush/10` forked from it and
merged back): the sweep's question `Landable(Merged)`, the cap's count both on
disk `[9, 10]`, the sweep's deed `Removed { branch_kept: Some("mush/10"),
landing: Merged }`, the cap's count after `[9]`. The arithmetic half cannot be
fixed on this side of the door: the sweep's facts — each node's base and fork —
live in the UI's tree, and the spawn road is an actor thread with no handle on
it. So the count is unchanged and the claim is corrected: `unlandable`,
`MAX_WORKTREES` and `too_many_worktrees` now say the question that was asked (not
landable *against HEAD*), name the nested-child case, and give a remedy that
works for both kinds — bring the branch's work to HEAD, or remove the checkout
and delete the branch. Pinned by
`the_worktree_cap_refuses_a_spawn_before_the_id_is_taken`, which now asserts the
refusal names the nested-child case and never calls a branch the sweep can land
“unmerged”. The audit's `a_nested_child_merged_into_its_parent_is_not_counted`
stays owed to whoever gives the spawn road the tree's base/fork pairs.

**The per-child books stop growing for the life of an actor (`d8ae94d`; A16).**
Three per-actor books grew by one entry per event and were never pruned:
`done_jobs` kept every job report the actor ever recorded (the audit's probe:
1,000 jobs, 61,893 bytes of line text), `delivered_jobs` kept every mark beside
them, and `forgotten` kept one id for every child the history window ever reaped.
Measured here, the same 1,000-report road through `absorb`: before, `done_jobs`
1,001 entries / 44,945 B and `delivered_jobs` 1,000 marks; after, `done_jobs` 16
entries / 726 B and `delivered_jobs` 15 marks. The job books now stop at
`REMEMBERED_JOBS` = `2 * jobs::MAX_JOBS`, which is the registry's own memory (that
many running plus that many finished): a report older than that is one the
registry cannot report again, and its line is in the transcript, where the fold
put it. Oldest first, and an *undelivered* report is never dropped —
`drain_signals` records one for the next boundary to fold in, and news is the one
thing a book may not forget — and the delivery mark goes with the report it
marks, so no `wait` can name a report that is not there. The `forgotten` set is
not bounded — it is gone. `children` is the book that says whose reports are
news, and `forget_child` empties it first, so `is_forgotten(id)` is
`!children.contains_key(&id)`: the absence *is* the tombstone, and a report from
an id no book names is swallowed exactly as before. **The tombstone design the
fix refused:** the audit's alternative — drop a tombstone once no in-flight
report can name it — cannot be done parent-side, because a parent cannot observe
the child actor's death (the tree drops its sender, but the child holds its own
`my_tx` and its thread may still send), so a parent-side drop would either swallow
a real report or stay as unbounded as the set it replaced; the reason is in
`ActorState::children`'s doc. Pinned by
`a_thousand_jobs_leave_the_job_books_the_size_of_the_registry`, which folds 1,000
reports through the real road, asserts both books stay at the registry's size,
that the one unread report outlives every prune, and that the oldest read
report's line is in the transcript rather than lost — then forgets a child and
asserts its late report is still swallowed without a tombstone to hold it; the
tombstone's own road stays pinned by
`forgetting_a_child_drops_its_books_and_swallows_a_late_report`,
`a_forgotten_child_handed_a_mailbox_keeps_no_book` and
`a_row_handed_over_after_a_forget_reopens_the_child`.

**The shared-workspace rule counts the directory (`d7f12a2`; F13's guard half).**
The guard read `state.shared ∩ state.running` — this actor's own children — while
its refusal and the delegation policy both claim a fact about the directory
(“only one such child may run at a time”). A grandchild is never in the
grandparent's books, so the root could spawn shared A, let A's run end while A's
own shared child B still worked in the same checkout, and then spawn shared C
into it: two writers, and a guard whose sentence was true of nothing but the book
it read. A tree-wide book now counts the writers of a directory, keyed by its
canonical workspace root: every run of a *shared* child books its id for as long
as the run lives, given up by a `WriterGuard`'s `Drop` — the same RAII reason as
`LiveGuard`, so a run that dies on its way to its ending does not leave a writer
that refuses a sibling forever. The writers this parent's own books name are
filtered out of that count: for a parent's own children the books are the finer
answer, and the two checks together are the sentence; the spawner is never
counted against itself, so a shared child may still delegate into the tree its
own run is in. The book is a `static` rather than a handle carried through
`TreeHandles`, because the writers are threads of one process and a revived actor
is rebuilt with an `AgentCtx` of its own. The refusal now names the id the old
count could not see, and its remedy says “wait for it to finish” rather than
“wait for it first”: the writer it names may be a grandchild the reader cannot
`wait` on. Pinned by
`the_shared_workspace_rule_counts_every_live_writer_in_that_directory`, which
drives the real road: a root spawns shared #1, #1 spawns shared #2 and ends its
run while #2's reply is held in flight, and the refusal must name #2 while the
root's own books hold only #1. **The sentence the audit's other half is owed:**
`DELEGATION`'s “only one such child may run at a time” describes the per-parent
rule and not the directory's, and `mush-core/src/prompt.rs` was not this commit's
to edit.

**A loose reply is still a reply (`fe83f8a`; B12's deserializer half).** A
throwaway probe deserialized the four wire shapes the finding names, from the
base tree, before any fix:

```
{"content":"hi"}                          -> missing field `role`
{"role":"assistant", …, "tool_calls":[{"id":"x","type":"function"}]}
                                            -> missing field `function`
…"function":{"name":"read_file","arguments":{"path":"a"}}…
                                            -> invalid type: map, expected a string
…"tool_calls":[{"id":1,…}]…               -> invalid type: integer `1`, expected a string
full reply {"choices":[{"message":{"content":"hi"},…}]}
                                            -> missing field `role`
```

Every one of those `Err`s became `could not parse model response: …` and ended
the run — the outcome the module's opening sentence (“intentionally loose … so
that the many ‘OpenAI-compatible’ servers out there all round-trip cleanly”)
exists to prevent. After the fix all five parses succeed: a missing `role`
defaults to the empty string, a missing `function`/`name` to an empty call, an
object `arguments` arrives as its JSON text `{"path":"a"}`, a numeric id as its
text `"1"`, and the whole `ChatResponse` the run parses reads a role-less choice.
`role` gets `#[serde(default)]`; `FunctionCall` derives `Default` and defaults
`name`; `ToolCall` defaults `function`; `id_from_wire` and `arguments_from_wire`
read the two fields the spec spells differently, each carrying its reason beside
it. Pinned by `a_loose_reply_is_still_a_reply`. **Recorded, not changed:** the
finding's other half — a malformed *reply* should be a refusal the model can
answer rather than an ended run, the shape the `ModelError::Status` arm already
gives a 400 — lives in `crates/mush/src/agent.rs` (the `ModelError::Malformed`
arm), which this branch did not own.

**A full lost pool forgets the oldest loss (`3483c4e`; A17).** `LOST_POOL`'s doc
said “the oldest lost number is forgotten”, but `lose_agent` read `if id.0 >=
agents.counter || agents.lost.len() >= LOST_POOL { return; }`, so a ninth loss
was the one dropped: the newest loss became the gap while the oldest eight stayed
pooled — the opposite of what `next_agent` pops first. Measured with the
acceptance test staged first (draw #1..#9, lose all nine, read the pool back by
drawing): before, the first draw was `AgentId(8)`, with #9 the gap and #1 coming
back; after, the first draw is #9, then #8 #7 #6 #5 #4 #3 #2,
then a fresh #10, with #1 the forgotten oldest that never comes out. `lose_agent`
now drops `lost[0]` when the pool is full and then pushes the incoming id; the
`id.0 >= agents.counter` guard stays. The newest is the entry to keep because it
is the number a retry draws: the pool exists for the failure that just happened,
and dropping the incoming id left exactly that retry unable to get its number
back. The reason is written beside the code — `LOST_POOL` says which entry
survives and why, `Agents::lost` says it holds at most `LOST_POOL`, and
`lose_agent`'s doc carries the eviction rule. Pinned by
`a_pool_full_names_which_lost_number_is_the_gap`.

**A command that left its process group is outside cleanup's reach (`d4c596c`;
A23).** The sentences that claimed more than the mechanism do now say what it
does: mush's reach is the process group it gave the command, and only what is
still in it. `setsid`/`setpgid`/a daemon's own session leaves that group, and
neither `Job::kill` (a group signal) nor `Job::end_group` (which reads the
group's members) can follow. Re-parenting alone is not the escape: a process
whose parent died is still in the group and is still taken. Measured through the
real `Shell` in a tempdir, with a throwaway probe whose command was `echo $$ >
leader; setsid sh -c 'echo $$ > survivor; sleep 30' &`: leader pid 863632 (its
process group), survivor pid 863633 with `ppid=1 pgrp=863633 sid=863633`, leader
ended `Exited(0)`, `job.end_group() = Ok(0)`, the survivor's `/proc` entry still
there, and the probe's own group kill the only thing that ended it. So
`end_group`'s ordinary `Ok(0)` is not “nothing was left behind”; it is “nothing
is left in the group mush can signal”, while a process that made its own session
runs on. `machine.rs`'s four sentences are rewritten around that (the module
doc, `Job::kill`, the build/cleanup comment, and `Job::end_group`'s boundary
paragraph), and the committed test
`a_command_that_left_its_process_group_is_outside_cleanup` pins the survivor's
own `/proc/<pid>/stat` alive after `end_group()`, with `pgrp`/`sid` equal to its
own pid, and a guard kills it however the assertions end. **Recorded, not
changed:** `crates/mush/src/jobs.rs:483` carries the same old “Stop it and
everything it started” sentence for the registry kill; that file was not this
commit's.

**A refusal calls the pid the lock file's last known holder (`926a12a`; E8).**
`acquire` takes the flock first and writes its own pid after, and the lock file is
deliberately never unlinked — so a refusal can read a dead process's number (an
earlier life's, or a pid-reuse) while the sentence said “quit it first”, as if
that number were the holder. The flock is the only thing that refuses; the
refusal now says what it can know:

```
before: another mush is already running in this workspace (pid 865572) — quit it
        first, or ask it things with `mush agents`
after:  another mush is already running in this workspace — pid 929178 is the
        lock file's last known holder; the flock is the lock, so that number may
        be out of date — quit the running mush first, or ask it things with
        `mush agents`
```

Staged before the fix, `a_refusal_does_not_name_a_dead_holder` held the lock
in-process, wrote a really-dead pid into the lock file and asked again; the red
run quoted exactly the before line, naming the dead child although the holder was
the test process. The audit's suggested “write the pid *before* the flock” is
refused in the doc beside `acquire`: the lock file is the one a refused acquirer
opens too, so writing first would overwrite the holder's pid with the refused
process's own, and the *next* refusal would name the process that was refused,
not the holder. Pinned by `a_refusal_does_not_name_a_dead_holder` and
`the_pid_the_refusal_names_is_the_holder`, with “already running in this
workspace” and the pid text kept so `a_second_acquire_is_refused_and_drop_releases_the_workspace`
and `scripts/smoke.py` still match. **The flake reading is the commit's half two,
not a code fix:** H28's row and the “Recorded, not changed” paragraph supposed
the one-off `lock::tests` failure was a stale `/tmp` lock left by a terminated
run, and that reading does not hold — `flock` dies with the fd and the process
(the audit's SIGKILLed holder was re-read here), the lock file is never
unlinked, and every test's `root()` removes its directory before taking the
lock, so no stale lock can survive a dead holder. The shape that does hold is a
`fork` copying the process's open lock descriptions into a child (CLOEXEC closes
them only at exec): a child forked beside a live `Guard` keeps that flock until
it execs, and the sibling lock tests' drop-and-reacquire assertions then fail on
a lock that is perfectly free. Measured: with the child forked inside the
dead-holder test, 16–17 of 20 runs of `cargo test -p mush lock::tests` were red,
with the failure message naming the test process's own pid as last known holder;
with that test skipped, 12 of 12 green, and a probe child that execs and sleeps
holds no copy. The dead pid is now taken once, at the first `root()` call —
before any test can hold a lock — so the fork cannot copy one, and 25 of 25 runs
of the filter are green;
`the_lock_file_is_not_removed_when_the_holder_leaves` is left as it is (now
safer). Left standing from the audit: `acquire` failing *before* the flock —
`open` on a full `/tmp` (ENOSPC), or a loaded suite's fd pressure (EMFILE) — is
the other shape the bool assertion would mis-blame on the lock.

---

## 8.84 A terminal, a home file, a dump and a schema (C8, C11, C12, F4, `11ad7dd`, `d338355`, `b1ee2f0`, `c96aae1`, merged `b836786`)

Four commits, four surfaces that said less (or more) than the code behind them:
the attach printers, the home config's key, the `--print-config` dump and
`run_command`'s schema.

**The attach printers defang what reaches the terminal (`11ad7dd`; C8).**
`escape_line` made a transcript line *one* line but did not defang it, and
`print_agents` did not even do that — so a stored session (a hand-editable file),
a model reply, a tool result or a roster title/activity/branch could put a
control sequence on the terminal of whoever ran `mush read`/`mush agents`. It was
the one road in the tree that did not go through `mush_core::text::sanitize`.
Measured at the pre-fix tree with a throwaway probe (the printers' own stdout): a
body whose every string carried `ESC ]0;PWNED BEL` left 1 ESC + 1 BEL in the
`read` row, and the `agents` row carried 4 ESC and 4 BEL, one in each of
activity, title, branch and worktree. After, both printers emit 0 ESC and 0 BEL
bytes, and the words the sequence wrapped stay (`look:` … `done`, `busy`, `/w`).
The door is one function: `escape_line` now runs `sanitize` first (the escape
sequence removed whole, the bidi and C0/C1 family dropped) and then the one-line
escaping, and every roster column goes through it; the tab `sanitize`
deliberately keeps is still escaped, because these rows are TSV, and the printing
was split into `lines_text`/`agents_text` so the pin reads what reaches the
terminal rather than that nothing panicked. Pinned by
`the_read_and_agents_printers_emit_no_control_sequence` and the grown
`a_read_line_is_escaped_onto_one_line`. **Recorded, not changed:** `print!` keeps
the old road's failure shape — a broken pipe still panics rather than returning
an error.

**An unrelated command leaves an environment key in the environment (`d338355`;
C11).** `persist_user_config` wrote the *resolved* key, so a key the human
supplied for one run through `MUSH_API_KEY` — the README's own road for a key
they did not want in a file — was written to the home config by any later `/url`,
`/model` or `/provider`, a command whose ack says nothing about a file. Measured
with one probe at the pre-fix tree and the same probe after, a cell whose key came
from the environment and a `/url` on the same host: before, the file held
`sk-env-0123456789` and the loaded key was `Some("sk-env-0123456789")`; after,
the file does not hold it and the loaded key is `None`. The rule now lives in the
save's own API: `UserConfig::save_to(path, KeyWrite)` takes the key by a *road* —
`Stated` (the value's key, `None` included: the statement a host change makes,
findings C6, D6) or `Keep` (the file's own key, present or absent; silence about
the key). `App::persist_user_config` picks the road: `/key`'s arm sets
`key_stated` — the one road where the human says “this key is for the file”, and
whose ack already says `saved to <path>` — so every other save is `Keep`, unless
the key in force is `None` (a host change's statement, which must reach the file
or the old host's key would be re-homed). The write's destination is the app's
own `home_config` field, the same value the `/key` and no-key acks read, so the
line and the write cannot name two paths. Pinned by
`an_unrelated_command_does_not_move_an_environment_key_into_the_home_config` and
`a_keep_save_leaves_the_files_own_key_where_it_was`, while
`a_save_states_the_key_even_when_it_has_none` still pins C6/D6. **Recorded, not
changed:** `key_stated` is not unset while the key `/key` wrote stays the key in
force — it is the human's for the file from then on — and a `Keep` save still
writes the file's own key back, which is why the core test pins the bytes rather
than “no key field”.

**`--print-config` answers the image gate (`b1ee2f0`; C12).** The dump is
advertised as “what a request will carry” and printed every request knob except
the one *capability* a request is refused for: whether the model may be sent a
picture. The fact lived only in the provider table (`provider::vision_capable`,
whose one vision row is `deepseek-flash`), so a human pasting a screenshot at a
custom endpoint learned it from a refusal. Measured with the real binary, a
throwaway config and workspace: a `deepseek-flash` request printed 14 rows with
none containing “vision” before and 15 rows with `vision  yes — image parts are
sent` after, and both refusals read as the gate's own answer
(`deepseek-v4-pro` → “no — the table does not document image parts for
deepseek-v4-pro”, no model → “no — no model yet”). The row is read through the
same function the three gate sites ask (`mush_core::provider::vision_capable`),
so the dump cannot advertise a picture the run would drop; it sits under `model`,
defanged like the model row above it, and `--help`'s list of what the dump prints
names it (“model and whether it can see”). Pinned by
`the_dump_answers_the_image_gate`.

**`run_command` says the output limit kills the command (`c96aae1`; F4).** The
schema described a *cut* where the code kills: it said “A result too big for the
context window is cut, and the cut says how to read on”, and said nothing about
`jobs::CMD_OUTPUT_LIMIT` — a command that writes past it is killed
(`jobs::stopping` → `Stopped::TooMuchOutput`), and only then does the result say
so. The machine block was worse: it stated the rule the kill violates, “A long
command detaches into a job instead of dying”, with no exception. Measured with a
throwaway probe over `tool_schemas()`: `run_command`'s description was 123 bytes,
containing “killed” nowhere, and the whole schemas were 5,996 bytes against the
6,000 the `schemas_fit_the_budget_reserve` bound allows; after, the description is
213 bytes and says “A command that writes past 8 MiB of output is killed and its
result says so; the road on is a narrower command”, the machine block names the
kill beside the detach it is the exception to, and the whole schemas are 5,985
bytes — the sentence is paid for by shedding facts another block already owns
(the workspace root, `exclusive`'s examples, `wait`'s quoted refusal) and
neutral compressions, not by growth. Pinned by
`the_command_schema_names_the_output_kill`. **Recorded, not changed:** the 8 MiB
figure is spelled in this crate because the number's one home (`jobs::CMD_OUTPUT_LIMIT`)
is out of reach from the schema — `mush` depends on `mush-core`, not the other
way round — so a change to the limit must touch prompt.rs too; and `docs/mush.md`'s
tool table still says “output capped to fit the window”.

---

## 8.85 The unfocused tree's cursor is ink, not a band (`3809884`, merged `7338d81`)

The human's ask, and the last landing of this stretch: the agents pane's selected
row was a filled band even when the keyboard was in the conversation pane beside
it, where a fill reads as a second cursor. The band *is* the cursor, so it stays
while the tree has the keyboard — the pane's own border already says focus, and
the band is the pair the bar's badge and the chat's selection wear. With the chat
focused the row is marked with the hue as its own ink instead: no cell's
background changes and no column is spent, which an outline, an underline or a
`› ` highlight symbol could not promise, because the row's leading cells are the
tree's `▶` and the agent's own `⊘`/`⏸`/`✓` glyph. No new theme role: both marks
are the one accent, so the fixed palette and every hue (named, workspace, indexed)
agree about what the cursor looks like by construction. The probe, the cells
`draw_agents` paints into a real `TestBackend` at 30×8, cursor on `✓ #2 ⏸2`:

```
before, chat focused — the whole inner row filled:
  y=3  fg=Black bg=Cyan   |     ✓ #2 ⏸2 row 2          |
after, chat focused — the same glyphs, the hue as ink, background Reset:
  y=3  fg=Cyan  bg=Reset  |     ✓ #2 ⏸2 row 2          |
focused, before and after, unchanged:
  y=3  fg=Black bg=Cyan   |     ✓ #2 ⏸2 row 2          |
```

Pinned by `the_agents_cursor_is_a_band_only_while_the_pane_has_the_keyboard`:
with the chat focused no cell of the painted pane wears the accent as a
background, and the cursor's row — and only that row — wears it as ink; with the
agents pane focused the band is back on every cell of the cursor's row. Every form
the accent resolves in is painted — the fixed cyan, a workspace hue's own bytes,
and the 256-colour nearest entry — and the two frames' symbols are compared cell
by cell, so nothing moved and the mark paints over no glyph.

---

## 8.86 The census after the minors' wave, and the drift the living docs still carry

`python3 scripts/census.py` at `7338d81`:

**TOTAL 86,623 · blank 4,972 · comment 24,395 · tests 38,164 · prod 19,092.**

Against §8.71's landing at `38d0438` (79,208 · 4,605 · 21,708 · 34,663 ·
18,232): this stretch is **7,415 lines — prod +860, tests +3,501, comments
+2,687, blank +367**. The census reads only `crates/**/*.rs`, so the six audit
files, the four duplication reports and the `docs/refactor.md` ledger move no
column; the delta is the fix waves — 45 findings closed, 3 half-closed, plus the
human's window report, the output view, the deadline rule and the quieter cursor,
none of which answers a finding. Comments are the largest growth after tests, and
by the same rule §8.71 stated: the bound, the guard or the corrected sentence
arrives with the reason beside it. No `cargo test` was run for this record pass:
it changes no line under `crates/`.

**The scoreboard.** 91 of the 104 findings are fixed, 4 partial (A19, B12, F7,
F13) and 9 open (A13, A14, A15, A18, A21, A22, C7, F3, F17); every blocker and
all three majors the earlier sheet left — A3, A6, F6 — are closed. The partials'
other halves and the residuals the fixers named are H54–H63, below in the open
queue.

**The drift §8.71 recorded, re-read at this base.** One item was repaired by the
landing it pointed at: `docs/refactor.md` §11's preamble no longer reads as the
only ledger, because the four passes are its subsections now (`0fe6e22`, §8.76).
The rest still stands, with the items this wave moved or added named where they
fall:

- `README.md:234–245` still describes the three-attempt transport retry and the
  half-hour worst case that `190886c` and `b6a59c3` (§8.58) removed; `retrying`
  repeats `Unsent` only, and one ask spends one 600 s deadline.
- `README.md:189`, `docs/mush.md:465`, `docs/mush.md:874` and the `.mush/` tree
  listing still describe `Ctrl-N` as one press that clears; it is two-step and
  first keeps `.mush/session.json.previous` (`d505d9e`, §8.57).
- `docs/refactor.md:480`'s B23 row still says “transport failures only”, which
  the same narrowing made false (the row's `findings.md` twin in §2.75 carries
  it).
- `docs/mush.md:203`'s `run_command` row still says “output capped to fit the
  window” where the schema now says the command is killed past 8 MiB (`c96aae1`,
  §8.84) — the row F4 was about, still open in the manual.
- `docs/mush.md:869/873` says the session is rewritten once a second where
  `SESSION_DEBOUNCE` is 60 s.
- `docs/mush.md:1291` says three `#[ignore]`d tests where there are four at this
  base (three on the wire — the model list, the shipped reply cap and a TLS
  handshake — and the 60 fps frame budget, `#[ignore = "the 16 ms budget needs an
  idle box…"]`); the list is the manual's to regenerate.
- `docs/refactor.md:474` says the reply cap is a quarter of the window where it
  is an eighth; `docs/refactor.md:424` says the read and listing caps “went with
  the file tools” where they came back (§8.36).
- `README.md:154`/`docs/mush.md:895`'s “only one such child may run at a time” is
  the sentence F13's guard half is still owed (H55), and
  `mush-core/src/prompt.rs:58` is its one home.
- The manual has no `/context` row and neither `README.md` nor `docs/mush.md`
  names `Ctrl-O` at this base — the command is back and in `COMMANDS` (§8.82),
  and the key is bound with its row printed by both help surfaces (§8.74);
  `docs/mush.md:445` still names only `/context N`.

**The drift in the code's own sentences, for the pass that owns those files.**
`crates/mush/src/agent.rs:18071` still says the spawn schema “requires one”
(`09c9446` made `title` optional, §8.78); `crates/mush/src/jobs.rs:483` still says
“Stop it and everything it started” (`d4c596c` fixed `machine.rs`'s four
sentences, §8.83); `crates/mush/src/app/commands.rs`'s `/url` arm still accepts a
control character without a sentence (`4f7793c`, §8.79); and
`docs/audits/agent-and-wire.md`'s blind-spot bullet still reads as open for A9's
overflow, which `330fe61` closed (§8.79) — evidence files stay as found.

---

## 8.87 A row whose parent the history window reaped wears ⚮ (#119, `4592da5`, merged `a65cdb5`)

The human's live report — “an old #58 row appearing” — was the history window's
own road. `CHILD_HISTORY = 50` drops the oldest children (`AgentTree::past_history`,
applied by `App::reap_history`), and a node does not inherit its parent's age, so
#49 was reaped while the probe it had spawned stayed; the frame read
`✓ #58 Adversarial write-road …` among the root's current children, claiming a
parent it does not have. The row had always been *ordered* at the top level —
`rows()` cannot place a node whose parent is not in the tree anywhere else — and
since D9 it is *indented* there too (`painted_depth` is zero for a node whose
parent is not in the tree): the exact shape a child of the root wears. Same
order, same indent, same glyph — the structure itself cannot tell the two apart,
which is why the fact has to be said in the row.

The row now says it. `⚮` (U+26AE, `DIVORCE SYMBOL`) is the one symbol Unicode
has for a severed pair, and here the severed pair is the parent link. It rides
the head, right after the id it qualifies — the head is the one field `fit_row`
never gives up (R1: the title yields first), which is where a mark that must
survive the 80×24 floor has to sit — and it costs one column, the arithmetic
`every_row_mark_is_one_column` pins for every mark a row can wear, `⚮` counted
beside `▶`, `⏸`, `✉`, `⚙` and `⚠`.

`AgentRow::parent_gone` reads `AgentTree::parent_gone` —
`node.parent.is_some_and(|parent| !self.has(parent))` — so it is true only when
`parent` names an id the *tree* does not hold. The four no's are the design, not
fallout: the root has no parent to lose, a leftover worktree found on disk never
had one in this tree, a root child's parent is the root, and any node whose parent
is in the tree is a child. The indent rule stays D9's: `AgentNode::depth` is still
where the agent was spawned (the actor's prompt and the spawn limit read it), and
the mark is about the missing parent alone.

Measured on the painted pane at the 80×24 floor, cursor on the probe, its parent
reaped by the window's own road (50 newer root children, then the probe, so
`past_history` drops the oldest two and `reap_history` lands what a tick does with
it):

```
before   "     ✓ #2 probe  done"    (#1's probe, two levels in, no mark)
after    " ✓ #2 ⚮ probe  done"      (#1 reaped, D9's top level, the mark)
```

Pinned by:
- `a_row_whose_parent_the_window_reaped_says_its_parent_is_gone` (`screen.rs`):
  the live window road read as painted cells, with the root and a root child
  asserting no mark, the pane unfocused, and the hidden-rows `▲` title;
- `a_parent_gone_row_says_so_in_every_state_it_can_wear` (`screen.rs`): focused
  and unfocused, 80×24 and 120×40, the four themes, the mark beside a landed row,
  a `⊘` stop and a `⏸1` count, and the rows that must stay unmarked;
- `a_reaped_parent_is_a_parent_gone` (`tree.rs`): the predicate's four no's, and
  the stored-depth/painted-depth pair it must not disturb;
- `a_row_without_a_painted_parent_is_painted_at_the_top_level` (`app/mod.rs`),
  the one line outside those modules, and only its expected cells moved: the
  orphan it reads is now `" ✓ #2 ⚮ 2  done"`.

**The merge's own one-line fix.** `fc22789` is the root's answer to a semantic
merge: #118's `agents_pane` fixture (`ui.rs`) builds `AgentRow` literals and #119
added `parent_gone` to the struct, in different regions of one file — git merged
both and the combination did not build. The fixture's four rows are not about the
severed-parent mark, so it passes `false`; the gate on the merge is what caught
it.

This is D9's leftover: that rule made the painted order and the painted indent
one spelling of the nesting, and a parent the window reaped is the one case where
the order says *top level* while the link says otherwise. The rule is kept and
the leftover is said — the row is still ordered and indented as a top-level row,
and the mark is what keeps its orphan from reading as the root's own child.

---

## 8.88 The three documents say what the code says (#123, `2b978a1`..`35c714b`, merged `df115bc`)

§8.86 wrote down the drift the living docs carried at `7338d81`; this is the pass
that paid it. `mush/123` is 52 commits, one fact each, over `README.md`,
`docs/mush.md` and `docs/refactor.md` — **3 files, 254 insertions, 145
deletions** — re-deriving every sentence it touched from the tree (`git show`,
`grep`, and, where a number was claimed, the command that produces it). The root
merged it as `df115bc`, a clean merge; no line under `crates/` moved.

**What §8.86 listed, answered item by item.**

- **The retry.** `README.md`'s paragraph described a transport failure as retried
  "three times in total" with half an hour as the worst case; `2b978a1` wrote what
  `retrying` does — one class repeated (`ModelError::Unsent`: a dial that never
  connected, a write that did not hand the whole request over), `RETRY_ATTEMPTS = 3`
  being the first try and two retries, and the one 600 s `CHAT_DEADLINE` handed to
  every attempt as what is left of it. `d736b1b` fixed `docs/mush.md`'s "A reset or
  a refused connection is asked again three times", `8a90e90` the per-phase
  ceilings (`RESOLVE_TIMEOUT` 10 s, `CONNECT_TIMEOUT` 5 s per address, `WRITE_TIMEOUT`
  30 s re-set per chunk, each the smaller of its own ceiling and `Watch::left`, a
  spent phase answering through `Watch::spend`), `9247b6b` `docs/refactor.md` §7's
  paraphrase and `349a892` the B23 row's "transport failures only".
- **`Ctrl-N`, `Ctrl-O`, `/context`.** `f7b6301`/`c81a689` made `Ctrl-N` two-step
  over a non-empty conversation, keeping `.mush/session.json.previous` and refusing
  the key if that copy cannot be written (C4); `a8fb33f`/`f601f26` named
  `Ctrl-O`/`Intent::ToggleOutput` in both key tables; `6bfed7c`/`819405f` named
  `/context` (its `Report`, `State` and `Auto` arms, and the four roads a window
  comes by, `WindowSource::words`). `docs/mush.md`'s commands line now lists every
  command `COMMANDS` parses.
- **The session debounce.** `d166aee`: `SESSION_DEBOUNCE` is 60 s
  (`app/mod.rs:475`), not a second a save.
- **`/key` vs `MUSH_API_KEY`.** `042a6ad` (README) and `6921452` (docs/mush.md):
  only `/key`'s arm saves `KeyWrite::Stated`; `/url`, `/model` and `/provider` save
  `KeyWrite::Keep`, so a key the run read from the environment is never copied into
  the home config (C11, `d338355`).
- **The reply cap.** `fc31aff`: `REPLY_SHARE_DIVISOR = 8` and
  `REPLY_SHARE_WORDS = "an eighth of the window"` (`mush-core/src/config.rs:69`,
  `:75`), floored at 1 024 and capped at 120 000 — where the row said a quarter.
- **The file-tool caps.** `26358e5`: `READ_FILE_CAP` (32 MiB), `SEARCH_FILE_CAP`
  (2 MiB) and `LIST_LIMIT = 400` back, and named, after H31 brought the tools back.
- **The ignored tests.** `e7da11b`: four, not three — the model list, the shipped
  reply cap and the TLS handshake in `http.rs`, plus `app/mod.rs`'s
  `a_frame_fits_in_a_60fps_budget_on_a_long_transcript`; `README.md:340` already
  said "the three live-endpoint checks … plus the frame-budget test."
- **The shared-writer sentence.** `20c30a7` (README) and `e5f67a1` (docs/mush.md):
  the count `spawn_tool` makes is the directory's live writers, tree-wide
  (`writers()`/`WriterGuard`, keyed by the canonical root), the spawner exempt —
  H55's manual half.
- **The census head.** `6802b27`: `docs/refactor.md` §11's anchor moves to
  `7338d81` — 86 623 lines (prod 19 092, tests 38 164, comments 24 395, blank
  4 972), reproduced for this record — while the two older heads (`38d0438`'s and
  `b8d8baa`'s) keep their own "left as it stands" notes.
- **The `run_command` row.** `309b679`: a command that writes past 8 MiB is killed
  (`Stopped::TooMuchOutput`), not merely cut, and its result says so (`c96aae1`).

**What §8.86 did not list, and the pass found.** The model seam (`4b72768`:
`ModelClient::chat` takes the call's `timeout`, so `retrying` can hand each attempt
its remainder; D3's `ask` verified landed and left as it stood); the group kill
(`5b3a47f`: `machine::kill_group` through `rustix`, not a `kill -9 -pgid` child —
`kill_command` has no definition left under `crates/`); a base is resolved in the
spawning agent's own workspace (`5870703`); `scripts/mock_llm.py` is for
hand-driven runs and nothing calls it (`9eeced6`); `--print-config`'s row list
(`2cceae5`: the `vision` row is the image gate, `home config` and `notice` are
conditional); the accent sites are nine (`bff3e2a`: `select_painted`'s cursor band
and selection, in the decision log's own count); a cut-off run can be a dead
thread (`a4994c9`, finding F6); a stop is news on the row, not a line in the
session (`e19df76`); the meter's four marks (`1e1b8d8`, `92de0c9`); the copy
promise covers every folded block (`d855fca`); the attachment rows shrink with the
box (`da3a4d8`); the path refusals a file tool makes, in full (`54b4202`); an edit
refuses a file that is not valid UTF-8 (`b0b8950`, finding B6); the tree's id
reserve is `reserve_agents` (`0e17c7a`, `37ba616`); the crate layout names every
module that exists, the six additions included (`33a4338`); `LOOP_ROUNDS` lives in
`agent.rs`, because the `agent/` split never landed (`23f4b88`); the design doc
says the tree has been read blind, and points at §8.51/§8.70 (`0c6e6de`,
`a139d0f`); and only the actor's waits read the clock through `AgentCtx` — `Watch`
is handed `clock::system()`, the deliberate exception `clock.rs:33`'s doc names
(`35c714b`). The ledger's R-rows whose statuses had moved were re-read: R55 by
`556b507`, R60 by `bb80762`, R66 by `dd704ff` (its wider `enter(phase)` refactor
deliberately not done), R71 by `6876220` and the tenth review's closing paragraph
by `bab693f`, R72 by `c2631c9`, R17 by `1310e0f`; and §11's preamble stopped
claiming to be the one ledger of everything those reviews found (`7041de9`). Two
commits changed no claim at all: `bde3f04` and `e1d56ba` re-wrapped paragraphs the
edits had made ragged.

**What the pass refused, and what it left.** It refused to rewrite history:
`docs/refactor.md` §1's diagnosis sentence is kept as it stood at `d4f80ae`
(`5b3a47f`), the two older census heads keep their own notes (`6802b27`), R17's
row names the `\`-to-`/` fold as the one `rel` then had (`1310e0f`), and
`mock_llm.py` is kept (`9eeced6`). It refused what it did not own:
`mush-core/src/prompt.rs`'s shared-child sentence ("not this file's to edit",
`20c30a7`), the wider `AgentNode::enter(phase)` refactor (`dd704ff`), and D3's
`ask` (`4b72768`). What it left is the code's own drifted sentences — entered
in the open queue as **H64–H69**, because they are sentences in files a docs pass
does not touch — and `docs/simplification-review.md:150`'s "`busy_children` and
`busy_counts` are the same count derived twice", which does not hold at
`df115bc`: `busy_children` has no definition left under `crates/`,
`AgentTree::busy_counts` (`tree.rs:1764`) is the one walk per frame, and `napping`
reads it — the ✓ beside the review's item is the fix `fc5982a` landed, and the
sentence beside it describes the defect that fix closed. §8.86's other code
sentences are still there and still not this pass's: `agent.rs`'s
`a_wrongly_typed_title_is_refused_never_silently_dropped` still says the schema
"requires one", `commands.rs`'s `/url` arm still names nothing about the
control-character refusal `set_base_url` makes, and `jobs.rs:483`'s "Stop it and
everything it started" is H68 in the open queue; the evidence files stay as found.
`docs/refactor.md`'s census anchor still speaks for
`7338d81`; moving it is the next record pass's, exactly as this pass moved it from
`38d0438`.

**The one lead the pass's own body reports, checked at the base.** `9247b6b`'s
body says the retry drift "stands in `docs/mush.md` §12's bullet, which is outside
this file and is reported rather than edited". At `df115bc` that bullet reads
"**A request that never left mush is retried; an answer is not.**", lists
`Unsent`, "at most twice" and the one 600 s `CHAT_DEADLINE` — `d736b1b`, earlier
in the same pass, had already rewritten it. The record could not find the drift
the body names, and does not repeat the claim.

**What this record could not verify.** The pass wrote no summary file — the three
documents and the commits are the whole of it — so only what the base settles is
entered here. *Unverified:* its claims about its own reading rather than the
tree's state, `33a4338`'s "every line already there was re-read against the module
it names and left as it was" being the plainest; the tree carries the result, not
the method.

**Census at `df115bc`** (`python3 scripts/census.py`): **TOTAL 87 068 · blank
4 999 · comment 24 513 · tests 38 456 · prod 19 100.** Against `7338d81`
(86 623 · 4 972 · 24 395 · 38 164 · 19 092) that is **+445 — prod +8, tests
+292, comments +118, blank +27**, and it is entirely #119's row and its pins:
the pass edits no `.rs`, and neither do the two record commits between the two
merges.
