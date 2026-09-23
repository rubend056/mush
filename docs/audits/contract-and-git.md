# The model's contract, and the git world under it, audited

A blind audit of what mush promises the model about its own world and whether
the world is what the prose says: the two system prompts and the ten tool
schemas (`prompt.rs`, `tools.rs`), the transcript algebra that decides the
*shape* of every request (`transcript.rs`), the repository reads and the
worktree machinery (`git.rs`), and the delegation contract — spawn → worktree →
run → commit → report → reclaim — as far as `agent.rs` and `app/mod.rs` carry
it.

**Base:** `5889590`. **How:** every file below read end to end, then probes:
one throwaway integration test in `crates/mush-core/tests/audit_probe.rs`
(six tests, calling `git::commit_all`, `git::reclaimable`, `git::unlandable`,
`git::reclaim` and `transcript::trim_history` themselves) and seven throwaway
repositories under `/tmp` driven with the exact git commands the code runs. The
delegation road inside two 15,000-line files was also read in parallel by a
delegated subagent with its own worktree and its own probes — its runs are
quoted below and marked *child probe*, and every claim taken from it was
re-checked against the code or re-run here before it was written down. Every
probe and every repository was deleted before this file was committed; the
quoted runs are real ones from this machine (Linux, git 2.55.0, cargo test in
the debug profile). Findings whose mechanism is proven by a run say so; findings
read out of the code alone are marked *suspected*.

Nothing here re-reports a closed row: §8.44–§8.50 and ledger H41–H48 are read
and their fixes checked rather than assumed (see *Verified sound*). Two blinds
landed before this one — `docs/audits/tools-and-workspace.md` (B1–B16, cited as
`B4`) and `docs/audits/secrets-session-config.md` (C1–C12, cited as `C5`) — and
where a mechanism is theirs this file writes "as in B4" and moves on. Their
proposed fixes are attacked at the end, because two of them would trade one bug
for another.

---

## Priority order

Ordered by what the human loses, not by what is ugliest.

1. **F1 — a run whose only work is in an ignored path is swept, and the row
   says "clean — nothing changed".** The only copy of the run's output is
   deleted by `git worktree remove --force` + `git branch -d`, under a sentence
   that denies anything happened (**major, proven**).
2. **F8 — a run-end commit taken in a workspace that is no longer a worktree
   commits the *human's* checkout on the *human's* branch.** `git -C` in a
   plain directory under the repository resolves to the main checkout, so
   `commit_all`'s `git add -A` stages and commits the human's working tree under
   mush's subject; the one-line guard the code is missing is the biggest single
   risk in this document (**major; mechanism proven, trigger a race**).
3. **F9 — `base="HEAD"` is resolved in the application's root, not the
   spawning agent's workspace.** A nested orchestrator forks its child from
   someone else's HEAD — silently — and the actor and the UI then reclaim against
   different bases (**major, proven**).
4. **F10 — an id is returned to the pool after a *partial* `worktree add`**, so
   the branch git already made collides with the very next isolated spawn and
   wedges the whole road for the session (**major, proven**).
5. **F11 — an isolated spawn is refused in a workspace that is a subdirectory
   of a repository**, with the sentence "not a git repository" about a directory
   git just answered for (**major, proven**).
6. **F6 — nothing bounds a panic in an actor thread**: no `ChildDone`, a row
   that spins forever, a parent that waits its full 600 s, and the corpse is then
   *resurrected* by `control message` because a dead mailbox reads as a parked
   child (**major; the silence is proven, the panic is not**).
7. **F2 — on a machine whose git signs commits, no isolated child ever commits
   again**, and every such run leaves an unlandable worktree that counts against
   `MAX_WORKTREES` until spawns are refused (**minor, proven**).
8. **F3 — the dropped-turns note is identified by its text**, so a user line
   that *is* that sentence is moved by the trimmer; when it was the opening
   task, the trim can insert a user message between a tool call and its result —
   the exact shape the same file's `repair_tool_pairs` exists to prevent
   (**minor, proven**).
9. **F4 — `run_command`'s schema says a too-big result is cut; the code kills
   the command at 8 MiB.** The prompt block says a long command "detaches into a
   job instead of dying" — this is the death it rules out (**minor, code path
   and an existing test**).
10. **F7 / F12–F17 — the smaller contract drifts**: the spawn cap's arithmetic
    is not the sweep's (F7); a wrongly-typed `base` silently drops isolation
    (F12); the one-shared-child rule is per-parent, not per-workspace (F13);
    `title` is required by the schema and optional in the code (F14); the commit
    subject does not round-trip for an error containing `"): "` (F15);
    `has_commits`'s "no commits yet" refusal is unreachable from the spawn road
    (F16); and a failed commit is a row tail the next run clears, not the
    transcript line its doc claims (F17).
11. **F5 — a child's worktree has no submodule contents.** The child's workspace
    is missing files its base ref holds, and `git status` in the child is silent
    about it (**minor, proven at the git level**).

---

## Census of this audit

Lines read, per file, end to end unless said otherwise:

| file | lines | what was read |
|---|---:|---|
| `crates/mush-core/src/prompt.rs` | 638 | the whole file: `RULES`, `MACHINE`, `DELEGATION`, `ROOT_ROLE`, `BEGIN_TASK`, all ten schemas, and every test |
| `crates/mush-core/src/transcript.rs` | 1,446 | the whole file: the trigger/target arithmetic, `repair_tool_pairs`, `drop_orphan_results`, `sanitize_tool_calls`, `trim_history`, the note, and every test |
| `crates/mush-core/src/git.rs` | 1,418 | the whole file: status/stat/branch, `worktree_add`, `reclaimable`/`probe`/`landing`/`reclaim`/`remove`, `isolated_ids`, `unlandable`, `commit_all`, `resolve`, and every test |
| `crates/mush-core/src/tools.rs` | 421 | the whole file: `ToolName`, the two derived name tables, `arg_string`/`arg_usize`/`arg_bool`/`arg_path`, `edits_arg`, `edit_text`, `edit_text_many` |
| `crates/mush/src/agent.rs` | ~1,050 of 15,867 | the contract roads only: the run end (1520–1660), the tool batch and cancel (2620–2860), `spawn_tool`/`too_many_worktrees` (3765–3920), `wait`/`on`/`status`/`control` (4132–4700), `commit_worktree`/`commit_subject`/`read_tool`/`write_tool`/`list_tool`/`search_tool` (358–373, 4700–4930), `reclaim_own_worktree`/`worktree_gone` (2059–2118) |
| `crates/mush/src/app/mod.rs` | ~200 of 15,831 | the delegation roads only: `Spawned` registration and the brief row (1500–1570), `worktree_gone` (2152), `attach_worktree`/`agent_root` (2600–2640), plus the branch/base resolution and sweep greps (966–1200) |
| `docs/findings.md` | §8.44–§8.50, H41–H48 | the closed record, for non-re-reporting |
| `docs/audits/*.md` | headings + B1–B16, C1–C12 | the two landed audits, for cross-reference |
| `docs/mush.md` | the tool table and the prose at 110–210, plus greps | the manual's claims about these tools |

One probe file was created and deleted (`crates/mush-core/tests/audit_probe.rs`),
and seven repositories under `/tmp` (`audit_probe_repo`, `audit_gpg`,
`audit_sub`, `audit_sub2`, `audit_sub3`, `audit_hook`, `audit_phantom`), all
removed — plus the delegated read's own throwaway tests and `/tmp/probe-road`,
which it removed itself.

---

## F1 — A run whose only work is in an ignored path reads as "clean — nothing changed", and is then deleted

**Severity: major. Proven through mush's own functions and through the exact
git commands the code runs.**

`commit_all` decides whether a run changed anything with `git status
--porcelain` (`crates/mush-core/src/git.rs:663`):

```rust
pub fn commit_all(dir: &Path, subject: &str) -> Result<Option<String>, String> {
    if run(dir, &["status", "--porcelain"])?.is_empty() {
        return Ok(None);
    }
```

and the reclaim probe asks the same question, through the same helper
(`git.rs:149`, `git.rs:488`):

```rust
fn dirty_paths(dir: &Path) -> Option<usize> {
    let porcelain = git(dir, &["status", "--porcelain"])?;
```

`git status --porcelain` does not list ignored paths. So a worktree whose only
new files match the repository's own `.gitignore` answers **clean** to both
questions, and the run-end road does the rest (`agent.rs:2076`):

```rust
fn reclaim_own_worktree(actor: &Actor, state: &ActorState) -> Option<git::Landing> {
    if actor.branch.is_none() || !state.running.is_empty() || !state.running_jobs.is_empty() {
        return None;
    }
    let base = actor.base.clone().unwrap_or_else(|| "HEAD".to_string());
    match git::reclaim(&actor.ctx.root, actor.id, &base, actor.fork.as_deref()) {
        git::Reclaimed::Removed { landing, .. } => Some(landing),
        _ => None,
    }
}
```

and `remove` is two verbs that do not ask again (`git.rs:580`, `git.rs:592`):
`git worktree remove --force` (deleting the ignored files with the directory)
and `git branch -d`.

**The probe, through mush's own code** (throwaway test, run
`cargo test -p mush-core --test audit_probe -- --nocapture`):

```
F1 commit_all -> Ok(None)
F1 status --porcelain -> Ok("")
F1 reclaimable -> Landable(NothingCommitted)
F1 reclaim -> Removed { branch_kept: None, landing: NothingCommitted }
F1 the run's only output still on disk? false
```

The repository the probe built had `.gitignore` = `/ignored/` + `*.log`; the
"run" wrote `ignored/report.txt` ("the only copy") and `run.log`. The same
sequence by hand shows what the two tests cannot see:

```
--- the code's dirty test: git status --porcelain (empty = 'clean')
--- what is really there (--ignored=matching):
    |!! ignored/
    |!! run.log
--- commit_all's sequence (add -A then commit)
    no commit: nothing staged
```

**Blast radius.** A delegation whose deliverable is not tracked is the normal
case, not the exotic one: "write me a `.env` with these values", "produce
`dist/app.js`", "leave the schema in `gen/`, it is generated", "write the notes
to `notes.md`" in a repo that ignores `*.md`, or a child whose only output is a
build artifact under `target/` — the path is ignored by the human's own
`.gitignore`, so by git's measure the branch adds nothing and by mush's measure
the run "changed nothing". The parent is told

```
· mush/12 clean — nothing changed
```

(`agent.rs:275`) and the *only copy* of the work is removed with the checkout.
The parent's next decision is made on a false fact: it may re-delegate the same
task, or report to the human that the child did nothing. Nothing in the tree
warns that a gitignored deliverable does not survive a run.

Note the interaction with the *edit* roads: `write_file`/`edit_file` do not
consult `.gitignore` at all (as in B1, every write goes through
`atomic_write`), so the model can *see* the file it just wrote — the loss
happens only at the sweep, seconds later, and only the row's sentence disagrees.

**Suggested fix.** Make "the run changed anything" and "the checkout is dirty"
tell ignored work apart from nothing, and let the sweep keep what it cannot
account for. In `git.rs`: give `dirty_paths` a second, ignored-aware reading
(`git status --porcelain --ignored=matching`, counting `!!` lines) used by the
reclaim probe and by `commit_all`'s empty test; a worktree with ignored paths
and no commits becomes `Reclaimed::Kept("{rel} holds N ignored path(s), which a
commit cannot keep — land or discard it by hand")`, and `commit_all`'s `None`
becomes a third answer so `Work::Clean`'s "nothing changed" is not said about a
run that changed the filesystem. The cost is real and must be named in the fix:
`target/` after any build is ignored work, so a child that merely compiled would
keep its checkout — which spends a `MAX_WORKTREES` slot and is visible on the
row (H17's refusal names it, the human can `/discard`). That trade — a bounded,
visible cost against a silent deletion of the only copy — is the one H10's
reclaim rule already makes everywhere else.

**Acceptance test.** `an_ignored_only_worktree_is_kept_and_named`: a repository
ignoring `/ignored/` and `*.log`, a real worktree via `git::worktree_add`, one
file at `ignored/report.txt` and one `run.log`, then assert `commit_all` does
not answer `None`-as-nothing (it names the ignored paths), `reclaimable` is
`Kept` and names `ignored/report.txt`, `reclaim` leaves both the branch and the
file, and — the fact the row reads — `Work::digest` never says "nothing
changed" for it.

---

## F2 — `commit.gpgsign` stops every commit, and the doc says it cannot

**Severity: minor. Proven.**

`commit_all`'s own doc makes a promise the code does not keep
(`git.rs:660`):

```rust
/// The identity and the message are supplied here (`-c user.name=…`,
/// `--no-verify`) so a commit never depends on the human's git configuration and
/// never runs their hooks.
```

Four flags are supplied (`git.rs:668`):

```rust
run(
    dir,
    &[
        "-c", "user.name=mush",
        "-c", "user.email=mush@local",
        "commit", "--no-verify", "-qm", subject,
    ],
)?;
```

`commit.gpgsign` — a line in the human's `~/.gitconfig` or in the repository,
not a hook — is not among them. The probe (`crates/mush-core/tests/audit_probe.rs`,
a repository with `commit.gpgsign=true` and `gpg.program=/bin/false`, which is
what a machine with no key available looks like):

```
F2 commit_all -> Err("error: gpg failed to sign the data:\n(no gpg output)\nfatal: failed to write commit object")
F2 reclaimable -> Kept(".mush/wt/8 has 1 uncommitted path in it")
```

and by hand, with the code's own command line:

```
$ git -c user.name=mush -c user.email=mush@local commit --no-verify -qm 'mush #9: work'
error: gpg failed to sign the data:
(no gpg output)
fatal: failed to write commit object
exit=128
$ git -c user.name=mush -c user.email=mush@local -c commit.gpgsign=false commit --no-verify -qm 'mush #9: work'
exit=0
```

**Blast radius.** On a machine whose git signs by default, *every* isolated
child ends as `Work::Uncommitted { error }` (`agent.rs:250`), so the parent
reads "· mush/8 uncommitted (error: gpg failed to sign the data: …)" instead of
a revision, and the worktree is then correctly *kept* — it holds uncommitted
work — which means it is counted by `git::unlandable` (the cap counts what no
sweep will take, `git.rs:626`) and after `MAX_WORKTREES` (70) spawns are
refused with "Land or drop one first" while the human's git is the thing at
fault. The work is not lost and the failure is announced, which is why this is
minor rather than major; the doc claim is simply false, and the fix is one flag.

**Suggested fix.** Add `-c commit.gpgsign=false` (and, for the same reason,
`-c gpg.format=openpgp` is *not* needed once signing is off; consider `-c
core.hooksPath=` if `--no-verify` ever proves insufficient) to the argv, or
weaken the doc to say which two configuration keys are overridden.

**Acceptance test.** `a_signing_config_does_not_stop_the_commit`: a repository
with `commit.gpgsign=true` and `gpg.program=/bin/false`, a real worktree with
one written file; `commit_all` returns `Ok(Some(sha))`, `subject_of` reads the
subject back, and the tree is clean afterwards.

---

## F3 — The dropped-turns note is identified by its text, so a user line that *is* it becomes the note (and can land inside a batch)

**Severity: minor. Proven** (the consequence is a request a strict server
rejects; the trigger is an exact match on one sentence).

Every reader of the note decides "is this the note?" by comparing the message's
text — `transcript.rs:474`:

```rust
pub fn is_dropped_note(message: &Message) -> bool {
    message.role == "user" && message.text() == DROPPED_TURNS_NOTE
}
```

and both writers act on it: `trim_history` retains every note-shaped message
*out* (`transcript.rs:384`), and `insert_dropped_note` puts one back at index 2
(`transcript.rs:464`):

```rust
fn insert_dropped_note(messages: &mut Vec<Message>) {
    let at = 2.min(messages.len());
    messages.insert(at, Message::user(DROPPED_TURNS_NOTE));
}
```

A `user` message is not only a turn boundary: it is the opening task (index 1,
which the trimmer may never cut), the human's nudge, and a parent's brief. Any
one of them that is word for word `DROPPED_TURNS_NOTE` is removed from where it
was and re-inserted at index 2. The probe (a four-message transcript, nothing to
cut at a 10,000-byte budget):

```
F3 before: ["system:you are mush", "user:The oldest turns of ", "assistant:working", "user:carry on"]
F3 after:  ["system:you are mush", "assistant:working", "user:The oldest turns of ", "user:carry on"]
```

— the human's line moved *past* an assistant message, and index 1, the opening
task [`trim_history`] promises to keep ("`messages[0]` is the system prompt and
`messages[1]` the opening task"), is now an assistant message.

With a tool call in play the same move lands the line inside a pair
(`transcript.rs:120`'s own doc calls the dangling call the shape a strict server
rejects):

```
F3b roles after a trim: ["system", "assistant(calls)", "user", "tool", "user"]
     0: system "you are mush"
     1: assistant "working"
     2: user "The oldest turns of this"
     3: tool "result" id=Some("call_0")
     4: user "carry on"
```

A `user` message now sits between an assistant's tool call and its result —
exactly the shape `repair_tool_pairs` exists to normalise (`transcript.rs:120`)
— and `repair_tool_pairs` runs only when a transcript is *adopted* (the UI's
copy at a run boundary, `agent.rs:1786`), so within a run every request carries
the broken shape until the human types. `chat.rs:2492` reads the same predicate
to paint the line in mush's voice, so a *pasted* note is also displayed as
mush's own sentence rather than the human's.

**Blast radius.** Low probability, high surprise: the human (or a parent
writing a brief) must reproduce a 200-character sentence exactly. When it
happens, the human's own words are silently relocated, the child's brief is
destroyed (its opening task is replaced by a sentence about a trim that never
happened), and the conversation can be rejected by the endpoint with a complaint
about `tool_call_id`s — a failure whose cause is invisible in every surface the
human can read.

**Suggested fix.** Identify the note by provenance, not by prose: a
`MessageKind`/`role`-adjacent flag (the type already has non-wire fields —
`images`, and `tool_call_id` is `Option`), set only by `Message::note()` and
read by `is_dropped_note`; the text stays exactly as it is for the model. A
cheaper stop-gap: `insert_dropped_note` may never insert at an index that
follows an assistant message with tool calls — but that only moves the symptom.

**Acceptance test.** `a_user_line_that_quotes_the_note_is_not_the_note`: a
transcript `[system, user(DROPPED_TURNS_NOTE), assistant(calls), tool, user]`
put through `trim_history` keeps the human's line in place, keeps `[system,
user, assistant, tool, …]` as its first five roles, and `trim_history` returns
`Some(note)` only when something was really cut; plus
`the_note_is_never_inserted_inside_a_batch` on `insert_dropped_note`'s callers.

---

## F4 — `run_command`'s schema describes a cut where the code kills the command

**Severity: minor. Proven by the code path and by an existing test** (the
behaviour is pinned; the missing fact is the one the model reads).

The whole of what the schema says about a big result (`prompt.rs:241`):

```
"Run a shell command in the workspace root. A result too big for the context \
 window is cut, and the cut says how to read on."
```

The code does something else: a command that writes past `CMD_OUTPUT_LIMIT`
(`jobs.rs:131`) is **killed**, on the same watcher whichever road launched it
(`jobs.rs:208`, `jobs.rs:224`):

```rust
if written > CMD_OUTPUT_LIMIT {
    return Some(Stopped::TooMuchOutput);
}
```

and the result tells the model afterwards (`agent.rs:5399`):

```rust
Ended::Stopped(jobs::Stopped::TooMuchOutput) => {
    format!("[killed: output passed {CMD_OUTPUT_LIMIT} bytes; the first {cap} are above]")
}
```

The foreground road is the one that matters here, and it is pinned:
`a_runaway_writer_is_stopped_at_the_output_limit` (`agent.rs:7762`) drives
`run_shell(..., Detach::No, ...)` with a writer and asserts `output passed` plus
`machine.kills() == 1`. `docs/mush.md:203` repeats the same half-truth ("output
capped to fit the window").

**Blast radius.** The model's plan is built on the wrong model of the tool: "the
result is cut, so I will read on with `offset`/`sed`" — but the command is
*dead*, its side effects are whatever it managed to do, and a `cargo test` run
killed at 8 MiB never printed its summary. The prompt's machine block is worse,
because it states the rule the kill violates: "A long command detaches into a
job instead of dying" (`prompt.rs:35`). The model learns the truth only from the
result line, after a kill it did not expect, and a second identical attempt is
then stopped as a loop (five identical rounds) rather than explained.

**Suggested fix.** One clause in `run_command`'s description (and, less
importantly, a sentence in `MACHINE`): a command that writes past
`CMD_OUTPUT_LIMIT` is killed, the result says so, and the road on is a narrower
command. Both numbers live in one place already (`jobs.rs`), so the sentence
cannot drift.

**Acceptance test.** `the_command_schema_names_the_output_kill`: assert
`tool_schemas()`'s `run_command` description contains "killed" and the
`CMD_OUTPUT_LIMIT`/MiB figure, and that the machine block does not claim a
command never dies — beside the existing behaviour test, which stays as the
proof that the sentence is true.

---

## F5 — A child's worktree has no submodule contents, and the child is not told

**Severity: minor. Proven at the git level.**

`worktree_add` runs git verbatim (`git.rs:332`):

```rust
run_named(
    dir,
    "worktree add",
    &["worktree", "add", "-b", &branch, path.to_str().unwrap_or(""), base.unwrap_or("HEAD")],
)
```

`git worktree add` does not populate submodules (there is no
`--recurse-submodules` to pass: `git worktree add -h | grep -i submodule` is
empty on git 2.55.0), and nothing in mush runs `git submodule update` in the new
checkout. The probe — a repository with one local submodule at `lib/sub`, then
`worktree add -b mush/3 .mush/wt/3 HEAD`, the shape `worktree_add` uses:

```
--- main checkout:
s.txt
--- a worktree forked from HEAD (worktree_add's own shape):
HEAD is now at 381d1eb addsub
.
..
--- is the submodule populated there?
EMPTY (the file the base ref holds is not in the child's workspace)
--- what git status says in the child:
```

`git status --porcelain` is empty: the child cannot tell that a tracked
directory is empty, and neither can the parent. The child's system prompt says
only that it works in "a worktree of your own branch" (`prompt.rs:114`), and the
delegation policy promises "the child gets its own worktree and branch forked
from that ref" (`prompt.rs:55`) — both true of refs and misleading about the
tree.

**Blast radius.** In any repository with submodules (a vendored dependency, a
`third_party/`, a docs theme), a child told to build or run the tests finds an
empty directory, and every surface reports the workspace as clean and the base
as present. The child's own road out is `git submodule update --init` — measured
in the same probe, and it works (the submodule appears at the recorded commit
and `git status` stays clean) — but only if the model thinks of it, and it needs
the network for a real remote. A child that does not will burn its run on a
build that cannot work, or "fix" the empty directory by writing over the
gitlink the base's tree carries. Nothing in the child's prompt, the brief road
or the spawn reply mentions that its tree is a checkout of refs rather than a
copy of the parent's.

**Suggested fix.** After a successful `worktree add`, run `git submodule update
--init --recursive` inside the new checkout when the tree carries a
`.gitmodules`, and if that fails, do not fail the spawn — name it in the spawn
reply and in the child's prompt ("this workspace's submodules are not
populated; `git submodule update --init` first"). The prompt sentence that
needs to exist either way is one line: *a fresh worktree is a checkout of refs,
not a copy of the parent's tree*.

**Acceptance test.** `a_submodule_repo_gets_its_submodules_in_the_new_worktree`:
a repository with a local submodule; `worktree_add` for an id; the submodule's
file exists at the recorded commit in the new checkout, or (if the fix is the
honest refusal) the spawn's answer names it and the child's prompt carries the
`git submodule update --init` road.

---

## F6 — A dead actor thread is invisible: a row that spins, a parent that waits 600 s, and a corpse the UI hands back as "parked"

**Severity: major. The silence is proven** (a throwaway model client that
panics, driven by the delegated deep read); **the trigger — a panic reaching an
actor — is not** (no reachable panic was found on model or repo input; the
closest two are the `expect`s below).

`start` drops the thread's handle and no surface polls liveness (`agent.rs:1514`),
and the bookkeeping that says "this agent is running" is only reached on the
normal path (`agent.rs:1576`):

```rust
let builder = std::thread::Builder::new().name(format!("mush-agent-{id}"));
if let Err(error) = builder.spawn(move || actor_main(actor, initial, start_immediately)) {
```

```rust
actor.ctx.live.fetch_add(1, Ordering::SeqCst);
let result = run_loop(&actor, &mut state, &mut transcript, &cancel);
actor.ctx.live.fetch_sub(1, Ordering::SeqCst);
```

A panic inside `run_loop` — a tool, a parse, a model reply — unwinds past the
`fetch_sub` and past the `ChildDone` that `tell_parent` sends at the end of the
body. The process-wide panic hook (`main.rs:1103`) restores the terminal and
nothing else; `catch_unwind` appears nowhere in the tree. *Child probe* — a
throwaway model client that panics mid-reply:

```
thread 'mush-agent-1' panicked at crates/mush/src/agent.rs:15952:17:
PROBE parent heard: Ok("ChildRunning { id: 1 }")
PROBE child mailbox alive: false
PROBE books still say running: true · listing: #1 ◐ running
```

So the run announces itself and then nothing: no `ChildDone`, no `Work`, no
`Done`/`Error`/`Stopped` event. Worse, the death is then *misread as a park*: the
only detector of a gone receiver is a failed send, and `park_history` treats
exactly that as "Already parked" (`app/mod.rs:3231`), so the node stays and
`control message` answers "its actor was parked, so mush is waking one: this
resumes it" (`agent.rs:4762`) — a revival that runs on the *UI's* copy of the
conversation, so whatever the dead run held that the UI never received is gone.
And the vocabulary exists: `Outcome::CutOff`'s own doc says a run was cut off
because "the process went away with it in flight, **or the actor's thread did**"
(`agent.rs:209`), and the restart road files it from the stored session
(`app/mod.rs:776`) — but nothing in a live session ever detects the second case.
Two `expect`s are the nearest reachable panics, both on the workspace root
(`agent.rs:1295`, `agent.rs:1406`):

```rust
let ws = Workspace::new(&ws_root).expect("workspace root must exist");
```

**Blast radius.** Facts asserted to the model and to the human with nothing
behind them. To the model: `spawn_tool` refuses with "cannot spawn: {MAX_AGENTS}
agents are already running tree-wide (the limit)" (`agent.rs:3799`) while some of
those sixteen are dead threads — a refusal whose own sentence is false, and no
`wait` can clear it. To the parent: `child_listing` prints `#N ◐ running`
(`agent.rs:4487`) for a child that will never finish, and `wait` blocks its full
600 s and answers "#N still running" (`agent.rs:4349`) — and the parent waits
again, forever, on a truth that cannot change. To the human: a corpus the UI
hands back as resumable, and a transcript that silently loses whatever the dead
run had in flight but never emitted. A `panic = "abort"` profile would turn the
same panic into the whole session's death, which is worse; nothing in between
exists today.

**Suggested fix.** `catch_unwind` around the actor body, filing the payload as
`Outcome::CutOff` — the variant exists for exactly this and the parent already
knows how to read it — and, independently, hold `ctx.live` in a guard struct
whose `Drop` decrements, so a slot cannot leak even when the reporting road is
not reached. `park_history` should tell "parked" from "dead" before it lets a
corpse be handed back as resumable (an alive flag the closing actor clears, or a
`JoinHandle::is_finished()` poll on the tick).

**Acceptance test.** `an_actor_thread_that_dies_mid_run_is_reported_cut_off`:
a model client that panics mid-reply; the parent receives one `CutOff` (never a
`running` line), `ctx.live` returns to its previous value, and a later `control
message` does not claim the child was parked.

---

## F7 — The spawn cap's arithmetic is not the sweep's, and its sentence claims more than it measured

**Severity: minor. Proven** (probe through `git::unlandable`/`reclaimable`).

`MAX_WORKTREES` counts "what no sweep will take" (`git.rs:626`):

```rust
pub fn unlandable(root: &Path) -> Vec<u64> {
    …
    .filter(|id| {
        matches!(
            probe(root, *id, "HEAD", &base_sha, None),
            Reclaimable::Kept(_)
        )
    })
```

Every worktree is asked the question against **`HEAD`** and with **no fork
revision**. The sweep that actually takes worktrees asks a different question,
from facts it owns (`app/mod.rs:1014`):

```rust
let base = node
    .parent
    .and_then(|parent| self.tree.node(parent))
    .and_then(|parent| parent.branch.clone())
    .unwrap_or_else(|| "HEAD".to_string());
…
let found = git::reclaimable(&root, id.0, &base, fork.as_deref());
```

For a nested child — one spawned with `base = mush/<parent>`, an ordinary depth-2
delegation — those differ. A parent branch that is not itself merged into HEAD
plus a child merged into it is fully landable, and `unlandable` cannot see it.
The probe:

```
F7 the sweep's question (the node's base, the node's fork) -> Landable(Merged)
F7 the cap's count, while both checkouts are still on disk -> [1, 2]
F7 the sweep's deed -> Removed { branch_kept: Some("mush/2"), landing: Merged }
F7 the cap's count after the sweep -> [1]
```

**Blast radius.** A tree of nested work — each child merged by its parent, each
parent unmerged — reaches the cap while almost nothing on disk is really
unlandable, and `spawn_tool` then refuses a delegation with
(`agent.rs:3782`):

```
"cannot spawn: {} isolated worktrees already exist and none of them is landable \
 (the limit is {}) — each holds an unmerged branch or uncommitted work: … \
 Land or drop one first: merge or delete its branch, then …"
```

The model is handed a fact about the repository that is false ("each holds an
unmerged branch"), a remedy for it that is already done ("merge or delete its
branch"), and no way to tell which of the named ids is the real one. At best it
wastes turns; at worst it deletes or discards a worktree the sweep was about to
land, or repeats the false sentence onward to the human. Nothing is lost — the
cap is deliberately conservative and a refused spawn is not a lost delegation —
which is why this is minor.

**Suggested fix.** Either give the counter the sweep's facts — the caller that
knows the tree (`App`, which already holds every node's `base` and `fork`) can
pass the pairs into the spawn road — or, much cheaper and honest, make the
sentence and the doc claim only what was measured: `unlandable` reports what is
not landable *against HEAD*, and the refusal says "N worktrees are not landable
against HEAD — a nested child merged only into its parent's branch counts
here". The mechanism is `probe(root, *id, "HEAD", …, None)`; the bug is the
word "landable" in its doc.

**Acceptance test.** `a_nested_child_merged_into_its_parent_is_not_counted`:
the probe's repository (a parent `mush/1` with a commit, a child `mush/2` forked
from `mush/1` and merged back into it); `unlandable` does not name 2 while
`reclaimable(root, 2, "mush/1", Some(fork))` is `Landable`, and the refusal for
a full cap never says "unmerged" about a branch the sweep can land.

---

## F8 — A run-end commit taken in a workspace that is no longer a worktree commits the human's own checkout on the human's branch

**Severity: major. The mechanism is proven; the trigger is a race** (a worker's
write after its worktree is removed). This is the biggest single risk in the
document.

`WORKTREE_DIR` is *inside* the repository (`git.rs:177`), so `.mush/wt/<id>` that
is no longer a worktree is an ordinary directory inside the human's checkout —
and `git -C <that dir>` walks up to the main repository. `commit_all` runs
exactly that (`git.rs:663`), and the run end calls it with the actor's workspace
root unconditionally (`agent.rs:1601`):

```rust
let work = actor.branch.clone().map(|branch| match commit_worktree(actor.ws.root(), actor.id, &actor.brief, &outcome) {
```

The probe (a plain directory at `.mush/wt/1` inside a repository, a human's
modified file and untracked file present):

```
--- a plain directory under .mush/wt, after a missed worktree removal:
NOT a worktree (plain dir)
--- what the code's commit_all would ask, run in that plain dir:
 M a.txt
?? notes.txt
--- and its add -A + commit:
COMMIT MADE ON THE HUMAN'S BRANCH
0dc0686 mush #1: the brief
 a.txt     | 1 +
 notes.txt | 1 +
```

The human's half-finished edit and untracked file are staged and committed — on
the human's branch, under mush's subject and mush's identity — and the row then
reads "committed 0dc0686 on mush/<id>" while that branch was deleted and had
nothing to do with it. The `git add -A` also leaves the human's index full of
files they never staged.

Two roads put an actor in that state, and the tree already documents both:

- **a hand `git worktree remove`** — named in `worktree_gone`'s own doc
  (`agent.rs:2143`) as the reason the guard exists;
- **`reclaim_isolated`** (`app/mod.rs:1184`), which asks
  `git::reclaim(&root, id, "HEAD", None)` for every `mush/<id>` branch git names
  **with no reference to the tree, to any node, or to anything in flight**, and
  runs mid-session on Ctrl-N, which rebuilds the tree and calls
  `discover_worktrees` (`app/mod.rs:3122`), whose first act is
  `self.reclaim_isolated()` (`app/mod.rs:1237`) — after `stop_all()`, which only
  *sends* `Shutdown`. The ordinary sweep is careful here — `refresh_git` excludes
  `in_flight` nodes and their `waking` parents (`app/mod.rs:1000`) and
  `reclaim_own_worktree` refuses while children or jobs are out (`agent.rs:2077`)
  — so this is a guard the code has everywhere except the road that runs on the
  human's key.

Every *message* road to a branch-carrying actor whose worktree is gone is gated
(`Nudge` and `Steer` at `agent.rs:1915`/`1928`, the human's own words at
`app/mod.rs:2308`, the parent's `control message` at `agent.rs:4719`), but a run
already in flight when the directory goes is not checked at all — and a tool call
already dispatched completes, its `write_file` recreating the removed directory
as a plain one (S1's shape, quoted in `worktree_gone`'s doc). The write after the
removal is the half that is *suspected*; the commit that follows is the half that
is proven.

**Suggested fix.** One guard closes the whole class, and it belongs in `git.rs`:
refuse to commit when the workspace is not a worktree — `dir.join(".git")`
absent, or `git rev-parse --show-toplevel` not equal to `dir` — and report it as
`Work::Uncommitted { error: "<dir> is no longer a worktree" }` instead of letting
`git -C` find the human's repository. Independently, give `reclaim_isolated` the
sweep's live-tree guard (skip ids a node holds in flight or waking), so the state
is not created under a live actor at all.

**Acceptance test.** `a_commit_never_leaves_the_worktree`: after
`git worktree remove --force` on a worktree whose directory is recreated as a
plain directory, `commit_all` refuses, the main checkout's `HEAD` and index are
untouched, and `Work` reports the failure; plus
`ctrl_n_never_reclaims_a_worktree_a_live_node_holds`.

---

## F9 — `base="HEAD"` is resolved in the application's root, not in the spawning agent's workspace

**Severity: major. Proven** (a *child probe* measured the two `HEAD`s; the code
path is one line).

`spawn_tool` resolves the base name against `ctx.root` — the root the *App* was
started with (`agent.rs:3815`):

```rust
let base: Option<String> = match named {
    Some(name) => Some(git::resolve(&ctx.root, name).ok_or_else(|| {
        format!("unknown base `{name}`: no commit, branch or tag by that name")
    })?),
    None => None,
};
```

For the root agent `ctx.root` *is* its workspace. For an isolated agent — a
depth-1 orchestrator, which the policy explicitly invites to spawn with a base —
its workspace is its own worktree, and `HEAD` there is its own branch. The
delegated probe measured them apart: `git rev-parse HEAD` in the main checkout
`acccb65…` against `f0c064d…` in `.mush/wt/2`. So the most natural spelling of
"fork my child from where I am now" forks it from *someone else's* HEAD —
silently: the worktree is created, the branch is made, and the reply says
`at <sha>` about a commit the caller never named. This is H7's class ("a base is
a promise about history, and a child running on the wrong one is worse than no
child"), reached through a base that *was* resolvable.

The second half is a divergence the same line creates: the actor keeps the
model's *name* and reclaims against it (`agent.rs:3966`, `agent.rs:2081`), while
the UI derives the base as the parent's branch (`fork_base`, `app/mod.rs:1137`,
and the inline copy at `app/mod.rs:1014`). The run-end verdict ("merged" / "1
commit nobody merged") and the row's reconciliation with the sweep can therefore
be answers to two different questions about one branch — while
`worktree_add`'s doc describes only the UI's derivation ("the parent agent's
branch, or `HEAD` when the caller has none").

**Blast radius.** A nested delegation that names a ref whose meaning depends on
where it is read: `HEAD`, `main`, `@~1`, a tag. The child's history lacks the
parent's commits, so its work is built on a tree that is not its parent's — and
nothing says so: both the reply and the row name a revision, and both are true
statements about the wrong commit.

**Suggested fix.** Resolve against the *caller's* workspace
(`git::resolve(actor.ws.root(), name)`) — the object store is shared, so
`worktree_add` can still run from `ctx.root` with the resolved id — and say in
the `base` description and the delegation policy whose `HEAD` is meant ("as this
agent's own workspace sees it"). Then keep one derivation, so the actor and the
UI ask their run-end question of the same ref.

**Acceptance test.** `a_nested_base_head_forks_from_the_parents_worktree`: a
parent on `mush/1` with one commit of its own, `spawn_agent(base="HEAD")` from
it; the child's fork revision equals the parent's HEAD and its history contains
the parent's commit; plus `the_actor_and_the_ui_agree_on_the_base`.

---

## F10 — An agent id is returned to the pool after a *partial* `worktree add`, and the next isolated spawn draws it and dies

**Severity: major. Proven** (a real repository with a failing `post-checkout`
hook, run here; found independently by the delegated read).

`spawn_tool` treats *every* `worktree_add` error as "git made nothing"
(`agent.rs:3894`):

```rust
Err(reason) => {
    ctx.ids.lose_agent(id);
    return Err(format!("cannot start from `{name}`: {reason}"));
}
```

and `lose_agent`'s own doc states the licence it is being used without: "once
`worktree_add` has returned, a `mush/<id>` branch or a `.mush/wt/<id>` directory
may exist, and the number must stay spent even if the spawn fails afterwards"
(`ids.rs:154`). The probe — one executable failing `.git/hooks/post-checkout`:

```
--- worktree add with a failing post-checkout hook:
HEAD is now at e80ce37 init
rc=1
--- was the branch created?
+ mush/1
--- is the checkout there?
a.txt
```

git returns 1 *after* creating the branch and the checkout; `lose_agent` pushes
the id back and `next_agent` pops the lost pool first (`ids.rs:133`), so the next
isolated spawn draws the same id and dies on `fatal: a branch named 'mush/1'
already exists` — and because the pair keeps being returned and re-drawn, every
isolated spawn does, for the rest of the conversation. The reason the model reads
is worse than the collision: `run_named` reports git's *stderr*, which on this
path is git's own `Preparing worktree (new branch 'mush/1')` — a sentence that
names no failure at all. The same shape from another door: a workspace path with
non-UTF-8 bytes hits `path.to_str().unwrap_or("")` (`git.rs:340`), and
`git worktree add -b mush/3 "" HEAD` creates the branch and then dies with git's
own assertion (`rc=134`, `BUG: builtin/worktree.c:498`), exactly as measured
here.

**Blast radius.** One failing hook (a repo-wide `core.hooksPath`, a linter, a git
LFS post-checkout), one interrupted checkout, or a workspace whose path is not
UTF-8, and isolated delegation is wedged for the session: every `base` spawn
fails with a branch-exists fatal whose text the model cannot act on. The escapes
are a *shared* spawn (which drains the lost pool by consuming the id) or the
human running `git branch -D mush/<id>` by hand.

**Suggested fix.** Do not infer "nothing was created" from an error: ask
(`git::resolve(root, &branch_name(id))` and `worktree_path(root, id).exists()`)
and only then hand the id back — otherwise reserve above it
(`ids.reserve_agents(id + 1)`), which is what a leftover worktree already does.
Replace the `unwrap_or("")` with a refusal naming the path that could not be
passed to git.

**Acceptance test.** `an_id_comes_back_only_when_git_created_nothing`: a
repository whose `post-checkout` hook fails; `spawn_agent(base=…)` refuses, the
branch git made is reserved (the next isolated spawn draws a *fresh* id and
succeeds), and `git branch -l 'mush/*'` shows exactly one branch after two
spawns.

---

## F11 — An isolated spawn is refused in a workspace that is a subdirectory of a repository, and the refusal names a false reason

**Severity: major. Proven** (three commands, measured here).

`worktree_add`'s first gate is a filesystem test (`git.rs:316`):

```rust
if !dir.join(".git").exists() {
    return Err("not a git repository".to_string());
}
```

while the base was resolved one call earlier with `git -C dir rev-parse`
(`git.rs:687`), which works *anywhere inside* a repository. In a workspace that
is a subdirectory of a repository — which is what running `mush crates/mush`, or
opening any folder below the repository root, gives — the two disagree:

```
--- resolve works in the subdirectory (what spawn_tool asks first):
068397e9d23970d13c99a57fd76117235f43cc30
rc=0
--- does .git exist in the subdirectory (what worktree_add requires)?
NO .git in sub (gate refuses here)
--- would git itself have made a worktree from the subdirectory?
HEAD is now at 068397e init
rc=0
```

`has_commits` uses the same `.git`-free `git` call, so this gate is the odd one
out in its own file.

**Blast radius.** The whole isolated road — the recommended one, whose
justification is parallelism — is unavailable in any workspace below the
repository root, and the sentence the model reads ("not a git repository") is
false about the repository the human opened mush inside. The model will either
report that onward as true, or fall back to a shared child, serializing the
siblings the human asked for. No work is lost, which is why it is not higher.

**Suggested fix.** Delete the `.git` test and let git's own failure speak, or ask
the question git answers: `git rev-parse --git-dir` in `dir` — the door `resolve`
and `worktrees` already use. Keep the refusals that are real (not a repository,
no commits, git's own message), because those are the ones a human can act on.

**Acceptance test.** `an_isolated_spawn_works_below_the_repository_root`: a
workspace at `repo/sub`; `spawn_agent(base="main")` creates
`repo/sub/.mush/wt/<id>` on `mush/<id>` forked from the base, and a genuinely
non-repository workspace still refuses with its own sentence.

---

## F12 — A wrongly-typed `base` silently drops isolation

**Severity: minor. Proven by reading.** `let named =
args.get("base").and_then(Value::as_str);` (`agent.rs:3814`) means
`{"base": ["main"]}` — or any non-string — reads as *no base*: the child runs
in the live checkout, the reply carries no `on mush/<id>`, and nothing is
refused. The schema declares `base` a string, so the model has to send a list to
trigger it — but the same crate's rule for typed arguments is the opposite
(`tools.rs:128`: "a value that is present but not a number is refused rather
than defaulted… how a read answers a question nobody asked"), and `wait`'s `on`
follows it (`agent.rs:4611`). This is B10's class, in the one argument whose
silent default costs isolation rather than a wrong number, and its blast radius
is F13's: a second writer in the human's checkout that the parent believes is in
its own worktree. *Fix:* read `base` like `on` — absent/`null` is the documented
default, anything else is a refusal naming the type. *Acceptance:*
`a_wrongly_typed_base_is_refused_never_read_as_no_base`.

---

## F13 — The one-shared-child rule is a per-parent book, so "this workspace" can hold two live writers

**Severity: minor. Proven by reading.** The guard filters this actor's own books
(`agent.rs:3848`): `state.shared ∩ state.running` — and only the parent's own
children are in them. A grandchild is never in the grandparent's books: the root
spawns shared A (the root's checkout), A's run ends while *its* shared child B —
working in the same checkout by construction — is still running, and the root is
now free to spawn shared C into it. The refusal's own words are the false part
("already runs in this shared workspace"), and so is the prompt's: "without one
the child works in this workspace, and only one such child may run at a time"
(`prompt.rs:57`) — true of one parent's children, not of the directory. *Fix:*
count live writers of the workspace from the tree (any node whose workspace root
is this one), or weaken the sentence to what the books measure. *Acceptance:*
`the_shared_workspace_rule_counts_every_live_writer_in_that_directory`.

---

## F14 — `title` is required by the schema, optional in the code, and not bounded to one line

**Severity: minor. Proven by reading** (the newline survives `sanitize`;
`text.rs:46`, and the delegated probe agrees). The prose promises more than the
code enforces in both directions. "title is three words naming it in the tree"
(`prompt.rs:54`) and "A 3 word description" (`prompt.rs:260`) is the only rule;
`spawn_tool` trims and takes anything non-empty (`agent.rs:3825`), the row
truncates to `TITLE_COLUMNS = 24` (`tree.rs:268`), and `truncate` *keeps* `\n`
and `\t`, so `title: "parser\nport"` reaches a one-line painter. Meanwhile the
schema *requires* `title` while the code degrades gracefully without it — the
model pays for a field the code treats as optional. Malformed rows are the row
painter's street (B9/B15's class), so the *paint* half is *suspected*; the
newline arriving is proven. *Fix:* fold the title to one line in `spawn_tool`
and say so in the description (or make the schema optional like the code).
*Acceptance:* `a_title_with_a_newline_cannot_reach_a_one_line_row`.

---

## F15 — `parse_commit_subject` is not the inverse when the failure text contains `"): "`

**Severity: minor. Proven by reading** (both functions quoted). `commit_subject`
writes `mush #{id} (failed: {error truncated to 40}): {brief}` (`agent.rs:369`)
and the parser splits the head with `rest.split_once("): ")` (`agent.rs:393`) —
the *first* such sequence. An error that itself contains `"): "` (an endpoint's
`refused (429): slow down`, a nested message) makes the split land inside the
error: the head still passes `strip_prefix("failed: ")`, so the subject parses
as a failure with a truncated error and a *brief* that is the error's tail. The
subject is the only record a leftover row has of its brief, so the row shows the
wrong task. *Fix:* parse the failed shape from the right, require the head's
parenthesis to close immediately after the bounded error, or escape the error.
*Acceptance:* `the_commit_subject_round_trips_for_any_error_text`.

---

## F16 — `has_commits`'s "no commits yet" refusal is unreachable from the spawn road

**Severity: minor. Proven by reading.** `worktree_add` has three refusals, and
the middle one — "the repo has no commits yet — commit first or drop isolated"
(`git.rs:319`) — fires only when `base` is `None`; `spawn_tool` resolves the base
first and passes `Some(sha)` whenever it is isolated, so on the only production
road this arm is dead, and the same `match` evaluates `has_commits(dir)` anyway:
one `git rev-parse` per isolated spawn whose answer is discarded. In a fresh
`git init` the model is told `unknown base \`main\`: no commit, branch or tag by
that name` — not the sentence written for this case. *Fix:* ask `has_commits`
before resolving the base (or fold the two gates into one) so the refusal a
human needs is the one they read, and the process is not spent. *Acceptance:*
`a_base_spawn_in_a_repo_without_commits_refuses_with_that_reason`.

---

## F17 — A failed commit is a row tail the next run clears, not the transcript line its doc claims

**Severity: minor. Proven by reading.** `Work::status_line`'s doc says the line
is "the transcript's status line and the listing agree" (`agent.rs:255`), but the
line travels as `AgentEvent::Status`, which the app paints as the row's activity
tail (`app/mod.rs:1573` → `ui.rs:175`), is refused for an agent that is not
running (`tree.rs:960`), and is cleared when the next run starts — while the
model sees `Work::Uncommitted` only if it calls `status` (`Work::digest`,
`agent.rs:272`). A commit that failed (a lock, a conflict, a full disk, F2's
gpg) is therefore easy for both the human and the parent to miss while the work
sits unlanded in a worktree. This is the §8.46 class — a sentence the model was
told and the human never was. *Fix:* file it through `chat.note_for` as well, so
the line outlives the next run, exactly as the doc claims. *Acceptance:*
`a_failed_commit_is_a_transcript_line_that_outlives_the_next_run`.

---

## The delegation contract, checked

The road — `spawn_agent` → worktree → run → commit → report → reclaim — was
read along its own length (`spawn_tool` `agent.rs:3791`, `too_many_worktrees`
`3765`, `commit_worktree` `4773`, `reclaim_own_worktree` `2076`,
`worktree_gone` `2097`, `message_agent` `4708`, `status_tool` `4451`,
`control_tool` `4517`, `wait_tool` `4132`, and on the app side the `Spawned`
registration `app/mod.rs:1509`, `discover_worktrees` `1229`,
`sweep_worktrees` `1089`, `agent_root` `2625`). What is **sound**, and against
what:

- **The prose's four claims about `base` are all true of the code.** A base does
give the child its own worktree and branch (`worktree_add`), forked from that
ref; siblings with bases do run in parallel (nothing serialises them but the
machine lock and `MAX_AGENTS`); without a base the child gets the *spawner's*
`Workspace` (`None => (actor.ws.clone(), None)`), so "this workspace" is the
spawner's, and a nested parent's shared child edits the parent's branch — which
is also what its own prompt names as its workspace; and the one-shared-child
rule counts only *this* actor's `state.shared` children that are *running*, so
a parked shared child does not block a spawn and an isolated sibling never does
(the old false reading is finding "audit row 7").
- **`base` is resolved before git sees it, and what the child got is read back.**
`git::resolve(&ctx.root, name)` turns the name into a commit id (so a `-`-shaped
name can never be an option), `worktree_add` gets the id, and the fork revision
is `resolve(new checkout, "HEAD")` — "the history the child *got*, not the one
that was asked for". The *name* is what the actor keeps for the run-end question,
and the doc says why (a merge during the run moves the tip).
- **The spawn reply carries what the parent needs to land the work**: the id, the
branch and the fork sha (`agent.rs:3987`), and `status` repeats the branch for
every isolated child (`agent.rs:4487`).
- **A failed spawn spends nothing**: `worktree_add` failing hands the id back
(`ctx.ids.lose_agent(id)`), and the reply names the base that could not be
resolved. A failure *after* the worktree exists keeps the id spent, which is the
invariant the comment states.
- **What the parent learns when a child ends**: `Work` has one sentence per
shape and every variant carries the branch (`Committed`/`Clean`/`Uncommitted`,
`agent.rs:249`), the digest is paired to the *run* it belongs to so an old branch
cannot read as a newer run's work (`work_for`, `agent.rs:973`), and a
`Work::Uncommitted` — the commit itself failed — reaches the parent's row and
`status` rather than being swallowed. A child **stopped** by the human is news
(`Outcome::is_news`), and a child whose actor was *parked* is not gone: a
`control message` is handed to the UI and resumes it (`hand_to_ui`), with the
parent's books marked running so `wait` does not answer the old result.
- **The keep/remove decision is git's answer, not mush's memory.** Both sides of
it — "does the base contain the branch" and "is the checkout dirty" — are
executed at the run's end, the fork revision is resolved once from the new
checkout, and an unresolvable base or a git refusal is a `Kept` with a sentence
a human can act on. The one gap in "dirty" is F1.
- **The child's prompt is the truth about the child's workspace**: `isolated` is
`branch.is_some()`, `root` is the workspace the actor's tools actually resolve
against (the same `Workspace` the actor carries), and `delegates` matches the
tool set (see the blind-spot below for the two spellings).
- **The report road is one road for four senders.** `tell_parent` is used for the
run's completion, a steered/woken child, a job's completion and a spawn that
failed to start a thread at all (`agent.rs:1534`), and a child's *run* identity
travels with the completion (`ChildDone { run, .. }`), which is what lets the
parent place the work fact on the right run.

## Verified sound

Checked, and found correct — each against the code or a test, not the prose:

- **The tool table is one list.** `TOOL_NAMES` is derived from `ToolName::ALL`
  by a `const fn` (`tools.rs:100`), `TOOL_NAMES` (`tools.rs:112`) and `parse`
  is the inverse with a round-trip
  test, and `prompt::tool_schemas` is asserted equal to `TOOL_NAMES` in order
  (`prompt.rs:331`). Adding a schema without an executor cannot compile in.
- **Every schema argument reaches the parser the same way.** Read argument by
  argument for all ten tools: `edit_file` (`path`, `edits` with
  `old_string`/`new_string`/`replace_all`), `read_file` (`offset` default 1,
  `limit` default "as many as fit the cap" — `arg_usize(args, "limit",
  usize::MAX)` then `result_cap`), `write_file`, `list_files` (`arg_path` default
  the root), `search` (`ignore_case` default false), `run_command`
  (`detach`/`exclusive`), `spawn_agent` (`brief`/`title` required, `base`
  optional), `status` (no properties, and it reads none), `control`
  (`id`/`action` required, `text` optional; the parser accepts `2`, `#2`, `c2`,
  `#c2` — strictly more spellings than the schema promises, which is the safe
  direction), `wait` (`on` optional string). `wait`'s `on` is real: the
  dispatcher routes it to `wait_on_tool` (`agent.rs:3746`), the property's
  "the machine lock is not waited for" matches that function, and a wrongly
  typed `on` is refused rather than defaulted (`agent.rs:4611`).
- **The numbers the schema prose quotes are the code's numbers.**
  `CMD_DETACH_AFTER` is 60 s (`jobs.rs:134`) and `JOB_MAX_AGE` is 4 h
  (`jobs.rs:145`); `WAIT_TIMEOUT_SECS` is 600 (`agent.rs:147`) against "gives up
  after 10 minutes"; `CMD_TIMEOUT_SECS` is 120 (`lib.rs:36`) against
  `docs/mush.md:203`; `MAX_JOBS` is 8 and named in the refusal the model reads
  (`agent.rs:5392`); `MAX_AGENTS` is 16 and `MAX_DEPTH` 3, both named in their
  refusals.
- **The turn cap and the write cap are gone from the prose, not just the code.**
  §8.47 and §8.48 removed them; `MACHINE`, `DELEGATION`, `ROOT_ROLE` and every
  schema sentence were read for a residue and there is none (the only "turn" a
  prompt names is the human's — "Ending your turn while children still run is
  fine"), and the spawn reply says "runs until it stops calling tools"
  (`agent.rs:3987`). `prompt.rs:531`'s test pins the absence of a turn budget.
- **The delegation policy is read by exactly the agents that have the tool.**
  `delegates` is `depth + 1 < MAX_DEPTH` at the spawn (`agent.rs:3944`) and the
  tool set is `actor.depth >= MAX_DEPTH` (`agent.rs:2124`), and a leaf's schema
  list is asserted to be `TOOL_NAMES` minus `ORCHESTRATION` (`prompt.rs:331`).
- **The transcript never leaves a call unanswered.** Every road out of a batch
  was read: a cancel mid-batch answers the rest with `error: cancelled`
  (`agent.rs:2834`), a reply cut off at the token cap answers each call with the
  reason, a refused reply does the same, a loop stop answers each call, and a
  cancel that arrives while a request is in flight drops the reply *before* it
  is recorded (so it cannot leave a dangling call at all, `agent.rs:2654`). The
  trimmer cuts only at `user` boundaries (`transcript.rs:429`), so a kept result
  always keeps its batch; `repair_tool_pairs` rebuilds a block with each call's
  result in call order and no message twice (`source` is a one-per-call
  assignment), and its tests cover the steering-inside-a-batch, dangling-call,
  duplicate-result, misplaced-result and legacy-id cases — read, and the
  behaviour matches each doc claim.
- **The arithmetic of the trim is one relation, saturating.** `trim_target` =
  4/5, `compaction_trigger` = 9/10, `Config::cmd_cap` = budget − 4/5 (the fifth
  a trim leaves), each with a test including `usize::MAX` and the
  `6_148_914_691_236_517_206` wrap case; `needs_compaction`'s bounds are strict
  below, inclusive above, and pinned by two tests.
- **`git.rs` never lets a name become an option.** `resolve` is the one door
  (`rev-parse --verify --quiet <name>^{commit}`) and `branch_stat` resolves both
  names before `diff`, with a test that `--output=evil` cannot reach git; the
  worktree path/branch names are one spelling each (`WORKTREE_DIR`,
  `BRANCH_PREFIX`, `worktree_rel`, `branch_name`, `worktree_id`) with a
  round-trip test.
- **The reclaim rule is conservative in the direction that matters.**
  `probe` refuses on an unresolvable base, on git failing to answer, on any
  commit the base does not contain, and on any uncommitted path; `remove` uses
  `branch -d` and never `-D`, reports a branch git would not delete as
  `Reclaimed::Removed { branch_kept: Some(..) }`, and the nested-child case is
  pinned by `a_merged_branch_git_will_not_delete_is_reported_as_left_behind`.
  A base that moved on after the spawn is still read as `NothingCommitted`
  thanks to the fork revision, and without a fork revision mush keeps the
  conservative `Merged` rather than guessing.
- **`worktree_add`'s three refusals are the three a human can act on** (not a
  repository; no commits yet — with `git binary unavailable` told apart from
  "no commits" by `head_answer`'s test; git's own message otherwise), and a
  failed add hands the agent id back (`ctx.ids.lose_agent(id)`) while a failure
  after it does not — which is the invariant the doc states.
- **`commit_subject` and `parse_commit_subject` are an inverse pair** for every
  outcome, one-line and word-bounded (`subject_brief`, `SUBJECT_COLUMNS`), with
  round-trip tests including the wide-character and ellipsis cases; and
  `commit_all` answers `None` rather than making an empty commit, with a test
  that says so.
- **The child's prompt names the path its tools really use**: `Workspace::new`
  canonicalizes the new worktree before `subagent_prompt(&child_ws.root_str(), …)`
  (`agent.rs:3944`), and that same `Workspace` is what the file tools resolve
  against and what `run_command` uses as cwd, so the prompt, the tools and the
  shell cannot disagree about where the child is.
- **`commit_all` works on a machine with no git identity at all.** A *child
  probe* ran the argv with `user.name`/`user.email` unset anywhere in the
  environment and repository: the commit succeeded with author
  `mush <mush@local>`, and an executable failing `pre-commit` was skipped by
  `--no-verify`. F2's `commit.gpgsign` is the one configuration key that gets
  through.
- **The startup order is the safe one**: `reclaim_isolated` runs *before* the
  session's agents are restored (`app/mod.rs:714`), so a leftover a restored node
  might claim is settled first, and `discover_worktrees` prunes git's stale
  registry entries (`git worktree prune`) before anything reads the list.
- **A completion is delivered once.** `ChildDone` is once-only per run and
  carries the run it belongs to, `AgentMsg::Work` is listed and never delivered,
  the delivery mark is the fold's own (`note_completion`), and the three roads
  (wait, fold, wake) are covered by `every_delivery_road_hands_a_result_over_once`.
- **`control stop` on a live child says `Stop::Parent`** (news to the parent), a
  park is not news, and the words for a run that was cut off without an actor
  exist (`Outcome::CutOff`) — F6 is the missing *detector*, not a missing
  sentence.
- **The stored session cannot smuggle a foreign tool call past the room:**
  `agent_root`/`live_branch`/`attach_worktree` answer "where does this agent
  work" from the same two facts (a branch and a directory on disk), so the UI's
  answer and the actor's cannot disagree (the same invariant H42/U13 pinned).

## Invariants this area claims with no test behind them

- **The prompt's `delegates` and the actor's tool set are one predicate spelled
  twice**: `depth + 1 < MAX_DEPTH` at the spawn (`agent.rs:3944`) and
  `actor.depth >= MAX_DEPTH` in `tool_schemas` (`agent.rs:2124`). Today they
  agree; nothing fails if someone changes one to `<=`. A test asserting "the
  spawned child's prompt carries `Delegation:` exactly when its schema list
  contains `spawn_agent`" would pin the pair.
- **"A reply with no tool calls ends the run" and its consequences for a
  *brief*'s boundedness** are pinned for 220 turns (§8.47's test) but not for
  the loop guard's exemptions (a refused batch, a `wait` that slept) *across a
  long brief*; the prompt tells the model its brief is "bounded by the work",
  and the only loop test counts identical rounds.
- **The `Role`-as-`String` invariant**: nothing asserts that the only `role`
  values mush writes are `system`/`user`/`assistant`/`tool`, while
  `drop_orphan_results` and `repair_tool_pairs` branch on those strings and any
  other value silently clears a batch (`transcript.rs:255`).
- **`Message::weight` is the trim's, the fold's, the meter's and the attach
  gate's one ruler**, but no test asserts that the number the meter prints for a
  transcript equals the number `trim_history` compares against the budget for
  the *same* transcript (H44 fixed the missing note; a future message kind with
  a non-text payload would sail past both).
- **`WORKTREE_DIR` inside the repository is invisible to git only because
  `.mush/.gitignore` says `*`** (C5's mechanism). If that file is ever wrong —
  or a repository ships its own — the human's `git add -A` in the *main*
  checkout meets `.mush/wt/<id>` as an embedded repository and stages a gitlink
  recording a child's HEAD. No test drives `git add -A` at a root with a live
  worktree, and no test drives what F1 leaves behind either.
- **"A run's work is committed" for anything git cannot see**: a path the
  repository ignores (F1), an uninitialised submodule's contents (F5). The
  commit road has tests for the ordinary shape only.
- **"An id is taken only when git created nothing"** (F10): no test drives a
  *partial* `worktree add` — a failing `post-checkout`, an interrupted checkout,
  a path that cannot be a Git path.
- **"A base spawn works in a workspace that is a repository's subdirectory"**
  (F11): the gate is a `.git` test with no test of its own either way.
- **"Reclaim never touches a worktree a live actor holds"** (F8): the sweep is
  guarded and tested; `reclaim_isolated` — reached from Ctrl-N through
  `discover_worktrees` — is neither.
- **"The actor's base and the UI's base are the same ref"** (F9): the actor
  stores the model's name, the UI derives the parent's branch, and nothing
  asserts they agree for any spawn shape (a base that is a branch, a tag, a sha,
  or an expression).
- **"The commit subject parses back for any error text"** (F15): the round-trip
  tests use tame errors; nothing generates one containing `"): "`.
- **"The spawn cap's count is the sweep's question"** (F7): `unlandable` has
  tests for merged/dirty/unmerged shapes, none for the nested child whose base
  is its parent's branch.

## Not verified

- **Consecutive `user` messages on the wire.** After a trim the first roles are
  `["system", "user", "user", "user", "assistant", …]` (probe:
  `F4 roles after a trim`), because the note is a `user` message inserted right
  after the task and the first kept turn is itself a `user` line. The OpenAI
  shape accepts this; an Anthropic-shaped endpoint validates alternation. I have
  no live endpoint and no provider fixture that validates roles, so whether this
  is a defect depends on which of `provider.rs`'s rows mush is pointed at —
  named here as needing a live endpoint, not asserted.
- **Whether a strict server rejects `[{assistant + tool_calls}, {user}, {tool}]`**
  — the F3b shape — is likewise taken from `transcript.rs`'s own prose; no
  request was sent.
- **The submodule finding end to end**: the git behaviour is measured, but I did
  not drive a real `spawn_agent` against `scripts/mock_llm.py` in a submodule
  repository, so "the child fails the brief's gate" is read, not watched. A
  remote submodule (my probe was a local path) and `protocol.file.allow`
  restrictions are untested.
- **A panic reaching an actor** (F6): no reachable panic was found; the two
  `expect`s need the workspace root to vanish between the check and
  `Workspace::new`. The *silence* was probed with a panicking model client, not
  with a panic in the real tool road.
- **F8's trigger**: the write-after-removal window was not driven inside a live
  run; only the commit that would follow was reproduced. Whether `reclaim_isolated`
  and a live actor can interleave on Ctrl-N is read from the ordering
  (`stop_all` sends, the UI thread reclaims), not raced.
- **A row painted with a `title` containing a newline** (F14): the newline
  surviving `sanitize` is read; the paint is the row painter's street (B9/B15's
  class) and was not driven through a window.
- **Windows and macOS git behaviour**: `worktree add -b`, `branch -d`, mode and
  case-insensitive filesystems, and non-UTF-8 workspace paths (F10's second
  trigger) were probed on Linux with git 2.55.0 only.
- **Anything needing a real remote**: a remote-tracking base, a submodule fetch,
  a `core.hooksPath` hook on the human's machine, and a `.gitignore` that lives in
  a parent of the worktree root.

## Attacking the landed audits' fixes

The two landed audits cross-referenced above; their mechanisms are not
re-reported, but two of their proposed fixes would move a bug rather than end
one, and one touches the schema prose this audit owns:

- **B1's fix must not read the umask by setting it.** "a new file gets
  `0o666 & !umask`" is the right *result*, but Rust reaches the umask only
  through `libc::umask`, which *sets* it — process-global state that this
  program shares with one thread per agent, every `run_command` child and the
  session writer. A read-modify-write of the umask on the write path is a race
  whose failure mode is a file created with the wrong mode by *another* actor.
  The fix that has no race is to let the kernel apply it:
  `OpenOptions::new().mode(0o666)` on the temp file (and `0o777 & !umask` is
  then unnecessary), copying the target's mode only when the target exists.
- **B4's fix must not canonicalize `resolve` itself.** `resolve` is the shared
  lexical door for paths that do not exist yet (a new `write_file`), for
  `list_files` of a missing path, and for the *result lines* the model reads
  back (`rel()`); canonicalizing inside it changes what the model is told
  ("wrote `out/x`" while the listing shows `real/x`) and puts a
  `canonicalize` on every file of a search. The fix that keeps the model's view
  honest is a separate real-path resolve used only where the filesystem is
  touched, returning *both* the path to open and the name to report — plus the
  "still under the root" check B2 asks for.
- **B5's fix must not cap the count `write_file` reports.** Their second half
  ("`write_file` reads the file it replaces whole") meets a sentence this audit
  owns: the tool answers `wrote f — {before} → {after}` (`agent.rs:4875`). If
  the read is bounded, `before` must come from a streaming count or be reported
  as a lower bound ("at least 5 lines") — otherwise a capped read *invents* the
  one number in the answer, which is the same class as B8's search line.

## Drift found in files this audit must not edit

Named for the human; none of these files was changed.

- `docs/mush.md:203` says `run_command`'s output is "capped to fit the window".
  The code also **kills** a command that writes past `CMD_OUTPUT_LIMIT`
  (F4) — the sentence needs the second half, and so does the schema.
- `docs/mush.md:125`'s "every big-text result is capped; inputs are not" is
  true of the *result* view and silent about the kill; the same paragraph's
  example sentence is the one the model never reads when it is killed instead.
- Nothing in `docs/mush.md`'s tree section or `docs/findings.md` states what
  happens to an isolated child's gitignored output (F1); H10/H17 describe the
  reclaim rule as "nothing unmerged or dirty is ever touched", which is the
  claim F1 falsifies for ignored paths (`git.rs:557`).
- `crates/mush-core/src/git.rs:660`'s "a commit never depends on the human's git
  configuration" is false for `commit.gpgsign` (F2) — a doc comment, so it is
  mine to name and yours to fix.
