# Duplication and extractable primitives: the store, the workspace and the CLI

A blind pass over production code only — no tests read as evidence, no build, no
change. The area is fifteen files, **~4,600 production lines**: `workspace.rs`
(1,838), `config.rs` (1,053), `git.rs` (945), `attach.rs` (764), `session.rs`
(448), `tools.rs` (409), `main.rs` (1,241), `clipboard.rs` (531),
`transcript.rs` (477), `userconfig.rs` (316), `prompt.rs` (323), `provider.rs`
(281), `message.rs` (629), `lock.rs` (268), `session_save.rs` (367).

Every count below is physical production lines: `net` is
`sites − (primitive + the sites after)`, docs included, because in this tree the
doc comment is the code's other half. A candidate that nets five lines is
called a rounding error where it is one.

## Ranked candidates

Ranked by value per unit of effort, which is why the largest net (§7) is not
first: each subcommand's refusals are a pinned feature of the CLI and rewriting
them is the one candidate that can make the product worse while the tests stay
green-by-rewriting.

| # | candidate | sites (`file:line`) | net | risk if left | effort |
|---|---|---|---|---|---|
| 1 | `wire` derive for the attach protocol's encoders | `attach.rs:534-573`, `attach.rs:591-630` | **50** | low — the wire keys are written twice (parse + encode) and only the round-trip test keeps them in step | low |
| 2 | `wait_bounded`: one bounded child for the reader and the writer | `clipboard.rs:216-275`, `clipboard.rs:470-530` | **28** | medium — the deadline's kill/reap and the remaining-time receive are copied by hand | medium |
| 3 | `backup_name`: one numbering rule for the file beside the file | `session.rs:313-321`+`347-364`, `userconfig.rs:294-315` | **20** | medium — the bound `100` and the "next free name" rule have two homes, and a divergence loses a backup | low |
| 4 | `past_cap`: one refusal shape for a payload past a cap | `workspace.rs:1326-1349`, `1373-1393`, `1395-1424` | **14** | medium — the "the length in hand is the buffer's, not the file's" clause is spelled three times | low |
| 5 | `parse_context(value, road)`, and the CLI's own flag reading | `config.rs:356-361`, `main.rs:121-129`, `main.rs:116-158`+`449-454` | **6** | **already diverged** — see §5 | trivial |
| 6 | one table for the four attach subcommands | `main.rs:195-216`, `217-231`, `233-269`, `281-384`, `386-392`, `395-423`, `425-436`, `440-461` | **40** (high effort) | medium — every subcommand is written out in six places | high |
| 7 | `search`'s read is not bounded like the other two | `workspace.rs:1060-1076` vs `498-514`, `846-883` | **0** (a fix, not a merge) | medium — the second bound the other readers keep is missing | trivial |
| 8 | `default_hint`: one sentence shape for the provider hints | `provider.rs:198-222`, `231-253` | **5** | low — a hint that drifts from the table lies in the home config's own header | low |
| 9 | `named_thread`: a refused spawn is data, not a panic | `session_save.rs:212-238`, `attach.rs:119-131`, `attach.rs:180-188`, `main.rs:1037-1052` | **12** | low | low |
| 10 | `paste_dir` / `paste_rel`: one spelling of `.mush/paste/` | `workspace.rs:717`, `798-807`, `1619-1637` | **4** | low, but two messages already name a file that is not there | trivial |
| — | `tools::arg_*` (five readers, one shape) — do not merge, §11 | `tools.rs:122-181` | **0** | none — the shape is pinned per function | — |

Total, if every candidate above is done: **≈180 production lines**, of which
~90 are the four low-risk extractions (§1, §3, §4, §5 and the cheap tail).

---

## 1. `wire`: the request and the response are written to the wire by hand

### The sites

`Request::encode`, `attach.rs:534-573` — thirty-odd lines that rebuild, by hand,
the exact object `parse_request` (`attach.rs:467-532`) reads:

```rust
pub fn encode(&self) -> String {
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), self.id.clone());
    match &self.op {
        Op::Read { agent, since } => {
            object.insert("op".to_string(), json!("read"));
            object.insert("agent".to_string(), json!(agent));
            object.insert("since".to_string(), json!(since));
        }
        Op::Agents => {
            object.insert("op".to_string(), json!("agents"));
        }
        Op::Focus { agent } => {
            object.insert("op".to_string(), json!("focus"));
            object.insert("agent".to_string(), json!(agent));
        }
        Op::Edit { agent, base, text, send } => {
            object.insert("op".to_string(), json!("edit"));
            object.insert("agent".to_string(), json!(agent));
            object.insert("base".to_string(), json!(base));
            object.insert("text".to_string(), json!(text));
            object.insert("send".to_string(), json!(send));
        }
    }
    Value::Object(object).to_string()
}
```

`Response::encode`, `attach.rs:591-630`, is the same shape for the answer:

```rust
pub fn encode(&self) -> String {
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), self.id.clone());
    match &self.reply {
        Reply::Ok(body) => {
            object.insert("ok".to_string(), body.clone());
        }
        Reply::Err(error) => {
            let mut body = serde_json::Map::new();
            body.insert("kind".to_string(), Value::String(error.kind.clone()));
            if let Some(message) = &error.message {
                body.insert("message".to_string(), Value::String(message.clone()));
            }
            if let Some(revision) = error.revision {
                body.insert("revision".to_string(), json!(revision));
            }
            object.insert("error".to_string(), Value::Object(body));
        }
    }
    Value::Object(object).to_string()
}
```

### The shared shape

Both encoders are a `match` over an enum whose *variant and field names are the
wire keys*, written beside a hand parser that reads the same names. `"op"`,
`"agent"`, `"since"`, `"base"`, `"text"`, `"send"`, `"ok"`, `"error"`, `"kind"`,
`"message"`, `"revision"` each appear twice in this file; renaming one on the
parse side alone leaves `encode` emitting a key nobody reads, and what catches it
is only `every_op_round_trips_through_its_line` — i.e. the parser calling itself.

The parse half must stay hand-written: its whole value is the sentence per wrong
field (`` `agent` must be a non-negative integer ``, the `id` kept with a bad
request), which serde cannot shape. The **encode** half has no such reason.

### The primitive

```rust
/// The line an [`Op`] travels as, derived from the type: one spelling of the
/// wire keys beside the parser that reads them, so a field renamed on one side
/// of the socket is a compile error rather than a client painting a blank
/// column (finding R23's class).
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op { … }
```

and, for the response, two optional keys in a struct rather than a `match`:

```rust
#[derive(Serialize)]
struct Wire<'a> {
    id: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ReplyError>,
}
```

Two caveats I can only name, not settle (see the method note): `serde_json`'s
error makes the derived encoder fallible where the hand-built `Value` cannot
fail, so `encode` needs one documented `.expect("a request is serializable")`
(the precedent is `Provider::spec`'s); and `ok: null` — a legal body the hand
encoder writes as `null` — must stay writable, which is why the response goes
through two optional keys and **not** `#[serde(flatten)]`, which refuses a
non-object payload.

### What it removes

Sites 40 + 40 physical lines. After: two ~4-line `encode` bodies, ~22 lines of
derives and the wire struct, ~10 of doc. **Net ≈ 50 lines**, and the wire keys
gain one home per direction.

---

## 2. `wait_bounded`: the clipboard's reader and writer are one road twice

### The sites

`clipboard.rs:216-275` (`run`, the reader's half) and `clipboard.rs:470-530`
(`deliver`, the writer's half). The prologues differ in which end is piped, and
from there on they are the same function:

```rust
// run, 216-275                                    // deliver, 470-530
let mut command = Command::new(program);           let mut command = Command::new(program);
command                                            command
    .args(args)                                        .args(args)
    .stdin(Stdio::null())                              .stdin(Stdio::piped())
    .stdout(Stdio::piped())                            .stdout(Stdio::null())
    .stderr(Stdio::null());                            .stderr(Stdio::null());
let mut child = match scrub(&mut command).spawn() { let mut child = match scrub(&mut command).spawn() {
    Ok(child) => child,                                Ok(child) => child,
    Err(_) => return Answer::Missing,                  Err(_) => return Delivered::Missing,
};                                                 };
let Some(stdout) = child.stdout.take() else {      let Some(mut stdin) = child.stdin.take() else {
    let _ = child.kill();                              let _ = child.kill();
    let _ = child.wait();                              let _ = child.wait();
    return Answer::Nothing;                            return Delivered::Refused;
};                                                 };
let (tx, rx) = std::sync::mpsc::channel();         let (tx, rx) = std::sync::mpsc::channel();
std::thread::spawn(move || {                       std::thread::spawn(move || {
    let _ = tx.send(drain(stdout));                    let _ = tx.send(stdin.write_all(text.as_bytes()));
});                                                });
loop {                                             loop {
    match child.try_wait() {                           match child.try_wait() {
        Ok(Some(status)) => {                              Ok(Some(status)) => {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(drained) = rx.recv_timeout(left)            let Ok(sent) = rx.recv_timeout(left)
                else { return Answer::TimedOut };                  else { return Delivered::TimedOut };
            return if status.success() && !drained.bytes       return if status.success() && sent.is_ok()
                .is_empty() { Answer::Bytes(drained) }             { Delivered::Taken }
                else { Answer::Nothing };                          else { Delivered::Refused };
        }                                                  }
        Ok(None) => {}                                     Ok(None) => {}
        Err(_) => {                                        Err(_) => {
            let _ = child.kill();                              let _ = child.kill();
            let _ = child.wait();                              let _ = child.wait();
            return Answer::Nothing;                            return Delivered::Refused;
        }                                                  }
    }                                                  }
    if Instant::now() >= deadline {                    if Instant::now() >= deadline {
        let _ = child.kill();                              let _ = child.kill();
        let _ = child.wait();                              let _ = child.wait();
        return Answer::TimedOut;                           return Delivered::TimedOut;
    }                                                  }
    std::thread::sleep(POLL);                          std::thread::sleep(POLL);
}                                                  }
```

### The shared shape

A bounded child whose other end must be held by a thread (a png is larger than a
pipe buffer; a writer's stdin can fill), waited on by a poll loop under one
shared deadline, killed and reaped on every path out, and answering with the
thread's own send — because a `join` has no deadline and a grandchild can hold
the pipe after the child exits. The *deadline discipline* is the thing two
copies keep in step by hand: use the remaining time in the receive, kill and
reap before returning, and never await a thread past the deadline. A reader copy
that forgets the `wait` after `kill` leaks a zombie; a writer copy that used the
whole deadline in the receive would double the human's wait.

### The primitive

```rust
/// What waiting on a bounded child produced.
enum Waited<T> {
    /// It exited; the thread holding its other end answered with this.
    Ended(std::process::ExitStatus, T),
    /// The deadline arrived first — the child was killed and reaped, and the
    /// answer is nobody's to wait for.
    TimedOut,
}

/// Wait for `child` under `deadline`, reading the answer a thread produced from
/// the end of its pipe: the one home of the kill-and-reap, the remaining-time
/// receive and the poll interval, so a reader and a writer cannot disagree about
/// when a clipboard program is out of time.
fn wait_bounded<T>(mut child: Child, answers: Receiver<T>, deadline: Instant) -> Waited<T>
```

Each site then keeps only what differs: which end it wires, what its thread does
with it, and the two lines that read [`Waited`] into its own answer type.

### What it removes

Sites 60 + 61 physical lines; the primitive is ~26 code + 10 doc. The two
`run`/`deliver` bodies after it are ~28 lines each with their docs intact.
**Net ≈ 28 lines**, and one home for a hang's semantics instead of two.

---

## 3. `backup_name`: the file beside the file, numbered, twice

### The sites

`session.rs:313-321` and `session.rs:347-364`:

```rust
/// How many backup names mush will try beside an unreadable session before it
/// gives up looking. A workspace that has been hand-broken a hundred times has
/// a problem that no file name solves.
const BACKUP_TRIES: u32 = 100;

/// Where an unreadable session is kept, before the caller starts numbering:
/// `.mush/session.json.bak`.
fn first_backup(root: &Path) -> PathBuf {
    mushroom_dir(root).join(format!("{SESSION_FILE}.bak"))
}

pub fn keep_unreadable(root: &Path) -> Result<PathBuf, String> {
    let from = session_path(root);
    let base = first_backup(root);
    for step in 1..=BACKUP_TRIES {
        let to = if step == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{step}", base.display()))
        };
        if to.exists() {
            continue;
        }
        return fs::rename(&from, &to)
            .map(|()| to)
            .map_err(|error| cannot_keep(&from, error));
    }
    Err(cannot_keep(&from, "every backup name beside it is taken"))
}
```

`userconfig.rs:294-315`:

```rust
fn keep_unparsable(path: &Path) -> std::io::Result<PathBuf> {
    /// How many names a hand-broken file may burn before the problem is not
    /// the name.
    const TRIES: u32 = 100;
    let base = PathBuf::from(format!("{}.bak", path.display()));
    for step in 1..=TRIES {
        let to = if step == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{step}", base.display()))
        };
        if to.exists() {
            continue;
        }
        fs::rename(path, &to)?;
        return Ok(to);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("every backup name beside {} is taken", path.display()),
    ))
}
```

### The shared shape

Eight lines of the loop are identical character for character; the bound is
`100` in two places; and the rule — never overwrite a copy already beside the
file, because one already there is one the human already needed — is written
twice. The two docs even say so out loud: `userconfig.rs:288` calls the rule
"the session store's" and points at `keep_unreadable`, which is exactly the
sentence a shared function should be making unnecessary. The only real
difference is how `base` is spelled and which error type comes out.

### The primitive

```rust
/// The first free name beside `path` — `<path>.bak`, then `.bak.2`, `.bak.3`, …
/// up to the one bound mush will burn looking — so the two files mush sets
/// aside when it cannot read them cannot disagree about the numbering, and a
/// copy already beside the file is never the second accident.
pub fn backup_name(path: &Path) -> Result<PathBuf, String>
```

It belongs beside `atomic_write` in `workspace.rs`: both are the store-file
plumbing every writer shares. `userconfig` maps the `String` through
`std::io::Error::other` (it already imports from `workspace`); `session`
prefixes `cannot_keep`.

### What it removes

Sites 27 + 22 = 49 physical lines: the const and `first_backup` go entirely
(9), session's 13-line loop becomes a 2-line call, userconfig's 18-line body
becomes 4. Primitive ~9 code + 9 doc + the bound's own comment. **Net ≈ 20
lines**, and `100` gets one home.

---

## 4. `past_cap`: three refusals, one shape

### The sites

`workspace.rs:1336-1349`, `1379-1393`, `1406-1424`:

```rust
// over_read_cap, 1336-1349
fn over_read_cap(rel: &str, size: Option<u64>) -> String {
    let cap = READ_FILE_CAP / (1024 * 1024);
    let road = format!("read part of it with run_command (`sed -n '1,200p' {rel}`)");
    match size {
        Some(size) => format!(
            "{rel} is {size} bytes — past the {cap} MB cap on a whole read, and a window cannot \
             get past it (the file is opened whole first): {road}"
        ),
        None => format!(
            "{rel} grew past the {cap} MB cap while it was being read — its size is not known, \
             and a window cannot get past the cap (the file is opened whole first): {road}"
        ),
    }
}

// image_too_big, 1379-1393
fn image_too_big(name: &str, mime: &str, size: Option<u64>) -> String {
    let cap = IMAGE_FILE_CAP / (1024 * 1024);
    let format = mime.strip_prefix("image/").unwrap_or(mime);
    let road = format!(
        "Downscale it with run_command (`convert {name} -resize 50% small.png`) and read that"
    );
    match size {
        Some(size) => format!(
            "{name} is a {format} image of {size} bytes — past the {cap} MB cap on an image. {road}"
        ),
        None => format!(
            "{name} is a {format} image past the {cap} MB cap on an image — the read stopped at \
             the cap before the file's end, so its size is not known. {road}"
        ),
    }
}

// clipboard_image_too_big, 1406-1424
fn clipboard_image_too_big(mime: &str, size: Option<u64>) -> String {
    let cap = IMAGE_FILE_CAP / (1024 * 1024);
    let format = mime.strip_prefix("image/").unwrap_or(mime);
    let road = "Save it to a file and downscale it (`wl-paste -t image/png > shot.png`, \
                then `convert shot.png -resize 50% small.png`), then copy the smaller one";
    match size {
        Some(size) => format!(
            "the clipboard image is a {format} of {size} bytes — past the {cap} MB cap on an \
             image. {road}"
        ),
        None => format!(
            "the clipboard image is a {format} past the {cap} MB cap on an image — it was cut \
             off at the cap before its end, so its true size is not known. {road}"
        ),
    }
}
```

### The shared shape

`cap / (1024 * 1024)`, `mime.strip_prefix("image/")`, a road, and a `Some/None`
pair of sentences. The `None` arm is the interesting one: it exists because
naming a buffer's length as the file's own was a real defect, and the sentence
that says "its size is not known" is the fix. That clause is written three
times, so the next reader that learns about it — or the next one that forgets it
— has to find all three.

### The primitive

```rust
/// The one refusal a payload past a cap gets, whichever road read it: what it
/// is, the cap in the unit a human reads, and the road that makes it readable —
/// naming a length only when it is the file's own, because a read that stopped
/// at the cap has only the buffer's to offer.
fn past_cap(what: &str, cap: u64, size: Option<u64>, road: &str) -> String
```

Each site keeps its own `what` ("a whole read", "an image", "the clipboard
image") and its own road; the cap arithmetic and the size clause move in.

### What it removes

Sites 14 + 15 + 19 = 48 code-heavy lines plus ~27 lines of doc; after: a ~10-line
builder with doc and three ~6-line call sites with their docs trimmed to the part
that is theirs. **Net ≈ 14 lines**, and the false-number clause gets one home.

---

## 5. A stated window: one rule, two spellings, already diverged

### The sites

`config.rs:353-361` and `main.rs:121-129`:

```rust
// config.rs:356 — the environment's road
pub fn parse_context_env(value: &str) -> Result<usize, String> {
    match value.trim().parse::<usize>() {
        Ok(tokens) if tokens > 0 => Ok(tokens),
        _ => Err(format!("MUSH_CONTEXT needs a token count, got `{value}`")),
    }
}

// main.rs:121 — the flag's road
"--context" => {
    let value = args.next().ok_or("--context needs a value")?;
    let tokens = value
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("--context needs a token count, got `{value}`"))?;
    overrides.context = Some(tokens);
}
```

### The divergence, in the code as it stands

`parse_context_env` **trims**; the flag's arm does not. So

* `MUSH_CONTEXT=" 8192"` → `Ok(8192)`,
* `--context " 8192"` → `Err("--context needs a token count, got ` 8192`")`.

The same number, stated on the two roads a human has, is accepted on one and
refused on the other. Nothing pins the flag's behaviour to the env's; the two
were written separately and one keeps a `.trim()` the other never had. This is
the class the brief asks to be called out: not a future risk but a present
disagreement, in five lines of code.

### The primitive

```rust
/// A window a human stated, and the road they stated it by: every road accepts
/// the same spelling of a token count — surrounding space and all — and refuses
/// everything else by naming the road it came in by, so `--context` and
/// `MUSH_CONTEXT` cannot read one number two ways.
pub fn parse_context(value: &str, road: &str) -> Result<usize, String>
```

`parse_context_env(value)` becomes `parse_context(value, "MUSH_CONTEXT")`, and
`main.rs`'s arm becomes two lines.

The same file's flag reading has the sibling smell, cheap to fold in:
`args.next().ok_or("--url needs a value")` is spelled **seven times**
(`main.rs:116,117,119,122,131,143,154`), each with the flag's name retyped into
the sentence, while `main.rs:449-454`'s `number(value, flag)` spells the same
sentence a second way, as `format!("{flag} needs a value")`.

### What it removes

~6 lines. A rounding error by lines, a live disagreement by behaviour: this is
the first thing I would do (see *The first thing I would do*, below).

---

## 6. The four attach subcommands, written out in six places

### The sites

One subcommand exists as: an `enum Cli` variant (`main.rs:195-216`), a row in
`ATTACH_FLAGS` (`225-231`), an arm in the flag loop of `Cli::detect`
(`281-384`), an arm in its final construction `match` (`350-380`), an arm in
`Cli::dir` (`386-392`), an arm in `Cli::request` (`395-423`), an arm in
`Cli::run` (`425-436`), a printer (`474-504`), and a block of `help_text`
(`523-...`). Four subcommands × six places ≈ **250 lines** of plumbing, plus the
two hand-rolled flag parsers in one file (`parse_from`, `89-174`, and
`Cli::detect`, `281-384`) with separate value reading, separate `--` handling
and separate `--help` exits.

### The shared shape

The shape is a row: name, the flags it takes, how many positionals it owns, the
`Op` it becomes, and the printer for its answer. What is *good* here is the
opposite of a smell and is why this candidate is ranked by effort: each
subcommand's refusals are pinned findings — `--since` on `agents` refused by
name (A16's class), both ids for `focus` refused (H26), a flag given twice
refused (H26), `--` ending the options (A5) — and they are what the tests read.

### The primitive

```rust
/// One attach subcommand: the name a human types, the flags it takes, how many
/// positionals it owns, and how its parsed words become a request — the one
/// table `Cli::detect`, the flag refusals, the request and the help block all
/// read, so a subcommand cannot be advertised without a parser or parsed
/// without an advertisement.
struct Sub {
    name: &'static str,
    flags: &'static [&'static str],
    own: usize,
    build: fn(Words) -> Result<Cli, String>,
}
```

with `struct Words { agent: Option<u64>, since: Option<usize>, base: Option<u64>,
send: bool, positional: Vec<String> }`. The flag loop becomes generic;
`Cli::dir`/`Cli::request`/`Cli::run` collapse into one match over a request the
row builds.

### What it removes

~250 lines of sites → a table of ~60 and four `build` functions of ~6. **Net
≈ 40 lines** on a conservative reading (the two-id rule, `trailing_dir` and the
printers stay). Effort high; I would take the ~6 lines of §5 first and touch
this only with the tests in hand.

---

## 7. `search` reads without the bound the other two readers keep

### The sites

`workspace.rs:1060-1076`, inside `search`'s walk closure:

```rust
let Ok(meta) = fs::metadata(path) else {
    skipped += 1;
    return true;
};
if meta.len() > SEARCH_FILE_CAP {
    skipped += 1;
    return true;
};
let Ok(bytes) = fs::read(path) else {
    skipped += 1;
    return true;
};
if bytes.contains(&0) {
    skipped += 1;
    return true;
}
```

against `whole_read` (`498-514`), which after the same stat check reads under a
bound:

```rust
let file = fs::File::open(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
let mut bytes = Vec::new();
file.take(READ_FILE_CAP + 1)
    .read_to_end(&mut bytes)
    .map_err(|e| format!("cannot read {rel}: {e}"))?;
if bytes.len() as u64 > READ_FILE_CAP {
    return Ok(WholeRead::PastCap(None));
}
```

and `image_at` (`876-882`), which keeps the same `cap + 1` bound with the
comment naming why: "a file can grow between the stat and the read".

### The shape and the finding

Two readers check the cap *from the stat* and then bound the read; the third
checks it from the stat and then calls `fs::read`, which has no bound at all. A
file that grows past `SEARCH_FILE_CAP` between the stat and the read is read
whole — the one thing the other two roads' `take(cap + 1)` exists to prevent.
The window is small (a race, and only a race), so this is not a live bug; it is
the drift the brief's second item is about, and it is one line away from being
closed.

### The primitive (a fix, not a saving)

```rust
/// The bytes of `path` under `cap`, from the stat first and a read bounded to
/// `cap + 1` bytes second, so a file that grew behind the stat is caught by the
/// length rather than loaded: the one read behind every road that has to answer
/// for a file larger than it will read.
fn read_capped(path: &Path, cap: u64) -> io::Result<Capped>
```

Honestly: **this does not pay for itself in lines** (`whole_read`'s and
`image_at`'s orders differ on purpose — `image_at` must sniff the head *before*
refusing an over-cap non-image, and that difference is documented). I would take
the fix — `File::open` plus `take(SEARCH_FILE_CAP + 1)` in `search`, counting a
skip when the bound is hit — and leave the three readers as they are.

---

## 8. `default_hint`: two hints, one sentence

`provider.rs:198-222` (`thinking_default_hint`) and `231-253`
(`effort_default_hint`) are the same paragraph twice: collect what the rows
state, ask whether any row states nothing, return `no \`X\` field anywhere` when
none does, else the join plus its "elsewhere" clause.

```rust
pub fn thinking_default_hint() -> String {          pub fn effort_default_hint() -> String {
    let on: Vec<&str> = PROVIDERS                       let stated: Vec<String> = PROVIDERS
        .iter()                                              .iter()
        .filter(|spec| spec.thinking_by_default)             .filter_map(|spec| spec
        .map(|spec| spec.name)                                   .reasoning_effort_by_default
        .collect();                                              .map(|e| format!("`{e}` for {}", spec.name)))
    let off_somewhere = PROVIDERS.iter()                     .collect();
        .any(|spec| !spec.thinking_by_default);          let silent_somewhere = PROVIDERS.iter()
    if on.is_empty() {                                       .any(|spec| spec.reasoning_effort_by_default.is_none());
        return "no `thinking` field anywhere".to_string();   if stated.is_empty() {
    }                                                            return "no `reasoning_effort` field anywhere".to_string();
    let sentence = format!("on for {}", on.join(", "));      }
    if off_somewhere {                                       let sentence = stated.join(", ");
        format!("{sentence}, off elsewhere")                  if silent_somewhere {
    } else {                                                     format!("{sentence}, no field elsewhere")
        sentence                                             } else {
    }                                                            sentence
}                                                            }
```

```rust
/// A per-row default spelled as the sentence the help and the home config's
/// header print — the rows that state one, and the word for the rows that do
/// not — so a hint cannot drift from the table it describes.
fn default_hint(field: &str, stated: impl Fn(&ProviderSpec) -> Option<String>, elsewhere: &str) -> String
```

Sites 53 physical lines → a ~16-line builder and two ~10-line wrappers.
**Net ≈ 5 lines** — a rounding error in lines, and said plainly: its value is
that a new provider row shows up in every hint by construction, which is the
one thing these four functions exist for.

---

## 9. Named threads: a refused spawn is data, not a panic

Four sites, one shape — `thread::Builder::new().name("mush-…").spawn(…)` whose
`Err` is returned or handled:

```rust
// session_save.rs:221-236                          // attach.rs:119-131
let started = thread::Builder::new()                thread::Builder::new()
    .name("mush-save".to_string())                      .name("mush-attach".to_string())
    .spawn(move || {                                    .spawn(move || accept_loop(listener, ui_tx, limits))
        let _alive = Alive(inner.clone());              .map(|_| ())
        writer(inner, woken);                           .map_err(|error| format!(
    });                                                     "could not start the attach thread: {error}"))
match started {
    Ok(worker) => { … }
    Err(error) => { … Err(format!("could not start the session writer: {error}")) }
}
```

`attach.rs:180-188` (a connection thread's refused start gives its slot back)
and `main.rs:1037-1052` (the model-discovery thread; a refused start sends an
empty list) are the other two. `clipboard.rs:242` and `:490` spawn unnamed
threads, which is a smaller version of the same question.

```rust
/// Start a thread with the name a panic can be traced to, and answer with the
/// reason a refused start is not a raised panic: every road here has a human to
/// tell and something useful to do without the thread, which is why a thread the
/// OS will not give is not a reason to die.
fn named_thread(name: &str, work: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, String>
```

Sites ≈ 50 lines of thread-start and error-shaping → ≈ 20; the primitive is ~10
code + 8 doc. **Net ≈ 12 lines**; the real gain is that the thread-naming scheme
and the never-raise rule have one home.

---

## 10. `.mush/paste/` has four spellings, and two of them lie

`workspace.rs:798-807`, `717` and `1619-1637`:

```rust
let dir = self.root().join(session::MUSH_DIR).join("paste");           // 798
    .map_err(|e| format!("cannot create {}/paste: {e}", …))?;          // 800
        .map_err(|e| format!("cannot write {}/{name}: {e}", …))?;      // 803  ← .mush/<name>
    path: format!("{}/paste/{name}", session::MUSH_DIR),               // 807
```

and in `create_paste_file`, for the same directory:

```rust
Err(e) => return Err(format!("cannot write {}/{name}: {e}", session::MUSH_DIR)),  // 1636 ← .mush/<name>
```

The directory is built one way, displayed another, and the two write failures
name `.mush/pasted-<millis>.png` for a file that is at
`.mush/paste/pasted-<millis>.png` — a message the human is meant to act on,
pointing at a path that is not there. `Image.path` (`807`) is the load-bearing
one: it is what a placeholder keeps and what the model's own tools resolve.

```rust
/// Where a pasted picture lives under a workspace: the directory the bytes are
/// written into and the workspace-relative name an [`Image`] carries, one
/// spelling each, so a message about a paste cannot name a file that is not
/// there.
pub fn paste_dir(root: &Path) -> PathBuf          // <root>/.mush/paste
pub fn paste_rel(name: &str) -> String            // .mush/paste/<name>
pub const PASTE_REL: &str = ".mush/paste";        // the directory in a sentence
```

Sites ≈ 10 lines → ≈ 6. **Net ≈ 4 lines** and two wrong sentences fixed; a
rounding error in lines, cheap in effort, and it ends a way the code contradicts
itself.

## 11. `tools::arg_*`: five readers that look like one, and should stay five

`tools.rs:122-181` holds `arg_string`, `arg_string_opt`, `arg_usize`,
`arg_bool`, `arg_path`, each a `match args.get(key)` whose absent arm is either
`Ok(None)`/`Ok(default)` or an error. A generic
`arg_or(args, key, default, want, read)` was my first candidate and does not
survive its own arithmetic: `arg_string` is *required* (`missing \`{key}\``),
the two string readers quote the value they got (`got {other}`) while the
numeric ones do not, and each message is the sentence a model acts on. A
generic costs ~15 lines to remove ~15 and risks rewording five pinned
sentences. **Net ≈ 0: leave it.** The rule they *do* share — `null` is absent,
a wrongly typed value is refused by name and never defaulted (A7, F12) — is
already written once per function in prose, and the tests at the bottom of the
file pin it per function.

---

## Looks duplicated but is not

* **`IMAGE_FILE_CAP` and `SEARCH_FILE_CAP` are both `2 * 1024 * 1024`**
  (`workspace.rs:54`, `:59`). Two caps that happen to hold the same number, with
  two different threats: one bounds what mush will put on the wire; the other
  bounds what a walk will open in memory. Merging them would make raising the
  image cap raise the search cap silently, which is the drift this report is
  about, in the other direction. Keep them separate — but see §7, where the
  *search* road is the one that should read under its cap rather than before it.
* **`read_file` (strict) and `read_window` (lossy)** — already one road
  (`whole_read`), with the decoding passed in, and the difference is deliberate:
  what is written back must not be lossily decoded (finding B6).
* **`truncate_for_model` and `tail_for_model`** (`workspace.rs:1647`, `:1669`):
  same cap arithmetic, opposite ends, and the two *sentences* are the whole
  behaviour ("read on with offset", "the end is shown"). A three-argument
  `cut(text, cap, side)` would net ~4 lines and make each call site read as a
  flag instead of a promise. Leave them.
* **`git()` and `run_named()`** (`git.rs:54-61`, `90-101`): the four-line
  invocation (`-C`, `args`, `LC_ALL=C`, `scrub`) is duplicated, but the two
  policies — a question with no answer (`None`) versus a mutation whose failure
  the human must read (`GIT_UNAVAILABLE`) — are the point of having two. One
  `fn git_command(dir, args) -> Command` (~3 lines + doc) is the honest
  extraction, and it nets **zero**: the two sites are four lines each. A
  rounding error; take it only with a reason to touch this file.
* **`Parse the same number` — `parse_context_env`'s `> 0` and
  `parse_context_hint`'s `MIN..=MAX`** (`config.rs:358`, `:1025`): the first is a
  human's statement with a floor it can be clamped to; the second is an
  endpoint's guess, where an implausible number is evidence of a misparse.
  Different threats, different bounds — keep.
* **`session::Stored` (`Absent`/`Loaded`/`Unusable`) and `userconfig::Loaded`
  (`config` + `complaint`)**: one three-way fact with two vocabularies. The
  session's must not be flattened (the flatten is what overwrote a lost
  conversation, S3); the home config's may be, because a missing file and an
  empty file are the same defaults and only the complaint carries news (C3). Two
  types, two policies, deliberately.
* **`now_secs` (`session.rs:129`) and `now_millis` (`workspace.rs:1602`)**: the
  same `SystemTime::now().duration_since(UNIX_EPOCH)…unwrap_or(0)` in five
  lines twice. Shareable as `epoch_millis()` (`now_secs` = `millis / 1000`),
  nets ~2 lines, and changes nothing a reader can see. Leave it or fold it; it
  is not a finding.
* **`session_save::FLUSH_DEADLINE` (10 s) and `attach::ASK_TIMEOUT`/
  `IDLE_TIMEOUT` (30 s)**: three timeouts, three reasons, each stated beside
  its code. Same number-as-word smell, different facts; keep.

## The first thing I would do

The cheap divergences, in one commit, before any extraction: route `--context`
through `config::parse_context(value, "--context")` so the flag and the
environment read one number one way (§5), and give `.mush/paste/` one spelling
so the two write failures stop naming a file that is not there (§10). Six
production lines, three live disagreements between two roads that should agree,
and each one is the kind of contradiction that costs a human an hour and a
reader their trust in the code. The first *structural* one after that is
`backup_name` (§3): twenty lines, two files, one bound.

## Method

* Every production line of the fifteen files was read; test modules were read
  only as evidence of what a message must keep saying, never counted. The
  production boundary is the `#[cfg(test)]` line per file, except
  `session_save.rs`, where the first one is a `use` and the module comes later.
* Spans are from `grep -n`, a normalised `sort | uniq -c` over production lines
  with identifiers, literals and digits stripped (which is how the two clipboard
  poll loops and the two backup loops surfaced), and a read of each site's exact
  range before quoting.
* Nothing was built, run or changed; every claim about behaviour is a claim about
  the code as written.
* What reading alone could not settle: (a) whether the derived encoders of §1
  pass `every_op_round_trips_through_its_line` and
  `a_response_is_one_line_and_round_trips` — the derives can be checked only by
  running them, and the `ok: null` caveat is exactly the case a
  `#[serde(flatten)]` would break; (b) whether `--context`'s missing `.trim()`
  is deliberate — the brief's own rule says an asymmetry between two roads for
  one value is a defect, but no comment in either file states an intent for the
  flag's spelling; (c) whether `.mush/paste/<name>` in the two error messages is
  intentionally relative to `.mush/` — no test asserts the message text.
