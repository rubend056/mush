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
  the base's history, and did the run commit anything of its own.
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
  the row says why — so it is a row and not a defect.
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
  §8.26 left its own full-suite flake.
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
  restore with a child saved mid-phase.
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
  model can say that.
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
  there); the end-to-end test asserts the books instead.
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
  `a_spawn_forks_from_the_named_base_and_says_so`.

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

## 8.45 What goes on the wire, audited: the fifth a cut leaves, and the fold's own request (`e34e49a`..`991be92`, `mush/19`)

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
