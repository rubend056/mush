//! The system clipboard: the image on it, read the way a human would read it,
//! and the text put on it the way a human would put it there.
//!
//! There is no portable clipboard API, and a crate that offers one is a
//! dependency mush does not want to pay for a screenshot: the programs every
//! desktop already ships carry the clipboard both ways — wayland's `wl-paste`
//! and `wl-copy`, X11's `xclip`, macOS's `pngpaste` and `pbcopy` — and shelling
//! out to them is what a human does by hand. Windows's `clip` joins them on the
//! write road alone: it takes the text on stdin and reads nothing back, so a
//! Windows paste is not a road this module carries. Which one exists is
//! discovered by trying them, because a program that is not installed fails to
//! spawn instantly and costs nothing to ask. Every one of them is started
//! without mush's secrets ([`mush_core::secrets::scrub`]): a clipboard tool
//! does not talk to the provider, and the credential is not in the environment
//! it is handed (finding C1).
//!
//! This module lives in the binary crate, not in `mush-core`: it is a
//! subprocess and the clipboard is a machine facility, and `mush-core` is
//! deliberately free of both. What travels back is a workspace [`Image`] —
//! already saved under `.mush/paste/` (`Workspace::save_pasted_image`) — so
//! the caller on the UI thread only has to attach it. What travels *in*, on the
//! write road, is a `&str` and nothing else: the text the human copied in order
//! to paste it somewhere else, put on the clipboard exactly as it was handed in.
//!
//! Nothing here may block the human's keyboard: the app calls it on a thread of
//! its own, and every command is bounded anyway, because "a clipboard tool that
//! waits for an owner that will never answer" is a hang the app's thread must
//! not inherit. `Command::output()` would be exactly that mistake: it waits for
//! exit before it reads the pipe, which deadlocks on an image past the pipe
//! buffer and hangs forever on a program that never exits. The write road
//! mirrors it, because the same hang has a second shape: the text goes to the
//! child's stdin on a thread of its own, since a program that stops reading
//! fills the pipe and a caller that wrote the text itself would block in
//! `write` on exactly the program that never came back for the read.
//!
//! Both helper threads a road needs — the drain that reads a reader's stdout and
//! the writer that fills a writer's stdin — are started through this module's
//! own [`WorkerSpawn`] road, the same contract the app's four worker roads keep
//! (finding R11's residual): a box at its thread limit gets a sentence, never a
//! panic on the worker the app started — whose answer would then never come, so
//! the key did nothing in silence. What a refusal means is decided beside each
//! use: the reader is killed, because nothing can read what it would write
//! ([`Answer::NoThread`]), and the writer is killed *before* its pipe is let go,
//! so a writer waiting for EOF cannot read a closed, empty pipe as the copy
//! ([`Delivered::NoThread`]).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mush_core::message::Image;
use mush_core::secrets::scrub;
use mush_core::workspace::{image_mime, IMAGE_FILE_CAP};
use mush_core::Workspace;

/// How long one clipboard road may take, every program on it together: the
/// readers of an image, or the writers of a text.
///
/// One deadline for the sequence rather than one per command: the human is
/// waiting at the keyboard, and nine readers that each take their own two
/// seconds is eighteen seconds of a frozen paste. The common clipboard answers
/// in milliseconds — a program that hangs is one waiting on a clipboard owner
/// that will never speak — so two seconds is generous for what is really there
/// and short enough that its absence is noticed as a hiccup rather than a hang.
const DEADLINE: Duration = Duration::from_secs(2);

/// How often the running program is checked for having exited: fine enough that
/// a fast answer costs about one poll, coarse enough that nine readers are not a
/// busy loop.
const POLL: Duration = Duration::from_millis(5);

/// The most of a reader's stdout that is ever held in memory: one byte past the
/// image cap, so a picture that is too big is *detected* as too big while its
/// bytes are still bounded. The rest of a runaway's output is drained and
/// dropped, never buffered — and a buffer that reached the cap is reported as
/// such ([`Drained::filled`]), so the refusal it earns says the picture is
/// past the cap instead of reading the buffer's length aloud as the picture's.
const READ_CAP: usize = IMAGE_FILE_CAP as usize + 1;

/// The mimes a reader is asked for, in the order it is asked for them. Png
/// first because that is what a screenshot is on every desktop that has this
/// feature at all, and the four are the ones `image_mime` recognises — asking
/// for a format mush cannot name would be a round trip for a refusal.
const MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// How this module starts the one helper thread a clipboard road needs: a named
/// `std::thread::Builder` thread, whose refusal is returned and not raised.
///
/// `std::thread::spawn` panics when the OS refuses a thread — a container's
/// `pids.max`, a `ulimit -u`, a fork storm — and that panic lands *inside* the
/// clipboard worker the app started: the send that would carry the answer never
/// runs, so `Ctrl-V` and `Ctrl-Y Enter` do nothing and say nothing. The app's
/// four worker roads answer that refusal through its own `WorkerSpawn` (finding
/// R11); this is the same road spelled a second time, because the clipboard is a
/// machine facility *below* the UI — the app calls it, never the other way — and
/// a subprocess module reaching up into the TUI module for its thread would
/// point the dependency the wrong way. One rule, two spellings:
/// `Builder::new().name(...).spawn(job).map(|_| ())`, and the `Err` is what the
/// caller turns into the sentence the human reads.
///
/// The road is a parameter of the two bodies below rather than a bare call
/// because a test has to drive a refusal without exhausting the machine's own
/// threads — the same reason the app keeps its road in a field. The production
/// road is [`spawn_worker`].
type WorkerSpawn = fn(&str, Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>;

/// The production [`WorkerSpawn`]: a named `Builder` thread, whose refusal is
/// the `Err` that `std::thread::spawn` would have panicked on.
fn spawn_worker(name: &str, job: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(job)
        .map(|_| ())
}

/// The image on the system clipboard, saved into the workspace. `Ok(None)` is
/// "the clipboard holds no image"; `Err(line)` is "it cannot be attached, and
/// this is the sentence to say".
///
/// The first reader that exits successfully with bytes wins; bytes that are not
/// an image come back as `Ok(None)` (see [`saved`]). When not one reader could
/// even be spawned, the clipboard is unreadable on this machine, and the
/// refusal names what to install rather than pretending the clipboard was
/// empty — the two facts need different actions from the human.
///
/// A drain thread the machine refused is a third refusal: the drain that reads
/// a reader's stdout is this road's own worker, and a refused spawn
/// ([`Answer::NoThread`]) ends the read with a sentence — the reader is killed,
/// because nothing can read what it would write, and the file road still works
/// — never with a panic on the app's worker, whose message would then never
/// come.
///
/// The same is true of a reader killed at the deadline: it is its own refusal
/// ([`Answer::TimedOut`]) because the picture may well be on the clipboard and
/// the reader stuck, so "the clipboard holds no image" would send the human
/// looking in the wrong place. The sentence names the reader and the wait, and
/// points at the road that always works — the picture's own file.
///
/// The bodies live in [`run_readers`] because a test has to be able to hand in
/// a reader that never answers; installing one on the machine's PATH would be
/// the test's own lie about the machine.
pub fn read_image(ws: &Workspace) -> Result<Option<Image>, String> {
    run_readers(ws, readers(), Instant::now() + DEADLINE, spawn_worker)
}

/// [`read_image`] with the readers, the deadline and the spawn road handed in:
/// the loop, the four answers that are not a picture, and the sentences each one
/// earns.
fn run_readers(
    ws: &Workspace,
    readers: Vec<(&'static str, Vec<String>)>,
    deadline: Instant,
    spawn: WorkerSpawn,
) -> Result<Option<Image>, String> {
    let mut ran = false;
    let mut stalled = None;
    for (program, args) in readers {
        if Instant::now() >= deadline {
            break;
        }
        match run(program, &args, deadline, spawn) {
            Answer::Missing => {}
            Answer::Nothing => ran = true,
            Answer::NoThread(error) => {
                // The drain could not start, and the refusal is the machine's,
                // not this program's: every reader after this one would need a
                // drain thread of its own, so the road stops here instead of
                // spending what is left of the deadline on children it cannot
                // read. The key still has a road — the path of an image file —
                // and the sentence names it rather than leaving the human with
                // a keystroke that did nothing.
                return Err(format!(
                    "could not start the clipboard read: {error} — paste the image's path instead"
                ));
            }
            Answer::TimedOut => {
                ran = true;
                stalled = Some(program);
                // The deadline is shared — one wait for the whole sequence, so
                // nine readers cannot each spend two seconds of a frozen paste
                // — so the readers after a stall have no time left to run.
                break;
            }
            Answer::Bytes(drained) => return saved(ws, drained),
        }
    }
    if !ran {
        return Err(
            "no clipboard reader on PATH — install `wl-clipboard` (wl-paste), `xclip`, or \
             macOS's `pngpaste`"
                .to_string(),
        );
    }
    if let Some(program) = stalled {
        return Err(format!(
            "`{program}` did not answer within {}s — it may be waiting on a clipboard owner that \
             never speaks. Try again, or save the picture to a file and paste its path",
            DEADLINE.as_secs()
        ));
    }
    Ok(None)
}

/// Every way this reads a clipboard, in the order it tries them: wayland's
/// `wl-paste`, then X11's `xclip` with the same four types, then macOS's
/// `pngpaste` — which takes no type because it only ever hands back an image.
///
/// The order is not a guess at the session: a program of the wrong display
/// server fails just as fast as a missing one (no `$WAYLAND_DISPLAY`, no X
/// display), so trying all of them is how this stays one code path on three
/// platforms. `--no-newline` is `wl-paste`'s, and it matters: without it the
/// tool appends a `\n` to whatever it copies, and one stray byte after a png
/// is a png the endpoint would read wrong.
fn readers() -> Vec<(&'static str, Vec<String>)> {
    let mut readers = Vec::with_capacity(MIMES.len() * 2 + 1);
    for mime in MIMES {
        readers.push((
            "wl-paste",
            vec!["--no-newline".into(), "--type".into(), mime.into()],
        ));
    }
    for mime in MIMES {
        readers.push((
            "xclip",
            vec![
                "-selection".into(),
                "clipboard".into(),
                "-t".into(),
                mime.into(),
                "-o".into(),
            ],
        ));
    }
    readers.push(("pngpaste", vec!["-".into()]));
    readers
}

/// What a reader's stdout yielded: the bytes worth keeping, and whether the cap
/// was reached.
///
/// A filled buffer means the reader had more to say and the rest was read and
/// dropped — so these bytes are the first [`READ_CAP`] of the picture and their
/// length is the buffer's, not the picture's. [`saved`] carries that fact into
/// the refusal, which must not name a size it does not know.
struct Drained {
    bytes: Vec<u8>,
    filled: bool,
}

/// What one run of one reader produced.
enum Answer {
    /// The program is not on this machine.
    Missing,
    /// It ran and handed back no image bytes: no such type on the clipboard, or
    /// an exit that failed. A reader that failed is "no image" rather than its
    /// own sentence because a failed reader can serve no picture either way, and
    /// the human's move — copy the picture again, or paste its path — is the one
    /// an empty clipboard asks for.
    Nothing,
    /// The machine refused the thread that reads this reader's stdout
    /// ([`WorkerSpawn`]): the reader is up and nothing can collect what it
    /// writes, so it was killed and this reader hands nothing over. Not
    /// [`Answer::Missing`] — the program is on the machine — and not
    /// [`Answer::TimedOut`] — no deadline was reached; the box is at its thread
    /// limit, and the caller says *that* rather than naming a reader that never
    /// had a chance to answer.
    NoThread(String),
    /// It was still running when the shared deadline arrived and was killed.
    /// That is not the same fact as [`Answer::Nothing`]: the clipboard may hold
    /// the picture and the reader may be stuck, so the caller says a reader did
    /// not answer, never that the clipboard is empty.
    TimedOut,
    /// It exited successfully with these bytes.
    Bytes(Drained),
}

/// Run one reader against `deadline`, with its stdout drained on a thread while
/// it runs.
///
/// The drain is the point: a png is larger than a pipe buffer, so a child that
/// is writing one blocks the moment the buffer is full, and a loop that waited
/// for exit before reading the pipe would wait forever on a child that is
/// waiting for the read. The reader thread runs beside the poll instead, and
/// what it keeps is bounded by [`READ_CAP`] — a runaway that pours out gigabytes
/// has the rest drained and dropped, so the memory a stuck tool can cost is a
/// constant rather than the machine's.
///
/// The drain thread goes through [`WorkerSpawn`], and a refused thread is its
/// own answer ([`Answer::NoThread`]): the reader is killed and reaped at once,
/// because its stdout can never be read — the bytes it writes are what the drain
/// exists for, and a child left running would only fill a pipe nobody drains —
/// so the caller says the machine refused a thread rather than naming a reader
/// that never had a chance to answer.
fn run(program: &str, args: &[String], deadline: Instant, spawn: WorkerSpawn) -> Answer {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match scrub(&mut command).spawn() {
        Ok(child) => child,
        Err(_) => return Answer::Missing,
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Answer::Nothing;
    };
    // The bytes come back over a channel rather than through the thread's join:
    // a join has no deadline, and a grandchild that inherited the child's stdout
    // would keep the pipe open after the child exited, holding this caller for
    // as long as the grandchild lives. The wait below is bounded like every
    // other one here; a thread left behind ends when the pipe finally closes.
    let (tx, rx) = std::sync::mpsc::channel();
    let started = spawn(
        "mush-clipboard-drain",
        Box::new(move || {
            let _ = tx.send(drain(stdout));
        }),
    );
    if let Err(error) = started {
        // Nothing can read this reader's stdout, so it is killed and reaped
        // there and then, before the poll below could spend the deadline on a
        // pipe nobody drains. The refusal is the machine's fact, and the answer
        // says so rather than reporting a reader that did not answer.
        let _ = child.kill();
        let _ = child.wait();
        return Answer::NoThread(error.to_string());
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok(drained) = rx.recv_timeout(left) else {
                    // The child exited but its bytes did not arrive in the time
                    // left: the deadline is the fact, not an empty clipboard.
                    return Answer::TimedOut;
                };
                return if status.success() && !drained.bytes.is_empty() {
                    Answer::Bytes(drained)
                } else {
                    Answer::Nothing
                };
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Answer::Nothing;
            }
        }
        if Instant::now() >= deadline {
            // Kill and reap. The reader is not awaited past this: killing the
            // child closes the pipe it is reading, and a thread that ends on its
            // own a moment later is one this caller never has to wait for. What
            // the deadline bought is its own answer ([`Answer::TimedOut`]), not
            // an empty clipboard.
            let _ = child.kill();
            let _ = child.wait();
            return Answer::TimedOut;
        }
        std::thread::sleep(POLL);
    }
}

/// Read a reader's stdout to the end, keeping at most [`READ_CAP`] bytes and
/// reporting whether that cap filled. The rest is read and dropped rather than
/// left in the pipe: a child whose output nobody reads blocks on a full pipe
/// and never exits, which would spend the whole deadline of a reader that is
/// working fine. [`Drained::filled`] is the difference between a whole picture
/// and the first `READ_CAP` bytes of one, and the refusal owes the human that
/// difference: the old code dropped the rest silently and then named the
/// buffer's length as the picture's.
fn drain(mut stdout: impl Read) -> Drained {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => {
                return Drained {
                    filled: bytes.len() == READ_CAP,
                    bytes,
                }
            }
            Ok(read) => {
                let room = READ_CAP.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buf[..read.min(room)]);
            }
        }
    }
}

/// The bytes a reader handed back, as the workspace sees them: the saved image
/// when they are one, and `Ok(None)` when they are not.
///
/// The sniff is here and not in `save_pasted_image` alone because the two
/// answers differ: bytes that are not an image mean the clipboard held
/// something else — a reader that exited successfully with words — and that is
/// "no image", not "an image that cannot ride". A picture past the cap *is* the
/// second, and its refusal sentence comes from the save, which is told whether
/// the buffer was cut at [`READ_CAP`] ([`Drained::filled`]): a picture of any
/// size past the cap then earns "past the 2 MB cap" and no size, where the old
/// road called every one of them exactly 2,097,153 bytes.
fn saved(ws: &Workspace, drained: Drained) -> Result<Option<Image>, String> {
    if image_mime(&drained.bytes).is_none() {
        return Ok(None);
    }
    ws.save_pasted_image(drained.bytes, drained.filled)
        .map(Some)
}

/// Put `text` on the system clipboard, exactly as it was handed in. `Err(line)`
/// is "it cannot be copied, and this is the sentence to say".
///
/// The first writer that takes the text wins; a writer that is not installed,
/// or that leaves without the text having landed, is passed over for the next
/// one ([`Delivered`]). When not one writer could even be spawned, the clipboard
/// cannot be written on this machine, and the refusal names what to install
/// rather than a failure the human cannot act on — the same two facts the read
/// road keeps apart.
///
/// A stdin thread the machine refused is its own fact ([`Delivered::NoThread`]):
/// the text never entered the pipe, and the sentence says the write did not
/// start rather than reading the refusal as a failed write.
///
/// Nothing is added and nothing is taken away: a multi-line text keeps its
/// newlines and tabs, and a text that ends in `\n` keeps that newline, because
/// what the human copied is what they mean to paste elsewhere. `wl-copy`'s `-n`
/// is `--trim-newline` on this road and would drop that newline, so [`writers`]
/// passes it no flags; the reader's `--no-newline` is `wl-paste`'s flag against
/// the newline *it* appends, and has no counterpart on the way in.
///
/// A writer still running when the deadline arrives is killed and earns its own
/// sentence ([`Delivered::TimedOut`]): the text may be on its way to the
/// clipboard, and "the text did not reach the clipboard" would be a different
/// fact from "nobody answered in time".
///
/// The bodies live in [`run_writers`] because a test has to be able to hand in a
/// writer that never reads its stdin; putting one on the machine's PATH would be
/// the test's own lie about the machine.
// The write road's primitive, bound by `App::write_clipboard`'s default: the
// select mode's `Enter` reaches the writers through this function. The value
// seam exists so a test can press that key without writing the human's real
// clipboard, or depending on which of the four writers the machine has on
// `PATH`.
pub fn write_text(text: &str) -> Result<(), String> {
    run_writers(writers(), text, Instant::now() + DEADLINE, spawn_worker)
}

/// [`write_text`] with the writers, the deadline and the spawn road handed in:
/// the loop, the fall-through, the refusal that stops it, and the sentences the
/// failures earn.
fn run_writers(
    writers: Vec<(&'static str, Vec<String>)>,
    text: &str,
    deadline: Instant,
    spawn: WorkerSpawn,
) -> Result<(), String> {
    // Shared with the thread that writes it, so a text handed to four writers
    // is copied once, and never once per poll.
    let text: Arc<str> = Arc::from(text);
    let mut ran = false;
    let mut stalled = None;
    for (program, args) in writers {
        if Instant::now() >= deadline {
            break;
        }
        match deliver(program, &args, &text, deadline, spawn) {
            Delivered::Missing => {}
            Delivered::Taken => return Ok(()),
            Delivered::Refused => ran = true,
            Delivered::NoThread(error) => {
                // The write thread could not start, and the refusal is the
                // machine's, not this program's: every writer after this one
                // would need a write thread of its own, so the road stops here
                // instead of starting writers it cannot feed. The text never
                // entered the pipe, and the sentence says the write did not
                // start rather than calling it a failed write.
                return Err(format!(
                    "could not start the clipboard write: {error} — the text was not copied; try \
                     again"
                ));
            }
            Delivered::TimedOut => {
                ran = true;
                stalled = Some(program);
                // The deadline is shared — one wait for the whole sequence, so
                // four writers cannot each spend two seconds of a frozen
                // keyboard — so the writers after a stall have no time left.
                break;
            }
        }
    }
    if !ran {
        return Err(
            "no clipboard writer on PATH — install `wl-clipboard` (wl-copy), `xclip`, or macOS's \
             `pbcopy`"
                .to_string(),
        );
    }
    if let Some(program) = stalled {
        return Err(format!(
            "`{program}` did not take the text within {}s — it may be waiting on a clipboard \
             owner that never speaks",
            DEADLINE.as_secs()
        ));
    }
    Err(
        "the text did not reach the clipboard — every writer on PATH exited without taking it"
            .to_string(),
    )
}

/// Every way this writes a clipboard, in the order it tries them: wayland's
/// `wl-copy`, then X11's `xclip` with the clipboard selection in, then macOS's
/// `pbcopy`, then Windows's `clip`. All four take the text on stdin, so the
/// three that have nothing else to say take no arguments at all.
///
/// The order is not a guess at the session: a program of the wrong display
/// server fails just as fast as a missing one (no `$WAYLAND_DISPLAY`, no X
/// display), so trying all of them is how this stays one code path on four
/// platforms. `wl-copy` is passed no flag where `wl-paste` is passed
/// `--no-newline`: `-n` on the write side is `--trim-newline`, which drops a
/// trailing newline the human put there ([`write_text`]). Nor is a type asked
/// for the way the readers ask for mimes: the text is written as text and the
/// program decides how that reads on the clipboard, where the reader must ask
/// because the clipboard holds whatever was last put on it.
fn writers() -> Vec<(&'static str, Vec<String>)> {
    vec![
        ("wl-copy", Vec::new()),
        (
            "xclip",
            vec!["-selection".into(), "clipboard".into(), "-i".into()],
        ),
        ("pbcopy", Vec::new()),
        // Windows's own: `clip` takes the text on stdin and sets the clipboard.
        ("clip", Vec::new()),
    ]
}

/// What one run of one writer made of the text.
enum Delivered {
    /// The program is not on this machine.
    Missing,
    /// It ran and the text did not land: it exited non-zero, or its stdin write
    /// failed — a pipe whose reader has gone takes nothing, whatever the program
    /// returned. The next writer is tried, because a writer that took no text
    /// can serve none. The evidence stops at the stdin, and the prose has to
    /// stop there too: a text that fits the pipe is handed over whether or not
    /// the program ever reads it, and no exit status can be asked about bytes
    /// already buffered.
    Refused,
    /// The machine refused the thread that would fill this writer's stdin
    /// ([`WorkerSpawn`]), so the text never entered the pipe: the writer was
    /// killed before the caller let the pipe go, and this is its own fact — not
    /// [`Delivered::Refused`], where the program ran and the text did not land,
    /// and not [`Delivered::TimedOut`], where the deadline was reached.
    NoThread(String),
    /// It was still running when the shared deadline arrived and was killed.
    /// That is not the same fact as [`Delivered::Refused`]: the text may be on
    /// its way to the clipboard and the writer merely stuck on an owner that
    /// never speaks, so the caller says the writer did not take the text in
    /// time, never that the write failed. It is the same distinction the readers
    /// draw between an empty clipboard and nobody answering.
    TimedOut,
    /// It took the text and exited successfully.
    Taken,
}

/// Run one writer against `deadline`, with the text written to its stdin on a
/// thread of its own.
///
/// The thread is the point, and it is the mirror of the reader's drain: a
/// program that stops reading fills the pipe, and a caller that wrote the text
/// itself would block in `write` on the program that never came back for the
/// read, holding the human's keyboard for a hang the deadline exists to cut.
/// The write's own result comes back over a channel rather than through the
/// thread's join — a join has no deadline, and a thread left behind by a killed
/// writer ends when its pipe finally closes — and it is what says whether the
/// text landed, because the exit status cannot: a program can exit `0` with the
/// pipe closed under it — a text past the buffer it never read — and that `0` is
/// not a copy. It is evidence about the stdin and nothing more: bytes buffered
/// in a pipe are the program's to read or to leave, and that gap is the price of
/// not blocking on a writer that may never read them.
///
/// The stdin thread goes through [`WorkerSpawn`], and a refused thread is its
/// own answer ([`Delivered::NoThread`]): the writer is killed and reaped while
/// the pipe is still held open, so a writer that waits for EOF cannot read a
/// closed, empty pipe as the copy, and a text that never entered the pipe is
/// not reported as a write that failed.
fn deliver(
    program: &str,
    args: &[String],
    text: &Arc<str>,
    deadline: Instant,
    spawn: WorkerSpawn,
) -> Delivered {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match scrub(&mut command).spawn() {
        Ok(child) => child,
        Err(_) => return Delivered::Missing,
    };
    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Delivered::Refused;
    };
    // The pipe's write end lives in a slot both sides hold: the thread takes it
    // if it starts, and the refusal below takes it back — *after* killing the
    // writer — because a spawn the OS refuses drops the job it was handed, and a
    // pipe that closed with nothing in it is an EOF a writer waiting for the
    // text (every one of them does) could read as the copy. The text itself is
    // not in the pipe on that road: the thread that would have written it never
    // ran.
    let stdin = Arc::new(Mutex::new(Some(stdin)));
    let feeding = Arc::clone(&stdin);
    let (tx, rx) = std::sync::mpsc::channel();
    let text = Arc::clone(text);
    let started = spawn(
        "mush-clipboard-stdin",
        Box::new(move || {
            let taken = feeding
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let Some(mut stdin) = taken else {
                // The refusal took the pipe: the writer it belonged to is dead,
                // and there is nothing left to write to.
                return;
            };
            // The drop of `stdin` when this closure ends is the end of the text:
            // a writer that waits for EOF — every one of the four does — is
            // waiting for exactly that. The send is what the caller reads as
            // "the write succeeded"; a write that failed sends its error
            // instead.
            let _ = tx.send(stdin.write_all(text.as_bytes()));
        }),
    );
    if let Err(error) = started {
        // The write thread never ran, so the text never entered the pipe. The
        // writer is killed and reaped first and the pipe is let go after, so the
        // EOF it sees is its own death and not an empty copy; what the caller
        // says is the refusal ([`Delivered::NoThread`]).
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        return Delivered::NoThread(error.to_string());
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok(sent) = rx.recv_timeout(left) else {
                    // The child exited but the write's answer did not arrive in
                    // the time left: the deadline is the fact, not a write that
                    // failed.
                    return Delivered::TimedOut;
                };
                return if status.success() && sent.is_ok() {
                    Delivered::Taken
                } else {
                    Delivered::Refused
                };
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Delivered::Refused;
            }
        }
        if Instant::now() >= deadline {
            // Kill and reap. The writer is not awaited past this: killing the
            // child closes the pipe the write thread is filling, so a thread
            // that ends on its own a moment later is one this caller never has
            // to wait for. What the deadline bought is its own answer
            // ([`Delivered::TimedOut`]), not a failed write.
            let _ = child.kill();
            let _ = child.wait();
            return Delivered::TimedOut;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use mush_core::scratch::{Held, Scratch};

    use super::*;

    /// A workspace on a scratch root of its own: the guard travels back with
    /// the workspace, so the files the test saves into it go when it ends.
    fn temp_workspace(name: &str) -> Held<Workspace> {
        let dir = Scratch::new(&format!("clipboard-{name}"));
        let ws = Workspace::new(&dir).unwrap();
        dir.hold(ws)
    }

    /// Both clipboard roads spawn their child through
    /// [`mush_core::secrets::scrub`] (finding C1), and neither `wl-paste` nor
    /// `wl-copy` is guaranteed on the machine: what is under test is the spawn
    /// every reader and writer goes through, not the clipboard program, so the
    /// child here is `sh` — which cannot be missing — and it writes what
    /// `printenv` printed to a file. `PATH` and the rest ride through the
    /// removal untouched; the two children are chosen for what they prove.
    #[test]
    fn a_clipboard_child_never_sees_mushs_key() {
        let dir = Scratch::new("clipboard-child-key");
        let previous = std::env::var_os("MUSH_API_KEY");
        std::env::set_var("MUSH_API_KEY", "sk-probe-inheritance-0123456789");
        let read = dir.join("read");
        let answer = run(
            "sh",
            &[
                "-c".into(),
                format!("printenv MUSH_API_KEY > {}; true", read.display()),
            ],
            Instant::now() + DEADLINE,
            spawn_worker,
        );
        let write = dir.join("write");
        let delivered = deliver(
            "sh",
            &[
                "-c".into(),
                format!(
                    "printenv MUSH_API_KEY > {}; cat >/dev/null",
                    write.display()
                ),
            ],
            &Arc::from("some text"),
            Instant::now() + DEADLINE,
            spawn_worker,
        );
        match previous {
            Some(previous) => std::env::set_var("MUSH_API_KEY", previous),
            None => std::env::remove_var("MUSH_API_KEY"),
        }
        assert!(
            matches!(answer, Answer::Nothing),
            "the reader road ran to its own answer"
        );
        assert!(
            matches!(delivered, Delivered::Taken),
            "the writer road reached its child"
        );
        for (road, path) in [("reader", &read), ("writer", &write)] {
            let seen = fs::read_to_string(path).unwrap();
            assert_eq!(seen, "", "the {road} child saw mush's credential: {seen:?}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A reader that pours out more than the cap has the rest drained and
    /// dropped: the memory a stuck clipboard tool can cost is a constant, and a
    /// child whose output nobody read would block on a full pipe instead. The
    /// cap reaching its end is the fact the refusal needs — a buffer that
    /// filled is not the picture, and one that ended short of the cap is.
    #[test]
    fn a_runaway_reader_is_capped_and_drained() {
        let flood = vec![7u8; READ_CAP * 3];
        let kept = drain(flood.as_slice());
        assert_eq!(
            kept.bytes.len(),
            READ_CAP,
            "three times the cap, one cap kept"
        );
        assert!(kept.filled, "and the cap is reported as filled");
        assert!(kept.bytes.iter().all(|byte| *byte == 7));

        let whole = drain(&flood[..READ_CAP - 1]);
        assert_eq!(whole.bytes.len(), READ_CAP - 1);
        assert!(!whole.filled, "a short reader is a whole picture");
    }

    /// The bytes of a png, as far as `image_mime` is concerned: the magic
    /// number is the whole of what it reads, and the padding lets a test craft
    /// one past the cap without a real picture.
    fn png(padding: usize) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.resize(8 + padding, 0);
        bytes
    }

    /// Bytes a reader handed back are saved into the workspace and handed on as
    /// the image that names them — the file exists, and it holds the bytes.
    #[test]
    fn clipboard_bytes_that_are_an_image_are_saved() {
        let ws = temp_workspace("saved");
        let bytes = png(4);
        let image = saved(&ws, drain(bytes.as_slice()))
            .unwrap()
            .expect("a png is an image");
        assert_eq!(image.mime, "image/png");
        assert!(image.path.starts_with(".mush/paste/pasted-"), "{image:?}");
        assert_eq!(fs::read(ws.root().join(&image.path)).unwrap(), image.bytes);
    }

    /// Bytes that are not an image are "the clipboard holds no image" — a
    /// reader that exited successfully with words said something other than a
    /// picture, and that is not a refusal to show the human.
    #[test]
    fn clipboard_bytes_that_are_not_an_image_are_no_image() {
        let ws = temp_workspace("junk");
        assert!(saved(&ws, drain(&b"hello, world"[..])).unwrap().is_none());
    }

    /// A picture past the cap cannot ride, and the sentence names the road a
    /// clipboard image has: save it, downscale it, copy the smaller one.
    #[test]
    fn a_clipboard_image_past_the_cap_names_a_road() {
        let ws = temp_workspace("big");
        let picture = png(IMAGE_FILE_CAP as usize);
        let refused = saved(&ws, drain(picture.as_slice())).unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(refused.contains("wl-paste"), "{refused}");
        assert!(refused.contains("convert"), "{refused}");
    }

    /// A picture cut at the reader's cap is refused without a size being named:
    /// the reader stopped at [`READ_CAP`], so the length in hand is the
    /// buffer's, not the picture's — the old road called every picture over the
    /// cap exactly 2,097,153 bytes, whatever it really was. A whole picture
    /// handed straight to the save still gets its exact length, because that is
    /// the half of the rule that is known.
    #[test]
    fn a_picture_cut_at_the_reader_cap_is_refused_without_a_number() {
        let ws = temp_workspace("cut");
        let picture = png(READ_CAP * 2);
        let drained = drain(picture.as_slice());
        assert!(drained.filled, "the buffer stopped at the cap");
        let refused = saved(&ws, drained).unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(refused.contains("wl-paste"), "{refused}");
        assert!(refused.contains("convert"), "{refused}");
        assert!(
            !refused.contains("2097153"),
            "the buffer's length may not be read as the picture's: {refused}"
        );
        assert!(
            refused.contains("true size is not known"),
            "and the sentence says the size is unknown: {refused}"
        );

        let whole = png(IMAGE_FILE_CAP as usize + 4096);
        let refused = ws.save_pasted_image(whole.clone(), false).unwrap_err();
        assert!(
            refused.contains(&format!("of {} bytes", whole.len())),
            "the exact-size sentence stays for known sizes: {refused}"
        );
    }

    /// A reader killed at the deadline is its own fact with its own sentence:
    /// the old road folded the kill into the same `Nothing` as an empty
    /// clipboard, so the human was told there was no picture when the truth was
    /// that nobody answered. The sentence names the reader and the wait — 2s,
    /// the shared deadline — and points at the file road, because the clipboard
    /// may hold the picture yet.
    #[test]
    fn a_reader_killed_at_the_deadline_is_a_timeout_and_says_so() {
        let ws = temp_workspace("timeout");
        let readers = vec![("sh", vec!["-c".to_string(), "sleep 30".to_string()])];
        let refused = run_readers(
            &ws,
            readers,
            Instant::now() + Duration::from_millis(50),
            spawn_worker,
        )
        .unwrap_err();
        assert!(
            refused.contains("`sh` did not answer within 2s"),
            "{refused}"
        );
        assert!(
            refused.contains("paste its path"),
            "the file road: {refused}"
        );
        assert!(
            !refused.contains("no image") && !refused.contains("no clipboard reader"),
            "and it is not the empty clipboard's claim: {refused}"
        );
    }

    /// A reader that exits — zero or non-zero — is "no image", deliberately:
    /// it can serve no picture, so the human's move is the one an empty
    /// clipboard asks for, and only the deadline needs a sentence of its own.
    #[test]
    fn a_reader_that_exits_without_a_picture_is_no_image() {
        let ws = temp_workspace("failed");
        for exit in ["exit 0", "exit 3"] {
            let answer = run(
                "sh",
                &["-c".into(), exit.into()],
                Instant::now() + Duration::from_secs(5),
                spawn_worker,
            );
            assert!(
                matches!(answer, Answer::Nothing),
                "`{exit}` is no image, not a timeout"
            );
        }
        let readers = vec![("sh", vec!["-c".to_string(), "exit 3".to_string()])];
        assert!(
            run_readers(
                &ws,
                readers,
                Instant::now() + Duration::from_secs(5),
                spawn_worker,
            )
            .unwrap()
            .is_none(),
            "a failed reader answers as an empty clipboard does"
        );
    }

    /// Not one reader on PATH is a third fact: the machine cannot read its
    /// clipboard at all, and the sentence names what to install rather than
    /// pretending either that the clipboard was empty or that a reader stalled.
    #[test]
    fn no_reader_at_all_names_what_to_install() {
        let ws = temp_workspace("no-reader");
        let readers = vec![("mush-no-such-reader", Vec::new())];
        let refused = run_readers(
            &ws,
            readers,
            Instant::now() + Duration::from_secs(5),
            spawn_worker,
        )
        .unwrap_err();
        assert!(refused.contains("no clipboard reader on PATH"), "{refused}");
        assert!(refused.contains("wl-clipboard"), "{refused}");
    }

    /// The reader list is the three platforms' tools in the order the module
    /// documents, and every type-taking reader gets all four mimes: a binding
    /// dropped here is a format that silently stops arriving.
    #[test]
    fn every_reader_and_every_mime_is_asked_for() {
        let readers = readers();
        let queries: Vec<(&str, Option<&str>)> = readers
            .iter()
            .map(|(program, args)| {
                let mime = args
                    .iter()
                    .position(|arg| arg == "--type" || arg == "-t")
                    .and_then(|at| args.get(at + 1))
                    .map(String::as_str);
                (*program, mime)
            })
            .collect();
        assert_eq!(
            queries,
            vec![
                ("wl-paste", Some("image/png")),
                ("wl-paste", Some("image/jpeg")),
                ("wl-paste", Some("image/gif")),
                ("wl-paste", Some("image/webp")),
                ("xclip", Some("image/png")),
                ("xclip", Some("image/jpeg")),
                ("xclip", Some("image/gif")),
                ("xclip", Some("image/webp")),
                ("pngpaste", None),
            ],
            "the readers, in order"
        );
    }

    /// A path in a scratch root one test owns: it names the test and the
    /// process, so a leftover from an earlier run cannot be read as this run's
    /// answer, and the guard — which comes back with the path — removes it.
    /// The file itself is never written by the fixture: a test that asserts
    /// "this was never written" has to know it was not there.
    fn temp_path(name: &str) -> Held<std::path::PathBuf> {
        let dir = Scratch::new(&format!("clipboard-file-{name}"));
        let path = dir.path().join(name);
        dir.hold(path)
    }

    /// A writer that takes its stdin the way a real one does and puts it in a
    /// file instead of on the clipboard: the write road's fake, the shape the
    /// readers' tests hand in as `("sh", vec!["-c", "…"])`. The machine's own
    /// clipboard is never what a test writes to.
    fn capturing(path: &std::path::Path) -> Vec<String> {
        vec!["-c".to_string(), format!("cat > '{}'", path.display())]
    }

    /// The text goes in exactly: newlines and tabs stay where they were, a
    /// trailing newline is the human's and is neither dropped nor doubled, and a
    /// text that ends without one does not gain it. `wl-copy`'s `-n` is
    /// `--trim-newline` on this road, so a writer list that passed it would drop
    /// the trailing newline of every case here that has one; the reader's
    /// `--no-newline` is the read road's and has no counterpart here.
    #[test]
    fn the_text_arrives_byte_for_byte() {
        let path = temp_path("exact");
        for text in [
            "first line\nsecond line\n",
            "tabbed:\tone\ttwo\n\nand a blank line\n",
            "no trailing newline",
            "\n",
        ] {
            run_writers(
                vec![("sh", capturing(&path))],
                text,
                Instant::now() + Duration::from_secs(5),
                spawn_worker,
            )
            .unwrap_or_else(|refused| panic!("a `cat` takes any text, {text:?}: {refused}"));
            assert_eq!(
                fs::read(&path).unwrap(),
                text.as_bytes(),
                "byte for byte, nothing added and nothing trimmed: {text:?}"
            );
        }
    }

    /// The first writer that takes the text wins the road: one that is not
    /// installed and one that exits non-zero are both passed over without a
    /// sentence of their own, and once a writer has the text the writers after
    /// it never run — the text is already on its way, so running another program
    /// could only put a second, staler copy on the clipboard.
    #[test]
    fn the_first_writer_that_takes_the_text_wins() {
        let path = temp_path("first-writer");
        let later = temp_path("later-writer");
        let writers = vec![
            ("mush-no-such-writer", Vec::new()),
            ("sh", vec!["-c".to_string(), "exit 3".to_string()]),
            ("sh", capturing(&path)),
            ("sh", capturing(&later)),
        ];
        run_writers(
            writers,
            "the winner",
            Instant::now() + Duration::from_secs(5),
            spawn_worker,
        )
        .expect("the third writer takes what the first two could not");
        assert_eq!(fs::read(&path).unwrap(), b"the winner");
        assert!(
            !later.exists(),
            "a writer after the one that took the text never runs"
        );
    }

    /// A writer that exits successfully without having read the text has put it
    /// nowhere, and its exit status alone cannot say so: `exit 0` closes stdin
    /// and leaves. The write thread's own answer is what knows — a text past the
    /// pipe buffer can never have landed in a program that took none of it — so
    /// the road falls through instead of reporting a copy that never happened.
    #[test]
    fn a_writer_that_exits_without_taking_the_text_is_not_a_success() {
        let path = temp_path("unread");
        let text = "x".repeat(1024 * 1024);
        let writers = vec![
            ("sh", vec!["-c".to_string(), "exit 0".to_string()]),
            ("sh", capturing(&path)),
        ];
        run_writers(
            writers,
            &text,
            Instant::now() + Duration::from_secs(5),
            spawn_worker,
        )
        .expect("the second writer takes what the first dropped");
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
    }

    /// A road where every writer ran and none took the text is its own refusal:
    /// the text is not on the clipboard, and that is the whole of what can be
    /// said — there is no reader to point at and no file road, as the image half
    /// has. It is not the missing-program sentence either, because a program that
    /// is installed and failed asks the human for something else.
    #[test]
    fn a_text_no_writer_took_did_not_reach_the_clipboard() {
        let writers = vec![("sh", vec!["-c".to_string(), "exit 3".to_string()])];
        let refused = run_writers(
            writers,
            "undelivered",
            Instant::now() + Duration::from_secs(5),
            spawn_worker,
        )
        .unwrap_err();
        assert!(refused.contains("did not reach the clipboard"), "{refused}");
        assert!(
            !refused.contains("no clipboard writer on PATH"),
            "the writer is on PATH — it just failed: {refused}"
        );
    }

    /// Not one writer on PATH is a third fact: the machine cannot write its
    /// clipboard at all, and the sentence names what to install — the programs
    /// themselves, so the human has something to do about it.
    #[test]
    fn no_writer_at_all_names_what_to_install() {
        let writers = vec![("mush-no-such-writer", Vec::new())];
        let refused = run_writers(
            writers,
            "nowhere",
            Instant::now() + Duration::from_secs(5),
            spawn_worker,
        )
        .unwrap_err();
        assert!(refused.contains("no clipboard writer on PATH"), "{refused}");
        assert!(refused.contains("wl-clipboard"), "{refused}");
        assert!(refused.contains("wl-copy"), "{refused}");
        assert!(refused.contains("xclip"), "{refused}");
        assert!(refused.contains("pbcopy"), "{refused}");
    }

    /// A writer that never reads is killed at the deadline and earns its own
    /// sentence: it may be waiting on a clipboard owner that will never speak,
    /// so "the text did not reach the clipboard" would send the human looking in
    /// the wrong place. The text here is past the pipe buffer, which is what the
    /// deadline is for: the caller's own thread never writes it, so a program
    /// that stops reading cannot hold the human's keyboard on a full pipe.
    ///
    /// The writer's own pid, written before it sleeps, is the instrument the
    /// reap is read with — and the kill races that first line: a machine slow
    /// to start `sh` has the deadline arrive before the `echo` has run, and a
    /// read that assumes the file is there fails on that race rather than on
    /// anything the module did. Waiting for the file cannot mend it — the
    /// writer is killed and reaped before the call returns, and a name it never
    /// wrote cannot appear after it — so the naming is given room instead: a
    /// quarter of a second, where the old 50 ms lost the race on a loaded
    /// machine. A kill that still wins it is its own case below: the deadline's
    /// facts (the sentence and the elapsed time) are this test's either way,
    /// and the reap is what the pid buys when the writer had time to name
    /// itself. On Linux the killed writer is checked to be really gone — a
    /// `kill` that left a zombie would answer here and never reap anything
    /// again.
    #[test]
    fn a_writer_that_never_takes_the_text_is_killed_at_the_deadline() {
        let pid_file = temp_path("hung-pid");
        let writers = vec![(
            "sh",
            vec![
                "-c".to_string(),
                format!("echo $$ > '{}'; sleep 30", pid_file.display()),
            ],
        )];
        let text = "x".repeat(1024 * 1024);
        let started = Instant::now();
        // Room for the writer's first line: the pid it echoes is the reaping
        // check's instrument, and 50 ms is less than a loaded machine takes to
        // start a shell (see the doc).
        let refused = run_writers(
            writers,
            &text,
            Instant::now() + Duration::from_millis(250),
            spawn_worker,
        )
        .unwrap_err();
        let waited = started.elapsed();
        assert!(
            refused.contains("`sh` did not take the text within 2s"),
            "{refused}"
        );
        assert!(
            refused.contains("clipboard owner"),
            "the sentence names the wait as the likely reason: {refused}"
        );
        assert!(
            !refused.contains("no clipboard writer on PATH")
                && !refused.contains("did not reach the clipboard"),
            "a stall is neither a missing writer nor a failed write: {refused}"
        );
        // The guard is sized from the measurement and must stay below the
        // production `DEADLINE` (2 s) it discriminates: the call returned
        // 251-253 ms after the start — its own 250 ms deadline — measured
        // standing still (load 25) and with twelve extra busy loops on the
        // box. A regression that ignored the deadline it was handed and gave
        // up only at the module's 2 s would come back past this, where a
        // ceiling of the module's own size would have let it pass.
        assert!(
            waited < Duration::from_secs(2),
            "30s of sleep may not be waited on: the call took {waited:?}"
        );
        #[cfg(target_os = "linux")]
        {
            // The writer names its pid and then sleeps, and the kill decides
            // which of the two this reads (see the doc): a whole pid when the
            // naming got there first, and no file at all when the deadline won
            // the race — which is the deadline's own fact, not a failure to
            // read. Anything but "no such file" is the fixture failing.
            match fs::read_to_string(&pid_file) {
                Ok(named) => {
                    let pid: i32 = named.trim().parse().expect("sh's `$$` is a pid");
                    assert!(
                        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                        "the killed writer is reaped, not left as a zombie: /proc/{pid} is still \
                         there"
                    );
                }
                Err(error) => assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "the writer names its own process before it sleeps: {error}"
                ),
            }
        }
    }

    /// A [`WorkerSpawn`] that refuses every thread, like a box at its limit
    /// (`ulimit -u`, a container's `pids.max`, a fork storm) — the seam the
    /// app's own `WorkerSpawn` tests drive their four roads with. The breath
    /// before the refusal is what lets the reader or writer the road has
    /// already started name itself: the tests below read the kill and the reap
    /// off the pid it wrote, and a process killed before `sh` could run would
    /// leave the instrument unread rather than the fact unproven.
    ///
    /// Wall-clock bounds in this module are runaway guards, never claims about
    /// this machine — see the rule on `settle_sweep` in `app/mod.rs`'s tests:
    /// scale where the subject is complexity, else size the ceiling as a
    /// multiple of a measured worst case and say where the measurement came
    /// from.
    ///
    /// One second carries that reason, where the old 300 ms was a claim: `sh`
    /// naming itself measured at most 70 ms (it had written its pid on all
    /// twenty runs when the child was given a 70 ms deadline), standing still
    /// (load 25) and with twelve extra busy loops on top — so the breath is
    /// some fourteen times the worst reading, and the pid the tests below read
    /// is there before the refusal is even made.
    fn refuse_after_a_breath(
        _name: &str,
        _job: Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<()> {
        std::thread::sleep(Duration::from_secs(1));
        Err(std::io::Error::other("the box is at its thread limit"))
    }

    /// A drain thread that cannot start is a returned sentence, not a panic on
    /// the worker the app started — whose answer would then never come, so
    /// `Ctrl-V` would do nothing in silence (finding R11's residual). The
    /// reader the road had already started is killed and reaped, because
    /// nothing can read its stdout: a picture past the pipe buffer would sit in
    /// a full pipe until the deadline, and the bytes already in it could never
    /// be collected. The sentence says the read did not start and names the road
    /// that still works; the reader after it never runs, because the refusal is
    /// the machine's and not the program's, and it would need a drain thread of
    /// its own.
    #[test]
    fn a_drain_reader_that_cannot_start_kills_the_reader_and_says_the_read_did_not_start() {
        let ws = temp_workspace("no-drain");
        let pid_file = temp_path("no-drain-pid");
        let later = temp_path("no-drain-later");
        let readers = vec![
            (
                "sh",
                vec![
                    "-c".to_string(),
                    format!("echo $$ > '{}'; sleep 5", pid_file.display()),
                ],
            ),
            (
                "sh",
                vec![
                    "-c".to_string(),
                    format!("echo ran > '{}'", later.display()),
                ],
            ),
        ];
        let refused = run_readers(
            &ws,
            readers,
            Instant::now() + Duration::from_secs(5),
            refuse_after_a_breath,
        )
        .unwrap_err();
        #[cfg(target_os = "linux")]
        {
            // The reader named itself while the refusal was being made, and
            // the road killed and reaped it before the sentence came back: a
            // `/proc` entry would be a reader still running.
            let named = fs::read_to_string(&pid_file)
                .expect("the reader names itself while the refusal is being made");
            let pid: i32 = named.trim().parse().expect("sh's `$$` is a pid");
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "the reader whose drain could not start is killed and reaped: /proc/{pid} is still \
                 there"
            );
        }
        assert!(!later.exists(), "the readers after the refusal never run");
        assert!(
            refused.contains("could not start the clipboard read"),
            "{refused}"
        );
        assert!(
            refused.contains("the box is at its thread limit"),
            "the machine's refusal is carried into the sentence: {refused}"
        );
        assert!(
            refused.contains("paste the image's path instead"),
            "the road that still works: {refused}"
        );
        assert!(
            !refused.contains("no clipboard reader on PATH") && !refused.contains("no image"),
            "a refused thread is neither a missing reader nor an empty clipboard: {refused}"
        );
    }

    /// A stdin thread that cannot start is a returned sentence, not a panic on
    /// the worker the app started — whose answer would then never come, so
    /// `Ctrl-Y Enter` would do nothing in silence (finding R11's residual). The
    /// writer the road had already started is killed and reaped *before* the
    /// pipe is let go, because a writer that waits for EOF — every one of them
    /// does — must not read a closed, empty pipe as the copy: the text never
    /// entered the pipe, no program took it, and the sentence says the write did
    /// not start. The writer after it never runs, because the refusal is the
    /// machine's and not the program's, and it would need a write thread of its
    /// own.
    #[test]
    fn a_stdin_writer_that_cannot_start_kills_the_writer_and_says_the_text_was_not_copied() {
        let path = temp_path("no-write");
        let pid_file = temp_path("no-write-pid");
        let later = temp_path("no-write-later");
        let writers = vec![
            (
                "sh",
                vec![
                    "-c".to_string(),
                    format!(
                        "echo $$ > '{}'; cat > '{}'",
                        pid_file.display(),
                        path.display()
                    ),
                ],
            ),
            (
                "sh",
                vec!["-c".to_string(), format!("cat > '{}'", later.display())],
            ),
        ];
        let refused = run_writers(
            writers,
            "the text that must not land",
            Instant::now() + Duration::from_secs(5),
            refuse_after_a_breath,
        )
        .unwrap_err();
        #[cfg(target_os = "linux")]
        {
            // The writer named itself while the refusal was being made, and the
            // road killed and reaped it before the sentence came back: a
            // `/proc` entry would be a writer still holding a pipe nobody fills.
            let named = fs::read_to_string(&pid_file)
                .expect("the writer names itself while the refusal is being made");
            let pid: i32 = named.trim().parse().expect("sh's `$$` is a pid");
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "the writer whose stdin thread could not start is killed and reaped: /proc/{pid} \
                 is still there"
            );
        }
        assert!(
            !matches!(fs::read(&path), Ok(bytes) if !bytes.is_empty()),
            "the text never reached the writer: {:?}",
            fs::read(&path)
        );
        assert!(!later.exists(), "the writers after the refusal never run");
        assert!(
            refused.contains("could not start the clipboard write"),
            "{refused}"
        );
        assert!(
            refused.contains("the box is at its thread limit"),
            "the machine's refusal is carried into the sentence: {refused}"
        );
        assert!(
            !refused.contains("no clipboard writer on PATH")
                && !refused.contains("did not reach the clipboard"),
            "a refused thread is neither a missing writer nor a failed write: {refused}"
        );
    }

    /// The writer list is the four platforms' tools in the order the module
    /// documents, with `xclip` told which selection to put the text in: a binding
    /// dropped here is a platform that silently stops accepting text. `wl-copy`
    /// takes no flags where `wl-paste` takes `--no-newline`, because `-n` on this
    /// side would trim a trailing newline that is the human's, and `pbcopy` and
    /// Windows's `clip` take no flags at all.
    #[test]
    fn the_writers_are_the_four_programs_in_the_documented_order() {
        assert_eq!(
            writers(),
            vec![
                ("wl-copy", Vec::new()),
                (
                    "xclip",
                    vec![
                        "-selection".to_string(),
                        "clipboard".to_string(),
                        "-i".to_string(),
                    ],
                ),
                ("pbcopy", Vec::new()),
                ("clip", Vec::new()),
            ],
            "the writers, in order"
        );
    }
}
