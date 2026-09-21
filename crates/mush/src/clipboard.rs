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
/// dropped, never buffered.
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
pub fn read_image(ws: &Workspace) -> Result<Option<Image>, String> {
    let deadline = Instant::now() + DEADLINE;
    let mut ran = false;
    for (program, args) in readers() {
        if Instant::now() >= deadline {
            break;
        }
        match run(program, &args, deadline) {
            Answer::Missing => {}
            Answer::Nothing => ran = true,
            Answer::Bytes(bytes) => return saved(ws, bytes),
        }
    }
    if !ran {
        return Err(
            "no clipboard reader on PATH — install `wl-clipboard` (wl-paste), `xclip`, or \
             macOS's `pngpaste`"
                .to_string(),
        );
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

/// What one run of one reader produced.
enum Answer {
    /// The program is not on this machine.
    Missing,
    /// It ran and handed back no image bytes — no such type on the clipboard,
    /// an error it reported, or a kill at the deadline.
    Nothing,
    /// It exited successfully with these bytes.
    Bytes(Vec<u8>),
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
                let Ok(bytes) = rx.recv_timeout(left) else {
                    return Answer::Nothing;
                };
                return if status.success() && !bytes.is_empty() {
                    Answer::Bytes(bytes)
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
            // own a moment later is one this caller never has to wait for.
            let _ = child.kill();
            let _ = child.wait();
            return Answer::Nothing;
        }
        std::thread::sleep(POLL);
    }
}

/// Read a reader's stdout to the end, keeping at most [`READ_CAP`] bytes. The
/// rest is read and dropped rather than left in the pipe: a child whose output
/// nobody reads blocks on a full pipe and never exits, which would spend the
/// whole deadline of a reader that is working fine.
fn drain(mut stdout: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                let room = READ_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..read.min(room)]);
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
/// second, and its refusal sentence comes from the save.
fn saved(ws: &Workspace, bytes: Vec<u8>) -> Result<Option<Image>, String> {
    if image_mime(&bytes).is_none() {
        return Ok(None);
    }
    ws.save_pasted_image(bytes).map(Some)
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
    /// child whose output nobody read would block on a full pipe instead.
    #[test]
    fn a_runaway_reader_is_capped_and_drained() {
        let flood = vec![7u8; READ_CAP * 3];
        let kept = drain(flood.as_slice());
        assert_eq!(kept.len(), READ_CAP, "three times the cap, one cap kept");
        assert!(kept.iter().all(|byte| *byte == 7));
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
        let image = saved(&ws, png(4)).unwrap().expect("a png is an image");
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
        assert!(saved(&ws, b"hello, world".to_vec()).unwrap().is_none());
    }

    /// A picture past the cap cannot ride, and the sentence names the road a
    /// clipboard image has: save it, downscale it, copy the smaller one.
    #[test]
    fn a_clipboard_image_past_the_cap_names_a_road() {
        let ws = temp_workspace("big");
        let refused = saved(&ws, png(IMAGE_FILE_CAP as usize)).unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(refused.contains("wl-paste"), "{refused}");
        assert!(refused.contains("convert"), "{refused}");
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
