# The app and its panes: one shape said twice

A duplication pass over the app's state machine and its panes — production code
only, in `crates/mush/src/app/{mod,tree,screen,keys,commands,settings}.rs` and
`crates/mush/src/input.rs` (9,228 lines of production source, **4,800** of them
code; the rest is comment and blank, and the test modules were not read).
Nothing here is a doc or audit finding: every candidate comes from the code,
including its own doc comments where those state the intent the code then spells
twice.

**Counts.** "Sites" are `file:line` spans; **net** is `(code lines at the
sites) − (code lines at the sites after) − (code lines of the primitive)`,
counting code lines only — a doc comment that moves with the primitive is not
counted, so an entry whose net looks low may still be worth doing for the prose
it puts in one place. A net of ≤5 is a rounding error and is listed under
*below the line* rather than argued.

## Ranked candidates

| # | candidate | sites | net | correctness risk if left | effort |
|---|---|---|---|---|---|
| 1 | the two-step warning: armed, armed-again, kept true, disarmed — twice | `app/mod.rs:4340-4352,4357-4369,4420-4458,4460-4509,2691` | **+29** | **drift:** the warning *kinds* are hand-written at `2691` while two functions each know one; the "keep the clock" rule exists twice | medium |
| 2 | the image box weighed by two doors (`attach_image`, `attach_images`, and `deliver`'s send gate) | `app/mod.rs:3990-3999,4013-4039,4055-4066,4082-4116,4182-4190,4213-4219,4223-4275` | **+18** | **drift:** the three bounds (window, box bytes, room) are spelled twice; one picture over the window gets one explanation alone, another in a paste of four | medium |
| 3 | three `AgentNode` constructors, one field list | `app/tree.rs:730-747,826-843,857-880` | **+12** | low (literals are exhaustive — a new field fails to compile); hazard is field *order*, not drift | low |
| 4 | four picker openers, one struct literal | `app/mod.rs:3097-3133,3142-3173,3175-3189,3191-3208` | **+12** | low (compiler-guarded); the cursor's default row is derived twice | low |
| 5 | the help page's two columns | `app/keys.rs:308-339`, `app/commands.rs:185-214` | **+10** | drift: `4 + w + 2` and the continuation indent are two spellings; only visible when a page wraps | low |
| 6 | "the run may not be labelled": the busy/fold guard | `app/tree.rs:968-990,1003-1016,1250-1258` | **+9** | low-medium: a new setter that forgets "only a run in flight" or "a fold is never replaced" is a lie on a row | low |
| 7 | the three dim footer lines | `app/screen.rs:1074-1082,1088-1093,1098-1107` | **+9** | minor drift: `-2`, `-2`, `-14`, `-6` are hand-counts of the label beside them | low |
| 8 | the refusal road of `deliver` | `app/mod.rs:2391-2396,2404-2412,2428-2436,2483-2487,2497-2501,2513-2519` | **+8** | low: six sites say-and-return by hand; a road that forgets `fail` refuses silently | low |
| 9 | the bar's row budget is derived three times | `app/mod.rs:456-459,486-491,495-502,505-512` | **+6** | drift, already present in prose: `JOB_TITLE_COLUMNS` (30) says it is "the same bound as an agent's title", and `tree::TITLE_COLUMNS` is 24 | low |
| 10 | a host change's ack, and a model change's ack | `app/mod.rs:2923-2948,3066-3068,3214-3234,3315-3317` | **+6** | drift: the `" · {context}"` tail and the optional no-key clause exist twice, and the model ack string twice | low |
| 11 | a phase and its clock, written as a pair in eleven places | `app/tree.rs:938-939,985-986,1013-1014,1034-1035,1076-1077,1104-1105,1118-1119,1131-1132,1158-1159,1202-1211,1260-1261` | **+5** | **live drift:** `nudge_failed` (`1270-1274`) puts the phase back without its clock | low |
| 12 | three endings of a run, one body | `app/tree.rs:1101-1139` | **+4** | low-medium: an ending that forgets `result_unread` silently drops the `✉` a parent's `wait`/fold reads | low |
| 13 | the wire's "no agent #N", spelled three times | `app/mod.rs:2724-2726,2837-2840,2857-2859` | **−5** | low: a client can learn two spellings of one refusal; the helper is a wording guard, not a saving | low |
| 14 | below the line (see its section for the sites) | 5 spans | **+5** | low: three of them are zero-net consistency moves | low |
| 15 | the message box: the ask and the paint are two sums | `app/screen.rs:144-147` vs `:646-649` | **−7** | **drift:** the invariant is verbal only; the box can be granted fewer rows than it paints | medium |
| 16 | the chat column's split, twice | `app/screen.rs:425-427` vs `:624-625` | **−5** | **drift:** raise `Min(3)` in one and the zen view hands the box rows the two-pane layout does not | low |
| 17 | a share of the terminal, two integer types | `app/screen.rs:77-79` vs `:94-98` | **0** | **real, narrow:** `terminal_width * 60` overflows `u16` above 1092 columns — a debug-build panic where the other spelling uses `u32` | low |
| 18 | the reap window and the park window, two populations | `app/tree.rs:1308-1312` vs `:1443-1452` | **−4** | **drift:** the doc says "one arithmetic"; the reaper counts droppable children, the parker counts all of them | medium |

Positive nets sum to **+133**; the four that spend lines cost **−21**; the pass's
net is **≈ +112 code lines** (2.3 % of the 4,800): `app/mod.rs` ≈ +75,
`app/tree.rs` ≈ +26, `app/keys.rs` + `app/commands.rs` +10, `app/settings.rs`
+4, `app/screen.rs` ≈ −3.

---

## 1. The two-step warning: armed, armed-again, kept true, disarmed — twice

`crates/mush/src/app/mod.rs:4420-4433` and `:4440-4453`:

```rust
fn arm_quit(&mut self, kills: &[String]) {                 fn arm_new_chat(&mut self, lines: usize) {
    let set_at = if self.quit_armed() {                        let set_at = if self.new_chat_armed() {
        self.status.as_ref().map(|status| status.set_at)           self.status.as_ref().map(|status| status.set_at)
    } else {                                                   } else {
        None                                                       None
    };                                                         };
    self.set_status(StatusKind::Quit,                          self.set_status(StatusKind::NewChat,
        quit_warning(kills, QUIT_LINE_COLUMNS));                   new_chat_warning(lines));
    if let (Some(at), Some(status)) = (set_at, self.status.as_mut()) {
        status.set_at = at;                                    // the same three lines, again
    }
}                                                          }
```

`:4357-4369` (the disarm pair) and `:4460-4509` (the refresh pair) are the same
shape a third and fourth time: guard on *armed*, derive the line, clear the
status when there is nothing left, re-arm when the line has moved — the second
function's own doc says "`Self::arm_quit`'s shape, for the same reason" and
"`Self::refresh_quit_warning`'s shape, clock included". The two rules the pairs
encode are real and must not drift: a warning keeps the clock it already had (a
tick that recomposes it cannot extend the arming), and it is derived from the
line rather than stored beside it.

The **third** spelling of the same class is at `:2691`, where the attach road
puts a warning back:

```rust
.filter(|status| matches!(status.kind, StatusKind::Quit | StatusKind::NewChat));
```

**Shared shape.** A two-step key's warning: which kind is armed (`armed`), the
line built from the tree or the conversation, the arming that preserves an
existing clock, the tick that keeps it true, and the key that takes it back.

**Primitive.**

```rust
/// Whether a two-step key is waiting for its second press: the bar's line is
/// that key's warning, inside the line's own life ([`INFO_TTL`]), so nothing
/// can leave an arm standing with no notice to show for it (findings H9, C4).
fn armed(&self, kind: StatusKind) -> bool;

/// Arm a two-step key's warning, keeping the clock of one already standing, so
/// a tick that recomposes the line cannot extend the arming (findings H9, C4).
fn arm_warning(&mut self, kind: StatusKind, text: String);

/// Keep an armed warning true to the facts it names: `now` is the line they
/// make on this tick, and `None` ends the arm — a warning nobody can see is not
/// a warning (findings H9, C4).
fn refresh_warning(&mut self, kind: StatusKind, now: Option<String>);

/// Take back a two-step key's warning: the human did something other than press
/// it again, so the line goes with the moment it was said (findings H9, C4).
fn disarm(&mut self, kind: StatusKind);
```

plus, on the enum, `StatusKind::waits_for_a_second_press(self) -> bool` (the set
`2691` writes by hand). **Removes** 76 site lines → 16, and 31 lines of
primitive: **+29**, and the "is this kind a warning" question gets one home.

## 2. The image box, weighed by two doors

Three doors weigh the same box: `deliver` at the send (`:2404-2412`, asking the
attach gate again at the wire), `attach_image` for one picture, `attach_images`
for a paste — and the batch door *calls* the single door for a batch of one
(`:3986-3988`), which is the code saying the two must agree.

`crates/mush/src/app/mod.rs:4013` and `:4223` — the same line, 210 lines apart:

```rust
let room = budget.saturating_sub(self.chat.used_weight_for(target, budget));
```

The sums, twice (`:4014-4019` is the batch's; `:4213-4219` the single door's):

```rust
let pending: usize = self                                       let pending: usize = self
    .chat                                                           .chat
    .attachments()                                                  .attachments()
    .iter()                                                         .iter()
    .map(Image::weight)                                             .map(Image::weight)
    .fold(0, usize::saturating_add);                                .fold(0, usize::saturating_add);
```

and the same job for bytes at `:4055-4064` (two sums) and `:4254-4259`. Then the
batch counts "how many of these cross the bound, and the first one's path" —
**twice, in the same function**, for two different bounds (`:4021-4039` against
the whole budget, `:4082-4098` against the room):

```rust
let mut running = pending;                        let mut running = pending;
let mut over_budget = 0usize;                     let mut at_stake = 0usize;
let mut first_over: Option<String> = None;        let mut first_at_stake: Option<String> = None;
for image in &images {                            for image in &images {
    let cost = image.weight();                        let cost = image.weight();
    if cost.saturating_add(running) > budget {        if cost.saturating_add(running) > room {
        over_budget += 1;                                 at_stake += 1;
        if first_over.is_none() {                         if first_at_stake.is_none() {
            first_over = Some(image.path.clone());            first_at_stake = Some(image.path.clone());
        }                                                 }
    }                                                 }
    running = running.saturating_add(cost);           running = running.saturating_add(cost);
}                                                 }
```

**Where it has already drifted.** The verdicts agree today; the sentences do
not. `attach_image` distinguishes a picture *bigger than the whole budget*
(`:4225-4233`, "even with every older turn dropped…") from one that only crosses
it with the box counted (`:4234-4242`); the batch door has no such branch and
says "the pictures already in the box weigh {pending}" for both (`:4041-4050`).
One 10 MB screenshot pasted alone and pasted with three others gets two
different explanations of one refusal. And a fourth spelling of the same gate
sits in `deliver` (`:2391-2412`), which must refuse the same model the attach
doors refuse — the string `"no model yet — /model picks one, /url points mush at
an endpoint"` is written out **three times** (`:2394`, `:3992`, `:4184`).

**Primitive.**

```rust
/// What a set of pictures weighs in the two units the box is bounded in — the
/// window's weight (pixels where a header named them, bytes where it did not)
/// and the bytes the box holds while they wait — summed the way every bound
/// sums them (saturating: a header can claim a picture larger than any
/// `usize`).
struct Held { weight: usize, bytes: usize }          // + Held::of(&[Image]), Held::plus(image)

/// The pictures of a batch that cross `bound`, in the order the box would take
/// them: how many, and the first one's path for the line that names it. The
/// running sum is the pictures before each one, so a paste of four is asked the
/// question `attach_image` asks of one, four times.
fn at_stake(images: &[Image], from: Held, bound: usize) -> (usize, Option<String>);
```

with `const NO_MODEL_YET: &str` for the three-way literal, and — if the gate
itself is wanted in one place — `fn verdict(image: &Image, running: Held, budget:
usize, room: usize) -> Verdict` (`Fits | OverWindow | OverBytes | Tight`), which
costs ~5 more than it saves and is worth it only for the invariant. **Removes**
(without `verdict`): 54 site lines → 8, 28 lines of primitive: **+18**.

## 3. Three `AgentNode` constructors, one field list

`crates/mush/src/app/tree.rs:730-747` (the root), `:826-843` (a spawn) and
`:857-880` (a restore) are the same fifteen fields three times, in three
different orders, differing in a handful:

```rust
tree.agents.push(AgentNode {            self.agents.push(AgentNode {        self.agents.push(AgentNode {
    id: AgentId::ROOT,                      id: spawn.id,                       id: node.id,
    parent: None,                           parent: Some(spawn.parent),         parent: node.parent,
    depth: 0,                               depth: spawn.depth,                 depth: node.depth,
    brief: "you (root agent)"…,             brief: spawn.brief,                 brief: node.brief,
    phase: Phase::Idle,                     phase: Phase::Thinking,             title: node.title,
    since: Instant::now(),                  since: Instant::now(),              phase: node.phase,
    branch: None, fork: None,               branch: spawn.branch,               since: Instant::now(),
    summary: None, leftover: false,         fork: spawn.fork,                   branch: node.branch,
    title: None, result_unread: false,      summary: None, leftover: false,     fork: node.fork, …
    landed: None, kept: None,               landed: None, kept: None,           result_unread: node.result_unread,
});                                         result_unread: false,           });
                                        });
```

**Shared shape.** "A node that has not run": idle, no branch, no fork, no
summary, nothing swept, no result owed a read, the clock started now.

**Primitive.**

```rust
/// A node that has not run: idle, nothing swept, no result owed a read, from
/// this instant — the fields a constructor does not state, stated once, so the
/// root, a spawn and a restore cannot differ about what "has not run" means.
fn blank(id: AgentId, parent: Option<AgentId>, depth: usize, brief: String) -> AgentNode;
```

The root is then one line, the spawn three fields over `blank`, the restore
eight. **Removes** 47 site lines → ~17, 17 lines of primitive: **+12** (the
doc comments that move with it are another six).

## 4. Four picker openers, one struct literal

`crates/mush/src/app/mod.rs:3125-3129`, `:3154-3158`, `:3184-3188`,
`:3203-3207`; and the row literal at `:3119-3122`, `:3159-3163`, `:3179-3183`,
`:3194-3197`:

```rust
self.picker = Some(Picker {              self.picker = Some(Picker {
    kind: PickerKind::Model,                 kind: PickerKind::Provider,
    items,                                   items,
    cursor,                                  cursor,
});                                      });
```

with `.map(|row| PickerItem { id: None, label: row })` twice and the same
"open on the current value, else the top" rule twice —
`.position(|model| model.id == self.cfg().model)` (`:3106-3110`) against
`.position(|item| item.id.as_deref() == Some(…))` (`:3199-3202`), two different
key extractors for one rule.

**Primitive.**

```rust
/// The one door a picker opens through: the four lists differ in their rows and
/// in where the cursor starts — the value in use, the head of the newest note,
/// the top — and in nothing else.
fn open_picker(&mut self, kind: PickerKind, items: Vec<PickerItem>, cursor: usize);

impl PickerItem {
    /// A row a human can choose, from its machine value and its label.
    fn choice(id: impl Into<String>, label: impl Into<String>) -> Self;
    /// A row that is only a reading (`Notes`, `Help`): no value behind it, so
    /// `Enter` and `Esc` do the same thing.
    fn reading(label: impl Into<String>) -> Self;
}
```

**Removes** 20 + 16 site lines → 8, ~16 lines of primitive: **+12**. Risk is
low — the compiler already refuses a literal missing a field — so this is
tidiness with a real payoff at the call sites.

## 5. The help page's two columns

`crates/mush/src/app/keys.rs:313-338` and `crates/mush/src/app/commands.rs:197-213`
are one arithmetic twice: measure the left column, reserve the indent and the
gutter, wrap the description into what is left, and hang every continuation
under the description column.

```rust
// keys.rs                                  // commands.rs
let description_column = 4 + key_width + 2;  let description_column = 4 + usage_width + 2;
let room = width.saturating_sub(description_column).max(1);
let mut out = String::new();
    let lead = format!("    {:<key_width$}  ", binding.keys);
    let mut wrapped = wrap_text(binding.help, room).into_iter();
    if let Some(first) = wrapped.next() { out.push_str(&lead); out.push_str(&first); out.push('\n'); }
    for continuation in wrapped {
        out.push_str(&format!("{:description_column$}{continuation}\n", ""));
    }
out.trim_end().to_string()
```

Both callers already share `mush_core::text::wrap_text`; the columns around it
are what is written twice. The drift is quiet: an indent changed for the popup
(`/help`) would not touch `mush --help`'s unwrapped form, so the two surfaces
would disagree only where a description wraps.

**Primitive** (next to `wrap_text`, which both already import):

```rust
/// A two-column page, aligned and wrapped: the left column holds its width, and
/// a right column too long for the surface hangs under its own column rather
/// than under the left one, so a popup no wider than a phone does not read as a
/// broken page (finding U15). A row with no description is a heading.
fn columns(rows: &[(String, Option<&str>)], width: usize) -> String;
```

**Removes** 60 site lines → ~24, 26 lines of primitive: **+10**.

## 6. "The run may not be labelled": the busy/fold guard

`crates/mush/src/app/tree.rs:978-987` and `:1006-1015` are the same eight lines,
and `nudge` at `:1250-1258` asks a third version of it:

```rust
if !self.is_busy(id) {                                    if !self.is_busy(id) {
    return;                                                   return;
}                                                         }
if let Some(node) = self.node_mut(id) {                   if let Some(node) = self.node_mut(id) {
    if node.phase.compacting().is_some() {                    if node.phase.compacting().is_some() {
        return;                                                   return;
    }                                                         }
    node.phase = Phase::Activity(label.into());               node.phase = Phase::Thinking;
    node.since = Instant::now();                              node.since = Instant::now();
}                                                         }
```

**Primitive.** `fn folding(&self, id: AgentId) -> bool` — "One question, asked
before a setter takes the node, because a fold is never replaced by a label: the
request the human is waiting for outranks the ones the run keeps announcing."
**Removes** 27 site lines → ~11, 7 lines of primitive: **+9**.

## 7. The three dim footer lines

`crates/mush/src/app/screen.rs:1077-1079`, `:1090`, `:1101-1103`:

```rust
lines.push(Line::from(Span::styled(     lines.push(Line::from(Span::styled(     lines.push(Line::from(Span::styled(
    format!(                                format!(" {}",                         format!(
        " {}",                                  truncate(&unread,                       " {} jobs · {}",
        truncate(&detail.join(" · "),           width.saturating_sub(2))),              jobs.len(),
                 width.saturating_sub(2))),  dim(),                                    truncate(&jobs.join(" · "),
    dim(),                                  )));                                             width.saturating_sub(14))
)));                                                                                   dim(),
                                                                                       )));
```

**Shared shape.** "A line the pane cuts to the columns it has": the reserve
(`2`, `2`, `14`, and `6` for the id line at `:1059-1063`) is a hand-count of the
label painted beside the text, so the two spellings of one width are the label
and the number.

**Primitive.** `fn footer_line(label: String, text: &str, width: usize) -> Line<'static>`
— "One footer line, cut to the columns the pane really has: the label is
measured rather than counted by hand beside it." **Removes** 25 site lines → ~10,
4 lines of primitive: **+9**.

## 8. The refusal road of `deliver`

`crates/mush/src/app/mod.rs:2393-2395` and five more (`:2409-2410`,
`:2433-2434`, `:2484-2485`, `:2498-2499`, `:2516-2517`):

```rust
self.fail(line);            self.fail(&line);          self.fail(&line);
return Err(line.to_string());   return Err(line);          return Err(line);
```

**Primitive.** `fn refuse(&mut self, line: String) -> Result<(), String>` — "A
message that did not land, said once: the bar gets the reason and the caller
gets the `Err`, so no road can refuse invisible or refuse twice." **Removes** 30
site lines → ~12, 3 lines of primitive: **+8**.

## 9. The bar's row budget, derived three times

`crates/mush/src/app/mod.rs:486-491`, `:495-502`, `:505-512`:

```rust
/// … one row at the ubiquitous 80×24 — 73 columns, the ` chat ` badge
/// and the space after it taking seven of the eighty — with a column spare.
const CURSOR_LINE_COLUMNS: usize = 72;
/// … 73 columns at the ubiquitous 80×24, less the 36 the fixed head and tail
/// take between them (finding D3).
const UNKNOWN_NAME_COLUMNS: usize = 36;
/// … One row at the ubiquitous 80×24 is 73 columns — the ` chat ` badge …
const QUIT_LINE_COLUMNS: usize = 72;
```

The same derivation ("one bar row is 73 columns, less the badge's seven, less a
column") is prose in two and arithmetic in one; the badge's width is a fact of
`ui`'s, written into three constants. And `:456-459` claims a fourth:

```rust
/// How wide a job's handle may be: the same bound as an agent's title, for the
/// same reason — a handle, whose full text is the report in the transcript.
const JOB_TITLE_COLUMNS: usize = 30;
```

while `crates/mush/src/app/tree.rs:268` has `const TITLE_COLUMNS: usize = 24` —
a bound whose comment says it is the same one as the agent title's, and is not.
The *other* pair of bounds in this file is deliberate and documented
(`CURSOR_LINE_COLUMNS` vs `TITLE_COLUMNS`: "a handle on a row" against "a
sentence on the bar"), which is why the job handle's 30 needs either that
sentence or that number.

**Primitive.**

```rust
/// One bar row at the ubiquitous 80×24: 80 columns less the ` chat ` badge and
/// the space after it, less one column of air. Every bound the bar cuts by is
/// derived from this, so a badge that grows cannot leave one of them wider than
/// the row it is painted in (findings D3, H9, H26).
const BAR_ROW_COLUMNS: usize = 72;
```

with `CURSOR_LINE_COLUMNS`, `QUIT_LINE_COLUMNS` and `UNKNOWN_NAME_COLUMNS`
derived from it. **Removes** ~16 comment/derivation lines around four constants
→ 4, 8 lines of primitive: **+6**, and the false claim goes.

## 10. A host change's ack

`crates/mush/src/app/mod.rs:2931-2946` (`/url`) and `:3222-3231`
(`/provider`):

```rust
let mut line = format!("endpoint: {} · {}",              let mut line = format!("provider: {} · {}",
    self.cfg().base_url, self.context_label());              provider.name(), self.context_label());
if forgotten {                                          if forgotten {
    line.push_str(" · ");                                   line.push_str(" · ");
    line.push_str(&self.no_key_hint());                     line.push_str(&self.no_key_hint());
}                                                       }
self.say(line);                                         self.say(line);
```

and the model ack is the same string twice — `:3066-3068` (a discovered model)
and `:3315-3317` (a picked one): `self.say(format!("model: {} · {}", self.cfg().label(), self.context_label()))`.
`no_key_hint`'s own doc already says it exists so "the two cannot describe the
same loss two ways"; the sentence *around* it is still two.

**Primitive.** `fn host_line(&self, head: String, forgotten: bool) -> String` —
"The acknowledgement a host change earns, in one spelling: what moved, the
window that came with it, and — when the new host took the key with it — where a
new one goes (finding C6)." **Removes** 31 site lines → ~19, ~6 lines of
primitive: **+6**.

## 11. A phase and its clock, written as a pair

`crates/mush/src/app/tree.rs:1034-1035`, `:1260-1261`, and nine more
(`:938-939`, `:985-986`, `:1013-1014`, `:1076-1077`, `:1104-1105`, `:1118-1119`,
`:1131-1132`, `:1158-1159`, `:1202-1211`):

```rust
node.phase = Phase::Compacting(kind);      node.phase = Phase::Thinking;
node.since = Instant::now();               node.since = Instant::now();
```

**Live drift.** `nudge` sets the pair; `nudge_failed` (`:1270-1274`) restores
only the phase —

```rust
pub fn nudge_failed(&mut self, id: AgentId, was: Option<Phase>) {
    if let (Some(node), Some(was)) = (self.node_mut(id), was) {
        node.phase = was;                                    // and its clock is the nudge's
    }
}
```

— against its own doc ("Put the row back exactly as it was"). A row that said
`waiting on results 4m` when a nudge was attempted says `waiting on results 0s`
after the delivery failed: the age is the failed nudge's, not the phase's. Two
lines fix it, but only if the saved value carries the clock, which is the
argument for the pair being the primitive.

**Primitive** (on the node, so a caller holding the borrow keeps it):

```rust
impl AgentNode {
    /// Enter a phase, clock included: the instant a row's age is measured from
    /// is the instant it entered what it says it is doing, so the two are one
    /// write — and a phase put back without its clock is an age that lies.
    fn enter(&mut self, phase: Phase);
}
```

with `AgentTree::nudge` returning `Option<(Phase, Instant)>`. **Removes** 21
site lines → 11, 5 + 2 lines of primitive and its fix: **+5**.

## 12. Three endings of a run, one body

`crates/mush/src/app/tree.rs:1101-1110`, `:1115-1123`, `:1128-1139`:

```rust
pub fn finish(&mut self, id, summary) {     pub fn fail(&mut self, id, error) {     pub fn stopped(&mut self, id) {
    if let Some(node) = self.node_mut(id) {     if let Some(node) = self.node_mut(id) {    if let Some(node) = self.node_mut(id) {
        let unread = node.parent.is_some();        let unread = node.parent.is_some();       let unread = node.parent.is_some();
        node.phase = Phase::Done;                  node.phase = Phase::Failed(error);        node.phase = Phase::Stopped;
        node.since = Instant::now();               node.since = Instant::now();              node.since = Instant::now();
        node.summary = summary;                                                              node.result_unread = unread;
        node.result_unread = unread;               node.result_unread = unread;          }
    }                                          }                                           self.agent_cancel.remove(&id);
    self.agent_cancel.remove(&id);                 self.agent_cancel.remove(&id);     }
}
```

**Primitive.** `fn end(&mut self, phase: Phase)` on `AgentNode` — "End a run:
the phase it left behind from this instant, and the two facts every ending
shares — a child's result is owed a read, and nothing from the run before it is
still being announced (findings B14, H4)." **Removes** 25 site lines → ~16, 5
lines (over §11's `enter`): **+4**.

## 13. The wire's "no agent #N", spelled three times

`crates/mush/src/app/mod.rs:2724-2726` and `:2857-2859` (and the bare error line
at `:2839`):

```rust
let id = AgentId(agent);
if !self.tree.has(id) {
    return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent {id}")));
}
```

**Primitive.** `fn no_agent(id: AgentId) -> attach::Reply` — "The one line the
wire says about an agent the tree does not hold, so a client cannot learn two
spellings of the same refusal (finding A1)." The membership check keeps its
three lines, so this **costs** the helper's five: **−5**. It is in the table
because the refusal is a contract three arms write out, not because the lines
are there.

## 14. Below the line

Each of these nets ≤5; they are candidates of the same kind, listed for
completeness, and three of them are zero-net consistency moves rather than
savings — worth doing only when the file is being touched anyway.

- **`app/settings.rs:124-135` vs `:188-194`** — the `believable` → `adopt_context`
  sequence run by both faces of the config cell. `fn adopt_believable(cfg: &mut
  Config, tokens: usize, source: WindowSource) -> bool` — "The one sequence both
  faces of the cell run, so the UI cannot accept a number an actor refused."
  **+4.**
- **`app/mod.rs:4791-4805`** — the picker's cursor is clamped by `move_picker`
  (`clamp(0, last)`) and by `set_picker_cursor` (`min(len-1)`), while the tree
  clamps its own (`tree.rs`'s `cursor_top`/`cursor_bottom`/`move_cursor`).
  `Picker::set_cursor(&mut self, row: usize)` — "The picker's only cursor setter,
  so `j`/`k`, `g`/`G` and a page cannot clamp against two different ends."
  **+2.**
- **`app/mod.rs:4734-4736` vs `:4744-4746`** — `tree_walk` and `move_tree_cursor`
  each write `for _ in 0..steps.abs() { self.tree.move_cursor(steps.signum()); }`
  because the tree's only cursor setter moves a sign.
  `fn walk_rows(&mut self, steps: i64)` — "One row per call is the tree's own
  rule, so every walk over it takes the same steps." **0**: `move_tree_cursor`
  collapses into a call to it, so the two 3-line loops become one call site and
  one 4-line function.
- **`app/mod.rs:1693-1695` vs `:3604-3606`** — the same two lines that pick a
  writer's error up. `fn take_save_error(&mut self)` — "A write that failed on
  the writer's thread has no caller to return to, so both the tick and a flush
  say it the same way." **0.**
- **`app/mod.rs:179-187`** — `Picker::title` spells `line {}/{}` with
  `self.cursor + 1, self.items.len()` in two arms. `fn position(&self) -> String`
  — "The 1-based row a list of this size is on, once." **−1**; left alone.
- **`app/mod.rs:1544,1567,1595`** — three `Msg` arms drop a message whose
  conversation has moved on. Not worth a helper (each is one `if`, and the
  `Msg::Models` arm stamps an endpoint instead); noted so it is not reinvented.

That is +4 + 2 + 0 + 0 − 1 = **+5** for this section, which is the table's row
14.

## 15. The message box: the ask and the paint are two sums

`crates/mush/src/app/screen.rs:646-649` (the ask) and `:144-146` (the paint):

```rust
pub(super) fn input_rows(&self) -> u16 {                  fn content_rows(field_height: u16, images: &[Image]) -> (Vec<String>, usize) {
    let input_lines = (self.chat.input().line_count()     let attachments = attachment_rows(images,
        as u16).clamp(1, MAX_INPUT_LINES);                    (field_height as usize).saturating_sub(1));
    let attachment_count = self.chat.attachments()        let text_rows = (field_height as usize)
        .len().min(MAX_ATTACHMENT_ROWS) as u16;               .saturating_sub(attachments.len());
    input_lines + attachment_count + 2                    (attachments, text_rows)
}                                                     }
```

One sum: the draft's lines (capped), the attachment rows (capped) and the box's
**two** border rows — and the paint reads the same sum from the other end with a
different constant: the `+ 2` (borders) lives only in the ask and the
one-row-kept-for-the-text only in `content_rows`. The docs on both sides say
"the two agree whenever the ask is granted", which is exactly the hand-kept
invariant this pass is looking for. Change the `+ 2` to `+ 1` and a box granted
`draft + attachments − 1` rows paints `draft + attachments − 2`: one draft line
and one image come out as a box with no attachment row while the title still
counts the image.

**Primitive.**

```rust
/// The rows the message box is made of, from one place — the draft's lines, the
/// attachment rows and the two the border charges: the ask and the paint are the
/// same sum read from the two ends, so a short column cannot hand the box fewer
/// rows than it paints (finding D5).
fn box_rows(draft_lines: usize, attached: usize, granted: Option<u16>) -> (usize, usize);
```

This one **costs** lines — 8 site lines → 2, ~13 lines of primitive: **−7** —
and is worth them for the invariant, not the line count.

## 16. The chat column's split, twice

`crates/mush/src/app/screen.rs:425-427` (the zen view) and `:624-625`
(`chat_pane`):

```rust
// … "its own split, not a re-derivation" …
let split = Layout::vertical([Constraint::Min(3),      let rows = Layout::vertical([Constraint::Min(3),
    Constraint::Length(self.input_rows())])                Constraint::Length(self.input_rows())])
    .split(chat_area);                                     .split(area);
```

The box's rows do come from one place (`input_rows`); the *split* — the
transcript's floor of three rows against the box's ask — does not, and the
comment above 425 claims it does. Raise the floor in one and the zen view hands
the box rows the two-pane layout does not, which moves the box when the tree
takes the screen.

**Primitive.** `fn chat_split(&self, area: Rect) -> (Rect, Rect)` — "The chat
column's split — the transcript's floor and the box's ask — in one place, so the
zen view cannot hand the box rows the two-pane layout does not." **Costs** 5
site lines → 2, 8 lines of primitive: **−5**.

## 17. A share of the terminal, two integer types

`crates/mush/src/app/screen.rs:77-79` and `:94-98`:

```rust
fn picker_width(terminal_width: u16) -> u16 {           fn agents_columns(terminal_width: u16) -> u16 {
    (terminal_width * 60 / 100)                              let share = (terminal_width as u32 * 34 / 100) as u16;
        .clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH)           share
}                                                            .clamp(AGENTS_MIN_COLUMNS, AGENTS_MAX_COLUMNS)
                                                             .min(terminal_width.saturating_sub(CHAT_MIN_COLUMNS))
                                                        }
```

One formula — a percentage of the terminal, clamped — with two integer types.
The `u16` one is a real defect, not a style: `terminal_width * 60` overflows
`u16` above 1092 columns (a debug build panics on the multiply; a release build
wraps and the clamp then quietly yanks the popup to 40 columns on the widest
terminal). A 1093-column terminal is rare, and that is the whole defence of the
line as written.

**Primitive.** `fn share(whole: u16, percent: u32, min: u16, max: u16) -> u16` —
"A clamped share of the terminal, computed in the wider integer, so a formula
copied to a second pane cannot overflow where the first did not." The two sites
lose four lines and the primitive costs three, so this is **line-neutral**: it is
a bug fix, not a saving (and the overflow is a two-character fix without the
primitive).

## 18. The reap window and the park window count different populations

`crates/mush/src/app/tree.rs:1308-1312` (the reaper) and `:1443-1452` (the
parker):

```rust
// past_history: the children it may drop                     // parkable: "the warm window and the reap window are one arithmetic"
let mut eligible: Vec<AgentId> = self.agents.iter()           let children: Vec<&AgentNode> = self.agents.iter()
    .filter(|node| self.reapable(node, &kept_above, jobs_live))   .filter(|node| node.parent.is_some())
    .map(|node| node.id).collect();                               .collect();
let over = eligible.len().saturating_sub(CHILD_HISTORY);      let warm = children.len().saturating_sub(WARM_CHILDREN);
eligible.truncate(over);                                      let window = children.len().saturating_sub(CHILD_HISTORY);
```

`eligible` is the children the tree *may* drop — a child whose result nobody has
read does not spend one of the [`CHILD_HISTORY`] slots (`past_history`'s doc
says so). `children` is every parented node. So the two windows are one number
and two populations, and they part company as soon as one kept child exists:
with fifty-one children and one unread result, the reaper's `over` is 0 (it
drops nothing, the unread result being outside its count) while the parker's
`window` is 1 — so a child ranked in the band between them is asked
`past_window = true`, `result_unread` stops protecting its thread, and the actor
whose completion its parent's model has not been handed yet is parked. The
wake path (`deliver_to_actor` through `ChildAsleep`) recovers the message, so the
cost is a promise broken rather than a line lost — but the promise is written
into `may_park`'s comment.

**Primitive.** The reaper's rank, offered once —
`fn past(len: usize, cap: usize) -> usize { len.saturating_sub(cap) }` — fed the
same population by both, or `parkable` taking the reaper's list. **Costs** ~4
lines and is kept for the invariant: **−4**.

---

## Looks duplicated but is not

- **`keys::picker` / `keys::tree` / `keys::chat`'s movement arms** (`keys.rs:537-620`):
  the same `j/k, g/G, PgUp/PgDn` rows three times, mapping to three different
  `Intent` families. Merging them would need one Intent carrying the pane, and
  the module's stated invariant — no key is read twice — is easiest to see with
  one arm per pane. Keep.
- **`screen::picker_width` / `picker_text_width`** (`screen.rs:77-88`): the same
  derivation nested on purpose (the −6 is the border, the `› ` and the indent).
  This is the pattern the other candidates should copy, not a duplicate.
- **`phase_glyph` / `phase_detail`** (`screen.rs:1217`, `:1254`): one phase
  partition, two deliberate vocabularies (`⊘` against `stopped · re-send to
  resume`); both matches are exhaustive, so a new phase fails to compile.
- **`Msg::Clipboard` / `Msg::Copied` / `Msg::Agent`'s conversation guard**
  (`mod.rs:1544`, `:1567`, `:1595`): the same expression at three different
  boundaries — and `Msg::Models` guards an *endpoint* instead, which a shared
  helper would hide.
- **The re-asks of a stale snapshot** — `adopt_git`'s `tree.has` filter
  (`mod.rs:1159-1164`), `sweep_worktrees`'s second `has` (`:1193`) and the
  sweep worker's `Msg::SweepAsk`, answered by `App::worktree_in_use`: the docs
  say why each is asked again (a remove must be one decision), so these are one
  decision asked at one moment, not two spellings.
- **`fork_base` read in `refresh_git` (`mod.rs:1117`) and again in
  `sweep_worktrees` (`:1203`)**: two moments, and both call the one derivation
  (`mod.rs:1241`'s `App::fork_base`, over `agent::fork_base`), so nothing can
  drift.
- **`help_table_at`/`table_at`'s row building** (keys' contexts against the
  commands' specs): only the column arithmetic is shared (§5); the rows are two
  different lists.
- **`Input::cursor_line().1` / `Input::cursor_column()`** (`input.rs:113-131`):
  columns within the cursor's own line against columns from the start of the
  box — two questions, and `window_line` is already the one spelling of "what is
  visible" that both the box and its lines use.
- **`StatusKind::Quit` / `StatusKind::NewChat` themselves**: two variants are
  right — the point of §1 is that their *rules* are one shape, not that the
  warnings are one warning.

## The first thing I would do

**§11's clock, because it is a live lie and it is six lines.** `AgentNode::enter`
plus `nudge` returning the pair it replaced, and `nudge_failed` putting both
back: a row that said `waiting on results 4m` and then failed a nudge must not
say `0s`. It is the only finding here where the code is already wrong rather
than merely liable to become wrong, and it is the smallest change in the report.

Then §1, which is the largest measured win in one file (net +29) and removes the
last hand-written list of the warning kinds.

## Method

I read all seven files' production ranges whole — `app/mod.rs:1-4827`,
`app/screen.rs:1-1281`, `app/tree.rs:1-1786`, `app/keys.rs:1-622`,
`app/commands.rs:1-275`, `app/settings.rs:1-211`, `input.rs:1-226` — and no
docs, README or audit file; doc comments inside those files are evidence of
intent, and several candidates are the code contradicting its own comment.
`tree.rs` and `screen.rs`/`settings.rs` were read in parallel by two delegated
readers working to the same blind rule; every span they named that appears here
I re-read at the line before quoting, and their line counts were recomputed
(which is why a few of their nets are lower here: I count code lines only).

For the shapes I did not find by reading, the passes were a normalised
`sort | uniq -c` over stripped lines, a scan of every 3- and 4-line window of
each file with comments and literals stripped, and `grep -n` for the repeated
literals (`"no model yet"` ×3, `usize::saturating_add` ×5, `since =
Instant::now()` ×11, `take_error()` ×2, `bad_request(format!("no agent` ×3).
Line counts are from a script over the files as committed at `f47bfdb`; no code
was changed and nothing was built.

**Not determinable by reading alone:** whether the park/reap band (§18) is
reachable in a real session, since it needs a child count in the forties with a
kept child and an unread result at the boundary; the terminal width at which
§17's `u16` share actually overflows, which needs the size event; and which of
these are already pinned by tests, since the test modules were out of scope for
this pass.
