//! The image on the system clipboard, read the way a human would read it.
//!
//! There is no portable clipboard API, and a crate that offers one is a
//! dependency mush does not want to pay for a screenshot: the programs every
//! desktop already ships read the clipboard — wayland's `wl-paste`, X11's
//! `xclip`, macOS's `pngpaste` — and shelling out to them is what a human does
//! by hand. Which one exists is discovered by trying them, because a program
//! that is not installed fails to spawn instantly and costs nothing to ask.
//!
//! This module lives in the binary crate, not in `mush-core`: it is a
//! subprocess and the clipboard is a machine facility, and `mush-core` is
//! deliberately free of both. What travels back is a workspace [`Image`] —
//! already saved under `.mush/paste/` (`Workspace::save_pasted_image`) — so
//! the caller on the UI thread only has to attach it.
//!
//! Nothing here may block the human's keyboard: the app calls it on a thread of
//! its own, and every command is bounded anyway, because "a clipboard tool that
//! waits for an owner that will never answer" is a hang the app's thread must
//! not inherit. `Command::output()` would be exactly that mistake: it waits for
//! exit before it reads the pipe, which deadlocks on an image past the pipe
//! buffer and hangs forever on a program that never exits.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mush_core::message::Image;
use mush_core::workspace::{image_mime, IMAGE_FILE_CAP};
use mush_core::Workspace;

/// How long the whole read may take, every reader together.
///
/// One deadline for the sequence rather than one per command: the human is
/// waiting at the keyboard, and nine readers that each take their own two
/// seconds is eighteen seconds of a frozen paste. The common clipboard answers
/// in milliseconds — a program that hangs is one waiting on a clipboard owner
/// that will never speak — so two seconds is generous for what is really there
/// and short enough that its absence is noticed as a hiccup rather than a hang.
const DEADLINE: Duration = Duration::from_secs(2);

/// How often a reader is checked for having exited: fine enough that a fast
/// answer costs about one poll, coarse enough that nine readers are not a
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
    run_readers(ws, readers(), Instant::now() + DEADLINE)
}

/// [`read_image`] with the readers and the deadline handed in: the loop, the
/// three answers that are not a picture, and the sentences each one earns.
fn run_readers(
    ws: &Workspace,
    readers: Vec<(&'static str, Vec<String>)>,
    deadline: Instant,
) -> Result<Option<Image>, String> {
    let mut ran = false;
    let mut stalled = None;
    for (program, args) in readers {
        if Instant::now() >= deadline {
            break;
        }
        match run(program, &args, deadline) {
            Answer::Missing => {}
            Answer::Nothing => ran = true,
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
fn run(program: &str, args: &[String], deadline: Instant) -> Answer {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
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
    std::thread::spawn(move || {
        let _ = tx.send(drain(stdout));
    });
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_workspace(name: &str) -> Workspace {
        let dir =
            std::env::temp_dir().join(format!("mush-clipboard-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Workspace::new(&dir).unwrap()
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
        let refused =
            run_readers(&ws, readers, Instant::now() + Duration::from_millis(50)).unwrap_err();
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
            );
            assert!(
                matches!(answer, Answer::Nothing),
                "`{exit}` is no image, not a timeout"
            );
        }
        let readers = vec![("sh", vec!["-c".to_string(), "exit 3".to_string()])];
        assert!(
            run_readers(&ws, readers, Instant::now() + Duration::from_secs(5))
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
        let refused =
            run_readers(&ws, readers, Instant::now() + Duration::from_secs(5)).unwrap_err();
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
}
