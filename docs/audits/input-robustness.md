# The input mush does not create, audited

A blind audit of every road an outside value travels into mush: the model
endpoint's request and reply framing and its phases, the tool-call arguments and
the commands they run, the paths and the filesystem, the session file, the
attach socket, the terminal that paints what a model, a command or a file name
says, and the environment and config files. The rule audited against is the
workspace's own: **a value from a wire, a file, an environment variable, a
terminal or a model is untrusted until it is read through a bounded, honest
road**, and a failure must be *said* rather than hidden.

**Base:** `e926526` (`merge: the record enters the two waves, their rows, and the
sentences they made false`).
**How:** the source read end to end (or every line of the parts named), with
`grep`/`sed`/`python3` probes over files only — no `cargo`, no test suite, no
smoke script, no server, no mush binary. Every finding below is a reading of the
code and names the exact bytes that make it bite; where settling a claim would
need a run, it is in *not verified* instead of asserted. Three readers worked
three slices in parallel and this file's author re-read every quoted line at its
line: the wire and the tool calls (this one), the paths and the filesystem, and
the session, the attach wire, the terminal and the environment.

**What this audit does not re-report.** Every row of `docs/findings.md` was
grepped first, and the row lists of all six `docs/audits/*.md` and four
`docs/dedup/*.md` files were read by id and title. Anything that is a row is
cited and skipped: **A1–A23** (A19 open), **B1–B27** (B12's remainder open),
**C1–C12**, **D1–D26**, **E1–E10**, **F1–F17**, **H1–H72**, **S1–S8**, **U1–U15**
and the ledger rows; still open and therefore not filed here: **H73–H80**,
**A19**, the §8.102 flakes and the M5 notes. **A21** and **A22** stay open as
recorded. Two fix agents were editing `app/mod.rs` (session restore) and
`app/tree.rs`/`ui.rs` (row placement) while this was written: the findings that
read those files say so, and their line numbers may move under the editors.

---

## The ledger, ranked

| id | severity | one line |
|---|---|---|
| IN1 | **major** | a 16-hex-digit chunk size on any chunk *after the first* wraps `out.len() + size`: a panic on the actor thread in a debug build, and in the shipped release build a `Refused` that blames the endpoint for the framing's own lie — A10's class, one road over |
| IN2 | **major** | an id one *below* the ceiling — a branch `mush/18446744073709551614`, or a stored `"id": 18446744073709551614` — is accepted and pins the agent counter at `u64::MAX`, whose next draw does `counter += 1`: a panic in debug, and in release a child on the root's own id `0` |
| IN3 | **major** | `.mush/session.json` is read with `fs::read`, with no shape check and no cap: a FIFO named as the store parks the open forever *before the first frame ever paints*, a committed symlink to `/dev/zero` reads without bound, and `restore_notices` is O(n²) in the stored rows |
| IN4 | **major** | the attach server's answer has no write deadline — only a read one — so a client that stops reading parks its thread for good, and 64 of them wedge the attach surface for the session |
| IN5 | **major (suspected)** | the message box paints the human's paste raw, and the bar paints the `⌂ <root>` cell raw: the record's own measured leak says a control byte in a Span reaches the terminal, while `docs/audits/tui.md:898` claims ratatui's `Buffer::set_stringn` filters it — one of the two is false, and the probe that settles it is one line |
| IN6 | **major** | the request mush *weighs* is priced in pixels while the request it *sends* is priced in megabytes: an image-bearing body of tens to hundreds of MB passes the window gate and is built in one `String`, where the message box's own byte bound shows the transcript could have one too |
| IN7 | **minor** | the listing and the search descend into every live child's `.mush/wt/<id>`: `list_files("")` answers with siblings' checkouts, and a path it prints is one `edit_file` will write into a *live* sibling's tree |
| IN8 | **minor** | the attach socket's *type* is never asked: a planted symlink at `.mush/mush.sock` either disables attach for the session or makes `mush read`/`edit --send` answer from, and find, a stranger |
| IN9 | **minor** | `list_files` walks and sorts the whole subtree before applying its cap, where `search` stops the walk at the cap's own limit — memory and time are the tree's, not the answer's |
| IN10 | **minor** | the whole-disk guard reads `find /*` as a path named `/*`, not as `/`: the one command the guard was written for runs |
| IN11 | **minor** | the TLS handshake is the one phase with no cancel poll — a Stop waits up to ten minutes — and the one phase whose own 30 s write ceiling is reported as the call's deadline |
| IN12 | **minor** | `/key` (and `/provider`, and a picked model) report `saved to …` *after* the save may have failed, and the single status slot erases the failure line |

The rest, in the appendix: IN13 (an IPv6 `Host:` header is not bracketed),
IN14 (an unset `HOME` puts the home config in the cwd), IN15 (`connect()` on
both attach sides has no deadline before it runs), IN16 (`list_files` runs with
`{}` when the arguments were sanitized away).

---

## IN1 — A late chunk-size line wraps the cap check: a dev-build panic, and in release the framing is refused as the endpoint's answer

**Severity: major.** `crates/mush/src/http.rs:1337-1353`:

```rust
        let size_field = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_field, 16)
            .map_err(|_| framing(format!("malformed chunk size: {size_field:?}")))?;
        ...
        if out.len() + size > MAX_BODY_BYTES {
            return Err(framing(format!(
                "a chunk size of {size} bytes would put the body past the {MAX_BODY_BYTES}-byte body cap"
            )));
        }
        out.extend_from_slice(&read_chunk_bytes(reader, size, watch)?);
```

`size` is a `usize` parsed from an endpoint-supplied hex line, so
`size = usize::MAX` is one legal claim (`FFFFFFFFFFFFFFFF`). Once a first chunk
has left `out` non-empty, `out.len() + size` overflows.

**The input.** A chunked `200` whose *second* chunk-size line is sixteen `F`s —
the claim A10 was filed about, one chunk later:

```
HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\nFFFFFFFFFFFFFFFF\r\n0\r\n\r\n
```

**Cost, two builds, two failures.**

- **debug** (`cargo run`, the whole test suite; `[profile.release]` sets no
  `overflow-checks`, so a debug build checks): `5 + usize::MAX` panics with
  *attempt to add with overflow* on the actor thread. What the human sees is
  finding F6's shape — the row spins, the parent waits out its 600 s cap, and
  nothing connects it to a byte that arrived on a socket.
- **release** (the shipped binary): the sum wraps to `out.len() - 1`, under the
  cap, so the `framing` refusal is skipped. `read_chunk_bytes` calls
  `read_exact`, whose own guard answers `body_too_large()`
  (`http.rs:830-833`) — a plain `InvalidData` **without** the `Framing` marker.
  `model.rs:180-186` classifies that `Refused`, not `Framing`: the run dies
  with *the endpoint's reply was refused: the response body is larger than
  83886080 bytes*, blaming a healthy endpoint for its framing breaking, and the
  connection is dropped rather than kept.

That is exactly the misdiagnosis finding A10 was filed for
(`docs/findings.md:4850`), reached by the second chunk. The test that pins A10 —
`a_chunk_size_claim_past_the_cap_is_framing_not_the_endpoints_refusal`
(`http.rs:2063`) — sends `FFFFFFFF` as the **first** size line, where
`out.len() == 0` and the sum cannot wrap. **Fix shape:** compare
`size > MAX_BODY_BYTES - out.len()` (or `out.len().checked_add(size)`), so the
wrap can only join the refusal the first chunk already gets; the acceptance test
is the probe above, asserting `is_framing(&error)` and no panic.

## IN2 — An id one below the ceiling pins the agent counter at `u64::MAX`, and the draw's unchecked `+ 1` panics or hands out the root's id

**Severity: major.** `crates/mush-core/src/git.rs:330-336`:

```rust
pub fn worktree_id(branch: &str) -> Option<u64> {
    branch
        .strip_prefix(BRANCH_PREFIX)?
        .parse()
        .ok()
        .filter(|id| *id < u64::MAX)
}
```

`git.rs:1379` pins the hole as intended: `assert_eq!(worktree_id("mush/18446744073709551614"), Some(u64::MAX - 1));`
— "one past the space reads as no name at all", but `id + 1` of `MAX - 1` **is**
`MAX`. The reservation sites saturate (`crates/mush/src/app/mod.rs:1515`, and
the same `reserve_agents(id.saturating_add(1))` at `1520` and `1599`), which
puts the floor *at* the ceiling instead of refusing it:

```rust
                git::Reclaimed::Kept(why) => {
                    // Kept work is not a reason to hand the number out again.
                    self.tree.reserve_agents(id.saturating_add(1));
```

and `crates/mush/src/ids.rs:137-147` is the draw that is *not* saturating:

```rust
    pub fn next_agent(&self) -> AgentId {
        let mut agents = self.agents();
        match agents.lost.pop() {
            Some(id) => AgentId(id),
            None => {
                let id = agents.counter;
                agents.counter += 1;
                AgentId(id)
            }
        }
    }
```

The session road reaches the same ceiling, and *accepts* the row that pins it —
`crates/mush/src/app/mod.rs:941-951`:

```rust
        for agent in stored {
            let floor = agent.id.saturating_add(1);
            if floor > agent.id {
                self.tree.reserve_agents(floor);
            }
            let why = if agent.id == AgentId::ROOT.0 {
                "it holds the root's id"
            } else if agent.id == u64::MAX {
                "the counter cannot be kept above its id"
```

For `id = u64::MAX - 1` the floor is `MAX`, the reservation runs, and the
refusal chain that follows never fires: the id is not the root's, not `MAX`, not
taken, and its parent is the root — so the row is **accepted**. (This arm is in
`app/mod.rs`, where two fix agents were working on the restore road; the lines
may have moved, and the code quoted is the code at `e926526`.)

**The input, two roads.** (a) a repository that carries an *unmerged* branch
`refs/heads/mush/18446744073709551614` — a clone, a colleague's push, a hand
`git branch`, a CI job; `reclaim_isolated` names it, `worktree_id` accepts it,
and an unmerged branch is `Kept`. (b) `.mush/session.json` with one row whose
`"id"` is `18446744073709551614` — a hand edit, a committed file, another
version's write.

**Cost.** The next `spawn_agent` on that tree draws:

- **debug**: `counter += 1` panics with *attempt to add with overflow* in the
  parent actor's thread; the run is filed as its ending (`actor_main`'s
  `catch_unwind`), so the parent's run is gone.
- **release**: the draw hands out `u64::MAX` (a child whose branch
  `mush/18446744073709551615` `worktree_id` then refuses, so that worktree gets
  no row and no sweep ever reclaims it), and the *next* draw returns `0` — the
  root's id, and two nodes of one id is finding D2's own blocker ("two rows of
  one id lose a painted row, and a real key then panics the pane").

**Recorded, and this escapes it:** D3 ("`agent.id + 1` overflows: a startup
panic from a file and from a git ref", `f7174db`) refused the *top* name and
kept the reasoning in `worktree_id`'s own doc — "Refusing the name here … keeps
the floor usable". `MAX - 1` is the name that shows it does not: the last
*holdable* id is `MAX - 2`. Fix: `*id < u64::MAX - 1` at the one door every
branch→id road passes, and a `next_agent` that refuses (or saturates with a
line) when the counter has no room, since a hand-edited file reaches the same
place. Both readers of this audit found this independently, which is why the two
roads are quoted together.

## IN3 — The store is read as bytes, whatever the name is and however big

**Severity: major.** `crates/mush-core/src/session.rs:368-372`:

```rust
    pub fn read(root: &Path) -> Stored {
        let bytes = match fs::read(session_path(root)) {
            Ok(bytes) => bytes,
```

The project already has this door for the *tools*' reads — `workspace.rs`'s
`whole_read` decides the shape from `fs::metadata` before anything is opened
("so a FIFO cannot park a read") and refuses a non-file — and the write door
refuses FIFO/device/socket too (`entry_for_write`). `Session::read` has neither
check, and it runs **after** the lock and **before** the terminal is entered:
`crates/mush/src/main.rs:1040` reads the session, `:1097` serves attach, and
only `:1143` reaches `TerminalGuard::enter()`. Second arm, same file:
`crates/mush/src/app/chat.rs:1604-1608` (`restore_notices`, called from
`App::new` at `app/mod.rs:891`, before the first frame):

```rust
        for notice in stored {
            ...
            restored.retain(|earlier| earlier.agent != AgentId(notice.agent));
            restored.push(Notice {
```

**The input.** (a) `mkfifo .mush/session.json` — POSIX `open(O_RDONLY)` on a
FIFO with no writer *blocks*; the repo's own §8.44 lesson on an image path is
"a FIFO named like an image blocked the open until a writer appeared". (b) a
repository that commits `.mush/session.json` as a **symlink to `/dev/zero`**
(git stores symlinks; `/dev/zero`'s `st_size` is 0, so `fs::read` never stops
growing). (c) 31 MB of ~400k stored `notices` rows.

**Cost.** (a) mush hangs before the first frame, holding the workspace lock;
nothing on screen says why, and the first `Ctrl-C` only sets a flag the
not-yet-started event loop reads (the *second* one kills at once —
`signals.rs`'s `register_conditional_default` — so it is a silent, frameless
start rather than an unkillable process: the human has to guess that a second
press is the road). (b) an unbounded read → the OOM killer ends mush in
that workspace. (c) the `retain` scan is ~10¹¹ `AgentId` comparisons — minutes
with no frame, from a file inside the documented store size. The *unsurprising*
31 MB case is only slow (the transcript parse), which is why the finding names
the shapes the missing check cannot survive rather than the size. Fix shape:
ask `symlink_metadata` for `is_file` and refuse past a cap with a line naming
the file, exactly as `whole_read` and `entry_for_write` already do; and build
the notices map in one pass instead of `retain` per row.

## IN4 — The attach server bounds what it reads and not what it writes

**Severity: major.** `crates/mush/src/attach.rs:321-352`:

```rust
fn serve_connection(stream: UnixStream, ui_tx: &Sender<Msg>, idle: Duration) {
    let from = peer_label(&stream);
    if stream.set_read_timeout(Some(idle)).is_err() {
        return;
    }
```
```rust
        let mut out = response.encode();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() || writer.flush().is_err() {
            return;
        }
```

The connection's only clock is the read timeout (`IDLE_TIMEOUT`); no
`set_write_timeout` exists on the server side (the CLI's `ask` sets one for
itself, `attach.rs:400-407`, and no server socket does). `SO_RCVTIMEO` cannot
fire while the thread is inside `write_all`, so the idle window never expires
for a reader that has stopped reading.

**The input.** A client that sends `read` with `since: 0` — the default — and
never drains the answer: `read`'s body is the whole transcript (no cap), which
overflows the ~200 KB socket buffer, and `write_all` waits in the kernel. 64
such clients hold `MAX_CONNECTIONS` (`attach.rs:63`) and every later client —
the human's own `mush agents` included — gets *unavailable: the attach surface
already holds 64 connections — retry when one is free*, a retry that never
comes.

**Cost.** The attach surface is dead until mush restarts, with 64 threads
parked; the TUI itself keeps painting. Bounded but wrong: the cap and the
refusal are honest and correct (B11), and the missing half is the write
deadline — the fix's own words, "every road *into* the socket is bounded", do
not cover the road out.

## IN5 — Two terminal-bound surfaces paint foreign text with no `sanitize` call, and the record disagrees with itself about whether ratatui filters it

**Severity: major (suspected — see the conflict below).**
`crates/mush/src/ui.rs:356-361`:

```rust
        rendered.push(Line::from(vec![
            Span::styled(lead, style),
            Span::raw(line.clone()),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(rendered)), input_inner);
```

`crates/mush/src/input.rs:61-65` (the paste/type road, no defanging) and
`crates/mush/src/app/mod.rs:1691` (the paste arm folds line endings only):

```rust
    pub fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, text);
```
```rust
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
```

Second site, same class — `crates/mush/src/app/screen.rs:891` builds the bar's
workspace cell and `crates/mush/src/ui.rs:461` paints it whole:

```rust
    let mut cells = vec![format!(" ⌂ {shown}")];
```
```rust
            Paragraph::new(Line::from(Span::styled(facts.clone(), dim()))),
```

**The input.** A bracketed paste whose bytes contain `\x1b` — copying terminal
output that kept its colours, a log or a repro with a raw escape, text copied
from a page. For the second site: mush launched with a workspace path that
contains an ESC. Both are the human's own input, not a model's — which is what
keeps this from being worse, and does not keep it from being an escape.

**Cost if ratatui does not filter.** The cell is written to the terminal
verbatim: the window is retitled (`ESC ]0;PWNED BEL`) or the frame wiped
(`ESC[2J`) while the human types. This repository measured the mechanism itself
at the pinned ratatui 0.29.0 (in the lock since the first commit), in
`33409d4`'s own message: *"The verifier's error body put a real carriage return,
an OSC window-title set and a `2J` frame wipe on the bar; a reply ending in a
20 000-character token put a `\r` in the agents footer, which returned the cursor
over the pane's own border."* The fixes that claim the door is total say so
in another (`crates/mush/src/main.rs:492-493`: "the one door every terminal-bound
string in this tree goes through"). It is a convention, and these two surfaces
never call it. (`ui.rs` is the file the row-placement fix is editing, so the
`Span::raw` call above may have moved by the time this is read.)

**The conflict, stated rather than hidden.** `docs/audits/tui.md:898` records the
opposite: "The painter cannot emit a terminal command: `Buffer::set_stringn`
filters graphemes containing controls, so even the unsanitized `⌂ {root}` cell
and `Config::label()`'s endpoint are cosmetic gaps, not escapes." If that
reading of ratatui 0.29 is right, `Span::raw("\x1b")` paints nothing (the
zero-width grapheme is skipped) and these two sites are blemishes. If
`33409d4`'s measurement is right, they are escapes. One of the two sentences in
the record is false, and it is not settled here: the source of the dependency is
outside this workspace and nothing may be run. **The probe that settles it** is
one `#[test]` that paints a `Span::raw("\x1b]0;PWNED\x07")` through `ui::draw`
at 80×24 and asserts the painted cells' symbols; if the ESC survives, these two
`Span::raw` calls (and the `⌂` cell) need `sanitize` like every other surface.

## IN6 — The request mush weighs and the request it sends are two different sizes

**Severity: major.** The guard, `crates/mush/src/agent.rs:2871-2876` (the box's
figures below are `app/mod.rs`'s, a file under live edit; the numbers are the
code's at `e926526`):

```rust
fn request_weight(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(Message::weight)
        .fold(0, usize::saturating_add)
}
```

the picture's price, `crates/mush-core/src/message.rs:248-257`:

```rust
        let payload = match self.pixels {
            Some((width, height)) => {
                crate::config::tokens_for_pixels(u64::from(width) * u64::from(height))
                    .saturating_mul(crate::config::BYTES_PER_TOKEN)
            }
            None => self.bytes.len(),
        };
```

the one road that could take the bytes back, `crates/mush/src/agent.rs:3003-3007`:

```rust
            .filter(|&index| {
                messages[index].role == "tool"
                    && messages[index].images.is_empty()
```

and the request writer, `crates/mush/src/http.rs:522-528`: `Content-Length` is
`body.len()`, built by one `serde_json::to_string` — the 80 MiB
`MAX_BODY_BYTES` (`http.rs:58`) is the *answer's* cap, not the request's.

**The arithmetic, exact.** A 2560×1440 png at `IMAGE_FILE_CAP` (2 MiB) is
3 686 400 pixels → `ceil(3 686 400 / 750)` = 4 916 tokens → × `BYTES_PER_TOKEN`
(3) = **14 748 bytes of weight**, while the `data:` URL it becomes is 2 MiB × 4/3
≈ **2.79 MB** — a factor of ~190, and an image's *header* is what sets the first
number: a crafted 2 MiB png whose IHDR says 100×100 weighs 42 bytes. Nothing
caps the count: an image-bearing message is exempt from `shed_newest_results`
(the `is_empty` above), and the trimmer only touches images when the *weight*
overflows. On the shipped 120 000-token window the budget
`(120 000 − 22 000) × 3 ≈ 294 KB` buys ~20 such pictures ≈ **56 MB of request
body**; with a window the endpoint itself advertised — `adopt_context` clamps at
`MAX_CONTEXT_TOKENS` = 10 000 000 (`config.rs:35, 853`), and `main.rs:1873`
adopts the model list's number — or `/context 1000000`, ~177 pictures ≈
**494 MB**, and GB-scale at the ceiling.

**Cost.** Hundreds of megabytes of allocation for a request the guard has just
said fits, held as the actor's image bytes plus the serialized `String`; the
write then has the call's 600 s deadline and no endpoint accepts ~500 MB in it,
so the turn is lost either way and an OOM takes the session. The recorded
decision (`docs/findings.md:2806`, `:3421`: "base64's 4/3 inflation deliberately
not modeled … an endpoint that tokenized the `data:` URL's base64 as text would
pay for the spelling too") covers the *meter's* pixels estimate, where pixels
are the right price; it does not cover the assembled request's bytes, and to
this guard the two are the same number. The repository already bound exactly
this shape one surface over, in the message box's own words
(`crates/mush/src/app/mod.rs:279-285`): "a picture is priced by its pixels, so a
100×100 png weighing 2 MB costs fourteen tokens — a hundred of them pass every
token bound the window has while the box holds 200 MB of bytes … so bytes are
what the box counts". The transcript has no such bound. Fix shape: a byte cap
on the pictures one request may carry (the box's rule generalised), or charging
an image `max(pixels, payload)` when the *request* is weighed.

## IN7 — The listing and the search walk into every live child's worktree

**Severity: minor.** `crates/mush-core/src/workspace.rs:19-32` — `.mush` is
deliberately not skipped ("whose session file a model may well be asked to look
at"), and the walk descends whatever it finds:

```rust
                if kind.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if !SKIP_DIRS.contains(&name.as_ref()) {
                        next.push(path);
                    }
                    continue;
                }
```

**The input.** An ordinary session with isolated children — the record's H10
measured 22 live worktrees; this repository has 67 files, so each child adds 67
names under `.mush/wt/<id>/`, all of them sorting before `Cargo.toml` (`.` is
0x2E, `C` is 0x43). Then the model's first orienting call, `list_files("")` (the
schema's default) or `search("symbol", "")`.

**Cost.** With ~6 children the 400-name listing is entirely `.mush/wt/<id>/…`
and the workspace's own files are past the cap; a search's first 200 matches can
all be a sibling's copy. Every path it prints is one the model will *act* on:
`real_path` accepts `.mush/wt/3/src/main.rs` (it is inside the root, and B4's
fix closed links, not worktrees), so `edit_file` there writes into a **live
sibling's checkout** — whose run-end `commit_all` (`git add -A` in that
worktree) then commits the parent's edit onto `mush/3` while the model's own
`src/main.rs` is untouched. The `SKIP_DIRS` doc names only the session file as
the reason `.mush` is not skipped; this road is not that one. Fix: one skip for
`git::WORKTREE_DIR` (`git.rs:258`, already the one spelling of `.mush/wt`).

## IN8 — The attach socket's name is never asked what it is

**Severity: minor.** `crates/mush/src/attach.rs:143-149`:

```rust
    let path = socket_path(root);
    if path.exists() && UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("could not bind {}: {error}", path.display()))?;
```

The premise is that "a file a live listener holds is another mush" — but on this
tree `lock::acquire` (`main.rs:1008`) refuses a second mush on the store and
runs *before* `attach::serve` (`main.rs:1097`), so nothing at this name is
another mush. No `symlink_metadata`/`is_socket` check exists anywhere in
`crates/mush/src` (grep).

**The input.** A same-user process (every sibling agent's shell today) that
plants `.mush/mush.sock` as a symlink to its own listening socket before mush
starts. `exists()` follows the link and `connect` succeeds *through* it, so the
removal is skipped and the bind fails: mush runs without attach and says so on
stderr. A *dangling* symlink and a plain directory take the milder half the same
way.

**Cost.** Two lies, both silent to the human: the session loses its attach
surface for as long as the file is there; and `mush read`/`mush agents` (which
only connect to the path) print the stranger's bytes as the transcript, while
`mush edit --send` exits 0 having steered nothing. The lock already refuses the
other mush this branch imagines; the name is the one place mush trusts a
stranger. Fix: refuse the bind unless `symlink_metadata` says socket (and unlink
only a socket), then compare the server's `id` echo (the contract gap T2 §17
already records) so a client can tell a stranger's answer from mush's.

## IN9 — `list_files` walks everything and sorts everything before its cap

**Severity: minor.** `crates/mush-core/src/workspace.rs:1165-1176`:

```rust
        self.walk(&start, &mut |path: &Path| {
            match self.name_for_model(path) {
                Some(name) => found.push(name),
                None => unnamed += 1,
            }
            true
        });
        found.sort();
        let truncated = found.len() > limit;
        found.truncate(limit);
```

The closure always returns `true`, so the walk visits every file under `rel`,
`found` holds every name, `found.sort()` sorts them all, and only then is the cap
applied. `search` does the opposite — it returns `false` at its limit
(`workspace.rs:1283`, "a walk that stops at a cap stops at a *deterministic*
place") — so the shape that bounds the walk exists in one of the two callers.

**The input.** `list_files("")` in a tree with one large directory: a `vendor/`,
a data directory, or one a single `run_command` fills
(`seq 1 200000 | xargs -I{} touch big/f{}`). IN7's worktrees multiply it.

**Cost.** Bounded by the tree rather than by mush — ≈50 bytes of `String` per
name, so ~10 MB and a sort at 200k files, ~50 MB at a million — held in the actor
thread to print 400 names. A cost, not an escape; the fix is the one line
`search` already has.

## IN10 — The whole-disk guard reads `/*` as a path named `/*`

**Severity: minor.** `crates/mush-core/src/whole_disk.rs:169-173` — the
"refuses to guess" rule covers `~`, `$` and a backtick:

```rust
fn resolve(operand: &str, cwd: &Cwd) -> Option<PathBuf> {
    if operand.starts_with('~') || operand.contains('$') || operand.contains('`') {
        return None;
    }
```

`*` is an ordinary `Normal` component, so `normalize` keeps it and `walk_is_root`
(`whole_disk.rs:161-166`) compares `"/*" != "/"`.

**The input.** `find /*` — or `du -sh /*`, `ls -R /*`, `rg foo /*`. The shell
expands the glob to every top-level directory, which is the whole disk, and the
guard allows it.

**Cost.** Exactly the thrash the guard exists for (its own module doc: "A
sibling agent's reconnaissance ran `find /` as a detached job and thrashed this
machine's disk"), reached by an *honest* spelling — unlike the shapes the doc
lists under "What it does not catch" (`f""ind /`, `find "$ROOT"`, `eval`,
`xargs`, a deeper nested shell), nothing here hides the text: the reading simply
fails to see that `/*` is a root. No test has a glob case. Fix: treat a glob
metacharacter in a root-position operand the way an expansion is treated, with
the direction reversed — refuse an operand whose normalised form is `/` followed
by glob-bearing components.

## IN11 — The TLS handshake gets the whole remainder and no watch

**Severity: minor.** `crates/mush/src/http.rs:927-939`:

```rust
        if tls {
            // A TLS handshake is a conversation, not a read, so during setup it
            // gets the whole remainder; ...
            stream.set_read_timeout(Some(left))?;
            let stream = match tls_connect(host, stream) {
                Ok(stream) => stream,
                Err(error) if is_timeout(&error) => return Err(watch.spend()),
                Err(error) => return Err(error),
            };
```

and `crates/mush/src/http.rs:1046`:

```rust
    retrying_interrupted(None, || stream.flush())?; // completes the handshake
```

**The input.** An `https://` endpoint whose TCP connect succeeds and whose peer
then never finishes the handshake — a captive portal that accepts and says
nothing, a middlebox that swallows the bytes, a VPN half-up. Nothing hostile is
needed.

**Cost.** Two facts, one phase. (1) The read timeout is the *whole remaining
call* (up to `CHAT_DEADLINE`'s 600 s) and the handshake runs through
`retrying_interrupted(None, …)`, which carries no `Watch`: while it stalls,
nothing consults the cancel flag, so a Ctrl-C lands when the socket read times
out — up to ten minutes late, where every other phase answers in a moment
(`READ_SLICE` = 200 ms with `Watch::check` between slices). The recorded claim
for B25 — "the cancel flag and the deadline consulted on each interrupt — write,
flush, read, connect, TLS handshake" (`docs/findings.md:992`) — does not hold at
this call site: the `None` is the missing watch. (2) The same arm maps *any*
timeout to `watch.spend()`, unlike the TCP connect arm four lines up
(`is_timeout(&error) && bound == left`, `http.rs:910-918`): a handshake whose
*write* hits `WRITE_TIMEOUT` (30 s, with 570 s of the deadline left) is reported
in the deadline's own voice — *the endpoint stopped responding* — and marked
spent, so `request` never wraps it `Unsent` and `retrying` never asks again. A
retryable class is turned final and the sentence names the wrong cause. Fix:
`READ_SLICE` for the handshake read and the watch passed to the retry loop, and
the `bound == left` rule applied to the arm.

## IN12 — `saved to …` is said after the save may have failed

**Severity: minor.** `crates/mush/src/app/mod.rs:3209-3214` (`/key`; the
provider and model-picker arms have the same order, and this file is under live
edit, so the lines may move):

```rust
                self.key_stated = true;
                self.persist_user_config();
                self.say(format!(
                    "api key set ({shown}…) — saved to {}",
                    self.home_config.display()
                ));
```

and `persist_user_config` reports its own failure into the *same single slot*
(`app/mod.rs:3313-3315`):

```rust
        if let Err(error) = user.save_to(&self.home_config, key) {
            self.fail(format!("could not save home config: {error}"));
        }
```

**The input.** A read-only `$HOME/.config/mush`, or any unwritable `MUSH_CONFIG`
target, then `/key sk-…`: `save_to`'s create/atomic-write fails and sets the
Error status, and the next line replaces it with the Info ack. The failure is a
plain `fail` — no notice, no pane line — so it is gone from the bar, the foot and
`/notes`.

**Cost.** The human believes the key is on disk; the next start has none (the key
lived only in the config cell), and they will not think to re-enter it. Bounded
but wrong: the write path is honest, its one sentence is spoken and then erased.
`/url`'s arm says before it persists, so the two orders disagree about the same
failure. Fix: say the ack only when the save returned `Ok`, and let the failure
keep the slot.

---

## Appendix

- **IN13 · latent · an IPv6 endpoint is addressed with an unbracketed `Host:`
  header.** `crates/mush/src/http.rs:513-516` interpolates the host raw;
  `parse_url` is tested to strip the brackets (`http.rs:1396-1401` answers
  `"::1"`), so `/url https://[::1]:8443` sends `Host: ::1:8443`, where RFC 7230
  §5.4 requires `[::1]:8443`. A server that validates Host answers 400 and the
  human reads it as the endpoint's fault. No injection (the host is a config
  value); the fix is to bracket a host containing a colon.
- **IN14 · latent · with `HOME` unset, the home config — key included — is read
  and written in the cwd.** `crates/mush-core/src/userconfig.rs:142-152`: when
  `MUSH_CONFIG` is unset and `dirs::config_dir()` is `None` (on Unix: no
  `$XDG_CONFIG_HOME` and no `$HOME`), the path is the *relative*
  `.mush-user-config.json`. `env -u HOME mush` then keeps a credential file in
  whatever directory it was launched from — usually a git repository, one
  `git add -A` from a commit — and a second workspace sees a different config.
  The file is 0600, so the road is git, not the umask; nothing warns.
- **IN15 · latent · `connect()` on both attach sides has no deadline before it
  runs.** `crates/mush/src/attach.rs:396-400` sets `ASK_TIMEOUT` *after* the
  connect, and the startup probe (`attach.rs:143`) has no timeout at all. A
  listener at the name whose accept queue never drains makes a blocking
  `UnixStream::connect` wait, and `serve` runs before `TerminalGuard::enter()`
  (`main.rs:1097`, `:1143`): the human's next mush in that directory hangs with
  no frame (the first `Ctrl-C` only sets a flag; the second kills), and the CLI
  hangs before its own bound exists. The kernel half is
  standard blocking-connect behaviour and was not run here; the missing deadline
  is code.
- **IN16 · minor · a tool call whose arguments were sanitized away *runs*
  `list_files` on the root.** `crates/mush-core/src/transcript.rs:282-297`
  rewrites invalid `arguments` to `"{}"` "so the tool executor returns a clear
  per-call error instead", but `list_files`'s schema has no required field
  (`prompt.rs:225-235`; `path` defaults to the root, `tools.rs:178-184`), so the
  call runs and answers a full root listing — an answer to a question the model
  did not ask — where `search` and `read_file` refuse for the same mangled
  arguments because their required fields are still missing.

---

## Suspect and held

What was checked and found sound, with the check that convinced me.

- **One logical deadline, phases bounded under it.** `http.rs`'s `Watch` owns the
  deadline; the resolver wait, the connect, the write and every read take the
  smaller of their own ceiling and what is left (`resolve_bounded`,
  `write_bounded`, `fill`), and a phase that spends the budget answers in the
  deadline's own voice and is never `Unsent`. Pinned by
  `no_phase_outlives_the_calls_deadline` (resolver, connect, write),
  `a_stop_lands_while_a_write_stalls`, `a_dribbling_endpoint_still_hits_the_deadline`.
  The one phase outside the watch is IN11.
- **Bodies and heads are bounded on every read road.** `MAX_HEAD_BYTES` is
  enforced per line *and* cumulatively across the status and header lines
  (`http.rs:419`, `452`, `811`); `MAX_BODY_BYTES` against `Content-Length`
  (`read_exact`), end-of-stream (`read_to_end`) and every chunk-size claim;
  chunk-size and trailer lines go through the same bounded `read_line`. A1's OOM
  is closed, and only IN1's wrap is left in that arithmetic.
- **A connection that did not frame itself is never reused.** Every `Err` drops
  the stream and leaves the pool empty, which is what makes B27's
  leftover-framing hypothesis unreachable. Pinned by
  `a_cancelled_body_is_not_kept_for_the_next_request`,
  `a_kept_connection_that_died_after_the_write_is_final`.
- **The retry classes are markers, not message text.** `is_unsent`/`is_framing`
  travel on the `io::Error`; `retrying` repeats exactly `Unsent`, and only while
  the one deadline has room; every failure after the write is final because the
  endpoint may already have read and billed it (A2). A 4xx/5xx is never retried;
  a cancellation outranks both.
- **UTF-8 boundaries cannot panic the reader.** Every line and body is built with
  `String::from_utf8_lossy`, every cut is made through `Vec<u8>` lengths, and no
  `str` slicing on a wire offset exists in `http.rs`; a truncated character is a
  U+FFFD and, at worst, a parse failure.
- **The endpoint's `u64`s are either saturating or clamped (A9's class checked
  closed).** `RunUsage::add` saturates all three fields (`agent.rs:77-82`,
  pinned by `a_reply_carrying_u64_max_saturates…`); the window an endpoint
  advertises or complains about is clamped to `[1 024, 10 000 000]`
  (`clamp_context`, `adopt_context`), the complaint road is gated by
  `believable` (`settings.rs:60-68`: `tokens < in_use && tokens.saturating_mul(8) >= in_use`),
  and `reply_cap`'s `share as u32` cannot truncate because the window is capped
  first. The one endpoint-driven `u64` that is *not* clamped is IN6's bytes.
- **Every typed tool argument refuses a wrong shape rather than defaulting.**
  `tools.rs:139-198` (`arg_string`, `arg_string_opt`, `arg_usize`, `arg_bool`,
  `arg_path`, `edits_arg`) refuse a present-but-wrong value and read `null` as
  absent; `detach`/`exclusive`/`base`/`title`/`replace_all` all go through them
  (A7/F12/B10 closed). `base` is resolved to a sha in the caller's workspace and
  passed positionally after `-b <branch> <path>`; `git::resolve` passes
  `--verify --quiet` and appends `^{commit}`, so a `-`-leading name fails to
  resolve instead of becoming an option.
- **The command roads hold.** `sh -c` in the workspace root, stdin null,
  stdout/stderr into `NamedTempFile`s created exclusively, `process_group(0)`,
  `MUSH_API_KEY` scrubbed by `secrets::scrub`, the whole-disk guard before
  anything is created (IN10 is the one shape it misreads), the 8 MiB output cap
  checked every poll with the group taken after the leader exits (E1/E3/E6
  closed; `kill` is an in-process `rustix` call, so the missing-`kill`-program
  shape cannot arise), and the caps are `SIGKILL` to the group, which no command
  can decline.
- **The image parsers and the two 2 MiB caps.** All four dimension parsers are
  bounds-checked and return `None` on a truncated or lying header; a
  `u32::MAX × u32::MAX` claim saturates in `Image::weight`; `IMAGE_FILE_CAP` and
  the clipboard reader's `READ_CAP` are one decision; a picture cut at the
  reader's cap is refused without naming a size it does not know. IN6 is the
  price, not the parser.
- **The terminal's other doors hold.** `text::sanitize`/`truncate`/`fit_row` run
  on every wrapped or cut surface (transcript, titles, footers, roster, pickers,
  toasts); `skip_escape` consumes CSI/OSC/short forms whole and drops a sequence
  cut at the bound; `is_control()` covers C0 *and* C1 and the bidi set is
  stripped (B15). IN5's two sites are the ones with no helper on their path.
  Terminal modes are entered once and restored on the owning thread (E10),
  mouse capture is never taken, and the event loop caps a resize storm at 4096
  events per frame.
- **The environment and config shapes are refused by name, not defaulted.**
  `MUSH_URL`/`MUSH_CONTEXT`/`MUSH_PROVIDER`/`MUSH_THINKING`/`MUSH_REASONING_EFFORT`/
  `MUSH_THEME` all go through `env_nonempty` plus a checked parse; a control
  character in a URL or a key is refused as its escape at every door (D19/C7);
  an empty `MUSH_CONFIG` counts as unset; a directory or an unparsable file
  there is a complaint with the path and a `.bak`. IN14 is the missing-`HOME`
  fallback, not the parsing.
- **The session's *content* failures are honest.** Wrong types, `null`s, missing
  fields, truncation, an empty file, a directory in the name and an id past
  `u64` become `Stored::Unusable` with serde's line and column, kept
  byte-identically as `.bak`, and announced with the file, the reason and where
  the copy went; `id: 0`, duplicates, `u64::MAX` and cycles/dangling parents are
  refused with a line naming the row (C9's rows, and the live editors' subject).
  IN2's `MAX - 1` and IN3's read are what the vet and the reader do not ask.
- **The attach read side is bounded.** `read_line_capped` holds constant memory
  and answers an over-cap line with a naming refusal; the connection cap is taken
  on the accept thread and a guard frees the slot on any exit; a silent client is
  reaped by the idle window; refusals carry fixed words and numbers, never client
  bytes. IN4 is the write half, IN8 the name.

## Known, not re-reported

**A1–A23** (A1's bound re-verified; A2's retry classes re-verified; A9 closed;
A10 re-verified for the first chunk and IN1 is its class one chunk later; A19
still open), **B1–B27** (B25's TLS sentence is IN11, and its own claim does not
reach that call site; B27's pool rule re-verified), **C1–C12** (C1's scrub
re-verified at every child road; C7's key check at the one head writer; C8's
printer road is a different one from IN11), **D1–D26** (D19's URL doors
re-verified; D2's two-rows-of-one-id consequence is what IN2's release half
produces), **E1–E10** (E5's scratch, E6's kill), **F1–F17** (F12's `base`, F9's
resolution), **H1–H72**, **S1–S8**, **U1–U15**, and the still-open **H73–H80**,
**A19**, the §8.102 flakes and the M5 notes. `findings.md`'s "the image's base64
4/3 inflation deliberately not modeled" (`:2806`) and the tui audit's
`Buffer::set_stringn` claim (`docs/audits/tui.md:898`) are each quoted where
this audit reads them differently (IN6, IN5).

## Not verified

- Whether a real server sends a 16-hex-digit second chunk-size line (IN1's
  release half needs one; the debug-build panic needs only the bytes, and an
  in-memory wire can send them — no harness was run here).
- **Which of the record's two sentences about ratatui is true** (IN5): the
  `33409d4` measurement says a `\r`/OSC from a `Span` reached the terminal on
  ratatui 0.29.0; `docs/audits/tui.md:898` says `set_stringn` filters it. The
  dependency's source is outside this workspace and nothing could be run, so the
  conflict is left standing with the probe named.
- Whether a stalled TLS handshake, a FIFO session file or a full accept backlog
  is hit in practice on this machine (the code paths are unconditional; the
  frequency is a guess).
- Whether a server mush is pointed at validates `Host` strictly (IN13's cost
  depends on it; the header bytes are wrong either way).
- How large an image-bearing request a real run reaches before the endpoint
  refuses it (IN6's arithmetic is exact; no run was made).
