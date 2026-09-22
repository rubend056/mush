# The tool and workspace layer, audited

A blind audit of the layer where a defect costs the human data: the path
resolution and the sandbox, the reads, windows, listings and searches, the
writes and their atomicity, the image roads and `.mush/paste/`, the tool table
and its argument parsing, the message endpoint's shape, the display arithmetic
and the markdown view, and the attach socket.

**Base:** `4436db5` (`merge: a run of a markdown marker is all or nothing`).
**How:** every file read end to end, then probes — throwaway integration tests
in `crates/mush-core/tests/` and a temporary `#[cfg(test)] mod audit_probe`
appended to `crates/mush/src/attach.rs`, both deleted before this file was
committed. Every probe run quoted below is a real run on this machine (Linux,
`cargo test` in the debug profile, `crates/mush-core` = `mush-core` and
`crates/mush` = the TUI crate). The one file that is not mine — `agent.rs`,
where two fixes are in flight — is named only as a **caller** or a **blast
radius**, never as the site of a fix.

Nothing here re-reports a closed row: §8.44 (the image road), §8.45 (the wire),
§8.46 (the meter), §8.47 (the turn ceiling), §8.48 (the write cap), §8.49 (the
copy mode) and §8.50 (the markdown view) are read, and their fixes are checked
rather than assumed (see *Verified sound*). The open queue's rows are checked
where they touch this layer: H23's false half is a sentence in `attach.rs`.

---

## Priority order

Data loss and escapes first, cosmetics last. Each item names the finding that
carries it.

1. **B1 — every write rebuilds the inode at mode 0600.** Every `write_file` and
   `edit_file` on a file that exists silently drops its mode: the executable
   bit, group/other readability, group writability. Git records it as a mode
   change in the human's own repository. The read-only bit does not stop a
   write either.
2. **B5 — `read_file` has no cap, and `write_file` reads the file it replaces
   whole.** A 512 MiB file costs a 512 MiB allocation (measured peak RSS
   514 MiB); `READ_FILE_CAP` is enforced on the model's read road alone. On a
   shared machine this is an OOM that takes the session with it.
3. **B6 — a non-UTF-8 text file that is edited is silently rewritten.**
   `from_utf8_lossy` on the way in, `write_file` on the way out: every invalid
   byte the model did not ask about becomes U+FFFD, permanently.
4. **B3 — a write never looks at what the name is.** `write_file` renames over a
   FIFO or a bound UNIX socket. The attach socket is the proof: writing
   `.mush/mush.sock` unlinks it and every `mush read/agents/focus/edit` in that
   directory dies with "no mush is running" until mush restarts.
5. **B4 — a symlinked directory inside the root is followed by the file tools.**
   `resolve` is lexical, so `read_file`, `write_file`, `list_files` and `search`
   all reach *and write* files outside the workspace through a link the root
   contains.
6. **B2 — a symlinked file is replaced, not written through.** The edit lands in
   the link's place; the real file keeps its old bytes. Same for a hard link.
7. **B7 — CRLF files cannot be edited from what the window shows.** The window
   strips the `\r`, so a multi-line `old_string` copied from it can never match,
   and a matching single-line edit inserts LF lines into a CRLF file.
8. **B8 — `search` shows the model a line the file does not hold** (display
   sanitizing, CR stripping, `trim_end` on the *model's* road).
9. **B9 — `rel()` rewrites a real name's `\` into `/`**: a listed or searched
   path the model cannot open. A name with a newline comes back as two lines.
10. **B10 — `edits_arg` silently defaults a wrongly-typed `replace_all`** while
    every other typed argument refuses a wrong type: the model is told to set a
    flag its own spelling of set does not reach.
11. **B11 — the attach socket has no bound anywhere**: an unbounded request
    line, one thread per connection, no idle timeout.
12. **B12 — `Message`'s deserializer is stricter than its own doc**, and a reply
    it refuses ends the run ("could not parse model response").
13. **B13 — `.mush/paste/` is never pruned.**
14. **B14 — the markdown view's claim to be `wrap_text` plus styles is false**
    for non-ASCII whitespace.
15. **B15 — `sanitize` keeps the bidi *marks* (LRM, RLM, ALM).**
16. **B16 — the read-modify-write window** (suspected; a staged demonstration,
    not a race driven inside one call).

No **blocker** was found: nothing on the ordinary path loses a file's *content*
outright. The two ordinary-path losses are metadata (B1) and encoding (B6).

---

## B1 — Every write rebuilds the file at mode 0600, and a 0444 file is replaced anyway

**Severity: major.** `atomic_write` writes through `tempfile::NamedTempFile`,
which creates the temporary file `O_CREAT|O_EXCL` at mode `0600`, and then
`rename`s it over the target. The rename carries the *temp file's* mode, so the
target's mode is whatever tempfile chose. Proven for both doors:

```
B1a: mode after write_file = 600 (was 755)
B1a: mode after write_file = 600 (was 644)
B1b: mode of a new file     = 600
    write_file onto a 0444 file: Ok("new\n")
```

`git` sees it in the human's own tree (a repo, `git add`, then the same
edit-write the tool does):

```
committed:  100644 … data.txt | 100755 … run.sh
after:      100644 … data.txt | 100644 … run.sh
git diff --cached --summary: "mode change 100755 => 100644 run.sh"
```

`crates/mush-core/src/workspace.rs:1315`:

```rust
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}
```

**What is wrong.** One function is every write in the tree — `write_file`
(`workspace.rs:998`), `session::save` (`session.rs:338`) and the human's own
`config.json` (`userconfig.rs:196`), the last two outside the workspace root —
and it decides the mode by accident. Four separate facts:

- the executable bit is lost: a script the human runs (`./scripts/x.sh`, a git
  hook, a `build.sh`) stops being executable, and `git` records the mode change,
  so the human's next commit flips it in the repository and every clone loses
  it;
- group and other permissions are lost: a file that was `0644` (another user, a
  sibling process, a container, a web root) becomes `0600` — silently
  unreadable to everything but the owner;
- the read-only bit is *no* protection: `write_file` onto a `0444` file succeeds
  (the rename needs only the *directory* to be writable), so a file the human
  marked read-only is replaced and comes back writable-by-owner;
- a *new* file is `0600` where every other tool on the box (`>` redirection,
  `vim`, `git`) would make it `0666 & ~umask` (`0644` on this machine) — the
  model's own output is invisible to the group it was written for.

**Blast radius.** The human's repository: mode changes that show up as their
own commit, executable scripts that stop running, files that other users and
services can no longer read (and the human's own `config.json`, whose mode
tightens to `0600` — the one case where the change is harmless to the owner and
invisible to everyone else). None of it is announced, and all of it is caused by
an edit that was supposed to change one line.

`docs/mush.md:112` claims only what is true — "Every file write — `write_file`,
`edit_file` — is temp-file + `rename`; readers never see a half-written file" —
and says nothing about the mode, so this is a hole rather than a lie. The
repair is still owed.

**Suggested fix.** In `atomic_write`, when the target exists, copy its mode onto
the temp file before the rename (`fs::metadata(path)?.permissions()` →
`tmp.as_file().set_permissions(..)`, or open the temp with `.mode(0o777 & !umask)`
for a new file); when it does not, create with `0o666 & !umask`. Optionally
refuse a target whose mode has no owner-write bit, so a `0444` file is a refusal
the model can read rather than a silent override.

**Acceptance test.** `a_write_keeps_the_files_mode`: write `run.sh` `0755` and
`data.txt` `0644`, edit both through the tool road, assert the modes are still
`0755`/`0644` and that `git diff --summary` is empty; a new file gets
`0o666 & !umask`; a `0444` target is either preserved read-only or refused with
a sentence naming the mode.

---

## B2 — A symlinked file is replaced, not written through: the edit lands in the wrong place

**Severity: major.** `atomic_write`'s `rename` replaces the *name*, whatever the
name was. If the name was a symlink, the link is gone and the target keeps its
old bytes — the model's edit is in a new file beside it. Reproduced:

```
B2: read_file(link) = "a = 1\n"          (the read follows the link)
B2: after edit — link is a symlink? false ; target holds "a = 1\n" ; link_path holds "a = 2\n"
```

The read follows the link (so the model sees `a = 1`), the edit "succeeds", and
the *real* file the human cares about — the one the link points at, and the one
the model believes it edited — still says `a = 1`. Nothing in the result says
so. A hard link is the same defect one step further: the other name keeps the
old bytes and the two names are no longer one inode, so a `config.yaml`, a
`Cargo.toml` or a vendored file kept in step by a hard link silently forks.

**Blast radius.** An edit that landed in the wrong place, silently, in the one
road whose whole promise is exactness (`docs/mush.md:123`: "**Edits are exact.**
… so an edit can never hit the wrong occurrence"). A dotfiles-managed file, a
vendored symlink, a hard-linked config: the model's change is not in the file
the human will read.

**Suggested fix.** In `write_file` (or inside `atomic_write`), resolve the target
with `fs::canonicalize` before the temp file is made, when the path exists as a
symlink, and write through it — checking that the resolved target is still under
the root (see B4) — or, if the link must not be followed, refuse with a sentence
naming the link and its target.

**Acceptance test.** `an_edit_follows_a_symlink_to_its_target`: commit
`real/config` and `link -> real/config`, edit through `link`, assert the link is
still a symlink, `real/config` holds the new bytes, and a hard-linked twin is
still one inode (`std::fs::metadata` ino equality).

---

## B3 — `write_file` renames over whatever the name is: the attach socket dies

**Severity: major.** `write_file` checks only that the path is not the root
(`workspace.rs:989`):

```rust
pub fn write_file(&self, rel: &str, content: &str) -> Result<(), String> {
    let path = self.resolve(rel)?;
    if path == self.root {
        return Err("refusing to write to the workspace root".to_string());
    }
    …
    atomic_write(&path, content.as_bytes()).map_err(|e| format!("cannot write {rel}: {e}"))
}
```

and `atomic_write`'s rename takes the name. A bound UNIX socket is a legal file
name; `rename` replaces it. The workspace's own attach socket is inside the
workspace (`.mush/mush.sock`), so this is the live surface:

```
before: is a socket? true
write_file(".mush/mush.sock") -> Ok(())
after:  is a socket? false ; holds "not a socket any more\n"
can a client still connect? Err("Connection refused (os error 111)")
```

and a FIFO is the same:

```
write_file("pipe") -> Ok(())
after:  is a fifo? false ; is a file? true
```

**What is wrong.** The tool's own doc ("Atomically create or replace a file")
assumes a file. Every *read* road in this module checks the shape first —
`read_file` (`workspace.rs:365`, "not a regular file"), `image_at`
(`workspace.rs:691`, stat before open) — precisely because a FIFO's open blocks
and a device never ends; §8.44 paid for that lesson. The write road never asks.
A device node inside the workspace (`/dev`-shaped files a repo can hold, a
character device a human made) is replaced by a regular file just as silently.

**Blast radius.** The whole external-driver surface: `mush read`, `mush agents`,
`mush focus`, `mush edit` all answer "no mush is running in <dir>" (the CLI's
message for a connect refused) while mush is alive and well, and nothing short of
a restart brings the socket back. A model asked to "clean up stale sockets" or
"make sure `.mush` is fresh" can do this with a single tool call; so can any
script. The message it gets back is `Ok(())`.

**Suggested fix.** In `write_file`, after `resolve`: if the path exists and
`fs::symlink_metadata` says it is neither a regular file nor a symlink-that-
resolves-to-one, refuse ("`{rel}` is a socket/FIFO/device — refusing to replace
it"); and in `atomic_write`, keep the same guard for callers that do not go
through `write_file`.

**Acceptance test.** `a_write_will_not_replace_a_socket_or_a_fifo`: bind a
`UnixListener` at a path inside a test workspace and `mkfifo` another; assert
both `write_file` calls are refused with a sentence naming the type, that the
socket still connects, and that the FIFO is still a FIFO; then assert an ordinary
file and a new path still write.

---

## B4 — A symlinked directory inside the root is followed: the file tools leave the workspace

**Severity: major.** `resolve` (`workspace.rs:322`) is purely lexical — it walks
`Path::components()` and refuses `..`, absolute paths and the root — and never
asks the filesystem what a component *is*:

```rust
for component in path.components() {
    match component {
        Component::Normal(part) => out.push(part),
        Component::CurDir => {}
        Component::ParentDir => return Err(format!("path escapes the workspace: {rel}")),
        _ => return Err(format!("invalid path: {rel}")),
    }
}
```

A symlink the workspace *contains* points wherever it points; every road built on
`resolve` then follows it. Reproduced with `root/out -> <outside>` (`<outside>`
holding `secret.txt` and `sub/deep.txt`):

```
read_file(out/secret.txt) = Ok("SEKRIT\n")
write_file(out/written.txt) -> Ok(())
did it land outside? Ok("ESCAPED\n")
list_files("out") = ["out/secret.txt", "out/sub/deep.txt", "out/written.txt"]
search("DEEP", "out") = ["out/sub/deep.txt:1: DEEP"] skipped=0
read_file(link.txt) = Ok("SEKRIT\n")          (a file symlink, same road)
```

**Why the listing's own claim is not the whole story.** `walk` is careful about
links *inside* the walk — `symlink_metadata`, `kind.is_dir()`/`kind.is_file()`,
so a symlinked child directory is skipped — but the walk's *start* is not
(`workspace.rs:934`):

```rust
fn walk(&self, start: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
    if start.is_file() {          // fs::metadata: follows a link
        visit(start);
        return;
    }
    …
    let Ok(entries) = fs::read_dir(&dir) else {   // read_dir: follows a link
```

so `list_files("out")` lists outside files, `search("x", "out")` *reads* outside
files (up to `SEARCH_FILE_CAP` each), and `list_files`'s doc — "a symlinked
directory is not followed, so a listing cannot leave the workspace" — is false
for the one path a model is most likely to name.

**Blast radius.** An edit that landed in the wrong place, and a read that shows
the model bytes the human did not open: `write_file("out/x", …)` writes into
whatever the link points at — with B1's mode change applied to it too. In a
worktree, `.mush/wt/<id>` links and vendored links are ordinary; and the model's
own tools are the ones the human believes are confined
(`docs/mush.md:115`: "Every file tool resolves its `path` against the root and
rejects an escape (`..`, absolute paths)"). The doc goes on to call confinement
"a convention, not a fence" — but the convention it states, that a *path* escape
is rejected, is exactly what this road does not do.

**Suggested fix.** Make `resolve` aware of links for the operations that touch
the filesystem: canonicalize the deepest existing prefix and refuse a result
outside `self.root` (the same check `agent_root`/`carry_images` already reason
about), or open every path with `O_NOFOLLOW`-style semantics for the final
component and a realpath check for the parents. Also fix `walk`'s start: decide
the start's type with `symlink_metadata` and refuse (or skip) a start that is a
symlink, so the listing's own claim becomes true.

**Acceptance test.** `a_link_inside_the_root_cannot_leave_it`: build the shape
above in a test workspace; assert `read_file`, `write_file`, `list_files` and
`search` all refuse `out/…` with a sentence naming the escape, that the outside
directory is untouched (`written.txt` does not exist), and that the *walk* still
skips a symlinked child directory. And a positive twin: a link to a file
*inside* the root keeps working.

---

## B5 — `read_file` is unbounded, and `write_file` reads the file it is about to replace

**Severity: major.** `READ_FILE_CAP`'s own doc (`workspace.rs:33`) says "The
largest file a read tool will open whole", and `read_window` enforces it from the
stat (`workspace.rs:746`). The function that actually does the whole read does
not: `read_file` (`workspace.rs:365`) reads and *then* decides. `read_file`'s doc
knows what it is for and never names a size — "this became `edit_file`'s private
read, which must see the whole file or refuse the edit" (`workspace.rs:361`),
"this stays the whole-file read" (`workspace.rs:364`) — and the one test that
pins the road says so too (`workspace.rs:2648`): "And nothing is cut: a file well
past any window a model would want comes back in full, because the caller is an
editor" — with a 64 KB file, which is not past anything.

```rust
let bytes = fs::read(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
if bytes.contains(&0) {
    return Err(format!("{rel} looks like a binary file"));
}
Ok(String::from_utf8_lossy(&bytes).into_owned())
```

Measured, on a 40 MiB text file and a 512 MiB sparse blob:

```
read_file returned 41943040 bytes (READ_FILE_CAP = 32 MiB)  ; RSS 3 → 43 MiB
read_window says: Some("big.log is 41943040 bytes — past the 32 MB cap on a whole read …")
peak before = 3 MiB
refused: blob.bin looks like a binary file                    (the whole 512 MiB was read first)
peak after  = 514 MiB
```

Two callers ride this road, both outside this file: `edit_file` (the model edits
a big file) and `write_file`, whose first act on an existing file is a read of it
*to count lines* — `crates/mush/src/agent.rs:4807`:

```rust
let before = actor.ws.read_file(&path).ok().map(|text| text.lines().count());
actor.ws.write_file(&path, &content)?;
```

A 4 GiB file the model overwrites costs a 4 GiB allocation to produce the
sentence "wrote x — 41 → 3 lines", with the machine already busy (this box: 15 GB
of RAM, several sibling agents and their builds).

**Blast radius.** An OOM kill of the mush process on a shared machine: the live
session, every actor's in-flight run, and every job in the tree go with it, and
the human sees a dead pane. Even below the killer's line, a multi-gigabyte
allocation is felt by everything else on the box.

**Suggested fix.** Enforce `READ_FILE_CAP` in `read_file` itself (a stat before
the read, refused with the sentence `read_window` already owns, naming
`run_command`), and give `write_tool` a bounded way to answer what it wants: a
`Workspace::line_count`-style helper that reads at most `READ_FILE_CAP` and says
"more" past it, or drop the before-count when the file is over the cap.

**Acceptance test.** `a_whole_read_is_capped_and_says_where_to_go`: a sparse file
past `READ_FILE_CAP` is refused by `read_file` (peak RSS measured around it stays
small — the probe above is the measurement), the refusal names `run_command`, and
`write_file` over that file still lands the bytes and answers "(replaced …)"
without a whole read.

---

## B6 — A non-UTF-8 text file is silently rewritten in UTF-8 when it is edited

**Severity: major.** `read_file` decodes with `from_utf8_lossy`
(`workspace.rs:374`) into the same string `edit_file` uses as its source and its
result, and `write_file` writes it back. Anything that is not valid UTF-8 and has
no NUL byte — a Latin-1 config, an old CSV, a Japanese Shift-JIS note, a
CESU-8-ish file — is "text", passes the binary guard, and every invalid byte
becomes U+FFFD on the way out. Reproduced with `caf\xe9 = 1\nna\xefve = 2\n` and
one unrelated one-line edit:

```
read back = "caf\u{fffd} = 1\nna\u{fffd}ve = 2\n"   (lossy)
after a one-line edit: [99, 97, 102, 239, 191, 189, 32, 61, 32, 57, 10, …]
bytes lost? true
```

`239, 191, 189` is U+FFFD. The file went in with two Latin-1 bytes and came out
with two replacement characters — the two lines the model did **not** touch are
the ones destroyed.

**What is wrong.** The module's own sentence names the rule it does not
implement — `workspace.rs:2640`, the doc of the very test that pins this road:
"A binary file is the one thing refused, because lossy UTF-8 would rewrite it" —
while `read_file`'s doc says only "Binary files are refused"
(`workspace.rs:352`). "Binary" is implemented as "contains a NUL byte", which is
not the same set as "is not valid UTF-8". `read_window` shows the lossy text
too, but a read that is shown is not a read that is written back; `edit_file` is
the door that turns the lossy decode into a lossy *write*.

**Blast radius.** Silent, irreversible corruption of the human's file, in lines
the model never mentioned, caused by a successful edit it was told had worked.
The bytes are gone from the working tree (and from any editor's buffer that has
it open, if it is saved afterwards).

**Suggested fix.** Make the lossiness explicit and one-way: `read_file` returns
`Err` for bytes that are not valid UTF-8 *and* not losslessly representable (a
sentence naming the encoding problem and the road — `iconv`, `run_command`), or
give `edit_file` a strict read (`str::from_utf8`) and refuse the edit; keep
`from_utf8_lossy` for the read-only roads (`read_window`, `search`) where the
other reading is a lie too large to accept, and say which road is which in the
docs.

**Acceptance test.** `a_non_utf8_file_is_not_edited_through_a_lossy_read`: write
the Latin-1 bytes above, call the edit road, assert the tool refuses with a
sentence that names the file and the encoding, and assert every byte of the file
is unchanged; a positive twin asserts a valid UTF-8 file with multi-byte
characters still edits.

---

## B7 — CRLF files cannot be edited from what the window shows

**Severity: minor.** `read_window` splits with `str::lines()`
(`workspace.rs:766`,`787`), which strips a trailing `\r` *and* joins the shown
lines with `\n`, so a CRLF file's window is not the file's bytes — while the
same doc says the window is exactly what the model copies into `old_string`
(`workspace.rs:742`: "a model copies what it reads into `edit_file`'s
`old_string`, and a numbered line is a string that cannot match"). Reproduced on
`alpha\r\nbeta\r\ngamma\r\n`:

```
window = "alpha\nbeta\ngamma" (holds CR? false)
edit with the copied two lines -> Some("edit 1: old_string not found in win.txt")
edit with one copied line      -> true
```

and when a single-line `old_string` does match, the inserted text uses LF:

```
after the edit: [97, 108, 112, 104, 97, 13, 10, 66, 49, 10, 66, 50, 13, 10, …]
                                 ^alpha ^\r\n  B1  \n  B2  \r\n   gamma
```

**Blast radius.** Every CRLF file (a Windows-authored repo, a `.bat`, a
`.gitattributes`-marked file, a file another program rewrote on a Windows box) is
editorially second-class: multi-line context — the only way to disambiguate a
`old_string` that appears twice — can never match, and the model burns turns
fighting a refusal it cannot understand, or reaches for `run_command` with
`sed -i`, which is the *one* thing `edit_file` exists to avoid. The edits that do
land leave mixed line endings, which the human's `git diff` shows as a
whole-file change the next time anything normalizes them.

**Suggested fix.** The smallest honest one: the window's trailing sentence says
the file uses CRLF and that line endings are not shown (one conditional word in
the sentence the window already prints), so the model can spell an `old_string`
that matches (a single line, or `\r`-aware content via `write_file`); the better
one is `edit_text`'s matching rule learning to match a CRLF file's lines, which
is a bigger decision than this audit should make.

**Acceptance test.** `a_crlf_window_says_so`: a CRLF file's window keeps its
lines' `\r` (or says they are hidden), a two-line `old_string` built from the
window applies, and the file's other lines keep `\r\n`.

---

## B8 — `search` shows the model a line the file does not hold

**Severity: minor.** The match line is built with the *display* sanitizer
(`workspace.rs:914`):

```rust
matches.push(format!(
    "{}:{}: {}",
    self.rel(path),
    number + 1,
    text::truncate(line.trim_end(), MATCH_LINE_CAP)
));
```

`text::truncate` begins with `sanitize` (`text.rs:737`), which deletes escape
sequences, `\r`, and control characters — right for a pane, wrong for the one
road whose result is *data for the model*. Proven:

```
search matches: ["esc.txt:1: beforeneedle after"]
read_file(esc)   = "before\u{1b}[31mneedle\u{1b}[0m after\n"
read_window(esc) = "before\u{1b}[31mneedle\u{1b}[0m after"
```

The matched line also loses its `\r` (`text.lines()`), and `trim_end` drops the
trailing whitespace a file may genuinely hold. `read_window` and
`truncate_for_model`/`tail_for_model` deliberately do *not* sanitize — they are
model roads — so this is one road disagreeing with its siblings about what the
model is shown.

**Blast radius.** A model that searched for a marker inside a line with an ANSI
code (log lines, a colored test runner's output, a terminal-capture fixture)
sees text that is not in the file; if it copies the shown line into
`old_string`, the edit is refused as "not found" — the same dead end as B7,
arrived at from the other side. And a search for a two-part pattern across the
shown line's whitespace can be answered with a line that does not contain the
pattern in the form asked.

**Suggested fix.** `search` is a model road: keep the raw line (`trim_end` is
still honest — a line's trailing whitespace is rarely the match) and cut it with
`boundary_at_or_before` + the truncation marker, exactly as `truncate_for_model`
does, or sanitize only when a caller says the result is for a pane. Say in the
doc which roads are painted and which are read.

**Acceptance test.** `a_search_match_keeps_the_files_bytes`: the file above
searched for `needle` returns the match with `\x1b[31m` intact; a second case
pins that a match line past `MATCH_LINE_CAP` is cut and marked, never silently
shortened.

---

## B9 — `rel()` rewrites a real name's backslash into a separator: a listed path the model cannot open

**Severity: minor.** On Linux `\` is an ordinary file-name character, and `rel`
(`workspace.rs:345`) folds it:

```rust
path.strip_prefix(&self.root)
    .unwrap_or(path)
    .to_string_lossy()
    .replace('\\', "/")
```

Proven on a file literally named `a\b.txt`:

```
list_files = ["a/b.txt"]
read_file("a/b.txt") -> Err("cannot read a/b.txt: No such file or directory (os error 2)")
search for `the real file` -> ["a/b.txt:1: the real file"]
read_file(r"a\b.txt") -> Ok("the real file\n")     (the real name opens; the listed one does not)
```

The same `rel` builds the `Image.path` of a pasted picture and the session
placeholder, so a picture whose name holds a `\` is recorded as a name that does
not exist (its copy is skipped for a name inside the root: `image_named`
(`workspace.rs:535`) asks `self.resolve(&label).is_ok()`, and the folded label
resolves — lexically — so no copy is made and the placeholder points at
`a/b.png`). The paste *parse* has the same shape from the other side:
`unescape_backslashes` turns a pasted `a\b.png` into `ab.png` (documented, and
`a\\b.png` is the escape the human can use), so the two roads disagree about what
a `\` in a name means.

A second, related shape: a file whose name holds a newline comes back from
`list_files` as one entry containing a newline, which the tool layer joins with
`\n` — the model is shown two lines, neither of which exists (`a\nb.txt` →
`["a\nb.txt"]` → `"a\nb.txt"`).

**Blast radius.** The model is told about a file and cannot open it: it reads as
"the listing is wrong" or "the file was deleted", and the road it takes next is a
shell command. Rare names, but the failure is a dead end rather than a refusal.

**Suggested fix.** Stop folding `\` into `/` for a name that came from the
filesystem (the fold exists for a Windows-built path; on a Linux workspace it
misnames data — and `list_files`/`search` output is the *model's* road, not a
pane's). Where a name may hold a newline, quote it (or name it as
`path (contains a newline)`) so the listing cannot read as two entries.

**Acceptance test.** `a_name_that_holds_a_backslash_is_listed_as_itself`:
`a\b.txt` lists as `a\b.txt`, `read_file` on the listed name works, and the same
for a name with a newline (one entry, one line, quoted).

---

## B10 — `edits_arg` silently defaults a wrongly-typed `replace_all`

**Severity: minor.** `crates/mush-core/src/tools.rs:207`:

```rust
replace_all: entry
    .get("replace_all")
    .and_then(Value::as_bool)
    .unwrap_or(false),
```

Every other typed argument in this module refuses a wrong type — `arg_usize`
("must be a whole number"), `arg_bool` ("must be true or false"), `arg_path`
("must be a string") — and the module's own doc gives the reason: "A value that
is present but not a number is refused rather than defaulted: silently reading
line 1 when the model asked for a window is how a read answers a question nobody
asked." The edit batch is the exception. Proven:

```
edits_arg({"edits": {"old_string": "a", "new_string": "b", "replace_all": "true"}}) -> Ok(false)
edits_arg({"edits": [{"old_string": "a", "new_string": "b", "replace_all": 1}]})       -> Ok(false)
```

**Blast radius.** The flag the *refusal sentence itself* tells the model to set
(`apply_one`'s "…or set `replace_all` to change every occurrence") is the flag a
model most often gets wrong — and a model that spells it `"true"` gets the
identical refusal back, unchanged, turn after turn: the loop guard's shape
(`count_round`), a run stopped as a loop and the work lost.

**Suggested fix.** Read it like the other booleans: absent/`null` → `false`, any
other JSON value that is not a bool → `Err("edit {n}: `replace_all` must be true
or false")`.

**Acceptance test.** `a_replace_all_that_is_not_a_bool_is_refused`: the two calls
above are errors naming the edit index and the field, `true`/`false`/absent still
work, and `edit_text`'s behaviour is unchanged.

---

## B11 — The attach socket has no bound anywhere: line, threads, or idle time

**Severity: minor.** `serve_connection` reads a request line into a `String` with
no cap (`crates/mush/src/attach.rs:129`,`132`):

```rust
let mut line = String::new();
loop {
    line.clear();
    match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) => {}
```

and `accept_loop` gives every connection a thread of its own, with no count
(`crates/mush/src/attach.rs:111`), and no read timeout at all. Measured with a
temporary probe inside the crate (test and server in one process):

```
PROBE after 64 MiB with no newline: rss = 74 MiB, peak = 74 MiB
PROBE answer to the 64 MiB line: {"error":{"kind":"bad_request","message":"not JSON: …"}}
PROBE threads before = 4, after 300 idle clients = 304
```

So 64 MiB travels into mush's heap before the newline arrives (and any amount
does), and 300 connections buy 300 threads that live until the client leaves,
with no idle timeout to reap them. The socket is created with the process's
umask under `.mush/`, so any same-user process can reach it — and this machine is
shared with every sibling agent's shell and every tool the human runs.

**Blast radius.** A runaway client, a buggy script, or a deliberately hostile
same-user process can OOM the mush process (taking the session, every actor and
every job) or spend the process's thread limit; the human sees a dead pane with
no explanation. Nothing in `attach.rs`, `docs/mush.md` §9 or `docs/mush.md` §M3
promises a bound, so this is a hole rather than a lie — but every other input
road in this tree is bounded (the clipboard's `READ_CAP`, the HTTP body's
`MAX_BODY_BYTES`, the command caps), which is the argument for one here.

**Suggested fix.** Three small ones in the same place: a `MAX_REQUEST_BYTES`
(64 KiB is generous for a JSON line, and 8 MiB is the wire's own idea of a big
message) refused with a `bad_request` and the connection kept; a cap on live
connections (say 64) with `unavailable` past it; and a read timeout per line so
an idle client is reaped. `ask` already has its own 30 s bound — the server half
owes the same.

**Acceptance test.** `a_request_line_is_capped_and_a_client_is_reaped`: a client
that sends `MAX_REQUEST_BYTES + 1` bytes without a newline is answered with a
`bad_request` naming the cap and the connection survives (a following good line
is answered); the connection count over the cap is answered with `unavailable`;
and a client that says nothing for the idle window is dropped (the test's clock,
not a sleep).

---

## B12 — `Message`'s deserializer is stricter than its own doc, and a refused reply ends the run

**Severity: minor.** The module's opening promise (`message.rs:3`) is
"intentionally loose (`Option` everywhere, `#[serde(default)]`) so that the many
'OpenAI-compatible' servers out there all round-trip cleanly", and §8.44's own
comment says "dying with 'could not parse model response' on it loses a whole
run". Four shapes break it; each fails the *whole* reply, not the field:

```
{"content":"hi"}                                                             -> ERR   (no `role`)
{"role":"assistant","content":[…],"tool_calls":[{"id":"x","type":"function"}]} -> ERR   (no `function`)
{"role":"assistant","content":"hi","tool_calls":[{"id":"x","type":"function",
  "function":{"name":"read_file","arguments":{"path":"a"}}}]}               -> ERR   (`arguments` an object)
{"role":"assistant","content":"hi","tool_calls":[{"id":1,"type":"function",
  "function":{"name":"read_file","arguments":"{}"}}]}                        -> ERR   (a numeric `id`)
    (and the shape a server is most likely to omit is fine: a `function` with no
     `arguments` parses — `arguments` is the one field in the group with a default)
```

The causes are `pub role: String` (`message.rs:204`, no default),
`FunctionCall`'s `name`/`arguments` and `ToolCall`'s `function`/`id`
(`message.rs:116`-`130`, where only `arguments` and `id` default, and `id`
defaults only for *absent*, not for a number). The failure surfaces at
`crates/mush/src/agent.rs:2532` as `Err("could not parse model response: …")`,
which ends the turn and the run — the very outcome the module's doc says the
looseness exists to prevent.

**Blast radius.** A server (or a proxy) that sends `arguments` as an object, a
numeric call id, or a message without `role`, costs the human the whole run: the
transcript, the tokens spent and the work in flight. The `content` half of this
loosening was done deliberately (`content_from_wire`); the `role`/`tool_calls`
half was not, and every one of these shapes was *parseable* by mush's own
predecessors in spirit — a message is a message.

**Suggested fix.** Make the two doors as loose as the doc claims: `role` with
`#[serde(default)]` (an empty role is still a message; the tool loop's match on
`"assistant"`/`"tool"` is the reader), a `function_type`-style default for a
missing `function`/`name`, `id` accepting any scalar (a number prints as its
text — [`assign_tool_call_ids`] already normalizes whatever arrives), and
`arguments` accepting a string *or* an object (serialize an object back to text
rather than failing the reply). A malformed *reply* should be a refusal the model
can answer, not an ended run — the shape `chat.rs`/`provider.rs` already handle
for a 400.

**Acceptance test.** `a_loose_reply_is_still_a_reply`: each wire shape above
deserializes into a `Message` (an `arguments` object arrives as its JSON text, a
numeric id as its text), and a reply with no `role` still answers rather than
ending the run.

---

## B13 — `.mush/paste/` is never pruned

**Severity: minor.** Every pasted picture — the clipboard road, the pasted-path
road and the app's carry — is written once and never removed: `write_pasted_image`
(`workspace.rs:640`) creates through `create_paste_file` (`workspace.rs:1251`) and
nothing in the tree deletes a `pasted-*` file (checked: no `remove_file` on that
directory outside tests). Five pastes leave five files:

```
5 pastes leave 5 files: ["pasted-…085-2.png", "pasted-…085.png", "pasted-…084.png", …]
```

**Blast radius.** Disk, on a machine where the box's own bound is 16.8 MB *per
message* (`BOX_IMAGE_BYTES`): a session that attaches a batch per turn, or a
human who pastes screenshots all day, keeps every byte forever, in a directory
`.mush/.gitignore` hides from `git status` — so `du` is the only thing that ever
says so.

**Suggested fix.** Prune at open (or on write) by mtime — keep the newest N days
or the newest N files, and never remove one the live transcript still points at
(the live `Image`s are in memory and their paths are known; the conservative
version deletes only files older than the session file's own start).

**Acceptance test.** `the_paste_directory_is_pruned_but_not_under_a_live_image`:
with a faked clock, write pastes across two "days", prune, assert the old ones
are gone, the newest are still there, and a path the live transcript names is
untouched.

---

## B14 — The markdown view's rows are not the plain wrapper's rows for non-ASCII whitespace

**Severity: minor (cosmetic).** `markdown_rows`' doc (`text.rs:315`) claims:

```rust
/// narrow. The rows this returns are the rows the plain wrapper would have
/// made for the same text, with the styles attached.
```

and `wrap_runs` implements the space break's tail differently from `wrap_capped`:
`wrap_capped` trims it with `trim_start()` (`text.rs:174`, which removes *any*
Unicode whitespace), `wrap_runs` removes only literal spaces
(`text.rs:678`-`681`):

```rust
current = rest;
while matches!(current.first(), Some((' ', _))) {
    current.remove(0);
}
```

A fuzz over the alphabet `{a, b, ' ', U+00A0, U+3000, U+2028, tab}`, lengths 1–5,
widths 1–6 found **8,823 divergences**:

```
DIVERGE " \u{a0}a" @ 2: wrap=["", "a"] view=["", "\u{a0}a"]
DIVERGE " \u{3000}b" @ 3: wrap=["", "b"] view=["", "\u{3000}b"]
DIVERGE " \u{2028}a" @ 2: wrap=["", "a"] view=["", "\u{2028}a"]
```

The view is the *more* honest of the two here (a no-break space is not a space),
so the defect is the claim and the missing test, not the painting: the existing
`a_plain_message_wraps_exactly_like_wrap_text` test covers only ASCII and CJK
text, which is why the divergence survived §8.50.

**Blast radius.** Cosmetic: rows differ between the markdown view and the plain
wrapper for a line whose break lands before an NBSP/ideographic space/line
separator. No data is lost (the copy road copies `Message::text()`), and the
width invariant holds on both roads (no row outgrows its width in either).

**Suggested fix.** Make the two share one tail rule (either `trim_start` in both,
or drop the leading-whitespace removal in both), or restate the doc: "the same
break points as `wrap_text`; the tail a break leaves keeps whitespace that is not
a space".

**Acceptance test.** Extend the equality test with the alphabet above and widths
1–12 — the fuzz that found this becomes the pin.

---

## B15 — `sanitize` keeps the bidi marks, and the invisible set is wider than the doc says

**Severity: minor.** `text.rs:26` says the rule removes "every other **C0/C1
control** and `DEL` … along with the bidi embedding and isolate characters: they
exist to command a display rather than to be read, and the one U+200D a ZWJ emoji
needs is not among them", and `invisible` (`text.rs:55`) is
`ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')`.
Probed:

```
LRM U+200E survives ; RLM U+200F survives ; ALM U+061C survives
ZWSP U+200B survives ; ZWNJ U+200C survives ; ZWJ U+200D survives
BOM U+FEFF survives ; SHY U+00AD survives
LRI U+2066 removed ; RLO U+202E removed
```

The *embeddings* and *isolates* are removed as documented; the *marks*
(U+200E/200F/061C) are not, and they are exactly the characters that reorder
neutral runs inside a line — the class the doc says it is guarding against. The
zero-width set (ZWSP, ZWNJ, BOM, SHY) is likewise not named, so a model's reply
can carry a path that looks like `src/main.rs` and is not.

**Blast radius.** A pane that can be made to paint an order other than the
bytes', and a "fact" (a path, a command, an error line) whose rendered form
differs from its bytes. The human's copy road copies the source bytes, so this is
display-only — but the display is where the human decides what to do.

**Suggested fix.** Add U+200E, U+200F, U+061C to `invisible`, and either drop the
zero-width characters or state them in the doc's own list (a `␀`-style marker for
them is one option; silent removal of a ZWJ breaks emoji, which is why it is
kept).

**Acceptance test.** `the_display_marks_that_command_are_gone`: the ten
characters above, each painted through `truncate`/`wrap_text`, leave no
display-commanding character; a ZWJ emoji sequence survives intact.

---

## B16 — The read-modify-write window (suspected)

**Severity: minor.** `edit_tool`'s shape is read → transform → write
(`crates/mush/src/agent.rs:4917`-`4924`), and nothing between the two steps
checks that the file is still what was read:

```rust
let current = ws.read_file(&rel)?;
let updated = tools::edit_text_many(&current, &edits, &rel)?;
ws.write_file(&rel, &updated)?;
```

The `rename` makes each write atomic (readers never see a torn file), but an
edit written from a stale read is a *lost update*: the other writer's change is
gone. Staged at the level the tool uses — the model's read, a sibling's write,
the model's write:

```
final bytes: "line one\nline 2\n"     (the sibling's line is gone)
```

**Not proven as a race.** The window is structural (two functions, no
compare-and-swap, no revision), and the demonstration is sequenced by hand; the
window itself was not driven inside one call, and I could not stage a writer
between `read_file`'s return and `write_file`'s rename without a hook. Whether
two writers on one file is plausible in practice is the human's answer: two
agents share one workspace when neither is isolated, and the human's own editor
is a third.

**Blast radius.** A sibling agent's or the human's edit to the same file,
silently reverted by the model's write. The file is left valid (no tear) so
nothing warns.

**Suggested fix (a decision more than a patch).** Either accept it and say so in
`edit_file`'s doc and schema ("the edit is applied to the file as it was read; a
concurrent writer's change is lost"), or make the write conditional: reread the
stat before the rename and refuse with "`{rel}` changed while the edit was being
applied — read it again" when size/mtime/ino moved. The second is ~10 lines in
one function and turns a silent loss into a refusal the model can act on.

**Acceptance test (needs a hook).** `an_edit_of_a_file_that_moved_is_refused`:
with the write paused between the read and the rename, a second writer's change
lands and the edit is refused naming the file; with no second writer, the edit
lands.

---

## Verified sound

Each line is what convinced me, not what the prose says.

- **`resolve` refuses `..`, absolute paths and the root** (`workspace.rs:322`;
  `write_file` refuses root by name): probe — `read_file("..")`,
  `read_file("/etc/passwd")`, `list_files("..")` all error; `write_file("")`,
  `write_file(".")` refuse; a NUL in a name and a 200-deep path error cleanly
  rather than panicking.
- **A non-regular file cannot park a reader** (the FIFO lesson, §8.44, kept on
  the roads §8.44 did not mention): probe with a *bound socket* —
  `read_file(".mush/mush.sock")` → "is not a regular file — cannot read it",
  `read_window` the same, `read_image` → `Ok(None)`, `search` → no match,
  `skipped 0`; and the file's own `mkfifo` watchdogs for both roads pass.
- **The write is atomic for a reader**: `NamedTempFile::new_in(parent)` +
  `persist` (`workspace.rs:1315`) — a rename within one directory, so a reader
  sees the old file or the new one and never a half-written one; two writers
  leave the last rename's file whole. Check: the code path plus
  `write_then_read_roundtrips` and the probe's own failures (a read-only
  directory, an unwritable parent) — every one of them leaves the target as it
  was.
- **A write onto a directory, or under a file, is refused** (probe):
  `write_file("adir")` → "Is a directory", `write_file("afile/child")` →
  "cannot create afile: File exists".
- **The image road's order, which §8.44 fixed, still holds**: `image_at`
  (`workspace.rs:691`) stats before opening, refuses anything that is not a
  regular file *before* an open, takes the cap from the stat, sniffs 16 bytes
  before the whole read, and bounds the whole read with `take(cap + 1 - read)`
  (`workspace.rs:722`); `00096c6`'s pins (empty slice, `RIFF`, `GIF87`, a bare
  png signature, a real over-cap image named with the stat's number, a sparse
  32 MiB log refused before any read, the `None`-size sentence) are in the
  suite and pass.
- **The header parsers cannot panic and cannot lie**: the suite's cuts at every
  prefix length for all four formats, the lying length fields (a png IHDR of 12
  or `0xffff_ffff`, a jpeg segment of 0 or past the end, a webp chunk past its
  bytes), `u32::MAX × u32::MAX`, zero sides, and junk all answer `None`; I
  re-ran the crate's tests and read the parsers (`png_dimensions`,
  `jpeg_dimensions`' marker walk with its fill bytes and three non-frames,
  `gif_dimensions`, `webp_dimensions`' three shapes) line by line.
- **`create_paste_file` cannot overwrite a paste** (`workspace.rs:1251`):
  `create_new(true)` makes the existence test and the create one step, so a
  same-millisecond second paste takes `-2` and a *symlink* planted at the name is
  an `AlreadyExists` that moves on rather than a write through it. The suite's
  same-millisecond test and the four-distinct-copies test are the check, plus
  `create_new`'s semantics.
- **`.mush/` cannot be dirtied and its `.gitignore` cannot be clobbered**:
  `session::ensure_mush_dir` writes `*\n` only when the file is absent
  (`session.rs:29`), and `save_pasted_image`'s own test asserts the file's bytes
  stay `*\n`.
- **The tool table is one list** (`tools.rs:33`): ten names, `ToolName::parse`
  round-trips every one, `TOOL_NAMES`/`ORCHESTRATION_TOOLS` derive from the enum,
  and `prompt::tool_schemas` is tested against it (checked by reading the
  round-trip test and the derivation macro).
- **`edit_text`'s match is literal, counted, and consistent**: `matches().count()`
  and `replacen`/`replace` agree about non-overlapping occurrences, an empty
  `old` is refused, and a batch is all-or-nothing in memory (the suite's
  `a_batch_is_all_or_nothing`, `replace_all_changes_every_occurrence`). A 1 MB
  `old_string` against a 1 MB file answers in 46 ms with no panic (probe).
- **The message shape round-trips as the wire expects**: `images` are never
  deserialized (`skip_deserializing`), so no session and no endpoint can
  resurrect bytes; the content array is text-then-images with a *single* text
  part only when there is text; base64 matches RFC 4648's vectors including the
  57-byte boundary; `weight` saturates at a header's `u32::MAX × u32::MAX` and at
  the message sum, on 32-bit and 64-bit alike; `drop_images` is idempotent and
  leaves one line for a path with a newline (checked by reading the tests and
  re-running the crate: `an_image_rides_inside_content_as_a_data_url`,
  `the_largest_pixels_a_header_can_claim_do_not_overflow_the_weight`,
  `a_dropped_image_whose_path_holds_a_newline_still_leaves_one_line`).
- **What the human copies is the model's own bytes**: the copy road takes
  `Message::text()` split on `\n` (`crates/mush/src/app/chat.rs:543`,
  `lines_of`), never a row of the view; so §8.50's markdown view can drop a
  fence line and rewrite a heading without any risk of the human pasting the
  view. (Read at the source and against §8.49's tests.)
- **`sanitize` does what its own list says** for controls, CSI/OSC/short
  escapes, `\r`, and the bidi embeddings/isolates: probe — `\x1b]0;PWNED\x07`,
  `\x1b[2J`, `\x1b(B`, `\x1b7`, an unterminated `\x1b[38;5`, `\x07`, `\x00`,
  `\x7f`, `\u{202e}`, `\u{2066}` are all gone; a tab and a `\n` stay; `\r\n`
  loses its `\r` and a lone `\r` becomes `␍`. The gaps are B14/B15.
- **Wrapping never outgrows its width**, on both roads: the crate's
  `a_wrapped_row_never_outgrows_its_width` (widths 4–12 over tabs, CJK, mixed)
  and my own fuzz (7-character alphabet, widths 1–6) found no row past its
  width on either road — the two roads differ in *text* (B14), not in columns.
- **The attach protocol's own contract holds**: `Request::encode`/`parse_request`
  round-trip every op; a bad line is answered and the connection kept; a `null`
  id survives; `unavailable` is its own kind; a stale socket is cleared and a
  live one is not stolen; the guard removes the socket on drop; the CLI's `ask`
  has a 30 s read and write bound. Check: the file's own tests (read and
  re-run) plus my probe's surviving connection.

## Blind spots — invariants this area claims with no test behind them

- **`atomic_write`'s identity**: no test in the tree asserts a mode, an inode, a
  symlink or a hard link after a write. The whole of B1–B3 is untested, which is
  why no wave — including the recent §8.44–§8.50 ones — has seen it.
- **`read_file`'s cap**: `READ_FILE_CAP`'s own sentence ("The largest file a read
  tool will open whole") is pinned only for the `read_window` road; the function
  that reads whole is not.
- **`read_file`'s encoding guard**: the "binary" refusal is tested only with a
  NUL-bearing blob; no test touches invalid-but-NUL-free UTF-8 (B6), so the
  lossy-write road has no pin at all.
- **CRLF**: no test in the crate feeds a `\r\n` file to `read_window`,
  `edit_file` or `search`; the only `\r` tests are `sanitize`'s and the
  wrapper's.
- **`rel()`'s fold of `\`**: the one test that touches it
  (`a_path_outside_the_workspace_is_shown_whole`) *asserts* the fold as a
  feature (`ws.rel(&ws.root().join("a\\b")) == "a/b"`), so the misnaming
  (B9) is pinned as intended behaviour rather than caught.
- **`search`'s match line**: `search`'s own tests live in `agent.rs` (the walk
  order, the skip count, the "not there" path) and none of them looks at the
  bytes a matched line shows; the sanitizing cut has no pin (B8).
- **The socket's bounds**: nothing tests a long line, a connection cap, or an
  idle client (B11) — and nothing documents one.
- **The markdown view's equality with the wrapper**: the test's corpus is ASCII
  and CJK only, so the whitespace divergence (B14) survived §8.50's landing.
- **The bidi marks**: `sanitize`'s tests cover the embeddings/isolates and a
  ZWJ; none of the ten characters in B15 is named.

## Not verified — what needs a real filesystem, a platform, or a live endpoint

- **Durability under a power loss.** `atomic_write` has no `fsync` before the
  rename, so the doc's "a crash cannot corrupt the original" is verified only for
  a *process* crash (the old inode is intact until the rename). A machine or
  filesystem crash can leave the renamed file truncated or empty. Staging it
  needs a power cut or a fault-injecting filesystem.
- **A 32-bit target.** `arg_usize` (`tools.rs:128`) casts `u64 as usize`; on a
  32-bit target a huge number wraps. This box is 64-bit only.
- **The image short-read.** `image_at`'s head sniff is a single `read` of 16
  bytes; a regular file's short read is essentially only at EOF, but I could not
  stage a growing file that returns 1–15 bytes and then the rest (a FUSE or
  network filesystem would). If it happens, the file is read as *text* rather
  than as an image — a wrong answer, not a panic.
- **A live endpoint's tolerance**: whether a 2.8 MB `data:` URL (an image at the
  2 MB cap) is accepted, whether an endpoint rejects a content array whose only
  part is an image with no text, and whether a thinking endpoint accepts a
  replayed turn without `reasoning_content` — all three are claims the code makes
  about *other* programs. B12's shapes are the same question from the reply side.
- **The clipboard programs.** `wl-paste`, `wl-copy` and `convert` exist on this
  box (`xclip`/`pngpaste` do not); the paste roads' behaviour under a live
  Wayland session, and the 2 s deadline's real-world margin, were not driven
  (§8.44 measured them; I re-read the code and did not re-measure).
- **macOS/Windows.** The workspace layer is Linux-shaped (`std::os::unix` in the
  probes; `\` folding exists for a Windows path), so B1's mode, B3's socket type
  and B4's links are questions about this platform; `pngpaste` is macOS-only and
  untested here.
- **The read-modify-write race's real frequency** (B16): needs two agents on one
  file, or the human's editor, in a live session.
- **`docs/mush.md` §2's convention-vs-fence reading of B4** is a *ruling* I cannot
  make: whether a symlinked directory inside the root should resolve (as now),
  refuse, or resolve-within-root is the human's call; I have only shown that the
  code and one sentence of the doc disagree.

## Recorded, not changed — checked

- **§8.44's "the growth race cannot be staged"** stands: `meta.len()` and the
  bounded `take` are one function, and I could not stage the growth either (the
  cap arithmetic at `workspace.rs:722` is `cap + 1 - read`, which is correct for
  every `read` a 16-byte buffer can deliver).
- **§8.44's "a refused attach may already have carried a copy"** and **§8.43's
  "a paste that turns out to be text may have copied the outside names read
  before the word that disqualified it"** are still true *and now have a second
  cost*: those copies are among the ones B13 says nobody ever prunes. Not worse
  than recorded, but the ledger's "gitignored scratch" is the reason B13 has no
  visible symptom.
- **§8.48's "no schema or prompt promised a limit on a write"** holds — but the
  write road's *read* of the file it replaces was not examined by that wave, and
  it is B5.
- **§8.45's base64 caveat** ("an endpoint that tokenized the `data:` text itself
  is stated on the constant and not modeled") is intact; the box's byte bound is
  the bound, and `Message::weight` still does not model the 4/3 inflation — by
  the constant's own decision.
- **H23** (open, waiting on a ruling): the draft half of `edit` lands in the one
  box the human owns while the ack names `#N`. The code is what H23 records; the
  *doc* half lives in this layer — `attach.rs`'s `Op::Edit` says "Set the message
  box's draft for `agent`" and the crate's own test asserts the human's single
  box (`crates/mush/src/app/mod.rs`, `attach_edit_with_a_fresh_base_lands_a_draft`
  reads `app.chat.input()`), so the sentence is false until the ruling lands. One
  line of doc can say what the code does whatever the ruling is.
- **H28's flake** did not appear in this audit's runs (`cargo test -p mush-core`
  and the `mush` bin's attach tests, ~4 runs, none in `lock::`).

## Census of this audit

Sixteen findings: 0 blocker, 6 major (B1–B6), 10 minor (B7–B16, one suspected).
Four are loss of the human's data or a lost write (B1's mode, B2's symlink
replace, B6's encoding, B16's lost update); two are an edit that lands in the
wrong place or destroys what it replaced (B3, B4); three are a missing resource
bound (B5 memory, B11 the socket, B13 disk); five are the model being told
something untrue (B7–B10, B12); two are display arithmetic (B14, B15). Every
finding carries a probe or a measurement except B16, which is labelled
suspected, and every suggested fix names the test that would pin it.
