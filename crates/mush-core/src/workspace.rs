//! Workspace filesystem access: safe paths, reads, listings, search, atomic
//! writes.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::git;
use crate::message::Image;
use crate::outline::Outline;
use crate::session;
use crate::text;
use crate::usages::{FileUsages, Usages};

/// Directories a walk never descends into: VCS metadata and build output, whose
/// contents are never the workspace's work, and mush's own `.mush` — the
/// session and its lock, the pastes, the isolated children's checkouts
/// (`.mush/wt`), the gate logs, and a note an agent kept beside them.
///
/// **`.mush` is here by the human's decision, and it is a narrow rule.** The
/// walk is the agent's map of the *work*: every root listing and every search
/// used to answer with mush's own bookkeeping among the sources — a model
/// reading a listing of this repo saw `session.json` and `.mush/paste/…` beside
/// the code — which is the noise a listing exists to avoid. Only *discovery*
/// goes away: a path under `.mush` is still opened by name (`read_file`, and
/// every road that resolves a path instead of walking one), and a walk
/// *started* at `.mush` still descends into it, because the start is never
/// name-checked — which is the road the paste placeholder sends a model down
/// when a picture's bytes were shed ("read the file again").
///
/// Hidden names are otherwise *not* skipped — `.github/`, `.gitignore` and
/// `.env.example` are exactly the files an agent is asked about (audit of the
/// prompt vs behaviour, row 8) — and the name `.mush` is mush's own by
/// convention: a project that keeps something else in a `.mush` of its own
/// pays for that convention here. One directory under it is skipped by *path*
/// as well as by name ([`Workspace::is_worktree_path`]).
const SKIP_DIRS: &[&str] = &[
    ".mush",
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vscode",
    "dist",
    "build",
    ".next",
    ".cache",
];

/// The largest file a read tool will open whole. Every whole read is checked
/// against it from the file's stat, before a byte is read — the model's window
/// ([`Workspace::read_window`]), the editor's read ([`Workspace::read_file`])
/// and the bounded count a `write_file` answer carries
/// ([`Workspace::line_count`]) — so past it the memory a read costs is the
/// agent's problem rather than the file's, and the sentence says so; a window
/// of a 200 MB log is `run_command`'s job (`tail`, `sed -n`).
pub const READ_FILE_CAP: u64 = 32 * 1024 * 1024;

/// The largest image a read will hand to a model, in *file* bytes — the
/// transport's cap, not the context budget's.
///
/// Two rulers measure an image and both are real. This one bounds what mush is
/// willing to base64 and put on the wire at all: past it the refusal names the
/// downscale, which is the one road that makes the picture readable. What the
/// picture *costs* the window is its pixels, a different number entirely (see
/// `Image::weight`, and `config::PIXELS_PER_TOKEN`): a picture under this cap
/// can still be too big for the room a transcript has left, and the app's
/// attach gate says so before it is sent — where the old code compared one
/// ruler against the other and got both cases wrong. Under this cap is not a
/// promise the wire carries the bytes, either: a conversation's own byte budget
/// ([`crate::message::IMAGE_BYTES_KEPT`]) can have given an older picture's
/// payload up already, and the wire spells the placeholder sentence there.
pub const IMAGE_FILE_CAP: u64 = 2 * 1024 * 1024;

/// The longest file `search` opens. A pattern that matches inside a 200 MB log
/// is a match the model does not need and a walk that takes minutes; `run_command`
/// is the road to that file.
pub const SEARCH_FILE_CAP: u64 = 2 * 1024 * 1024;

/// The most `context` lines `search` shows on each side of a match: a bigger
/// ask is clamped to this, not refused, because a giant context is a real
/// request and "as many as fit" is its honest answer. Ten either side is a
/// function's neighbourhood — the "what is this" a context is for — and still
/// leaves a 200-row answer room for the several matches a search exists to
/// find; past it the request is a file read, and `read_file` is the road.
pub const SEARCH_CONTEXT_MAX: usize = 10;

/// The sentence a read of a CRLF file appends, from its one home: the window
/// road ([`Workspace::read_window`]) writes it under the text it shows, and
/// [`crate::outline`] writes the same sentence under the rows it shows, because
/// a line copied out of a CRLF file crosses the same line-ending rule whichever
/// read handed it over (finding B7). Not a `pub` item: it is a sentence of
/// mush's own, and the roads that write it are in this crate.
pub(crate) const CRLF_NOTE: &str =
    "[mush: the file's lines end with CRLF — the \\r is not shown in a line; an \
     edit whose old_string or new_string holds a line break or a \\r is refused, a \
     line's own text still edits exactly, and run_command (`sed -i`, `perl -pi`) or \
     write_file is the road for anything across lines]";

/// How much of one row's line `search` shows, in *bytes*, so one minified
/// line cannot spend the whole result. A line past the cap is cut on a
/// character boundary and the cut is marked ([`row_line`]): a partial line a
/// model mistakes for the whole is a wrong fact, not a short one.
const MATCH_LINE_CAP: usize = 240;

/// The mime an image's own first bytes name, or `None` when they are not an
/// image's. Four formats, and all four are sniffed rather than trusted to a
/// name: these are what vision endpoints document, and the bytes are what the
/// `data:` URL will carry.
///
/// The signatures are the ones each format defines: png's eight bytes with
/// `\r\n` and `\x1a\n` in them (they exist to catch a transfer that mangled
/// newlines), jpeg's three-byte start-of-image marker, gif's two version
/// spellings, and webp's `RIFF` container with `WEBP` at the offset that makes
/// it a webp rather than any other RIFF file.
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// The pixel size an image's own header names, in the format `mime` claims —
/// `None` when it cannot be read: a truncated file, a header that lies (a zero
/// dimension, a length past the end of the bytes), bytes that are not that
/// format at all, or a format whose header carries no size.
///
/// Hand-rolled, because the dependency budget (§7 of the design doc) has no
/// image crate to lean on and the four headers are arithmetic: png's IHDR
/// chunk, jpeg's SOFn frame header, gif's logical screen descriptor, webp's
/// three chunk shapes. The number is what `Message::weight` prices a picture
/// by, so a picture's real size decides its share of the context window
/// instead of its file size — which a screenshot's compression can move by
/// 10×.
///
/// `mime` is what the caller sniffed with [`image_mime`]; every parser checks
/// its own signature anyway, because a mime is a claim like an extension is.
/// Every read is bounds-checked and every field that could lie is rejected, so
/// any byte string at all — `&[]`, a three-byte slice, a length past the end —
/// is a `None` and never a panic.
pub fn image_dimensions(mime: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match mime {
        "image/png" => png_dimensions(bytes),
        "image/jpeg" => jpeg_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/webp" => webp_dimensions(bytes),
        _ => None,
    }
}

/// A `width, height` pair worth returning: zero is not a size in any of the
/// four formats (a zero-sized frame is a header that lies), and treating it as
/// unknown sends the caller to the byte-count fallback rather than making an
/// image free.
fn nonzero(width: u32, height: u32) -> Option<(u32, u32)> {
    (width > 0 && height > 0).then_some((width, height))
}

/// `bytes[at..at + 2]` as the big-endian `u16` it is, or `None` when the slice
/// is shorter than that. Every read of a header field goes through one of the
/// five helpers here or below, so a truncated header is a `None` and never a
/// panic.
fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    let field = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([field[0], field[1]]))
}

/// [`be16`]'s little-endian twin.
fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    let field = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([field[0], field[1]]))
}

/// A `u32` field, big-endian.
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([field[0], field[1], field[2], field[3]]))
}

/// A `u32` field, little-endian.
fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

/// A 24-bit little-endian field, the width webp's extended header stores —
/// the fourth byte the format does not spend.
fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at + 3)?;
    Some(u32::from_le_bytes([field[0], field[1], field[2], 0]))
}

/// PNG: the eight-byte signature, then the IHDR chunk, whose first eight
/// payload bytes are width and height, each a big-endian `u32`. IHDR is
/// required to be the first chunk and to be 13 bytes long, so a file that
/// disagrees with either is not one this can read.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if !bytes.starts_with(SIGNATURE) || be32(bytes, 8)? != 13 || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    nonzero(be32(bytes, 16)?, be32(bytes, 20)?)
}

/// JPEG: an SOI marker, then a walk over the marker segments to the frame
/// header (SOFn), whose payload carries precision, height and then width,
/// each two bytes big-endian.
///
/// The walk has to skip the right things: the standalone markers (`RSTn`,
/// `TEM`, and the begin/end-of-image pair) carry no length field, the SOFn
/// range has three non-frame members (`DHT`, `JPG`, `DAC`), and any run of
/// `0xff` fill bytes before a marker is legal. `SOS` means the frame header
/// never came — the compressed data follows it — so a file whose header was
/// cut off answers `None` rather than reading a size out of the entropy data.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut at = 2;
    loop {
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        match marker {
            // Start of scan: everything after this is entropy-coded data.
            0xda => return None,
            // A stuffed byte where a marker should be: this is scan data, not
            // a header, and a walk that read on would be reading pixels.
            0x00 => return None,
            // The frame headers, less the three the range also holds: DHT
            // (0xc4), JPG (0xc8) and DAC (0xcc). The length counts itself, so
            // precision sits two bytes later, then height and width.
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                // The frame's own length is checked like any other claim: it
                // has to be long enough to hold the height and width, and the
                // bytes it names have to be there — a header cut off inside
                // itself is no size, not half a size.
                let length = be16(bytes, at)? as usize;
                if length < 8 || at.checked_add(length)? > bytes.len() {
                    return None;
                }
                let header = bytes.get(at + 2..at + 7)?;
                let height = u16::from_be_bytes([header[1], header[2]]) as u32;
                let width = u16::from_be_bytes([header[3], header[4]]) as u32;
                return nonzero(width, height);
            }
            // Standalone markers: no length field to read.
            0x01 | 0xd0..=0xd7 | 0xd8 | 0xd9 => {}
            _ => {
                let length = be16(bytes, at)? as usize;
                // A length under 2 cannot add up to a segment (it counts
                // itself), and trusting one would leave the walk standing
                // still.
                if length < 2 {
                    return None;
                }
                at = at.checked_add(length)?;
            }
        }
    }
}

/// GIF: the six-byte version signature, then the logical screen descriptor,
/// whose first four bytes are width and height, each a little-endian `u16`.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    nonzero(le16(bytes, 6)? as u32, le16(bytes, 8)? as u32)
}

/// WebP: a RIFF container holding one of the three chunk shapes, each of which
/// names the canvas in its own header —
///
/// - `VP8 ` (lossy): a three-byte start code, then width and height, each a
///   16-bit little-endian field whose low 14 bits are the dimension; the top
///   two bits are a scale hint, so they are masked off.
/// - `VP8L` (lossless): a signature byte, then a 32-bit little-endian word
///   whose low 14 bits are width - 1 and next 14 are height - 1.
/// - `VP8X` (extended): a flag byte and three reserved, then width - 1 and
///   height - 1, each a 24-bit little-endian field.
///
/// The chunk's declared payload size is checked, not trusted: the payload it
/// names has to be there, and each shape has to have room for the fields it
/// reads, or the "chunk" is a header with nothing behind it.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    let chunk = bytes.get(12..16)?;
    let length = le32(bytes, 16)? as usize;
    if 20usize.checked_add(length)? > bytes.len() {
        return None;
    }
    let payload = bytes.get(20..)?;
    if chunk == b"VP8 " {
        // The start code exists to keep a decoder from mistaking a frame's
        // first bytes for something else; without it this is not a VP8 frame.
        if length < 10 || payload.get(3..6)? != b"\x9d\x01\x2a" {
            return None;
        }
        let width = (le16(payload, 6)? & 0x3fff) as u32;
        let height = (le16(payload, 8)? & 0x3fff) as u32;
        nonzero(width, height)
    } else if chunk == b"VP8L" {
        if length < 5 || payload.first() != Some(&0x2f) {
            return None;
        }
        let bits = le32(payload, 1)?;
        nonzero((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
    } else if chunk == b"VP8X" {
        if length < 10 {
            return None;
        }
        nonzero(le24(payload, 4)? + 1, le24(payload, 7)? + 1)
    } else {
        None
    }
}

/// A single workspace root. All agent file access goes through here, which is
/// what keeps a runaway model inside the directory the human opened.
///
/// [`Workspace::new`] opens a directory that is there; [`Workspace::pending`]
/// opens the place one is about to be made at (an isolated agent's worktree
/// before its first run).
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    /// When this workspace was opened, in unix milliseconds: the line a prune
    /// draws between the pastes *this run* wrote — a live transcript's images
    /// are exactly those — and the ones an earlier run left behind
    /// ([`Workspace::prune_pastes`]).
    ///
    /// A fact of the run, not of the directory: the root workspace is opened
    /// once per app run and a child's once per spawn, and nothing here asks the
    /// filesystem or a transcript the workspace cannot see.
    opened: u128,
}

/// What a search found: the answer's rows, whether the cap cut them short,
/// how many files it never opened (binary, or past [`SEARCH_FILE_CAP`]), and
/// how many it found whose name cannot travel on the model's road
/// (`Workspace::name_for_model`: a line break in the name, bytes that are
/// not UTF-8, or ends `resolve` would trim).
///
/// A row is one line of the answer, and it has two shapes: a match is
/// `path:line: text` and a `context` neighbour is `path-line- text`
/// ([`Workspace::search`] argues both). The field is named for the rows and
/// not the matches because the cap counts rows: with a context, the row the
/// cap cut can be one no match landed on.
///
/// `skipped` is the field that keeps "no match" honest. A model reads a
/// miss as "the symbol does not exist", so a search that skipped a file must
/// say so — the count is what the tool tells it instead of a false negative.
/// `unnamed` is the same honesty for the *name*: a row is prefixed
/// with the path, and a path the model cannot pass back to `read_file` is a
/// dead end, so such files are not searched and are counted instead — the same
/// reason [`Workspace::list_files`] leaves them out (finding B9).
pub struct Matches {
    pub rows: Vec<String>,
    pub more: bool,
    pub skipped: usize,
    pub unnamed: usize,
}

/// What a window read found: the text the model reads, and whether the window
/// stopped short of the file's own end.
///
/// The text already *says* when it was cut, in its own trailing sentence
/// (`[mush: lines 1–40 of 900 — read on with offset=41]`). The flag exists so a
/// caller that has to *act* on the fact does not have to parse mush's own prose
/// back out of the text it just wrote — the digest is the one reader that reads
/// sentences, and it is a painter. The unbounded-read fallback in the app is
/// the caller: a model that asked for "the file" and would be shown forty lines
/// of nine hundred is better served by the file's outline (`outline::Outline`),
/// and this flag is how it knows the window was cut without trusting a string.
///
/// `false` covers every window that reached the end, the empty-file sentence,
/// and a single line shown whole; a refusal (`Err`) is not a `Window` at all.
#[derive(Clone, Debug)]
pub struct Window {
    pub text: String,
    /// The window stopped short of the file: its trailer says `read on with
    /// offset=…` or `shown in part`.
    pub truncated: bool,
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::ops::Deref for Window {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

/// What a bounded line count of a whole file can say, from
/// [`Workspace::line_count`]: the number of lines a file had when that number
/// is knowable without spending more than [`READ_FILE_CAP`] of memory, and the
/// honest word when it is not. `write_file`'s answer reads it, so a count it
/// prints is right and a count it cannot know it does not print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineCount {
    /// The file is text, no longer than [`READ_FILE_CAP`]: it has this many
    /// lines, counted from the same bytes the edit road would read.
    Lines(usize),
    /// The file is text but past [`READ_FILE_CAP`]: there are more lines than
    /// the cap will read, and how many is not knowable without the whole read
    /// the cap refuses. No number is reported, because a partial count would be
    /// a wrong one.
    More,
    /// No line count to report: binary (a NUL byte), not valid UTF-8, not a
    /// regular file, not readable, or not there.
    NotText,
}

/// The longest existing ancestor of `path`, canonicalized, with the missing
/// names appended verbatim: the root a workspace is about to be made at.
///
/// Used by [`Workspace::pending`] and nowhere else. Every error that is not a
/// missing leaf is the caller's own error: a path whose ancestor is a file is
/// not a root waiting to be made, and saying it is would open a workspace
/// nothing can write in.
fn canonical_ancestor(path: &Path) -> io::Result<PathBuf> {
    let mut missing: Vec<&OsStr> = Vec::new();
    let mut cursor = path;
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut base) => {
                for name in missing.iter().rev() {
                    base.push(name);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (cursor.file_name(), cursor.parent()) else {
                    return Err(error);
                };
                missing.push(name);
                cursor = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root)?,
            opened: now_millis(),
        })
    }

    /// Open the workspace of an agent whose root is not on disk *yet*: the
    /// place its worktree will be made, before it is made.
    ///
    /// A row lives on refs alone — a restored session keeps its branch, its id
    /// and its place and creates no checkout — so the actor revived from a
    /// stored session is built with a root that does not exist until the door
    /// before its first run makes it ([`git::ensure_worktree`]). The path is
    /// still the place all its tools resolve in, and the door makes exactly
    /// this path, so the workspace is opened as far as it exists: the longest
    /// existing ancestor is canonicalized and the missing names are appended
    /// verbatim, so a root under a symlinked directory reads the same here as
    /// [`Workspace::new`] would read it a moment later.
    ///
    /// A caller with a directory that must be there wants [`Workspace::new`]:
    /// this is only for the one root mush itself is about to create, and it
    /// must never be the thing that lets an application open a directory nobody
    /// has.
    pub fn pending(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let root = match fs::canonicalize(root) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => canonical_ancestor(root)?,
            Err(error) => return Err(error),
        };
        Ok(Self {
            root,
            opened: now_millis(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_str(&self) -> String {
        self.root.display().to_string()
    }

    /// Resolve a workspace-relative path, rejecting anything that escapes root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = rel.trim();
        if rel.is_empty() || rel == "." || rel == "./" {
            return Ok(self.root.clone());
        }
        let rel = rel.strip_prefix("./").unwrap_or(rel);
        let path = Path::new(rel);
        if path.is_absolute() {
            return Err(format!("absolute paths are not allowed: {rel}"));
        }
        let mut out = self.root.clone();
        for component in path.components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::CurDir => {}
                Component::ParentDir => return Err(format!("path escapes the workspace: {rel}")),
                _ => return Err(format!("invalid path: {rel}")),
            }
        }
        Ok(out)
    }

    /// The real thing a workspace path names, for a road that is about to
    /// touch it: [`Self::resolve`]'s lexical answer with its deepest existing
    /// prefix canonicalized and checked against `self.root`.
    ///
    /// Lexical resolution is not enough once a link is in the tree. `resolve`
    /// walks components and refuses `..`, so a path it accepts cannot climb
    /// out — but `root/out -> /tmp/elsewhere` is a name *inside* the root
    /// pointing outside it, and every road that then opened `out/secret.txt`
    /// was reading or writing the file outside (a probe wrote
    /// `out/written.txt` into `/tmp/elsewhere`), while the listing's own doc
    /// promised no symlinked directory was followed. So the filesystem is
    /// asked instead: the deepest prefix that exists is `canonicalize`d —
    /// every link in it resolved — and a result that is not under the root is
    /// a refusal naming the escape. A tail that does not exist yet is appended
    /// unresolved, because a name that is not there cannot be a link.
    ///
    /// A link that stays inside is not an escape and is not refused: it is the
    /// file the model named, and the returned real path is where a write
    /// through it lands (see [`atomic_write`]) while the link stays a link.
    ///
    /// One inside-the-root path is still refused: anything at or under this
    /// workspace's worktree directory ([`Self::is_worktree_path`]). That is
    /// where its isolated children have their own git checkouts, so a path
    /// there names a *sibling* actor's tree rather than this workspace's
    /// files — a listing of it would answer with a stranger's names and a
    /// write into it would be committed onto that child's branch (finding
    /// IN7). The check is asked of the resolved answer, so a link that points
    /// into the worktrees is refused exactly like the path itself.
    fn real_path(&self, path: &Path, rel: &str) -> Result<PathBuf, String> {
        let mut tail: Vec<OsString> = Vec::new();
        let mut probe = path.to_path_buf();
        loop {
            match fs::canonicalize(&probe) {
                Ok(real) => {
                    if !real.starts_with(&self.root) {
                        return Err(format!(
                            "{rel} resolves to {}, outside the workspace — refusing to touch it",
                            real.display()
                        ));
                    }
                    let mut out = real;
                    for part in tail.iter().rev() {
                        out.push(part);
                    }
                    if self.is_worktree_path(&out) {
                        return Err(format!(
                            "{rel} is inside {} — a live child agent's own checkout, where the \
                             child's run-end commit would carry an edit onto its branch while this \
                             workspace's copy stayed untouched; mush's file tools leave a \
                             sibling's tree alone (the shell is the road that reaches a worktree \
                             deliberately)",
                            git::WORKTREE_DIR
                        ));
                    }
                    return Ok(out);
                }
                Err(_) => {
                    let (Some(name), Some(parent)) =
                        (probe.file_name().map(OsString::from), probe.parent())
                    else {
                        return Err(format!("cannot resolve {rel}"));
                    };
                    tail.push(name);
                    probe = parent.to_path_buf();
                }
            }
        }
    }

    /// Whether a real path lies inside this workspace's worktree directory —
    /// the `.mush/wt` every isolated child is checked out under
    /// ([`git::worktree_dir`]).
    ///
    /// One directory holds one live git checkout per isolated child, and each
    /// of them is a *sibling's* tree: the child actor runs there with that
    /// path as its own root and its own branch as the thing its run-end
    /// `git add -A` commits onto. A file road that reached in would be wrong
    /// twice at once — a listing would answer with a sibling's checkout names,
    /// and a write at one of them would be committed by the child's branch
    /// while the file the model meant to change stayed untouched — so the walk
    /// does not descend into it and every path [`Self::real_path`] answers is
    /// checked for it (finding IN7).
    ///
    /// The rule is this one directory and not the name `wt`: a project's own
    /// `wt/` is nobody's checkout. Since `.mush` is skipped by name too
    /// ([`SKIP_DIRS`]), this one is the *path* half of the same rule — the
    /// roads that ask a path question ([`Self::real_path`], a write) must not
    /// depend on how a name is spelled. And a workspace rooted *at* a worktree — the
    /// child's own — has only its own children's checkouts below it, so the
    /// rule never locks an actor out of its own files.
    fn is_worktree_path(&self, path: &Path) -> bool {
        path.starts_with(git::worktree_dir(&self.root))
    }

    /// Workspace-relative display path for an absolute path: the name the
    /// filesystem holds, with a path outside the root left whole (a real path
    /// must not be rewritten into a relative one that means something else).
    ///
    /// Nothing here folds a `\` into `/`. On this box `\` is an ordinary
    /// byte of a file's name, and the fold made a name that no road can open
    /// again: a file named `a\b.txt` listed as `a/b.txt`, and a model's
    /// `read_file` on the listed name answered "No such file or directory" —
    /// the listing handed over a path the listing could not open (finding B9).
    /// A name is bytes, not display. The roads that *hand names over* go
    /// through `Self::name_for_model` instead, which refuses a name that
    /// cannot travel as itself; this one is infallible because its callers show
    /// a path rather than give one back, and so it may decode lossily.
    pub fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// The name a *model road* hands back for a real path, or `None` when the
    /// name cannot travel as itself.
    ///
    /// The listing and the search are data roads: what they report is what the
    /// model passes back to `read_file`, so a reported name must be one the
    /// tools open again — [`Self::resolve`] has to return the same path. Three
    /// shapes cannot:
    ///
    /// - a name that is not valid UTF-8: [`Self::rel`]'s lossy decode would
    ///   hand over U+FFFD where the file holds a byte, and the name would name
    ///   nothing;
    /// - a name holding `\n` or `\r`: a listing is one name per line, so it
    ///   would read as two entries, neither of them the file (B9's second
    ///   half);
    /// - a name whose ends [`str::trim`] would change: `resolve` trims the path
    ///   it is given, so a leading or trailing space (or NBSP, or any other
    ///   whitespace) is a byte the model's own road cannot carry — handing it
    ///   over would name a different file, or none.
    ///
    /// `None` is not a silent loss: the road counts what it could not name and
    /// the tool layer says how many and which road reaches them
    /// (`run_command`: `ls -b`, `rg`).
    fn name_for_model(&self, path: &Path) -> Option<String> {
        let name = path.strip_prefix(&self.root).unwrap_or(path).to_str()?;
        if name.trim() != name || name.contains(['\n', '\r']) {
            return None;
        }
        Some(name.to_string())
    }

    /// Read a text file whole, for the one road that writes back what it reads:
    /// `edit_file`, whose exact replacement needs every byte it is replacing.
    /// Binary files are refused, and so is anything that is not a regular file:
    /// `fs::read` on a FIFO blocks until a writer appears — an actor parked
    /// forever on a name as innocent as `x.png` — and a device may never end at
    /// all. The metadata answers what the path *is* before anything is opened,
    /// the same shape of check `Self::image_at` makes, so the two roads
    /// cannot disagree about which paths can be read. The name is resolved for
    /// real first (`Self::real_path`), because a link the root contains must
    /// not make this read a file outside it. The read itself is bounded by
    /// [`READ_FILE_CAP`] and checked from the stat before a byte is read (see
    /// `Self::whole_read`): past the cap the refusal is `over_read_cap`'s,
    /// the one sentence [`Self::read_window`] gives too, naming `run_command`
    /// as the road that works.
    ///
    /// The decode is **strict**: bytes that are not valid UTF-8 are refused
    /// (`not_utf8`), because this read's text is an edit's source *and* its
    /// result — a lossy decode here is what rewrote a Latin-1 `caf\xe9` as
    /// U+FFFD in the lines the model never touched (finding B6). The roads
    /// that *show* a file rather than write it stay lossy on purpose, and each
    /// says so in its own doc ([`Self::read_window`], [`Self::search`]). The
    /// model's own `read_file` tool takes the window road; nothing here is a
    /// display.
    ///
    /// It used to take a `cap`, keep the head of a long file and mark the cut
    /// with a sentence of its own — then the six-tool cut took the file tools
    /// away and this became `edit_file`'s private read, which must see the whole
    /// file or refuse the edit. The read tools are back ([`Self::read_window`],
    /// [`Self::read_image`]) because the shell cannot serve them behind a
    /// machine lock, so the cut lives there and this stays the whole-file read —
    /// whole within the cap, or refused.
    pub fn read_file(&self, rel: &str) -> Result<String, String> {
        self.whole_read(rel, Decoding::Strict)?.text_or_refusal(rel)
    }

    /// How many lines the text file at `rel` has, for an answer about a file
    /// that is being replaced. The read is bounded by [`READ_FILE_CAP`]: the
    /// stat answers "not a file" and past-the-cap ([`LineCount::More`])
    /// without opening anything, and only a text file within the cap is read —
    /// so the count costs the cap's memory at worst, never the file's own size
    /// (finding B5: counting a 4 GiB file's lines to say "41 → 3 lines" used to
    /// cost a 4 GiB allocation). Binary and non-UTF-8 files are
    /// [`LineCount::NotText`], the same fact [`Self::read_file`] refuses on the
    /// edit road.
    pub fn line_count(&self, rel: &str) -> LineCount {
        match self.whole_read(rel, Decoding::Strict) {
            Ok(WholeRead::Text(text)) => LineCount::Lines(text.lines().count()),
            Ok(WholeRead::PastCap(_)) => LineCount::More,
            _ => LineCount::NotText,
        }
    }

    /// A whole text file's bytes, read within [`READ_FILE_CAP`] and decoded by
    /// the road's own rule ([`Decoding`]). One read behind [`Self::read_file`],
    /// [`Self::read_window`] and [`Self::line_count`], so the cap cannot be
    /// checked in three places and drift in one, and the facts a road turns
    /// into a sentence are decided here once ([`WholeRead`]).
    ///
    /// The order of the decisions is the point, and each is made from a fact
    /// already in hand rather than from the read:
    ///
    /// - the *shape* comes from `fs::metadata` before anything is opened, so a
    ///   FIFO cannot park a read;
    /// - the *cap* comes from that same stat, before any whole read: a 512 MiB
    ///   blob is refused from its size instead of being loaded to find it out
    ///   (finding B5 measured peak RSS 3 → 514 MiB on exactly that);
    /// - the read is still bounded to the cap + 1 bytes, because a file can
    ///   grow between the stat and the read — the same bound [`Self::image_at`]
    ///   keeps, and a read that hits it refuses without a length in hand;
    /// - a NUL byte is a binary file on either decoding, because a NUL is valid
    ///   UTF-8 and the encoding check alone would let a blob through.
    fn whole_read(&self, rel: &str, decoding: Decoding) -> Result<WholeRead, String> {
        let path = self.real_path(&self.resolve(rel)?, rel)?;
        let meta = fs::metadata(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if !meta.is_file() {
            return Ok(WholeRead::NotRegular);
        }
        if meta.len() > READ_FILE_CAP {
            return Ok(WholeRead::PastCap(Some(meta.len())));
        }
        let file = fs::File::open(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        let mut bytes = Vec::new();
        file.take(READ_FILE_CAP + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read {rel}: {e}"))?;
        if bytes.len() as u64 > READ_FILE_CAP {
            return Ok(WholeRead::PastCap(None));
        }
        if bytes.contains(&0) {
            return Ok(WholeRead::Binary);
        }
        match decoding {
            Decoding::Lossy => Ok(WholeRead::Text(
                text::strip_bom(&String::from_utf8_lossy(&bytes)).to_string(),
            )),
            Decoding::Strict => match String::from_utf8(bytes) {
                Ok(text) => Ok(WholeRead::Text(text)),
                Err(e) => Ok(WholeRead::NotUtf8(e.utf8_error().valid_up_to())),
            },
        }
    }

    /// Read `rel` as an image, when it is one: the mime its own first bytes
    /// name, the pixel size its header names ([`image_dimensions`]), and the
    /// bytes whole — or `None` when the file is not an image, so the caller
    /// reads it as text.
    ///
    /// The format comes from the magic number and never from the name. An
    /// extension is a claim by whoever wrote the file; the sniff is the file
    /// saying what it is, and a `data:` URL's mime is read by an endpoint that
    /// never sees a name at all. The four are the ones vision endpoints
    /// document: png, jpeg, gif, webp.
    ///
    /// Past [`IMAGE_FILE_CAP`] the refusal names the downscale, because an
    /// image that big cannot be made to fit any other way — `offset`/`limit`
    /// are lines and an image has none. The name is resolved for real first
    /// (`Self::real_path`) so a link inside the root cannot make the model's
    /// read open a picture outside it; the human's paste road reaches
    /// `Self::image_at` without this check, because the human already has
    /// the file and is the one naming it.
    pub fn read_image(&self, rel: &str) -> Result<Option<Image>, String> {
        let path = self.real_path(&self.resolve(rel)?, rel)?;
        self.image_at(&path, rel)
    }

    /// The image a paste names, when the paste is nothing but the name of one.
    /// `Ok(None)` = "this is text, not an image". A paste that names several
    /// images — one drag of four files — is [`Self::pasted_images`]'s door.
    ///
    /// This is the *human's* door, and it is deliberately not the model's:
    /// [`Self::resolve`] refuses an absolute path because a run-away model
    /// must not read `/etc/passwd`, while the human already has the file and
    /// is choosing to show it. Drag-and-drop, a file manager's "copy", and a
    /// browser's `file://` URL all hand the terminal an absolute path; refusing
    /// it would make the one gesture the feature exists for a refusal. The
    /// trust is not extended to the model: the image is read from wherever the
    /// human named it but travels as a `data:` URL inside the message, and a
    /// clipboard image is [saved into the workspace](Self::save_pasted_image)
    /// before it can ride anywhere.
    ///
    /// What a paste may be, in the order each rule is tried:
    ///
    /// - Surrounding whitespace and newlines are the terminal's, not the
    ///   name's, so they are trimmed. An interior newline makes the paste a
    ///   paragraph, and an interior whitespace that no backslash escaped makes
    ///   it prose — a space is what separates words, and a paste with one is
    ///   read as the words it is. The two spellings a tool uses for a file's
    ///   *own* space, `\ ` and `%20`, survive to the name.
    /// - One pair of matching surrounding quotes is stripped: a quoted paste
    ///   is a tool saying "this is one name", so its interior spaces are the
    ///   name's and are not re-judged.
    /// - A `file://` scheme is dropped, a `localhost` host with it, and `%XX`
    ///   escapes are decoded as the UTF-8 bytes they are (a name arrives
    ///   percent-encoded from a browser, where a backslash would be a path
    ///   separator).
    /// - Backslash escapes are unescaped — `\ `, `\(`, `\)`, `\[`, `\]`,
    ///   `\\`, and generally `\x` → `x` — because that is how a terminal
    ///   pastes a dragged file's shell-special characters.
    /// - An absolute name is used as it is; a relative one goes through
    ///   [`Self::resolve`], so `..` and an escape are refused (and come back as
    ///   `Ok(None)`: they are text, not an image that cannot ride). `~` is
    ///   *not* expanded and the process's own directory is not consulted — the
    ///   workspace was opened with a folder, and that folder is the only
    ///   meaning a bare relative name has here.
    ///
    /// The file must exist and its first bytes must sniff as an image. Not
    /// existing, not being a regular file (a FIFO is never opened — its open
    /// would block until a writer appeared), and not being an image are all
    /// `Ok(None)`: the paste is inserted as the text it is. Past
    /// [`IMAGE_FILE_CAP`] it is an image that cannot ride, and the `Err` is the
    /// read tool's own refusal sentence — one file refused at either door is
    /// refused with the same words.
    ///
    /// The path the returned image carries is the one fact a shed payload
    /// leaves behind (a trim or [`Session::save`](crate::session::Session::save)
    /// keeps the path and drops the bytes), and the placeholder it leaves
    /// promises the model that the file can be read again. The model's own
    /// tools reach nothing outside the root — the human's privilege to name any
    /// path does not extend to the model — so an image named from *outside* it
    /// is copied into `.mush/paste/` through `Self::write_pasted_image`, the
    /// same writer, directory, name and cap the clipboard road uses, and the
    /// *copy* is the image's path: an image the model may have to look at again
    /// is kept where the model's own tools can reach it. That is what makes a
    /// screenshot pasted from anywhere survive a restart the way a clipboard
    /// screenshot always has. The human's original is read, never moved or
    /// rewritten, and its path is not recorded: it is the human's file to keep,
    /// and the copy is what mush can promise to keep. Nothing is copied for a
    /// paste that already names a file inside the root — that name resolves, so
    /// it is kept exactly as the human gave it (workspace-relative for an
    /// absolute spelling).
    pub fn pasted_image(&self, paste: &str) -> Result<Option<Image>, String> {
        let Some(name) = pasted_name(paste) else {
            return Ok(None);
        };
        self.image_named(&name)
    }

    /// The images a paste names, when the paste is nothing but names of
    /// images — one or several. `Ok(None)` = "this is text, not images".
    ///
    /// This is [`Self::pasted_image`]'s rule generalised from one name to the
    /// words of a paste, because one gesture may name four: dragging four
    /// files out of a file manager is one paste, and one paste of image paths
    /// is one "these pictures" gesture the same way one path is one "this
    /// picture". What a name may be is [`Self::pasted_image`]'s rules, word by
    /// word; the one thing new here is where a word ends. A word is a run no
    /// *bare* whitespace interrupts: a space, tab or newline separates words,
    /// while `\ `, a quoted span (`"my shot.png"`), and a `file://` URL's own
    /// `%20` are the spellings a tool uses for a file's own space and stay
    /// inside the word they spell. A paste of one name is one word, and reads
    /// exactly as it did.
    ///
    /// The gesture is atomic, exactly as one name's is: a word that is not an
    /// image makes the *whole* paste text — `Ok(None)` — so prose is not
    /// hijacked by a path-shaped word in it, and a typo is visible rather than
    /// half the batch attaching. The words are tried in the order pasted, and
    /// reading stops at the first that is not an image (a sentence's first
    /// word usually is not a path, so the filesystem cost stays one failed
    /// open). An image past [`IMAGE_FILE_CAP`] is the same `Err` refusal the
    /// one-name door reads, and it stops the batch the same way: no image is
    /// attached, and the caller lands the words as text — a copy made from an
    /// outside name read before the word that made the paste text stays in
    /// `.mush/paste/`, which is where pastes live.
    ///
    /// The images keep the paste's order, so the box's rows — and the message
    /// that reaches the model — read the way the human pasted them. Nothing
    /// here caps the *count*: what bounds a paste of a hundred pictures is what
    /// bounds one image — the room the conversation has left and the window
    /// (the app's attach gate, whose lines say so), and the per-file cap above
    /// — not a batch limit invented at this door.
    ///
    /// Each image takes [`Self::pasted_image`]'s road, the copy included: a
    /// name outside the root is read where the human keeps it and written into
    /// `.mush/paste/` through `Self::write_pasted_image`, so every picture in
    /// the batch carries a path the model's own tools can resolve.
    pub fn pasted_images(&self, paste: &str) -> Result<Option<Vec<Image>>, String> {
        let Some(names) = pasted_names(paste) else {
            return Ok(None);
        };
        let mut images = Vec::with_capacity(names.len());
        for name in names {
            match self.image_named(&name)? {
                Some(image) => images.push(image),
                // One word that is not an image makes the whole paste the
                // words it is; the words after it are not even tried.
                None => return Ok(None),
            }
        }
        Ok(Some(images))
    }

    /// The image one word of a paste names, or `Ok(None)` when that word is
    /// not an image. The word arrives as [`pasted_name`] read it — one pair of
    /// surrounding quotes stripped, `file://` and `%XX` decoded, backslashes
    /// unescaped — and this is the rest of the parse, one name's worth: an
    /// absolute name is used as it is, a relative one goes through
    /// [`Self::resolve`] (a refused escape or `..` is text, not an image that
    /// cannot ride), and a name outside the root has its bytes copied into
    /// `.mush/paste/` through [`Self::write_pasted_image`] — the *copy* is the
    /// image's path, the one the model's own tools can read again.
    ///
    /// One helper for the one-name and the several-names doors, so a batch can
    /// never read a name differently from the single paste it generalises.
    fn image_named(&self, name: &str) -> Result<Option<Image>, String> {
        let path = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            match self.resolve(name) {
                Ok(path) => path,
                // An escape or a `..` is not a name this door opens; the paste
                // goes in the box as the words it is.
                Err(_) => return Ok(None),
            }
        };
        // The name the human gave, and the name the placeholder would keep. It
        // answers the copy question too: a name [`Self::resolve`] accepts is
        // one the model's own tools reach, and needs no copy.
        let label = self.rel(&path);
        let Some(image) = self.image_at(&path, &label)? else {
            return Ok(None);
        };
        if self.resolve(&label).is_ok() {
            return Ok(Some(image));
        }
        // Outside the root: the bytes are copied where the model can reach
        // them, and that copy — never the human's path — is what the image
        // carries from here on.
        let Image { bytes, mime, .. } = image;
        self.write_pasted_image(bytes, &mime)
            .map(Some)
            .map_err(|e| format!("cannot copy {label} into {PASTE_REL}: {e}"))
    }

    /// Write bytes that came from outside the workspace — the clipboard, or a
    /// picture the app is carrying to the agent that will receive it — into
    /// `.mush/paste/` and hand back the image that names them.
    ///
    /// A clipboard image has no path — the clipboard is a buffer, not a file —
    /// and [`Image`] carries one: it is the name a shed payload's placeholder
    /// keeps, so the bytes are given one here. A picture the app carries *has* a
    /// path, but one the receiving agent's own workspace does not resolve; what
    /// it needs is the same thing a clipboard image needs — a name under this
    /// root — so it is given one by the same call. `.mush/` ignores itself via
    /// its own `.gitignore` ([`session::ensure_mush_dir`]), so a pasted
    /// screenshot cannot dirty the tree, and the file is named for the moment
    /// it was pasted rather than for the clipboard, which would let a second
    /// paste overwrite the first. The write itself is
    /// `Self::write_pasted_image`, shared with the copy
    /// [`Self::pasted_image`] makes of a file outside the root, so no road
    /// into that directory can drift in where the bytes land or what the image
    /// is called.
    ///
    /// `Err` is "these bytes are an image, and they cannot ride": bytes that
    /// sniff as no image at all, or one past [`IMAGE_FILE_CAP`], whose refusal
    /// names the clipboard's own road (`wl-paste -t image/png > shot.png`,
    /// then a `convert` downscale) because that is the only one a clipboard
    /// image has. The app's carry cannot reach either refusal: it holds a
    /// picture a road has already sniffed and read whole within the cap, and
    /// passes `false` for `cut_at_the_cap`.
    ///
    /// `cut_at_the_cap` is the caller's own fact: true when its read stopped
    /// at its cap before the bytes ended, so `bytes` is a prefix of the picture
    /// and its length is the buffer's rather than the picture's. Such bytes are
    /// refused whatever any cap arithmetic says (a prefix cannot ride), and the
    /// refusal names the cap and no size, because an exact number that is
    /// really the buffer's is a lie about the human's picture. A caller holding
    /// the whole picture passes `false` and gets the exact-size sentence.
    pub fn save_pasted_image(&self, bytes: Vec<u8>, cut_at_the_cap: bool) -> Result<Image, String> {
        let Some(mime) = image_mime(&bytes) else {
            return Err(
                "the clipboard bytes are not a png, jpeg, gif or webp image — copy the picture \
                 itself, or paste the path of an image file"
                    .to_string(),
            );
        };
        if cut_at_the_cap || bytes.len() as u64 > IMAGE_FILE_CAP {
            let size = (!cut_at_the_cap).then_some(bytes.len() as u64);
            return Err(clipboard_image_too_big(mime, size));
        }
        self.write_pasted_image(bytes, mime)
    }

    /// Write `bytes` — sniffed by the caller as `mime` — into `.mush/paste/`
    /// and hand back the image that names the copy.
    ///
    /// The one write of that directory, shared by every road bytes from outside
    /// the workspace arrive by: the clipboard
    /// ([`Self::save_pasted_image`]), which has no file behind the bytes; a
    /// paste naming a file outside the root ([`Self::pasted_image`]), which
    /// the model's own tools cannot reach; and the app's carry, which reaches
    /// the write through [`Self::save_pasted_image`] with a picture a road has
    /// already read whole. Two writes would be two spellings of one rule — the
    /// directory, the `pasted-<unix millis>.<png|jpg|gif|webp>` name and the
    /// [`Image`] that points at it — so there is one. `.mush/` ignores itself
    /// via its own `.gitignore` ([`session::ensure_mush_dir`]), so neither
    /// paste can dirty the tree, and the name carries the moment it was pasted
    /// rather than the clipboard or the source file; a name already taken
    /// moves to `-2`, `-3`, …, so neither a second paste nor the next picture
    /// of one batch can overwrite the one before it.
    ///
    /// The two doors that read files have already refused a payload that is no
    /// image and one past [`IMAGE_FILE_CAP`], each with the sentence its own
    /// road can act on (the clipboard names `wl-paste`, a file names the
    /// `convert` downscale); the app's carry cannot reach either refusal,
    /// because it holds a picture a road has already sniffed and read whole
    /// within the cap. What must not differ is where under-cap bytes land.
    /// `Err` here is "the copy cannot be written": the IO problem the directory
    /// or the file named.
    ///
    /// The write is also where the directory is *bounded*: every paste road
    /// ends here, so this is the one place a prune has to sit, and the file
    /// just written is never one of its candidates ([`Self::prune_pastes`] says
    /// which pastes are). `.mush/paste/` therefore holds this run's pictures
    /// plus a day of the run before it, rather than every picture the workspace
    /// has ever pasted (finding B13).
    fn write_pasted_image(&self, bytes: Vec<u8>, mime: &str) -> Result<Image, String> {
        session::ensure_mush_dir(self.root())
            .map_err(|e| format!("cannot create {}: {e}", session::MUSH_DIR))?;
        let dir = paste_dir(self.root());
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {PASTE_REL}: {e}"))?;
        let (name, mut file) = create_paste_file(&dir, now_millis(), mime)?;
        file.write_all(&bytes)
            .map_err(|e| format!("cannot write {}: {e}", paste_rel(&name)))?;
        drop(file);
        self.prune_pastes(&name);
        let pixels = image_dimensions(mime, &bytes);
        Ok(Image::new(paste_rel(&name), mime, bytes, pixels))
    }

    /// Delete the pastes this run can no longer be reading, and answer the
    /// names that are gone.
    ///
    /// Every road that pastes runs this as it writes (see
    /// [`Self::write_pasted_image`]), which is what makes the directory a
    /// bound rather than a pile; the files a *live* transcript names are the
    /// ones it never takes. A candidate is decided by two facts, both of them
    /// read from the name the writer gave the file:
    ///
    /// - a paste whose moment is at or after this workspace's own open is this
    ///   run's, and a live transcript's images are exactly that — an [`Image`]
    ///   is in memory because this run read or wrote it — so it stays, however
    ///   long the run has been going. That is the conservative form of "never
    ///   remove one the live transcript still points at": the workspace cannot
    ///   see a transcript, and the run's start is the line it can draw without
    ///   one (finding B13);
    /// - a paste older than [`PASTE_MAX_AGE_MILLIS`] is one an earlier run left
    ///   behind, and past this run's own pictures it is the history a prune
    ///   takes.
    ///
    /// `just_written` is the name being written as the prune runs: it is live
    /// by definition, so it is never a candidate even if the wall clock moved
    /// backwards between the write and the prune. A file whose name is not
    /// `pasted-<moment>.<ext>` is not mush's, is not aged by this rule, and is
    /// left alone — the directory is mush's, but a file a human put there is
    /// not the prune's to delete.
    fn prune_pastes(&self, just_written: &str) -> Vec<String> {
        let dir = paste_dir(self.root());
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let now = now_millis();
        let mut gone = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(moment) = paste_moment(&name) else {
                continue;
            };
            if name == just_written
                || moment >= self.opened
                || now.saturating_sub(moment) <= PASTE_MAX_AGE_MILLIS
            {
                continue;
            }
            // A directory named like a paste is not a paste: only a regular
            // file is ever removed, and a refusal (permissions, a race with
            // the human) leaves the file where it was rather than failing the
            // write that pruned.
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            if fs::remove_file(dir.join(&name)).is_ok() {
                gone.push(name);
            }
        }
        gone
    }

    /// The image at `path`, named `name` in the refusal and in the returned
    /// [`Image`]: the mime its own first bytes name, the pixel size its own
    /// header names (what the picture costs the context window, as distinct
    /// from how large the file is), and the bytes whole — or `Ok(None)` when
    /// they are not an image's, so a caller reads the file as text (or, in
    /// [`Self::pasted_image`]'s case, as the words the paste is).
    ///
    /// One reader for the two doors a picture comes in by — the model's
    /// `read_file` and the human's paste — because a cap, a sniff and a
    /// refusal sentence that differed between them would be the same decision
    /// made twice, and the copies would drift.
    ///
    /// The order of the decisions is load-bearing, and each is made from a fact
    /// already in hand rather than from the read:
    ///
    /// - the file's *shape* comes from `fs::metadata` before anything is
    ///   opened. `File::open` on a FIFO blocks until a writer appears, and this
    ///   runs on the UI thread (the human's paste) and in an actor (the model's
    ///   read): a name as innocent as `x.png` must not be able to freeze
    ///   either. Only a regular file can be an image, and anything else is
    ///   `Ok(None)` before an open is attempted;
    /// - the *cap* comes from the metadata's length, before any whole read: a
    ///   256 MB blob that starts with a png signature is refused from its size,
    ///   not after being loaded to find the size out;
    /// - the sixteen-byte head is still sniffed before the whole file, because
    ///   an over-cap file that is *not* an image is text — the head decides
    ///   that, and the cap must not turn it into a refusal;
    /// - and the whole read is bounded by [`IMAGE_FILE_CAP`] + 1 bytes whatever
    ///   the stat said, because a file can grow between the stat and the read.
    ///   A read that hits the bound is refused without a size named
    ///   ([`image_too_big`]): the length in hand is the buffer's, not the
    ///   picture's.
    fn image_at(&self, path: &Path, name: &str) -> Result<Option<Image>, String> {
        // A directory opens but does not read (or does not open at all,
        // depending on the platform), and a FIFO's open blocks until a writer
        // appears. Neither is an image, and the stat says so without opening
        // anything.
        let Ok(meta) = fs::metadata(path) else {
            // Not there, or not statable: not an image, so the caller's other
            // reading of the path is the answer.
            return Ok(None);
        };
        if !meta.is_file() {
            return Ok(None);
        }
        let mut head = [0u8; 16];
        let Ok(mut file) = fs::File::open(path) else {
            return Ok(None);
        };
        let Ok(read) = file.read(&mut head) else {
            return Ok(None);
        };
        let Some(mime) = image_mime(&head[..read]) else {
            return Ok(None);
        };
        if meta.len() > IMAGE_FILE_CAP {
            return Err(image_too_big(name, mime, Some(meta.len())));
        }
        // The head is already read, so the rest is bounded to what is left of
        // the cap: `read_to_end` on a `take` never asks the disk for more than
        // that. A file that grew past the stat is caught by the length rather
        // than loaded.
        let mut bytes = head[..read].to_vec();
        let room = IMAGE_FILE_CAP + 1 - read as u64;
        file.take(room)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read {name}: {e}"))?;
        if bytes.len() as u64 > IMAGE_FILE_CAP {
            return Err(image_too_big(name, mime, None));
        }
        let pixels = image_dimensions(mime, &bytes);
        Ok(Some(Image::new(name, mime, bytes, pixels)))
    }

    /// The declarations of one file with their line numbers, for the `outline`
    /// tool and the unbounded read's fallback: the same lossy whole read the
    /// window road takes (`Self::whole_read`), so a file a model may read is a
    /// file it may sketch, and a path's refusals — not there, a directory,
    /// binary, past [`READ_FILE_CAP`] — are the window road's own sentences
    /// rather than a second set that could drift from them.
    ///
    /// The decode is lossy for the window road's reason: an outline is shown and
    /// never written back, and a Latin-1 config still has a shape worth seeing.
    /// It is also where a leading BOM goes: the lossy read drops a signature
    /// because it is not the first line's first character, so a `.cs` or `.ps1`
    /// a Windows editor wrote is outlined from the declarations it spells
    /// rather than missed on its first line. What the rule reads as a declaration, and the
    /// invariant that a row never lies about the line it names, live in
    /// [`crate::outline`] — this method supplies the read and nothing else. A
    /// file with no definitions is not an error: the sentence that says so is
    /// [`Outline::render`]'s.
    ///
    /// The rows the answer may hold are bounded by [`crate::outline::DEFAULT_ROOM`]
    /// — this crate's ceiling for a result the model reads ([`crate::CMD_CAP`]),
    /// because this road serves tool results and the app renders one at or below
    /// that ceiling. The file is *counted* whatever the room is, so a file with
    /// more declarations than the room answers with the exact count and a
    /// sentence naming the rows it did not keep rather than with a refusal or a
    /// silent cut. A caller that knows the cap it will render at — the app's
    /// tool roads know the exact `result_cap` their turn has left — passes its
    /// own room to [`Self::outline_within`] and spends proportionally less.
    pub fn outline(&self, rel: &str) -> Result<Outline, String> {
        self.outline_within(rel, crate::outline::DEFAULT_ROOM)
    }

    /// The same read and the same rule as [`Self::outline`], bounded by the
    /// caller's own room: the rows the answer it is building can show, at most.
    ///
    /// The room is the caller's because the cap is: the answer is rendered by
    /// whoever asked for it, and [`crate::outline::room_for_cap`] is the one
    /// conversion from a cap in bytes to a room in rows. A room too small for
    /// the file is not a lie and not a failure — the count is still the file's
    /// own, and `Outline::render`'s closing note names every row it left — and a
    /// room past what the cap can paint costs one row of memory and loses
    /// nothing.
    pub fn outline_within(&self, rel: &str, room: usize) -> Result<Outline, String> {
        let text = self
            .whole_read(rel, Decoding::Lossy)?
            .text_or_refusal(rel)?;
        Ok(Outline::within(rel, &text, room))
    }

    /// A window of a text file, as the model reads it: `limit` lines from
    /// 1-based `offset`, cut to `cap` bytes, then one sentence if there is a
    /// rest — how much of the file this was and the `offset` that reads on.
    ///
    /// The answer is a [`Window`], whose text is that window and whose flag is
    /// the same fact for a caller that has to act on it: whether the window
    /// stopped short of the file's end. The trailer is written for the model;
    /// the flag is how the app's read tool knows a cut happened without parsing
    /// mush's own sentence back out of the text (the unbounded-read fallback:
    /// a request for the whole file that the cap cut answers with the file's
    /// outline instead of its head).
    ///
    /// No line numbers are printed beside the text, on purpose: a model copies
    /// what it reads into `edit_file`'s `old_string`, and a numbered line is a
    /// string that cannot match. The range is named once, in the trailing
    /// sentence.
    ///
    /// The window's lines are a *reader's* lines: [`str::lines`] drops the
    /// `\r` of a CRLF ending, so what is copied out of a CRLF file's window is
    /// a line's text, never its bytes, and the lossy decode drops a leading BOM
    /// for the same reason: a signature is not a line's first character. That
    /// is said rather than hidden — a file whose lines all end with CRLF gets a
    /// sentence saying so, and [`edit`] refuses an edit whose strings hold a
    /// line break or a `\r` in such a file: a single-line edit lands byte for
    /// byte, and the road for anything across lines is `run_command`
    /// (`sed -i`, `perl -pi`) or `write_file` (finding B7). The two used to be
    /// silent and disagreed — a copied multi-line `old_string` could never
    /// match, and a one-line edit that did match inserted LF lines into the
    /// CRLF file.
    ///
    /// [`edit`]: crate::tools::edit_text
    ///
    /// The cap is the same whole-read cap as everywhere else ([`READ_FILE_CAP`],
    /// checked from the stat by `Self::whole_read` before a byte is read): this
    /// road cannot get past it, and the refusal is `over_read_cap`'s one
    /// sentence, naming `run_command` as the road to a part of the file.
    ///
    /// The decode is **lossy on purpose**: this road shows what a file holds and
    /// never writes it back, so a byte that is not valid UTF-8 is shown as
    /// U+FFFD rather than refused — a model that can see a Latin-1 config is a
    /// model that can convert it. The edit road is the one that must not decode
    /// what it cannot re-encode, and [`Self::read_file`] says so in its own doc
    /// (finding B6).
    pub fn read_window(
        &self,
        rel: &str,
        offset: usize,
        limit: usize,
        cap: usize,
    ) -> Result<Window, String> {
        let text = self
            .whole_read(rel, Decoding::Lossy)?
            .text_or_refusal(rel)?;
        let total = text.lines().count();
        let offset = offset.max(1);
        if limit == 0 {
            return Err(format!(
                "`limit` must be at least 1 line — {rel} has {total}"
            ));
        }
        if total == 0 {
            return Ok(Window {
                text: format!("{rel} is empty"),
                truncated: false,
            });
        }
        if offset > total {
            return Err(format!(
                "{rel} has {total} lines — offset {offset} is past its end"
            ));
        }
        // The window: the lines asked for, then as many of them as the cap pays
        // for. One line longer than the cap is still shown in part, because a
        // refusal to show anything is not a read.
        let mut shown = Vec::new();
        let mut bytes = 0usize;
        let mut part = false;
        for line in text.lines().skip(offset - 1).take(limit) {
            let width = line.len() + 1;
            if shown.is_empty() && width > cap {
                let cut = text::boundary_at_or_before(line, cap);
                shown.push(&line[..cut]);
                part = true;
                break;
            }
            if bytes + width > cap {
                break;
            }
            bytes += width;
            shown.push(line);
        }
        let last = offset + shown.len() - 1;
        let mut out = shown.join("\n");
        if text::is_crlf(&text) {
            // The sentence has one home ([`CRLF_NOTE`]) because the outline
            // road appends the same one under the rows it shows: a line copied
            // out of a CRLF file crosses the same ending rule either way.
            out.push('\n');
            out.push_str(CRLF_NOTE);
        }
        if part {
            out.push_str(&format!(
                "\n[mush: line {offset} of {total} is longer than the {cap}-byte cap — shown in \
                 part; run_command (`sed -n '{offset}p' {rel}`) prints the rest]"
            ));
        } else if last < total {
            out.push_str(&format!(
                "\n[mush: lines {offset}–{last} of {total} — read on with offset={}]",
                last + 1
            ));
        } else if offset > 1 {
            out.push_str(&format!(
                "\n[mush: lines {offset}–{last} of {total} — end of file]"
            ));
        }
        Ok(Window {
            text: out,
            // The two trailers that mean "there is more": a window that stopped
            // before the file's end, and a single line the cap showed in part.
            // The CRLF note and the end-of-file line are *not* cuts.
            truncated: part || last < total,
        })
    }

    /// Every file under `rel` (default the workspace root), workspace-relative,
    /// in the order `Self::walk` reaches them, with the first `limit`,
    /// whether there were more, and how many of the names it reached cannot
    /// travel on the model's road (`Self::name_for_model`). Build and VCS
    /// directories are skipped (`SKIP_DIRS`) — mush's own `.mush` among them,
    /// so a listing is the work and not the bookkeeping — and so is the
    /// workspace's worktree directory (`Self::is_worktree_path`); a symlinked
    /// directory is not followed, so a listing cannot leave the workspace.
    ///
    /// The cap ends the walk where it lands rather than cutting a whole answer
    /// at the end: `list_files("")` used to visit every file under the root,
    /// allocate every name and sort them all, to print the first four hundred —
    /// the cost was the tree's, not the answer's (finding IN9), where
    /// [`Self::search`] has always stopped its walk at its own cap. The order
    /// of the answer is then the walk's own — each directory's files in name
    /// order, its subdirectories after them, depth-first — not a global sort's:
    /// the two cannot both hold, because a global sort is exactly what needs
    /// the whole tree. `Self::walk` reads each directory in name order, so
    /// the place the cap stops the walk is deterministic.
    ///
    /// That claim is why the name is checked for real before the walk
    /// (`Self::real_path`): `out -> /tmp/elsewhere` is a name inside the root
    /// whose listing used to be the outside directory's. The walk itself still
    /// runs on the name the model gave, so a link to a *file* inside the root
    /// answers about that file under the name it was asked about, while a link
    /// to a directory is left alone like every other symlinked directory.
    ///
    /// The third value is why the listing does not hand over a name it cannot
    /// open again: a name holding a line break reads as two entries, and a name
    /// `resolve` would trim names a different file — both are counted instead
    /// of reported, and the tool layer says how many and where they can be
    /// reached (finding B9). The listing's own shape must not make a file
    /// unreachable, or hand one over that is not there. The count stops with
    /// the walk: a file the cap never reached is counted by the cap's own note
    /// ("the first N files") rather than guessed at past it.
    pub fn list_files(
        &self,
        rel: &str,
        limit: usize,
    ) -> Result<(Vec<String>, bool, usize), String> {
        let start = self.resolve(rel)?;
        self.real_path(&start, rel)?;
        // "Empty" and "not there" are different facts, and a listing that
        // answers `no files` for a path that does not exist is a lie the model
        // cannot see through.
        if fs::symlink_metadata(&start).is_err() {
            return Err(format!("no such path: `{rel}`"));
        }
        let mut found: Vec<String> = Vec::new();
        let mut unnamed = 0usize;
        self.walk(&start, &mut |path: &Path| {
            match self.name_for_model(path) {
                Some(name) => {
                    found.push(name);
                    // The cap ends the walk where it lands. The name that made
                    // the list one over the cap is the whole answer to "were
                    // there more"; walking the rest would be the tree's cost,
                    // not the answer's.
                    found.len() <= limit
                }
                None => {
                    unnamed += 1;
                    true
                }
            }
        });
        let truncated = found.len() > limit;
        found.truncate(limit);
        Ok((found, truncated, unnamed))
    }

    /// Every line under `rel` matching `pattern` — a regex, compiled once
    /// before the walk — as rows of `path:line: text`, capped at `limit` rows
    /// plus the fact that there were more.
    ///
    /// The pattern *is* the whole syntax (the human's decision, reversing the
    /// literal this doc used to argue for): there is no flag argument to set,
    /// `(?i)` is how case-insensitivity is said in the pattern itself, and a
    /// metacharacter meant literally is escaped. The literal could not ask for
    /// "any digit" at all, and the old argument against an engine — `rg` is
    /// the shell's — is weakest exactly where this tool is needed: a held
    /// machine lock refuses `run_command`. The engine is `regex-lite` rather
    /// than the full `regex` on the same weighing — a measured +99 KB of
    /// release binary and +0.5 s of build against +1.5 MB and +16.5 s — at
    /// the cost of two syntax differences the schema states rather than lets a
    /// model discover: no `\p{…}` classes, and `\w`/`\b` are ASCII-only.
    ///
    /// **A row is a match's line or a neighbour of one.** `context` is how many
    /// lines either side of each match the answer shows, like `rg -C`, and it
    /// is clamped to [`SEARCH_CONTEXT_MAX`] rather than refused: a giant
    /// context is a real request, and "as many as fit" is its honest answer —
    /// past ten either side the question is what the whole file holds, and
    /// `read_file` is the road. The cap counts **rows**, not matches, and that
    /// is the same decision seen from the other side: `limit` is the answer's
    /// own size, so a `context` that could grow the answer with every match
    /// would spend the whole result on one window's surroundings and hide the
    /// matches the search exists for. With `context = 0` every row is a match
    /// and the answer is byte for byte the one this tool has always given;
    /// with a context, [`Matches::more`] keeps its meaning — there was one
    /// more row than could be shown, and the row that proved it is not shown.
    ///
    /// The two shapes are grep's own, and that familiarity is the argument:
    /// match rows stay exactly `path:line: text`, and a context row is
    /// `path-line- text` — the separators are the whole difference, so a reader
    /// of `rg -C` output reads the answer without learning a second grammar,
    /// and a reader that keeps only `path:line:` rows keeps the matches. A
    /// context row is numbered as the file numbers it, so it is still the
    /// anchor a `read_file {offset}` is built from.
    ///
    /// **One window per run of rows, not one per match.** Two matches close
    /// enough that their windows touch share one window — three lines apart
    /// with `context = 2` is one run — so no line is emitted twice, and a
    /// window is clipped to the file's first and last line, so a match on line
    /// 1 opens at line 1. The file's lines are walked once: each is matched,
    /// held only while it could still become a hit's pre-context (at most
    /// `context` of them, as slices of the file's own text, not copies), and
    /// emitted exactly once, as a match row or a context row according to the
    /// window state it lands in. Gathering every matching line number first
    /// would hold a minified file's million numbers to emit a window of ten:
    /// the bound is the answer's, the same trade
    /// [`crate::usages::rows_within`] makes.
    ///
    /// The match is per line and stays per line ([`text::file_lines`] is the
    /// haystack, so a huge file is never held whole): `^` and `$` anchor a
    /// line, and a `\n` in the pattern can never match. Binary files (a NUL
    /// byte) and files past [`SEARCH_FILE_CAP`] are skipped — the read is
    /// bounded to the cap + 1 like `Self::whole_read`'s, so a file that grew
    /// behind the stat is caught by its length rather than loaded whole — and a
    /// row's line is cut to `MATCH_LINE_CAP` bytes with the cut said, so one
    /// minified file cannot spend the result.
    ///
    /// A pattern the engine refuses is a refusal, not a "no match": the parse
    /// error travels back and the model fixes the pattern instead of reading a
    /// silent miss, and no file is opened to find that out. An empty pattern is
    /// refused for [`Self::usages`]'s reason — the empty regex matches every
    /// line, so a patternless call would be a capped listing rather than a
    /// search.
    ///
    /// A row's line is the *file's* line: no paint-time sanitizing, no
    /// `trim_end`, and a CRLF ending's `\r` stays ([`text::file_lines`]). This
    /// is a model road, and the model's roads are data — a line a search shows
    /// that the file does not hold is a line the model copies into an
    /// `old_string` and reads "not found" (finding B8). The pane that paints
    /// the result sanitizes its own copy ([`text::sanitize`]), which is where
    /// the escape sequences go.
    ///
    /// The decode is **lossy on purpose**, like [`Self::read_window`]'s and for
    /// the same reason: a search only shows what it found, so a file in another
    /// encoding is searched as U+FFFD rather than skipped — a Latin-1 config the
    /// model can still find a symbol in is worth more than a miss the model
    /// cannot see through. A file that is not valid UTF-8 is not "binary" here;
    /// the skip counter is for the files (blobs, NUL-bearing) that have no text
    /// to search at all.
    ///
    /// What it skipped is counted and travels back with the rows
    /// ([`Matches::skipped`]): a search that says "no match" while it never
    /// opened a file is a false negative a model will act on. Files whose name
    /// cannot travel on the model's road (`Self::name_for_model`) are not
    /// opened either, and are counted the same way ([`Matches::unnamed`]) — a
    /// row is prefixed with the path, and a path the model cannot pass
    /// back to `read_file` would be a dead end (finding B9).
    ///
    /// Like the listing, the name is checked for real before the walk
    /// (`Self::real_path`): a link inside the root cannot make the search
    /// read files outside it, and the files it does read are the ones under
    /// the name the model gave.
    pub fn search(
        &self,
        pattern: &str,
        rel: &str,
        limit: usize,
        context: usize,
    ) -> Result<Matches, String> {
        if pattern.is_empty() {
            return Err("`pattern` must not be empty".to_string());
        }
        // Compiled once, before the walk: a bad pattern is an argument error
        // the model can fix, not a fact about a file, and no file is opened to
        // find it out.
        let regex = regex_lite::Regex::new(pattern)
            .map_err(|err| format!("`pattern` is not a valid regex: {err}"))?;
        let start = self.resolve(rel)?;
        self.real_path(&start, rel)?;
        if fs::symlink_metadata(&start).is_err() {
            return Err(format!("no such path: `{rel}`"));
        }
        // A giant context is clamped, not refused: the ask is real and a
        // window of "as many as fit" answers it honestly. The clamp is here
        // and not in the schema because a caller that forgets it must still be
        // bounded.
        let context = context.min(SEARCH_CONTEXT_MAX);
        let mut rows = Vec::new();
        let mut more = false;
        let mut skipped = 0usize;
        let mut unnamed = 0usize;
        self.walk(&start, &mut |path: &Path| {
            // The name is needed before the file is opened: it is the row's
            // prefix, and a name that cannot travel is not searched.
            let Some(name) = self.name_for_model(path) else {
                unnamed += 1;
                return true;
            };
            let Ok(meta) = fs::metadata(path) else {
                skipped += 1;
                return true;
            };
            if meta.len() > SEARCH_FILE_CAP {
                skipped += 1;
                return true;
            }
            // Bounded like the other two readers: the stat above saw a file
            // within the cap, and one that grew past it since is a skip rather
            // than a whole load.
            let Ok(bytes) = read_bounded(path, SEARCH_FILE_CAP) else {
                skipped += 1;
                return true;
            };
            if bytes.len() as u64 > SEARCH_FILE_CAP {
                skipped += 1;
                return true;
            }
            if bytes.contains(&0) {
                skipped += 1;
                return true;
            }
            let text = String::from_utf8_lossy(&bytes);
            // The room the answer has left is the room this file's rows are
            // built against: `search_rows` answers one row past it when there
            // was more, which is this walk's proof, not the file's cost.
            let room = limit.saturating_sub(rows.len());
            rows.extend(search_rows(&regex, &name, &text, room, context));
            if rows.len() > limit {
                // The cap's own row is the proof of "there is more": it is not
                // shown, `more` is set, and the walk ends here.
                rows.truncate(limit);
                more = true;
                return false;
            }
            true
        });
        Ok(Matches {
            rows,
            more,
            skipped,
            unnamed,
        })
    }

    /// Every line under the workspace root that mentions `symbol` at a word
    /// boundary, grouped by file — the `usages` tool's walk.
    ///
    /// The walk is [`Self::search`]'s, and a line's rows are
    /// [`crate::usages::rows_within`]': the same `SKIP_DIRS`, [`SEARCH_FILE_CAP`],
    /// bounded read, binary skip, lossy decode, `name_for_model` rule and cap
    /// semantics, because a second walker would be a second answer to "what is
    /// a workspace file, and what may a result read?" and the two would drift.
    /// What differs is the match and the shape of the answer: a word instead of
    /// a substring, and rows grouped by file with the declaration-looking ones
    /// first. [`crate::usages`] owns those two rules and argues them.
    ///
    /// The symbol is a *word*, and this door says so twice. The empty needle is
    /// refused because it matches at every position of every line — a rule that
    /// cannot walk (`is_usage` would answer `Some(0)` forever) — and a needle
    /// holding a line break is refused because a line never holds one, so the
    /// walk could only ever answer the miss it would spend the whole tree
    /// proving. A `\r` is *not* refused: a lone carriage return really is a
    /// line's text, on a file whose lines end without a line feed or in the
    /// middle of one.
    ///
    /// The name is needed before the file is opened for `search`'s reason: a
    /// row under a name the model cannot pass back to `read_file` is a dead
    /// end, so such files are counted in [`Usages::unnamed`] and never
    /// searched. Files the walk never opened (binary, past the cap, unreadable)
    /// are counted in [`Usages::skipped`]; the files it *did* read are counted
    /// in [`Usages::scanned`], because a miss that says how much it read is a
    /// smaller claim than "no match" — and the claim a miss makes is the whole
    /// reason these counters exist.
    ///
    /// The cap stops the walk where it lands, like the search's and the
    /// listing's: the first row the cap cannot keep sets [`Usages::more`] and
    /// ends the walk. The row that made the answer one over the cap is the
    /// whole proof that there was more, and walking on past it would be the
    /// tree's cost rather than the answer's. The bound holds inside one file
    /// too ([`crate::usages::rows_within`]): the room left in the answer is
    /// what a file's rows are built against, so a file holding a row on every
    /// one of a million lines costs the answer and not the file.
    pub fn usages(&self, symbol: &str, limit: usize) -> Result<Usages, String> {
        if symbol.is_empty() {
            return Err("`symbol` must not be empty".to_string());
        }
        if symbol.contains('\n') {
            return Err(
                "`symbol` must be one line's text — a line never holds a line break, so this \
                 symbol can never be a word on one; run_command (`rg -U`) is the road for a \
                 pattern across lines"
                    .to_string(),
            );
        }
        let mut found = Usages::default();
        self.walk(&self.root, &mut |path: &Path| {
            // The name is needed before the file is opened: it is the group's
            // prefix, and a name that cannot travel is not searched.
            let Some(name) = self.name_for_model(path) else {
                found.unnamed += 1;
                return true;
            };
            let Ok(meta) = fs::metadata(path) else {
                found.skipped += 1;
                return true;
            };
            if meta.len() > SEARCH_FILE_CAP {
                found.skipped += 1;
                return true;
            }
            // Bounded like the other readers: the stat saw a file within the
            // cap, and one that grew past it since is a skip rather than a
            // whole load.
            let Ok(bytes) = read_bounded(path, SEARCH_FILE_CAP) else {
                found.skipped += 1;
                return true;
            };
            if bytes.len() as u64 > SEARCH_FILE_CAP {
                found.skipped += 1;
                return true;
            }
            if bytes.contains(&0) {
                found.skipped += 1;
                return true;
            }
            found.scanned += 1;
            let text = String::from_utf8_lossy(&bytes);
            // The room the answer has left is the room the file's rows are
            // built against: `rows_within` answers one row past it when there
            // was more, which is this walk's proof, not the file's cost.
            let room = limit.saturating_sub(found.hits());
            let mut rows = crate::usages::rows_within(symbol, &text, room);
            if rows.len() > room {
                // The cap's own row is the proof of "there is more": nothing
                // of it is shown, `more` is set, and the walk ends here.
                rows.truncate(room);
                found.more = true;
                if !rows.is_empty() {
                    found.groups.push(FileUsages { file: name, rows });
                }
                return false;
            }
            if rows.is_empty() {
                return true;
            }
            found.groups.push(FileUsages { file: name, rows });
            true
        });
        Ok(found)
    }

    /// Walk every file under `start` — files only, [`SKIP_DIRS`] by name, no
    /// symlinked directories, and never this workspace's worktree directory
    /// ([`Self::is_worktree_path`]) — calling `visit` until it answers
    /// `false`. A `start` that is itself a file visits that one file, so "list this path"
    /// and "search this path" answer about the file the model named instead of
    /// claiming there is nothing there. The start's own type is asked with
    /// `symlink_metadata`, like every child's is: `is_file` would follow a
    /// symlink and make the *start* the one symlinked directory the walk
    /// follows. A symlinked start that resolves to a *file* is still answered
    /// about — naming a file is a question about that file — while a symlinked
    /// directory is left alone exactly as it would be a level down, and
    /// "a symlinked directory is not followed" is then true of the start too.
    /// (The roads that call this check the name with [`Self::real_path`] first,
    /// so a link that leaves the root never reaches here.)
    ///
    /// One walker for the listing and the search: a second one is a second
    /// answer to "what is a workspace file", and the two drift.
    fn walk(&self, start: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
        let Ok(kind) = fs::symlink_metadata(start) else {
            return;
        };
        let kind = if kind.file_type().is_symlink() {
            // `metadata`, not `symlink_metadata`, only to tell a link to a file
            // (answer about it) from a link to anything else (do not follow).
            match fs::metadata(start) {
                Ok(target) if target.is_file() => target,
                _ => return,
            }
        } else {
            kind
        };
        if kind.is_file() {
            visit(start);
            return;
        }
        if !kind.is_dir() {
            return;
        }
        let mut stack = vec![start.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .map(|e| e.path())
                .collect();
            // Sorted here, not at the end: every directory is read in name
            // order, so a walk that stops at a cap stops at a *deterministic*
            // place instead of wherever the filesystem happened to list.
            children.sort();
            let mut next = Vec::new();
            for path in children {
                let Ok(kind) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if kind.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if !SKIP_DIRS.contains(&name.as_ref()) && !self.is_worktree_path(&path) {
                        next.push(path);
                    }
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                if !visit(&path) {
                    return;
                }
            }
            // Depth-first, and in name order: the stack takes the directories
            // reversed, so the first one read is the first one pushed.
            while let Some(path) = next.pop() {
                stack.push(path);
            }
        }
    }

    /// Whether a workspace-relative path exists. The fact `write_file` answers
    /// "(new)" from: a file it is about to replace is a file whatever its
    /// bytes are, and "new" over a blob is a false history fact.
    pub fn exists(&self, rel: &str) -> bool {
        self.resolve(rel)
            .map(|path| path.symlink_metadata().is_ok())
            .unwrap_or(false)
    }

    /// Atomically create or replace a file, creating parent directories.
    ///
    /// The name is resolved to what it really is before anything is made (see
    /// `Self::real_path`): a write through a symlink lands in the file the
    /// link points at and the link stays a link, while a name whose real path
    /// leaves the root is refused rather than followed. What the name *is*
    /// decides the rest: a socket, a FIFO or a device is refused rather than
    /// renamed over (`entry_for_write`), and a file with no owner-write bit
    /// is refused with the mode it has, so a `0444` file the human marked
    /// read-only is a sentence the model can read instead of an override it
    /// cannot see. The mode refusal lives here, at the model's door, and not in
    /// [`atomic_write`], which `session::save` and the human's own
    /// `config.json` writer also use: they may replace a file whatever its
    /// mode, but a model may not. The type refusal is shared with
    /// [`atomic_write`], because a rename over a socket destroys it whatever
    /// door it came through.
    ///
    /// The store's own files are refused *by name* (`Self::store_file_refusal`)
    /// on top of all that. They are regular files by construction, so the
    /// shape check cannot see them, and what a rename over one costs is not
    /// bytes but the workspace's own bookkeeping: `.mush/lock` is the flock
    /// that keeps two mushes off one store (finding E2), and `.mush/session.json`
    /// is the conversation. An ordinary file under `.mush/` is not one of
    /// those names — the human's pastes live in `.mush/paste/`, a note is a
    /// note — so it still writes; the refusal is the store's own names, not
    /// the directory.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<(), String> {
        let path = self.real_path(&self.resolve(rel)?, rel)?;
        if path == self.root {
            return Err("refusing to write to the workspace root".to_string());
        }
        if let Some(refusal) = self.store_file_refusal(&path, rel) {
            return Err(refusal);
        }
        let entry = entry_for_write(&path, rel)?;
        if let Ok(meta) = fs::metadata(&entry) {
            let mode = meta.permissions().mode() & 0o7777;
            if mode & 0o200 == 0 {
                return Err(format!(
                    "{rel} is mode {mode:04o} — it has no owner-write bit, and mush will not \
                     override that: `chmod u+w {rel}` first"
                ));
            }
        }
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", self.rel(parent)))?;
        }
        atomic_write(&entry, content.as_bytes(), Fresh::Box)
            .map_err(|e| format!("cannot write {rel}: {e}"))
    }

    /// The refusal a write to `real` gets when it would replace one of the
    /// store's own files ([`session::store_file`]), or `None` when it would
    /// not.
    ///
    /// The question is asked of the *real* path — what [`Self::real_path`]
    /// answered — and not of the name the model typed, because a link inside
    /// the workspace that points at `.mush/session.json` would otherwise be a
    /// write to that file under another name. Only the store's own level
    /// counts: `.mush/paste/lock` is a pasted picture that happens to be named
    /// `lock`, not the lock, and the files under `.mush/`'s subdirectories are
    /// mush's only in the sense that mush made the directory.
    fn store_file_refusal(&self, real: &Path, name: &str) -> Option<String> {
        let rel = real.strip_prefix(session::mushroom_dir(&self.root)).ok()?;
        if rel.components().count() != 1 {
            return None;
        }
        let why = session::store_file(rel.to_str()?)?;
        Some(format!("{name} is {why}; refusing to replace it"))
    }
}

/// How a whole-file read treats bytes that are not valid UTF-8.
///
/// Two roads read a file whole and they read it for different reasons, so they
/// answer the same bytes differently. The *edit* road ([`Workspace::read_file`])
/// is the one whose result is written back: a lossy decode there would rewrite
/// every byte it could not decode, in lines the model never touched, so it is
/// strict and refuses. The *window* road ([`Workspace::read_window`]) shows
/// what it read and never writes it back, so it decodes lossily: the
/// alternative is refusing to show the model a file it can still convert with
/// `run_command`. [`Workspace::search`] makes the same lossy choice for the
/// same reason, one file at a time, and says so in its own doc.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decoding {
    /// Refuse bytes that are not UTF-8 ([`not_utf8`]): what is read this way
    /// may be written back.
    Strict,
    /// Show them as U+FFFD, and drop the leading BOM when the file opens with
    /// one: what is read this way is shown, never written back, and a shown
    /// text does not keep a *signature* as if it were a character. A Windows
    /// editor's `\u{feff}fn f() {}` otherwise read as a first word that is
    /// neither `fn` nor a name, and the outline answered a miss for a
    /// declaration the file plainly holds ([`text::strip_bom`]). The strict
    /// road keeps every byte because its text is the edit's source *and* its
    /// result (finding B6).
    Lossy,
}

/// What a whole-file text read found. One read behind three roads — the strict
/// edit read ([`Workspace::read_file`]), the lossy window read
/// ([`Workspace::read_window`]) and the bounded count a `write_file` answer
/// carries ([`Workspace::line_count`]) — needs the same facts, so they are
/// decided once in [`Workspace::whole_read`] and each road turns them into its
/// own sentence. `Err` is reserved for the I/O with no decision in it (the path
/// does not resolve, `open` or `read` failed).
enum WholeRead {
    /// The file is text, decoded by the road's [`Decoding`].
    Text(String),
    /// Past [`READ_FILE_CAP`]. `Some(len)` is the file's own stat; `None` is a
    /// file that grew past the cap between the stat and the read, where the
    /// only length in hand is the buffer's and naming it would put a false
    /// number on the file (the same shape [`image_too_big`] uses).
    PastCap(Option<u64>),
    /// Not a regular file: a directory, a FIFO, a device.
    NotRegular,
    /// A NUL byte, on either decoding: binary whatever the encoding is, and a
    /// NUL is valid UTF-8, so the encoding check alone would let a blob past.
    Binary,
    /// The bytes are not valid UTF-8; the offset is the first byte that does
    /// not decode. The strict road refuses; the lossy road never sees this.
    NotUtf8(usize),
}

impl WholeRead {
    /// The whole read as text, or the sentence its caller reads. Both decoding
    /// roads map through here so neither spells a refusal of its own; the one
    /// arm a lossy read cannot reach — invalid UTF-8, which it decodes — is the
    /// strict road's refusal, answered rather than panicked on.
    fn text_or_refusal(self, rel: &str) -> Result<String, String> {
        match self {
            WholeRead::Text(text) => Ok(text),
            WholeRead::PastCap(size) => Err(over_read_cap(rel, size)),
            WholeRead::NotRegular => Err(format!("{rel} is not a regular file — cannot read it")),
            WholeRead::Binary => Err(format!("{rel} looks like a binary file")),
            WholeRead::NotUtf8(first_bad) => Err(not_utf8(rel, first_bad)),
        }
    }
}

/// The one refusal a read past [`READ_FILE_CAP`] gets, whichever road asked:
/// [`Workspace::read_file`] and [`Workspace::read_window`] are one whole read,
/// and a window cannot get past the cap either (the file is opened whole
/// first). One spelling, because it is one fact, and one road that works —
/// `run_command` reading part of the file.
///
/// `size` is the file's own length when the stat named it, and `None` when a
/// read bounded by the cap hit the bound before the file's end — the length in
/// hand is the buffer's, and naming it would put a number on the file that is
/// not its own (the same shape [`image_too_big`] uses).
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

/// The refusal a strict whole-file read gives bytes that are not valid UTF-8:
/// the edit road is the one whose text is written back, and a decode that
/// cannot keep a byte cannot edit the file without rewriting it — every
/// undecodable byte becomes U+FFFD, in lines the model never named (finding B6,
/// a Latin-1 `caf\xe9` that came back U+FFFD). `first_bad` is where the decode
/// stopped, which is where the encoding shows itself (offset 3 of a `caf\xe9`
/// is the `\xe9`). The road is a converter in the shell, which can be told the
/// encoding the model guessed: the bytes must be converted first, not edited
/// through.
fn not_utf8(rel: &str, first_bad: usize) -> String {
    format!(
        "{rel} is not valid UTF-8 — the first byte that does not decode is at offset {first_bad}, \
         so this is a file in another encoding (a Latin-1 config, a Shift-JIS note), and an edit \
         would rewrite every byte it cannot decode as U+FFFD, in lines the model never touched. \
         Every byte is left as it was: convert it first with run_command (`iconv -f ISO-8859-1 -t \
         UTF-8 {rel} > {rel}.utf8`), or change it with a tool that knows its encoding"
    )
}

/// The one refusal an image past [`IMAGE_FILE_CAP`] gets, whichever door it
/// came in by. One spelling, because it is one fact — the picture is too big
/// to travel — and one road, because `offset`/`limit` are lines and an image
/// has none: nothing but a downscale makes it readable.
///
/// `size` is the picture's length when the caller knows it — the file's own
/// stat, or the whole of a buffer — and `None` when a read stopped at the cap
/// before the file's end, where the length in hand is the buffer's and naming
/// it would put a number on the human's picture that is not its own.
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

/// The one refusal clipboard bytes past [`IMAGE_FILE_CAP`] get, whatever road
/// read them: the sentence names the clipboard's own road (`wl-paste -t
/// image/png > shot.png`, then a `convert` downscale) because that is the only
/// one a clipboard image has.
///
/// `size` is the picture's length when the caller knows it — the whole picture
/// was held — and `None` when the caller's own read stopped at its cap before
/// the picture ended, where the length in hand is the buffer's and naming it
/// would put a false number on the human's picture. The two sentences share
/// every word but that one clause, because they are one refusal.
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

/// The name one *word* of a paste may be, or `None` when that word is no name.
/// The whole parse of a pasted path lives here, so [`Workspace::pasted_image`]
/// (one name) and [`Workspace::pasted_images`] (the words [`pasted_names`]
/// splits) read as their rules rather than as their arithmetic; see
/// [`Workspace::pasted_image`] for why each rule is what it is.
fn pasted_name(paste: &str) -> Option<String> {
    let trimmed = paste.trim();
    if trimmed.is_empty() || trimmed.contains(['\n', '\r']) {
        return None;
    }
    let (name, quoted) = strip_quotes(trimmed);
    // A bare space is the separator between words; `\ ` and `%20` are how a
    // tool spells a file's own space, and a quoted paste is a tool saying the
    // whole string is one name. Any other whitespace is prose as well.
    if !quoted && has_bare_space(name) {
        return None;
    }
    let name = match name.strip_prefix("file://") {
        Some(rest) => {
            // `file://localhost/tmp/x` and `file:///tmp/x` are the same path:
            // an empty or `localhost` host is the local machine, and the path
            // begins at the first `/` that follows.
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            percent_decode(rest)?
        }
        None => name.to_string(),
    };
    let name = unescape_backslashes(&name);
    (!name.is_empty()).then_some(name)
}

/// The names a paste holds, or `None` when the paste is text: the split is
/// [`split_words`]'s, and every word is parsed by [`pasted_name`] — the
/// one-name rules, word by word — so a paste of several names can never read a
/// word differently from the single-name door. An empty paste, and a word that
/// is no name at all (an empty pair of quotes, a newline inside one), make the
/// whole paste text.
fn pasted_names(paste: &str) -> Option<Vec<String>> {
    let words = split_words(paste.trim());
    if words.is_empty() {
        return None;
    }
    words.into_iter().map(pasted_name).collect()
}

/// Split a paste at the whitespace *between* its words: a space, tab or
/// newline no backslash escaped and no quote holds separates two words, which
/// is the one-name door's whitespace rule — "interior whitespace that no
/// backslash escaped makes it prose" — read as the separator it is once more
/// than one name is allowed.
///
/// The words come back as the paste spelled them — quotes, escapes and all —
/// because each is parsed by the one-name rules, which strip and unescape what
/// they find. A quote opens a span wherever a word could start or continue
/// (`'` in `it's.png` holds the rest of the paste, which makes it one word
/// where a lone name would be one), and only whitespace *outside* a span and
/// unescaped splits.
fn split_words(paste: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (at, ch) in paste.char_indices() {
        if escaped {
            // The character a backslash escaped: part of the word, never a
            // separator or a quote (`start` was set when the backslash was
            // read).
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
            start.get_or_insert(at);
        } else if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            start.get_or_insert(at);
        } else if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                words.push(&paste[begin..at]);
            }
        } else {
            start.get_or_insert(at);
        }
    }
    if let Some(begin) = start {
        words.push(&paste[begin..]);
    }
    words
}

/// One pair of matching surrounding quotes, and whether they were there.
fn strip_quotes(text: &str) -> (&str, bool) {
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return (&text[quote.len_utf8()..text.len() - quote.len_utf8()], true);
        }
    }
    (text, false)
}

/// Whether any whitespace in `name` is unescaped: the test that tells a dragged
/// file from a sentence.
fn has_bare_space(name: &str) -> bool {
    let mut escaped = false;
    for ch in name.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            return true;
        }
    }
    false
}

/// `%XX` decoded into its bytes and joined back as UTF-8, so a name that
/// arrived as several escapes (`%C3%A9`) is the character it spells. A `%`
/// that is not followed by two hex digits is kept as it is — it is a legal
/// character in a file name, and a paste from a file manager is not encoded at
/// all — and bytes that are not UTF-8 make the whole paste text, because a name
/// that cannot be spelled is not a name.
fn percent_decode(text: &str) -> Option<String> {
    fn hex(byte: u8) -> Option<u8> {
        (byte as char).to_digit(16).map(|digit| digit as u8)
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                out.push(high << 4 | low);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8(out).ok()
}

/// `\x` → `x` — the escapes a terminal puts in a dragged file's name, the
/// space above all. A backslash with nothing after it is kept: it is a legal
/// character in a name, and dropping it would silently rename the file.
fn unescape_backslashes(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// The extension a saved clipboard image is written with: the mime's own name,
/// but `jpg` for jpeg, which is the spelling a file browser and `convert` both
/// read.
fn pasted_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

/// Unix milliseconds, the tail of a saved clipboard image's name: two pastes
/// are two files, and a name that sorts by when it happened is the one fact
/// about a clipboard image the clipboard itself does not have.
///
/// It is also the fact a prune ages a paste by ([`Workspace::prune_pastes`]),
/// read back out of the name with [`paste_moment`] — the name is the one place
/// the moment is written, so it is the one place it is read.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

/// How long a paste outlives the run that wrote it, in milliseconds.
///
/// A paste may still be named by a transcript that has been reloaded from disk,
/// so mush does not delete one the moment the run that made it ends: a file is
/// a candidate only when that run is over *and* it is older than this. One day,
/// because a working day is the shortest window in which "the picture from this
/// morning" is still being asked about; a prune never takes a paste the
/// *current* run wrote, however old its name says it is
/// (`Workspace::prune_pastes`).
pub const PASTE_MAX_AGE_MILLIS: u128 = 24 * 60 * 60 * 1000;

/// The moment a paste's name carries, in unix milliseconds — the number
/// [`create_paste_file`] writes after `pasted-`, before the extension or the
/// `-<n>` a same-millisecond collision adds — or `None` when the name is not
/// one mush wrote.
///
/// The name *is* the paste's age for a prune: it is the moment the writer
/// chose, it is the same on every machine that sees the file, and it is the one
/// a test can set without owning a wall clock or an `mtime` API. A name that is
/// not this shape is not mush's to age, which is why it answers `None` rather
/// than a guess.
fn paste_moment(name: &str) -> Option<u128> {
    let rest = name.strip_prefix("pasted-")?;
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Where pasted pictures live under a workspace, as the one spelling every
/// road that names the directory reads: the join that creates it, the path an
/// [`Image`] carries, and every message about a paste.
///
/// The spelling is one because two write failures used to say `.mush/<name>`
/// for a file whose path is `.mush/paste/<name>` — a refusal a human is meant
/// to act on, naming a file that is not there. A path spelled twice is a path
/// that can disagree with itself.
pub const PASTE_REL: &str = ".mush/paste";

/// The directory under `root` that pasted pictures are written into.
pub fn paste_dir(root: &Path) -> PathBuf {
    root.join(PASTE_REL)
}

/// The workspace-relative name of a file in [`paste_dir`]: the path an
/// [`Image`] carries and the path a message about a pasted file spells.
pub fn paste_rel(name: &str) -> String {
    format!("{PASTE_REL}/{name}")
}

/// Create the file a paste's bytes go in under `dir` (a [`paste_dir`]), and
/// hand back the name it took: `pasted-<unix millis>.<ext>`, or
/// `pasted-<millis>-2.<ext>` and on when that name is already there.
///
/// A paste of four pictures is four of these in the same millisecond, and
/// every image must keep the bytes that rode with it — a name taken is not a
/// name to overwrite. `create_new` is what makes the test and the create one
/// step, so two pastes can never land in one file however they interleave.
/// `millis` is a parameter rather than `now_millis()` inside because the
/// naming rule is a fact a test can pin: same millisecond, second name — and
/// because the name is also where [`paste_moment`] reads a paste's age back
/// out ([`Workspace::prune_pastes`]), so the rule that writes it and the rule
/// that ages by it are one spelling apart, not two.
fn create_paste_file(dir: &Path, millis: u128, mime: &str) -> Result<(String, fs::File), String> {
    let extension = pasted_extension(mime);
    let mut taken = 0u32;
    loop {
        taken += 1;
        let name = if taken == 1 {
            format!("pasted-{millis}.{extension}")
        } else {
            format!("pasted-{millis}-{taken}.{extension}")
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
        {
            Ok(file) => return Ok((name, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot write {}: {e}", paste_rel(&name))),
        }
    }
}

/// The bytes of `path` read under `cap`, bounded to `cap + 1` so a file that
/// grew behind the caller's own stat is caught by its length instead of loaded
/// whole — the bound [`Workspace::whole_read`] and [`Workspace::image_at`]
/// keep, and the one [`Workspace::search`]'s read used to keep only from the
/// stat. A bound checked from `metadata` alone is no bound on a file that is
/// still growing; `cap + 1` is what lets the caller answer "past the cap"
/// without coming back for the file's real size.
fn read_bounded(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(cap + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// One file's rows for [`Workspace::search`]: the match rows and the context
/// lines around them, at most `room + 1` rows.
///
/// The window state is one line wide of memory. `window_end` is the last line
/// the hit that opened the current window reaches; `pending` holds the lines
/// walked since the last emitted row that a later hit could still reach back
/// to — at most `context` of them, as slices of the file's own text, so a file
/// with no matches costs no allocation and a minified file's million matches
/// cost the answer's rows. A line inside an open window is emitted the moment
/// it is walked; the lines between windows are held until a hit claims them as
/// its pre-context or the next line proves they are out of reach. Every line
/// is walked once and emitted once, and no line is emitted twice — a hit
/// inside the open window it lands in only extends that window's end.
///
/// The bound is the answer's, the same shape [`crate::usages::rows_within`]
/// argues: one past `room`, and exactly `room + 1` rows when the file held a
/// further row — the caller's cut is then the walk's, and the row that made the
/// answer one over the room is the whole proof that there was more.
fn search_rows(
    regex: &regex_lite::Regex,
    name: &str,
    text: &str,
    room: usize,
    context: usize,
) -> Vec<String> {
    // One past the room: `room + 1` rows are the proof of "there is more",
    // and nothing beyond them can enter the answer the caller builds.
    let keep = room.saturating_add(1);
    let mut rows: Vec<String> = Vec::new();
    let mut pending: Vec<(usize, &str)> = Vec::new();
    let mut window_end: Option<usize> = None;
    for (number, line) in text::file_lines(text).enumerate() {
        if regex.is_match(line) {
            // The held lines are this match's pre-context and the window it
            // opens reaches back over them; they go out first, in line order.
            for (held, text) in pending.drain(..) {
                if rows.len() == keep {
                    return rows;
                }
                rows.push(context_row(name, held, text, context));
            }
            if rows.len() == keep {
                return rows;
            }
            rows.push(match_row(name, number, line));
            window_end = Some(number + context);
            continue;
        }
        if let Some(end) = window_end {
            if number <= end {
                // Inside the open window: this line is context of the hit that
                // opened it, and a later hit inside the window would make it
                // interior — the same row either way, emitted once.
                if rows.len() == keep {
                    return rows;
                }
                rows.push(context_row(name, number, line, context));
                continue;
            }
            window_end = None;
        }
        // No window reaches this line: it can only become the pre-context of a
        // *later* hit, and only while fewer than `context` lines stand between
        // it and that hit. An older line is out of reach of every hit to come,
        // so it is dropped rather than carried.
        pending.push((number, line));
        if pending.len() > context {
            pending.remove(0);
        }
    }
    rows
}

/// A match row: `path:line: text`, the shape this tool has always printed,
/// with the line's own number after the colon.
fn match_row(name: &str, number: usize, line: &str) -> String {
    format!("{name}:{}: {}", number + 1, row_line(line, None))
}

/// A context row: `path-line- text`, grep's shape for a line the match is not
/// on but the window shows. The separators are what tell it apart from a match
/// row, and the file's own line number stays in it so a context row is still an
/// anchor for `read_file {offset}`.
fn context_row(name: &str, number: usize, line: &str, context: usize) -> String {
    format!("{name}-{}- {}", number + 1, row_line(line, Some(context)))
}

/// One row's line, as `search` hands it to a model: the file's own bytes,
/// cut only past [`MATCH_LINE_CAP`] and with the cut said.
///
/// It is deliberately not [`text::truncate`]: that sanitizes for a pane, and it
/// would delete the line's escape sequences and control bytes, turn a bare `\r`
/// into `␍` and trim the line's own trailing whitespace — a line the file does
/// not hold (finding B8). Nor is it `trim_end`ed: a search result is a
/// location, and the model may ask about the spaces. The cut uses
/// [`text::boundary_at_or_before`] and the marker names the road that prints
/// the whole line, the same shape [`truncate_for_model`] uses for output; a
/// line the cap did not touch comes back exactly.
///
/// The road is the row's own: the pattern alone (`rg -n`) prints a match line
/// whole, while a context line needs the window it sits in (`rg -n -C n`) — a
/// marker naming the wrong road would send the model to a command whose output
/// cannot hold the line.
fn row_line(line: &str, context: Option<usize>) -> String {
    if line.len() <= MATCH_LINE_CAP {
        return line.to_string();
    }
    let cut = text::boundary_at_or_before(line, MATCH_LINE_CAP);
    let road = match context {
        Some(context) => format!("rg -n -C {context}"),
        None => "rg -n".to_string(),
    };
    format!(
        "{}… [mush: line cut at {MATCH_LINE_CAP} bytes — run_command (`{road}`) prints it whole]",
        &line[..cut]
    )
}

/// Cap text handed to a model, cutting on a char boundary and marking the
/// cut, so a partial result can never be mistaken for the whole output. The
/// marker says how much was kept and what to do next — the command result is
/// now the one road big text travels, and a model that cannot tell truncation
/// from completion is the defect this prevents. This is the one home of that
/// sentence: a file read is whole or refused (see [`Workspace::read_file`]).
pub fn truncate_for_model(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let cut = text::boundary_at_or_before(&text, cap);
    text.truncate(cut);
    text.push_str(&format!(
        "\n\n[mush: output truncated at {cap} bytes — rerun it narrower (rg, head, a smaller \
         path) to see the rest]"
    ));
    text
}

/// Cap text handed to a model from the *end*, marking the cut at the front.
///
/// A side effect of an unfinished command is a tail: the tests it printed last,
/// the error it died on, the panic at the bottom of the log. `truncate_for_model`
/// keeps the head instead, which is right for a file the model is about to edit
/// and wrong for a log it is about to read. Keeping the tail is also what makes a
/// crash legible at all — the interesting bytes are the ones written just before
/// it stopped. The marker names the cap and the way past it, like
/// `truncate_for_model`'s.
pub fn tail_for_model(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let cut = text::boundary_at_or_after(text, text.len() - cap);
    format!(
        "[mush: output truncated at {cap} bytes (the end is shown) — rerun it narrower to see \
         the rest]\n\n{}",
        &text[cut..]
    )
}

/// Who a *new* file is made for, when [`atomic_write`] has no mode to keep.
///
/// An existing target's mode is copied across the rename whatever this says;
/// this decides only what a name that did not exist comes out as. The two
/// values are the two facts a write road knows about its file:
///
/// - [`Fresh::Box`] — a workspace file, which belongs to the box: `0o666` with
///   the human's umask applied by the kernel at creation, the way `>`, `vim`
///   and `git` make one. The umask is the human's own decision about new files
///   and no safe way to read it exists (`umask(2)` is both a process-wide write
///   and unsafe, and this tree forbids `unsafe`), so spelling `0o644` here
///   would be a second guess at a number the kernel already knows — and a wrong
///   one on a box that chose `002` or `077`.
/// - [`Fresh::Private`] — mush's own store: the session file, the
///   `.mush/session.json.previous` copy a new chat keeps beside it, and the
///   key-bearing home `config.json` — conversations and the human's secrets.
///   `0o600`, because a human's `022` umask is about the files the box shares
///   and must not hand mush's private ones to the group.
///
/// One decision in one place: the difference is this match and the reason
/// beside it, never two copies of `0666 & !umask`.
pub enum Fresh {
    Box,
    Private,
}

impl Fresh {
    /// The mode a new file is *created* with. The kernel still applies the
    /// human's umask to it — which is the point for [`Fresh::Box`]: `0o666`
    /// becomes `0o644` under the usual `022` and `0o600` under a `077`.
    /// [`Fresh::Private`]'s `0o600` has no group or other bits for an umask to
    /// take, so mush's own stores come out `0600` whatever the human chose.
    fn mode(self) -> u32 {
        match self {
            Fresh::Box => 0o666,
            Fresh::Private => 0o600,
        }
    }
}

/// Write via a same-directory temp file plus `rename`, so readers never observe
/// a half-written file and a crash cannot corrupt the original.
///
/// The name is resolved to what it really is first (`entry_for_write`): a
/// symlink is written through and stays a link, and a socket, a FIFO or a
/// device is refused rather than destroyed. That guard lives here beside the
/// rename as well as at [`Workspace::write_file`], so a caller that does not
/// pass through the tool's door (`session::save`, the human's own
/// `config.json`) cannot rename over a socket either.
///
/// The mode is a fact of the file, not of this function. `tempfile`'s scratch
/// file is `0600` by default and the rename would carry that onto the target,
/// so an existing target's mode is copied onto the temp file before the rename
/// (finding B1: an executable script stopped being executable, and `git`
/// recorded the mode change); a name that did not exist is made the way
/// `fresh` says. A hard-linked twin is the one fact this cannot keep: `rename`
/// replaces the *name*, so the other name keeps the old bytes and the two stop
/// being one inode. The fork is the price of the rename's atomicity, and this
/// road will not trade that away for it — a copy-then-truncate would leave a
/// reader able to see a half-written file, which is the promise above — so the
/// fork is documented here and pinned by `a_hard_link_forks_under_the_rename`.
pub fn atomic_write(path: &Path, bytes: &[u8], fresh: Fresh) -> io::Result<()> {
    let entry = entry_for_write(path, &path.display().to_string()).map_err(invalid)?;
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let existing = fs::metadata(&entry)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o7777);
    let mut tmp = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(
            existing.unwrap_or_else(|| fresh.mode()),
        ))
        .tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    // A mode the file already had is copied exactly, not created through the
    // umask: the umask decides *new* modes, and this one is not new.
    if let Some(mode) = existing {
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    tmp.persist(&entry).map_err(|error| error.error)?;
    Ok(())
}

/// How many backup names mush will try beside a file it cannot use before it
/// gives up looking. A store that has been hand-broken a hundred times has a
/// problem that no file name solves.
pub(crate) const BACKUP_TRIES: u32 = 100;

/// The first free backup name beside `path` — `<path>.bak`, then `<path>.bak.2`,
/// `<path>.bak.3`, … up to `BACKUP_TRIES` — for the two files mush sets aside
/// rather than let the next write replace them: the unreadable session
/// ([`crate::session::keep_unreadable`]) and the unparsable home config
/// ([`crate::userconfig`]'s save).
///
/// The numbering is one rule so the two roads cannot disagree about it, and a
/// copy already beside the file is never overwritten: it is one the human
/// already needed, so the next free name is taken instead. The bound on how
/// hard mush looks is this one constant. `Err` is the sentence both callers
/// carry on: every name beside the file is taken.
///
/// The rename itself is each caller's, because *why* the copy is kept differs —
/// a conversation that must not be lost and a key that must not be replaced are
/// different accidents, kept for different reasons in the callers' own docs.
pub fn backup_name(path: &Path) -> Result<PathBuf, String> {
    let base = PathBuf::from(format!("{}.bak", path.display()));
    for step in 1..=BACKUP_TRIES {
        let to = if step == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{step}", base.display()))
        };
        if !to.exists() {
            return Ok(to);
        }
    }
    Err(format!(
        "every backup name beside {} is taken",
        path.display()
    ))
}

/// What a write to `path` must land on, or why it cannot: the file the name
/// really is.
///
/// A write is a `rename`, and a `rename` replaces the *name*, so the name is
/// asked what it is first. A symlink is resolved to its final target — the
/// whole chain, because that is where the write belongs: the model edited the
/// file the name points at, and a rename over the link would delete the link
/// and leave that file untouched.
/// A socket, a FIFO or a device is refused, because it is not content: renaming
/// a regular file over the workspace's own `.mush/mush.sock` unlinks the attach
/// socket and every `mush read`/`agents`/`edit` in that directory answers "no
/// mush is running" until mush restarts (finding B3), and a FIFO or a device
/// is the same destruction. A symlink to nothing is refused too: following it
/// would make a file at a path no caller checked, so the reader is told to make
/// the target first.
///
/// `name` is the caller's spelling of the path — the workspace-relative `rel`
/// at the tool's door, the path itself for a caller with no root — because the
/// refusal is read by a model or a human, not by `readlink`.
fn entry_for_write(path: &Path, name: &str) -> Result<PathBuf, String> {
    let Ok(kind) = fs::symlink_metadata(path) else {
        return Ok(path.to_path_buf());
    };
    if kind.is_file() {
        return Ok(path.to_path_buf());
    }
    if kind.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map(|target| target.display().to_string())
            .unwrap_or_else(|error| format!("(unreadable: {error})"));
        return match fs::canonicalize(path) {
            Ok(real) => match fs::symlink_metadata(&real) {
                Ok(real_kind) if real_kind.is_file() => Ok(real),
                _ => Err(format!(
                    "{name} is a symlink to {target} — which is not a regular file; refusing to \
                     write through it"
                )),
            },
            Err(_) => Err(format!(
                "{name} is a symlink to {target}, which does not exist — refusing to replace \
                 the link; create the target first"
            )),
        };
    }
    Err(format!(
        "{name} is a {} — refusing to replace it",
        what_it_is(&kind)
    ))
}

/// The type name a refusal sentence gives for a name that is not a file: the
/// words a model can act on, and the reason the write is refused at all.
fn what_it_is(kind: &fs::Metadata) -> &'static str {
    let kind = kind.file_type();
    if kind.is_dir() {
        "directory"
    } else if kind.is_socket() {
        "socket"
    } else if kind.is_fifo() {
        "FIFO"
    } else if kind.is_char_device() || kind.is_block_device() {
        "device"
    } else {
        "file that is not a regular file"
    }
}

/// [`entry_for_write`]'s refusal as an IO error: the kind that says "the name
/// is wrong", not "the disk is". The callers inside a workspace wrap it in the
/// sentence their model reads, and the callers outside one (`session::save`,
/// the home `config.json`) report it through their own error channel.
fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::{Held, Scratch};
    use std::time::{Duration, Instant};

    /// A workspace on a scratch root of its own: `mush-test-<name>-<pid>`. The
    /// returned guard travels with the workspace, so the files it makes are
    /// removed when the test ends — including when it fails.
    fn temp_workspace(name: &str) -> Held<Workspace> {
        let dir = Scratch::new(&format!("test-{name}"));
        let ws = Workspace::new(&dir).unwrap();
        dir.hold(ws)
    }

    /// A path's permission bits as a human reads them (`0755`): the low twelve,
    /// without the file-type bits `Permissions::mode` also carries.
    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// A picture file *outside* every test workspace — the paste road's own
    /// case: a name the human may give and the model's tools may not resolve.
    /// A scratch root of its own holds it, and the guard comes back with the
    /// path, so the file is removed when the test ends (the name it carries is
    /// the one the outside-paste tests already spell).
    fn outside_image(name: &str, bytes: &[u8]) -> Held<PathBuf> {
        let dir = Scratch::new(&format!("test-outside-{name}"));
        let path = dir.path().join(format!("{name}.png"));
        fs::write(&path, bytes).unwrap();
        dir.hold(path)
    }

    /// A pending workspace is the *place* a worktree will be made: the path is
    /// canonical as far as it exists and the missing names are appended
    /// verbatim, so it reads exactly like the workspace `new` opens a moment
    /// later, when the door has made the directory. A root that exists is the
    /// same root either way, and a path whose ancestor is a file is nobody's
    /// pending root.
    #[test]
    fn a_pending_workspace_is_the_place_the_worktree_will_be_made() {
        let dir = Scratch::new("pending-root");
        let missing = dir.join("a/b/c");
        assert!(
            Workspace::new(&missing).is_err(),
            "the directory is not there yet"
        );
        let pending = Workspace::pending(&missing).unwrap();
        assert_eq!(
            pending.root(),
            dir.path().join("a/b/c"),
            "the missing names are appended to the canonical root"
        );
        fs::create_dir_all(&missing).unwrap();
        assert_eq!(
            pending.root(),
            Workspace::new(&missing).unwrap().root(),
            "and it is the same root the strict door opens once it exists"
        );
        let file = dir.join("plain.txt");
        fs::write(&file, "not a directory\n").unwrap();
        assert!(
            Workspace::pending(file.join("below")).is_err(),
            "a path below a file is not a root waiting to be made"
        );
    }

    /// A path that is not under the root is shown as it is: a workspace opened
    /// as another directory must not have a real path rewritten into a relative
    /// one that means something else. This is the one elision rule — the session
    /// notice reads it rather than keeping its own copy (refactor R17).
    #[test]
    fn a_path_outside_the_workspace_is_shown_whole() {
        let ws = temp_workspace("rel");
        assert_eq!(
            ws.rel(Path::new("/elsewhere/session.json")),
            "/elsewhere/session.json"
        );
        assert_eq!(
            ws.rel(&ws.root().join(".mush/session.json.bak.2")),
            ".mush/session.json.bak.2"
        );
        // And a name's `\` stays the name's: on this box it is an ordinary
        // byte, and folding it into `/` named a path that does not exist
        // (finding B9).
        assert_eq!(ws.rel(&ws.root().join("a\\b")), "a\\b");
    }

    /// A path a tool hands the model is a path the tool opens again: a name is
    /// bytes, not display. A file named `a\b.txt` lists as `a\b.txt` — the old
    /// fold into `a/b.txt` named a file that does not exist — and a name that
    /// cannot travel *as itself* (a line break, bytes that are not UTF-8, ends
    /// `resolve` would trim) is not handed over as a path at all, because the
    /// listing's own shape would read it as two entries or as a different file:
    /// it is counted instead, for the tool layer to say so. A search names the
    /// same file the same way (finding B9).
    #[test]
    fn a_listed_path_can_be_opened_again() {
        use std::os::unix::ffi::OsStrExt;

        let ws = temp_workspace("listed-name");
        fs::write(ws.root().join(r"a\b.txt"), "the real file\n").unwrap();
        fs::write(ws.root().join("a\nb.txt"), "the newline file\n").unwrap();
        fs::write(
            ws.root().join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt")),
            "latin name\n",
        )
        .unwrap();
        fs::write(ws.root().join("trailing "), "trailing space\n").unwrap();

        let (listed, truncated, unnamed) = ws.list_files("", 100).unwrap();
        assert_eq!(
            listed,
            vec![r"a\b.txt".to_string()],
            "the backslash is a name byte, not a separator"
        );
        assert!(!truncated);
        assert_eq!(unnamed, 3, "the names that cannot travel are counted");
        for name in &listed {
            assert_eq!(
                ws.read_file(name).unwrap(),
                fs::read_to_string(ws.root().join(name)).unwrap(),
                "every listed path opens the file it named"
            );
        }

        // The search names the file the same way, and its path opens too. The
        // three unnameable files are counted, not reported under a name that
        // opens something else (or nothing).
        let found = ws.search("the real file", "", 10, 0).unwrap();
        assert_eq!(found.rows, vec![r"a\b.txt:1: the real file".to_string()]);
        assert_eq!(found.unnamed, 3);
        let named = found.rows[0].split(':').next().unwrap();
        assert_eq!(ws.read_file(named).unwrap(), "the real file\n");

        // A search that never looked into the newline-named file is not a
        // silent miss, and a directory of only such names is not "no files".
        let found = ws.search("the newline file", "", 10, 0).unwrap();
        assert!(found.rows.is_empty());
        assert_eq!(found.unnamed, 3);
        let dir = ws.root().join("only");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x\ny"), "no pattern\n").unwrap();
        let (files, _, unnamed) = ws.list_files("only", 100).unwrap();
        assert!(files.is_empty());
        assert_eq!(unnamed, 1, "the listing says it left one name out");

        let _ = fs::remove_dir_all(ws.root());
    }

    /// A live sibling's checkout is not a place mush lists, searches, reads or
    /// writes (finding IN7). `.mush` is deliberately not skipped — the session
    /// file is a file a model may be asked to look at — but `.mush/wt/<id>` is
    /// one live git worktree per isolated child, and every one of them is a
    /// *sibling's* tree: the walk descended into them, so `list_files("")`
    /// filled with other agents' checkout names (which sort before the
    /// workspace's own files), and a path it printed was one `edit_file` wrote
    /// into a *live* sibling's tree, where that child's own run-end commit
    /// carried the change onto `mush/<id>` while the file the model meant
    /// stayed untouched. The positive twin at the end is why the rule is that
    /// one directory and not the name `wt`: a child actor's own workspace *is*
    /// `.mush/wt/<id>`, and its own files must stay readable and writable.
    #[test]
    fn a_siblings_worktree_is_invisible_to_the_file_roads() {
        let ws = temp_workspace("worktree-road");
        let checkout = ws.root().join(".mush/wt/3");
        fs::create_dir_all(checkout.join("src")).unwrap();
        fs::write(checkout.join("src/main.rs"), "fn sibling() {}\n").unwrap();
        fs::write(ws.root().join("main.rs"), "fn mine() {}\n").unwrap();
        fs::write(ws.root().join(".mush/session.json"), "{}\n").unwrap();
        fs::create_dir_all(ws.root().join(".mush/paste")).unwrap();
        fs::write(ws.root().join(".mush/paste/shot.png"), "not an image\n").unwrap();

        // Neither listing answers with the checkout, and the workspace's own
        // files — `.mush`'s included — still answer.
        let (listed, _, _) = ws.list_files("", 100).unwrap();
        assert!(
            !listed.iter().any(|name| name.starts_with(".mush/wt")),
            "a sibling's checkout is not listed: {listed:?}"
        );
        assert!(listed.contains(&"main.rs".to_string()), "{listed:?}");
        let mush = ws.list_files(".mush", 100).unwrap().0;
        assert!(mush.contains(&".mush/session.json".to_string()), "{mush:?}");
        assert!(
            mush.contains(&".mush/paste/shot.png".to_string()),
            "{mush:?}"
        );
        assert!(ws.list_files(".mush/wt", 10).is_err());

        // The search never reads the sibling's file.
        let found = ws.search("fn sibling", "", 100, 0).unwrap();
        assert!(found.rows.is_empty(), "{:?}", found.rows);

        // A read of the path the old listing handed over is refused...
        let refused = ws
            .read_window(".mush/wt/3/src/main.rs", 1, 50, 4000)
            .unwrap_err();
        assert!(refused.contains(".mush/wt/3/src/main.rs"), "{refused}");
        assert!(ws.read_file(".mush/wt/3/src/main.rs").is_err());

        // ... and so is a write: the edit cannot land in the sibling's tree.
        let refused = ws
            .write_file(".mush/wt/3/src/main.rs", "EDITED\n")
            .unwrap_err();
        assert!(refused.contains(".mush/wt/3/src/main.rs"), "{refused}");
        assert_eq!(
            fs::read_to_string(checkout.join("src/main.rs")).unwrap(),
            "fn sibling() {}\n",
            "nothing landed in the sibling's checkout"
        );
        // The rule is the directory, not what it happens to hold: a write to a
        // path that does not exist yet under it is refused too, so no road can
        // recreate a checkout-shaped tree.
        assert!(ws.write_file(".mush/wt/9/new.rs", "x\n").is_err());
        assert!(!checkout.parent().unwrap().join("9").exists());

        // The positive twin: a workspace rooted *at* a worktree — a child
        // actor's own — reaches its own files exactly as before.
        let scratch = Scratch::new("worktree-road-child");
        let child_root = scratch.path().join(".mush/wt/3");
        fs::create_dir_all(&child_root).unwrap();
        fs::write(child_root.join("own.rs"), "fn own() {}\n").unwrap();
        let child = Workspace::new(&child_root).unwrap();
        assert_eq!(child.read_file("own.rs").unwrap(), "fn own() {}\n");
        child.write_file("own.rs", "fn own2() {}\n").unwrap();
        let (own, _, _) = child.list_files("", 100).unwrap();
        assert_eq!(own, vec!["own.rs".to_string()]);
    }

    /// `.mush` is mush's own state and not part of the walk's map of the work
    /// (the human's decision, [`SKIP_DIRS`]): a root listing and a search answer
    /// with the sources alone, where they used to carry `session.json`, the
    /// pastes and the gate logs among them. *Reach* is the other half and is
    /// untouched — a walk *started* at `.mush` still descends into it, and a
    /// file under it is still opened by name — because that is the road the
    /// paste placeholder sends a model down when a picture's bytes were shed.
    #[test]
    fn a_root_listing_omits_mushs_own_state_and_a_named_one_still_lists() {
        let ws = temp_workspace("mush-own-state");
        fs::write(ws.root().join("src.rs"), "fn held() {}\n").unwrap();
        fs::create_dir_all(ws.root().join(".mush/paste")).unwrap();
        fs::write(ws.root().join(".mush/session.json"), "the conversation\n").unwrap();
        fs::write(ws.root().join(".mush/paste/shot.png"), "PNG\n").unwrap();
        fs::write(ws.root().join(".mush/gate.log"), "held\n").unwrap();

        let (listed, _, _) = ws.list_files("", 100).unwrap();
        assert_eq!(
            listed,
            vec!["src.rs".to_string()],
            "the map is the work, not the bookkeeping"
        );

        // A search is the same walk, so it cannot spend a match on a gate log.
        let found = ws.search("held", "", 100, 0).unwrap();
        assert_eq!(found.rows, vec!["src.rs:1: fn held() {}".to_string()]);

        // Reach: the start directory is never name-checked, so a walk asked for
        // `.mush` still lists it, and the paste's own file still opens by name.
        let (inside, _, _) = ws.list_files(".mush", 100).unwrap();
        assert!(
            inside.contains(&".mush/session.json".to_string()),
            "{inside:?}"
        );
        assert_eq!(ws.read_file(".mush/paste/shot.png").unwrap(), "PNG\n");
    }

    /// The listing's cap ends the walk where it lands instead of walking and
    /// sorting the whole subtree first (finding IN9). The walk reaches the
    /// root's files in name order and *then* descends into the root's
    /// directories, so the order it produces is not a global sort's: a global
    /// sort puts `a/z.txt` before `z.txt` (`'.' < '/'`), and a listing cut
    /// *after* that sort answered `[a.txt, a/z.txt]` — the whole tree walked,
    /// every name allocated and sorted, to print two names, where `search` had
    /// always stopped its walk at its own cap. The cap now stops this walk at
    /// the third name.
    #[test]
    fn a_listing_cap_stops_the_walk_instead_of_the_sort() {
        let ws = temp_workspace("list-cap");
        fs::write(ws.root().join("a.txt"), "a\n").unwrap();
        fs::write(ws.root().join("z.txt"), "z\n").unwrap();
        fs::create_dir_all(ws.root().join("a")).unwrap();
        fs::write(ws.root().join("a/z.txt"), "inner\n").unwrap();

        let (listed, truncated, _) = ws.list_files("", 2).unwrap();
        assert_eq!(
            listed,
            vec!["a.txt".to_string(), "z.txt".to_string()],
            "the cap cuts the walk's own order, where it stopped"
        );
        assert!(truncated, "there was a third file the cap cut");

        // With room for all three, the answer is the walk's order end to end:
        // the global sort is the whole-tree cost the cap exists to avoid.
        let (all, truncated, _) = ws.list_files("", 10).unwrap();
        assert_eq!(
            all,
            vec![
                "a.txt".to_string(),
                "z.txt".to_string(),
                "a/z.txt".to_string()
            ]
        );
        assert!(!truncated);
    }

    #[test]
    fn resolve_rejects_escapes_and_absolutes() {
        let ws = temp_workspace("resolve");
        assert!(ws.resolve("../secret").is_err());
        assert!(ws.resolve("a/../../b").is_err());
        assert!(ws.resolve("/etc/passwd").is_err());
        assert!(ws.resolve("./src/main.rs").is_ok());
        assert_eq!(ws.resolve(".").unwrap(), ws.root());
    }

    #[test]
    fn write_then_read_roundtrips() {
        let ws = temp_workspace("roundtrip");
        ws.write_file("src/lib.rs", "fn a() {}\n").unwrap();
        assert_eq!(ws.read_file("src/lib.rs").unwrap(), "fn a() {}\n");
        assert!(!ws.resolve("src/missing.rs").unwrap().exists());
    }

    /// The edit road is read → transform → write, and the write is the whole
    /// file: a change another writer lands between the two is lost, silently
    /// (finding B16). This is the audit's staged window, kept as the fact the
    /// tool's description and [`crate::tools::edit_text_many`]'s doc now
    /// state — the decision is to name the loss, not to compare-and-swap it
    /// away.
    #[test]
    fn an_edit_written_from_a_stale_read_loses_the_other_writers_change() {
        let ws = temp_workspace("edit-stale-read");
        fs::write(ws.root().join("f.txt"), "line one\n").unwrap();

        // What the model reads before it decides on an edit ...
        let read = ws.read_file("f.txt").unwrap();
        // ... and what another writer — a sibling agent in the same checkout,
        // or the human's own editor — lands before the model's write.
        fs::write(ws.root().join("f.txt"), "line one\nline 2\n").unwrap();

        let edits = [crate::tools::Edit {
            old: "line one".to_string(),
            new: "LINE ONE".to_string(),
            replace_all: false,
        }];
        let updated = crate::tools::edit_text_many(&read, &edits, "f.txt").unwrap();
        ws.write_file("f.txt", &updated).unwrap();

        assert_eq!(
            fs::read_to_string(ws.root().join("f.txt")).unwrap(),
            "LINE ONE\n",
            "the sibling's line is gone: the write was the file as it was read"
        );
    }

    /// The bytes of a "png" for a paste test: the magic number is the whole of
    /// what [`image_mime`] reads, and the padding lets a test craft one past
    /// the cap without holding a real picture.
    fn png(padding: usize) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.resize(8 + padding, 0);
        bytes
    }

    /// A png with a real IHDR, the size the dimension parser reads, plus
    /// whatever payload a test sizes it with.
    fn png_of(width: u32, height: u32, padding: usize) -> Vec<u8> {
        let mut bytes = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, // signature
            0, 0, 0, 13, b'I', b'H', b'D', b'R', // the IHDR chunk and its length
        ];
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        // depth, colour type, compression, filter, interlace: the rest of the
        // 13-byte IHDR payload.
        bytes.extend([8, 6, 0, 0, 0]);
        bytes.resize(33 + padding, 0);
        bytes
    }

    /// A webp file around one chunk's payload: the RIFF/WEBP head, then the
    /// chunk's fourcc and a length field, then the payload. The RIFF size
    /// field is written as the format counts it, length included.
    fn webp_chunk(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend((4 + 8 + payload.len() as u32).to_le_bytes());
        bytes.extend(b"WEBP");
        bytes.extend(fourcc);
        bytes.extend((payload.len() as u32).to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    /// A jpeg marker segment: the `ff`, the marker, then a big-endian length
    /// that counts itself, then the payload.
    fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, marker];
        bytes.extend(((payload.len() + 2) as u16).to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    /// A jpeg frame header (SOFn): the precision, then height and width, each
    /// two bytes big-endian, then one component — the layout every member of
    /// the SOFn range has.
    fn jpeg_frame(marker: u8, width: u16, height: u16) -> Vec<u8> {
        let mut payload = vec![8];
        payload.extend(height.to_be_bytes());
        payload.extend(width.to_be_bytes());
        payload.push(1); // one component…
        payload.extend([1, 0x11, 0]); // …and its id, sampling and table
        jpeg_segment(marker, &payload)
    }

    /// A whole jpeg header: SOI, an APP1 segment carrying `app` bytes of
    /// metadata (EXIF is one, and it is large), then the frame itself.
    fn jpeg_header(marker: u8, width: u16, height: u16, app: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8];
        if !app.is_empty() {
            bytes.extend(jpeg_segment(0xe1, app));
        }
        bytes.extend(jpeg_frame(marker, width, height));
        bytes
    }

    /// A gif's screen descriptor at `width × height`: the version signature,
    /// then the logical screen descriptor's first four bytes.
    fn gif_of(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        bytes.extend([0xf7, 0x00, 0x00]);
        bytes
    }

    /// A lossy webp at `width × height`: the frame tag, the start code, then
    /// the two little-endian size fields.
    fn vp8_of(width: u16, height: u16) -> Vec<u8> {
        let mut payload = vec![0x00, 0x00, 0x00];
        payload.extend([0x9d, 0x01, 0x2a]);
        payload.extend(width.to_le_bytes());
        payload.extend(height.to_le_bytes());
        webp_chunk(b"VP8 ", &payload)
    }

    /// A lossless webp at `width × height`: the signature byte, then `width - 1`
    /// and `height - 1` packed into one little-endian word.
    fn vp8l_of(width: u32, height: u32) -> Vec<u8> {
        let mut payload = vec![0x2f];
        payload.extend(((width - 1) | ((height - 1) << 14)).to_le_bytes());
        webp_chunk(b"VP8L", &payload)
    }

    /// An extended webp at `width × height`: the flag byte and three reserved,
    /// then the two 24-bit `- 1` fields.
    fn vp8x_of(width: u32, height: u32) -> Vec<u8> {
        let mut payload = vec![0x10, 0x00, 0x00, 0x00];
        for side in [width - 1, height - 1] {
            payload.extend([side as u8, (side >> 8) as u8, (side >> 16) as u8]);
        }
        webp_chunk(b"VP8X", &payload)
    }

    /// Each of the four formats names its size in its own header, and the
    /// parser reads all four: png big-endian in IHDR, jpeg in an SOFn frame
    /// reached by walking the marker segments, gif little-endian in the screen
    /// descriptor, and webp in whichever of its three chunk shapes it is.
    #[test]
    fn every_image_format_names_its_size_in_its_header() {
        assert_eq!(
            image_dimensions("image/png", &png_of(1_920, 1_080, 0)),
            Some((1_920, 1_080))
        );
        assert_eq!(
            image_dimensions("image/jpeg", &jpeg_header(0xc0, 800, 600, &[])),
            Some((800, 600))
        );
        assert_eq!(
            image_dimensions("image/gif", &gif_of(320, 200)),
            Some((320, 200))
        );
        assert_eq!(
            image_dimensions("image/webp", &vp8_of(640, 480)),
            Some((640, 480))
        );
        assert_eq!(
            image_dimensions("image/webp", &vp8l_of(256, 128)),
            Some((256, 128))
        );
        assert_eq!(
            image_dimensions("image/webp", &vp8x_of(1_024, 768)),
            Some((1_024, 768))
        );

        // A lossy frame's size fields carry a scale hint in their top two
        // bits: a hint is not part of the dimension, so it is masked off.
        let mut payload = vec![0x00, 0x00, 0x00];
        payload.extend([0x9d, 0x01, 0x2a]);
        payload.extend((640u16 | 0xc000).to_le_bytes());
        payload.extend((480u16 | 0x8000).to_le_bytes());
        assert_eq!(
            image_dimensions("image/webp", &webp_chunk(b"VP8 ", &payload)),
            Some((640, 480))
        );
    }

    /// A jpeg walk steps over a segment by the length that segment declares,
    /// so a large APP1 — EXIF is one — whose payload is marker-shaped bytes is
    /// skipped whole. A walk that scanned for `ff c0` would read the wrong
    /// frame out of the metadata and never reach the real one.
    #[test]
    fn a_jpeg_walk_skips_a_segment_by_its_own_length() {
        let mut exif = vec![0x00; 4 * 1024];
        // A plausible SOF0, size and all, buried in the metadata.
        exif[1_000..1_010]
            .copy_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x20, 0x00, 0x10, 0x00, 0x00]);
        let jpeg = jpeg_header(0xc0, 800, 600, &exif);
        assert_eq!(image_dimensions("image/jpeg", &jpeg), Some((800, 600)));
    }

    /// The frame header is read whatever member of the SOFn range it is —
    /// baseline, extended, progressive and arithmetic alike — and the three
    /// markers in the same range that are not frames (DHT, JPG, DAC) are
    /// stepped over like any other segment.
    #[test]
    fn a_jpeg_frame_is_read_whatever_its_sof_variant_is() {
        for marker in [
            0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf,
        ] {
            let jpeg = jpeg_header(marker, 640, 480, &[0x00; 32]);
            assert_eq!(
                image_dimensions("image/jpeg", &jpeg),
                Some((640, 480)),
                "SOF marker {marker:02x}"
            );
        }

        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend(jpeg_segment(0xc4, &[0x00; 64])); // DHT, not a frame
        jpeg.extend(jpeg_segment(0xc8, &[0x00; 64])); // JPG, not a frame
        jpeg.extend(jpeg_segment(0xcc, &[0x00; 64])); // DAC, not a frame
        jpeg.extend(jpeg_frame(0xc2, 320, 240));
        assert_eq!(image_dimensions("image/jpeg", &jpeg), Some((320, 240)));

        // Fill bytes: any run of `ff` before a marker is legal, so the walk
        // skips the run instead of assuming exactly one.
        let frame = jpeg_frame(0xc0, 640, 480);
        let mut filled = vec![0xff, 0xd8, 0xff, 0xff, 0xff, 0xc0];
        filled.extend_from_slice(&frame[2..]);
        assert_eq!(image_dimensions("image/jpeg", &filled), Some((640, 480)));
    }

    /// Every format's header, cut at every length before it is complete, is no
    /// size at all: there is no offset where a prefix of a real header can be
    /// mistaken for the whole of one.
    #[test]
    fn a_header_cut_anywhere_before_its_size_is_no_size() {
        let png = png_of(1_920, 1_080, 0);
        assert_eq!(image_dimensions("image/png", &png), Some((1_920, 1_080)));
        for cut in 0..24 {
            assert_eq!(
                image_dimensions("image/png", &png[..cut]),
                None,
                "png cut at {cut}"
            );
        }

        let jpeg = jpeg_header(0xc0, 800, 600, &[0x00; 32]);
        assert_eq!(image_dimensions("image/jpeg", &jpeg), Some((800, 600)));
        for cut in 0..jpeg.len() {
            assert_eq!(
                image_dimensions("image/jpeg", &jpeg[..cut]),
                None,
                "jpeg cut at {cut}"
            );
        }

        let gif = gif_of(320, 200);
        assert_eq!(image_dimensions("image/gif", &gif), Some((320, 200)));
        for cut in 0..10 {
            assert_eq!(
                image_dimensions("image/gif", &gif[..cut]),
                None,
                "gif cut at {cut}"
            );
        }

        for webp in [vp8_of(640, 480), vp8l_of(256, 128), vp8x_of(1_024, 768)] {
            assert!(image_dimensions("image/webp", &webp).is_some());
            for cut in 0..webp.len() {
                assert_eq!(
                    image_dimensions("image/webp", &webp[..cut]),
                    None,
                    "webp cut at {cut}"
                );
            }
        }
    }

    /// A signature is not a header: a png whose IHDR length lies, a jpeg whose
    /// segment length is zero or runs past the bytes, and a webp whose chunk
    /// length claims payload the file does not hold all answer `None`.
    #[test]
    fn a_header_whose_length_field_lies_is_no_size() {
        // IHDR is required to be 13 bytes long; 0xffff_ffff or 12 is not a
        // header this can read, however plausible the bytes after it look.
        let mut png = png_of(1_920, 1_080, 0);
        png[8..12].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
        assert_eq!(image_dimensions("image/png", &png), None);
        let mut png = png_of(1_920, 1_080, 0);
        png[8..12].copy_from_slice(&12u32.to_be_bytes());
        assert_eq!(image_dimensions("image/png", &png), None);

        // A zero length would leave the walk standing still; one that runs
        // past the end is not a segment either.
        let still = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(image_dimensions("image/jpeg", &still), None);
        let past = [0xff, 0xd8, 0xff, 0xe0, 0xff, 0xf0, 0x00, 0x00];
        assert_eq!(image_dimensions("image/jpeg", &past), None);
        // A frame segment whose length lies: too short to hold its own fields,
        // or claiming more bytes than the file holds.
        let mut short = jpeg_header(0xc0, 800, 600, &[]);
        short[4..6].copy_from_slice(&7u16.to_be_bytes());
        assert_eq!(image_dimensions("image/jpeg", &short), None);
        let mut long = jpeg_header(0xc0, 800, 600, &[]);
        long[4..6].copy_from_slice(&0xffffu16.to_be_bytes());
        assert_eq!(image_dimensions("image/jpeg", &long), None);

        // gif's screen descriptor has no length field before its size, so the
        // only lies it can tell are a short file and a zero side.

        // The chunk length has to name payload that is there, and to be big
        // enough for the fields the shape reads.
        let mut webp = vp8x_of(1_024, 768);
        webp[16..20].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        assert_eq!(image_dimensions("image/webp", &webp), None);
        let mut webp = vp8x_of(1_024, 768);
        webp[16..20].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            image_dimensions("image/webp", &webp),
            None,
            "a chunk too short for what VP8X stores"
        );
    }

    /// A zero side is not a picture in any format that can spell one, and it
    /// is `None` rather than `Some((0, width))`: a zero would make an image
    /// weight nothing, where "unknown" walks the bytes.
    #[test]
    fn a_zero_dimension_is_no_size() {
        assert_eq!(image_dimensions("image/png", &png_of(0, 1_080, 0)), None);
        assert_eq!(image_dimensions("image/png", &png_of(1_920, 0, 0)), None);
        assert_eq!(
            image_dimensions("image/jpeg", &jpeg_header(0xc0, 0, 600, &[])),
            None
        );
        assert_eq!(image_dimensions("image/gif", &gif_of(0, 0)), None);
        let empty = webp_chunk(b"VP8 ", &[0x00, 0x00, 0x00, 0x9d, 0x01, 0x2a, 0, 0, 1, 0]);
        assert_eq!(image_dimensions("image/webp", &empty), None);
        // The other two webp shapes store size - 1, so the smallest they can
        // name is 1×1 — and 1×1 is a picture.
        assert_eq!(image_dimensions("image/webp", &vp8l_of(1, 1)), Some((1, 1)));
        assert_eq!(image_dimensions("image/webp", &vp8x_of(1, 1)), Some((1, 1)));
    }

    /// Junk is junk: the empty slice, a three-byte slice, several KB of `ff`,
    /// a text file and a bare signature are all no size for every format —
    /// nothing panics, and nothing guesses.
    #[test]
    fn junk_is_no_size_for_every_format() {
        let flood = vec![0xffu8; 8 * 1024];
        let junk: [&[u8]; 6] = [
            &[],
            &[0xff, 0xd8, 0x00],
            &flood,
            b"GIF8",
            b"not an image at all",
            &[0x89, b'P', b'N', b'G'],
        ];
        for bytes in junk {
            for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                assert_eq!(
                    image_dimensions(mime, bytes),
                    None,
                    "{mime} on {} bytes of junk",
                    bytes.len()
                );
            }
        }

        // The signature decides, not the mime name: one format's bytes under
        // another format's name are no size, and so is a name with no parser.
        for mime in ["image/jpeg", "image/gif", "image/webp"] {
            assert_eq!(image_dimensions(mime, &png_of(8, 8, 0)), None, "{mime}");
        }
        assert_eq!(image_dimensions("image/tiff", &png_of(8, 8, 0)), None);
        assert_eq!(image_dimensions("", &png_of(8, 8, 0)), None);
    }

    /// A slice shorter than a signature is no image, whatever it starts with.
    /// The sniff is four `starts_with` calls and one `len() >= 12`, and the
    /// guard had no test of its own: `b"RIFF"`, `b"GIF8"` and a three-byte
    /// slice must answer `None` rather than panic on the `[8..12]` the webp
    /// branch reaches for. Not a bug today; a test that fails tomorrow is the
    /// point.
    #[test]
    fn a_slice_shorter_than_a_signature_is_no_image() {
        let short: [&[u8]; 13] = [
            &[],
            b"R",
            b"RI",
            b"RIF",
            b"RIFF",
            b"RIFF\x00\x00\x00\x00",
            b"WEB",
            b"WEBP",
            b"GIF",
            b"GIF8",
            b"GIF87",
            &[0xff, 0xd8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a],
        ];
        for bytes in short {
            assert_eq!(image_mime(bytes), None, "{bytes:?} is not an image");
            for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                assert_eq!(image_dimensions(mime, bytes), None, "{mime} on {bytes:?}");
            }
        }

        // The full signatures still answer, so the floor is a floor and not a
        // ceiling: the guard may not swallow a real picture.
        assert_eq!(image_mime(&png(0)), Some("image/png"));
        assert_eq!(image_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"RIFF\x00\x00\x00\x00WEBP"), Some("image/webp"));
    }

    /// The parser does not shrink a claim it can read: a png that says
    /// `0xffff_ffff × 0xffff_ffff` is that size, and every other format reads
    /// up to its own ceiling — the weight arithmetic is what keeps the biggest
    /// of them from wrapping (the message tests pin that side).
    #[test]
    fn the_largest_dimensions_a_header_can_claim_parse_as_themselves() {
        assert_eq!(
            image_dimensions("image/png", &png_of(u32::MAX, u32::MAX, 0)),
            Some((u32::MAX, u32::MAX))
        );
        let full = u16::MAX as u32;
        assert_eq!(
            image_dimensions("image/jpeg", &jpeg_header(0xc0, u16::MAX, u16::MAX, &[])),
            Some((full, full))
        );
        assert_eq!(
            image_dimensions("image/gif", &gif_of(u16::MAX, u16::MAX)),
            Some((full, full))
        );
        // webp's lossy shape spends 14 bits on a side, its lossless one names
        // size - 1 in 14 bits, and its extended one spends 24 bits on size - 1.
        assert_eq!(
            image_dimensions("image/webp", &vp8_of(0x3fff, 0x3fff)),
            Some((0x3fff, 0x3fff))
        );
        assert_eq!(
            image_dimensions("image/webp", &vp8l_of(1 << 14, 1 << 14)),
            Some((1 << 14, 1 << 14))
        );
        assert_eq!(
            image_dimensions("image/webp", &vp8x_of(1 << 24, 1 << 24)),
            Some((1 << 24, 1 << 24))
        );
    }

    /// The three measures are three measures. File bytes decide the transport
    /// cap — an 8×8 picture stored in over 2 MB is still refused — while the
    /// pixels decide what the window pays, which for that picture is nothing.
    #[test]
    fn the_file_cap_counts_file_bytes_whatever_the_pixels_are() {
        let ws = temp_workspace("cap-measures");
        let heavy = png_of(8, 8, IMAGE_FILE_CAP as usize);
        assert!(
            heavy.len() as u64 > IMAGE_FILE_CAP,
            "a small picture, a big file"
        );
        fs::write(ws.root().join("heavy.png"), &heavy).unwrap();

        let refused = ws.pasted_image("heavy.png").unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        let refused = ws.save_pasted_image(heavy, false).unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");

        // Under the cap, the same 8×8 picture rides — and its weight is its
        // pixels, not its file size.
        fs::write(ws.root().join("small.png"), png_of(8, 8, 1_000_000)).unwrap();
        let image = ws.pasted_image("small.png").unwrap().unwrap();
        assert_eq!(image.pixels, Some((8, 8)));
        assert!(image.bytes.len() > 1_000_000);
        assert!(
            image.weight() < 64,
            "a megabyte of file, one token of picture: {}",
            image.weight()
        );
    }

    /// The size travels with the image, so everything downstream — the
    /// context budget, the attach gate, an image read again after its bytes
    /// were shed — counts the picture by what it is, not by how it was stored.
    /// Both doors that build one fill it in.
    #[test]
    fn a_pasted_image_carries_the_size_its_header_names() {
        let ws = temp_workspace("paste-pixels");
        fs::write(ws.root().join("screen.png"), png_of(1_920, 1_080, 500)).unwrap();
        let image = ws.pasted_image("screen.png").unwrap().unwrap();
        assert_eq!(image.pixels, Some((1_920, 1_080)));

        let pasted = ws.save_pasted_image(png_of(800, 600, 4), false).unwrap();
        assert_eq!(pasted.pixels, Some((800, 600)));
    }

    /// The fallback is a promise: an image that sniffs as a picture but whose
    /// header names no size weighs its bytes — the erring-high road — so a
    /// truncated screenshot is never counted as free.
    #[test]
    fn an_image_with_no_readable_size_weighs_its_bytes() {
        let ws = temp_workspace("unknown-pixels");
        // A png signature, then padding: an image by `image_mime`, no IHDR to
        // read a size from.
        fs::write(ws.root().join("shot.png"), png(4_000)).unwrap();
        let image = ws.pasted_image("shot.png").unwrap().unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.pixels, None);
        assert_eq!(
            image.weight(),
            image.bytes.len() + image.path.len() + image.mime.len(),
            "no size to read, so the bytes are the estimate"
        );
    }

    /// The shapes a human's paste arrives in and the name each one keeps: a
    /// workspace-relative name, an absolute path (inside the root or out of
    /// it), a quoted name, and the escapes a terminal uses for a file's own
    /// spaces — a backslash and a percent-encoding.
    #[test]
    fn a_paste_naming_an_image_is_read_as_one() {
        let ws = temp_workspace("paste-shapes");
        fs::create_dir_all(ws.root().join("shots")).unwrap();
        fs::write(ws.root().join("shots/a.png"), png(0)).unwrap();
        fs::write(ws.root().join("my shot.png"), png(4)).unwrap();

        // A bare workspace-relative name.
        let image = ws.pasted_image("shots/a.png").unwrap().unwrap();
        assert_eq!(image.path, "shots/a.png");
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.bytes, png(0));

        // An absolute path inside the root keeps the relative name: the
        // placeholder must mean the file in the workspace the model works in.
        let inside = ws.root().join("shots/a.png").display().to_string();
        let image = ws.pasted_image(&inside).unwrap().unwrap();
        assert_eq!(image.path, "shots/a.png");

        // An absolute path outside the root is copied into `.mush/paste/`: the
        // human may name what the model's tools may not, and the copy's name is
        // the one a placeholder can promise to a restart.
        let outside = outside_image("paste-shapes", &png(0));
        let image = ws
            .pasted_image(&outside.display().to_string())
            .unwrap()
            .unwrap();
        assert!(
            image.path.starts_with(".mush/paste/pasted-"),
            "the copy's name, not the human's path: {}",
            image.path
        );
        assert_eq!(
            fs::read(ws.root().join(&image.path)).unwrap(),
            png(0),
            "the copy holds the original's bytes"
        );

        // A `file://` URL from a browser: the scheme and the `localhost` host
        // go, and `%20` becomes the space it encodes.
        let url = format!("file://localhost{}/my%20shot.png", ws.root().display());
        let image = ws.pasted_image(&url).unwrap().unwrap();
        assert_eq!(image.path, "my shot.png");
        let url = format!("file://{}/my%20shot.png", ws.root().display());
        let image = ws.pasted_image(&url).unwrap().unwrap();
        assert_eq!(image.path, "my shot.png");

        // The two spellings a terminal uses for a dragged file's own space: a
        // backslash, and the quotes a "copy as path" puts around it.
        let image = ws.pasted_image("my\\ shot.png").unwrap().unwrap();
        assert_eq!(image.path, "my shot.png");
        let image = ws.pasted_image("\"my shot.png\"").unwrap().unwrap();
        assert_eq!(
            image.path, "my shot.png",
            "a quoted paste is one name, spaces and all"
        );
        let image = ws.pasted_image("\"shots/a.png\"").unwrap().unwrap();
        assert_eq!(image.path, "shots/a.png");
        let image = ws.pasted_image("'shots/a.png'").unwrap().unwrap();
        assert_eq!(image.path, "shots/a.png");

        // A `%XX` sequence is decoded as the bytes it is, so a name that
        // arrives UTF-8 percent-encoded is the character it spells.
        fs::write(ws.root().join("café.png"), png(0)).unwrap();
        let url = format!("file://{}/caf%C3%A9.png", ws.root().display());
        let image = ws.pasted_image(&url).unwrap().unwrap();
        assert_eq!(image.path, "café.png");

        // An escape around a shell-special character, and the whitespace a
        // paste may carry at its ends.
        fs::write(ws.root().join("a(1).png"), png(0)).unwrap();
        let image = ws.pasted_image("  a\\(1\\).png\n").unwrap().unwrap();
        assert_eq!(image.path, "a(1).png");
    }

    /// The human's ruling: a paste names a file anywhere, and the picture must
    /// be here after a restart. An image outside the root is read under the
    /// human's own path and then copied into `.mush/paste/` — the copy is the
    /// image's path, the one name the model's own tools can resolve — with the
    /// bytes, the mime and the pixel size unchanged, and the original left
    /// exactly where it was.
    #[test]
    fn a_paste_from_outside_the_root_is_copied_into_the_workspace() {
        let ws = temp_workspace("paste-outside");
        let original = png_of(1_920, 1_080, 500);
        let outside = outside_image("paste-outside", &original);

        let image = ws
            .pasted_image(&outside.display().to_string())
            .unwrap()
            .expect("the outside file is an image");
        assert!(
            image.path.starts_with(".mush/paste/pasted-"),
            "the copy, not the human's path: {}",
            image.path
        );
        assert!(image.path.ends_with(".png"), "{}", image.path);
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.pixels, Some((1_920, 1_080)));
        assert_eq!(image.bytes, original, "the bytes that ride are the file's");
        assert_eq!(
            fs::read(ws.root().join(&image.path)).unwrap(),
            original,
            "and the copy on disk is byte for byte the original"
        );
        assert_eq!(
            fs::read(&outside).unwrap(),
            original,
            "the human's file is read, not moved or rewritten"
        );
        assert_eq!(
            fs::read_to_string(ws.root().join(".mush/.gitignore")).unwrap(),
            "*\n",
            "the copy is invisible to git, like every paste"
        );
    }

    /// The copy is not one shape's special case: every spelling of an outside
    /// name this door reads lands a picture under `.mush/paste/` — a bare
    /// absolute path, a quoted one, the `\ ` a terminal escapes a dragged
    /// file's space with, and a `file://` URL from a browser.
    #[test]
    fn every_shape_of_an_outside_paste_copies_the_same_way() {
        let ws = temp_workspace("paste-outside-shapes");
        let plain = outside_image("paste-outside-plain", &png(4));
        let spaced = outside_image("paste-outside space", &png(8));
        assert!(
            spaced.to_string_lossy().contains(' '),
            "the file under test has a space in its name"
        );

        let plain = plain.display().to_string();
        let spaced = spaced.display().to_string();
        for paste in [
            plain.clone(),
            format!("\"{plain}\""),
            format!("file://{plain}"),
            spaced.replace(' ', "\\ "),
            format!("file://{}", spaced.replace(' ', "%20")),
        ] {
            let image = ws
                .pasted_image(&paste)
                .unwrap()
                .unwrap_or_else(|| panic!("{paste:?} names an outside image"));
            assert!(
                image.path.starts_with(".mush/paste/pasted-"),
                "{paste:?} is copied to a reachable name, got {}",
                image.path
            );
            assert_eq!(
                fs::read(ws.root().join(&image.path)).unwrap(),
                image.bytes,
                "{paste:?} wrote its bytes to the copy"
            );
        }
    }

    /// A paste naming an image inside the root is not copied: the name the
    /// human gave is already one the model's tools resolve, so the paste writes
    /// nothing at all — no `.mush/paste/` entry to drift from the original.
    #[test]
    fn a_paste_from_inside_the_root_names_the_file_and_writes_nothing() {
        let ws = temp_workspace("paste-inside");
        fs::create_dir_all(ws.root().join("shots")).unwrap();
        fs::write(ws.root().join("shots/a.png"), png(0)).unwrap();

        for paste in [
            "shots/a.png".to_string(),
            ws.root().join("shots/a.png").display().to_string(),
        ] {
            let image = ws.pasted_image(&paste).unwrap().unwrap();
            assert_eq!(image.path, "shots/a.png", "{paste}");
        }
        assert!(
            !ws.root().join(".mush").exists(),
            "no copy and no `.mush/paste/`: the paste wrote nothing"
        );
    }

    /// One paste may name several pictures: four paths separated by the bare
    /// spaces a terminal pastes are four images, in the order pasted, each
    /// with its own bytes. This is the human's report at the door it failed —
    /// the one-name parse read the whole paste as prose.
    #[test]
    fn a_paste_of_several_image_names_reads_as_several_images() {
        let ws = temp_workspace("paste-many");
        fs::create_dir_all(ws.root().join("shots")).unwrap();
        let names = ["shot-1.png", "shot-2.png", "shot-3.png", "shot-4.png"];
        for (i, name) in names.iter().enumerate() {
            fs::write(ws.root().join("shots").join(name), png(i)).unwrap();
        }
        let paste = names
            .iter()
            .map(|name| format!("shots/{name}"))
            .collect::<Vec<_>>()
            .join(" ");

        let images = ws
            .pasted_images(&paste)
            .unwrap()
            .expect("four names, four images");
        assert_eq!(images.len(), 4);
        for (i, image) in images.iter().enumerate() {
            assert_eq!(image.path, format!("shots/{}", names[i]), "paste order");
            assert_eq!(image.bytes, png(i), "the {}th file's own bytes", i + 1);
        }
        // The same string through the one-name door is not one name, and is
        // text: the single-image contract is untouched.
        assert!(ws.pasted_image(&paste).unwrap().is_none());
    }

    /// The one-name rule's intent, for a list: one word that is not an image
    /// makes the whole paste the words it is — `Ok(None)`, nothing attached —
    /// so prose is not hijacked by a path-shaped word in it and a typo cannot
    /// half-attach a batch.
    #[test]
    fn a_paste_with_a_word_that_is_not_an_image_is_text() {
        let ws = temp_workspace("paste-many-text");
        fs::create_dir_all(ws.root().join("shots")).unwrap();
        fs::write(ws.root().join("shots/a.png"), png(0)).unwrap();
        fs::write(ws.root().join("shots/b.png"), png(4)).unwrap();

        for paste in [
            "shots/a.png notes.txt",
            "notes.txt shots/a.png",
            "shots/a.png missing.png",
            "shots/a.png shots/b.png and look at this",
            "look at shots/a.png",
            "shots/a.png ../secret.png",
        ] {
            assert!(
                ws.pasted_images(paste).unwrap().is_none(),
                "{paste:?} is text, not images"
            );
        }
    }

    /// The shapes compose in one paste: a quoted name among several (a file's
    /// own space), newline-separated names, `\ `-escaped spaces, and a
    /// `file://` URL with `%20` — each batch attaches what it names, in the
    /// order pasted.
    #[test]
    fn every_shape_of_a_name_composes_in_one_paste() {
        let ws = temp_workspace("paste-many-shapes");
        fs::create_dir_all(ws.root().join("shots")).unwrap();
        fs::write(ws.root().join("shots/a.png"), png(0)).unwrap();
        fs::write(ws.root().join("shots/b.png"), png(4)).unwrap();
        fs::write(ws.root().join("shots/c.png"), png(6)).unwrap();
        fs::write(ws.root().join("my shot.png"), png(8)).unwrap();
        fs::write(ws.root().join("my other shot.png"), png(12)).unwrap();
        let paths = |paste: &str| -> Vec<String> {
            ws.pasted_images(paste)
                .unwrap()
                .unwrap_or_else(|| panic!("{paste:?} names images"))
                .into_iter()
                .map(|image| image.path)
                .collect()
        };

        // A quoted name between two bare ones: the spaces inside the quotes
        // are the name's, the spaces outside them are separators.
        assert_eq!(
            paths("shots/a.png \"my shot.png\" shots/b.png"),
            ["shots/a.png", "my shot.png", "shots/b.png"]
        );

        // Newline-separated names, the shape four paths from a file manager's
        // "copy" arrive in.
        assert_eq!(
            paths("shots/a.png\nshots/c.png\nshots/b.png"),
            ["shots/a.png", "shots/c.png", "shots/b.png"]
        );

        // The two spellings of a file's own space, each a whole word: the
        // backslash a terminal escapes it with, and a `%20` inside a
        // `file://` URL.
        assert_eq!(
            paths("shots/a.png my\\ shot.png shots/b.png"),
            ["shots/a.png", "my shot.png", "shots/b.png"]
        );
        let url = format!("file://{}/my%20other%20shot.png", ws.root().display());
        assert_eq!(
            paths(&format!("shots/a.png {url} shots/b.png")),
            ["shots/a.png", "my other shot.png", "shots/b.png"]
        );
    }

    /// Every member named from outside the root is copied into `.mush/paste/`,
    /// and the copy — never the human's path — is the image's path: the rule
    /// the single-image paste landed, reused for the batch rather than
    /// re-implemented, so every picture ends up one the model's own tools can
    /// reach. Four outside pictures pasted at once get four *distinct* copies,
    /// each holding its own bytes.
    #[test]
    fn every_outside_member_of_a_paste_is_copied_into_the_workspace() {
        let ws = temp_workspace("paste-many-outside");
        let originals: Vec<Vec<u8>> = (0..4)
            .map(|i| png_of(100 + i, 50, 4 + i as usize))
            .collect();
        let outside: Vec<_> = originals
            .iter()
            .enumerate()
            .map(|(i, bytes)| outside_image(&format!("paste-many-outside-{i}"), bytes))
            .collect();
        let paste = outside
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");

        let images = ws
            .pasted_images(&paste)
            .unwrap()
            .expect("four outside pictures, four images");
        assert_eq!(images.len(), 4);
        for (i, (image, original)) in images.iter().zip(&originals).enumerate() {
            assert!(
                image.path.starts_with(".mush/paste/pasted-"),
                "the copy, not the human's path: {}",
                image.path
            );
            assert_eq!(image.bytes, *original, "picture {i} rides its own bytes");
            assert_eq!(
                fs::read(ws.root().join(&image.path)).unwrap(),
                *original,
                "and the copy on disk is byte for byte its own original"
            );
        }
        let paths: Vec<&str> = images.iter().map(|image| image.path.as_str()).collect();
        let unique: std::collections::HashSet<&str> = paths.iter().copied().collect();
        assert_eq!(
            unique.len(),
            4,
            "four copies, not one overwritten three times: {paths:?}"
        );
    }

    /// Several pictures written in one millisecond — a paste of four is exactly
    /// that — must not take one another's name: the first is
    /// `pasted-<millis>.png`, the next `-2`, the next `-3`, and each file keeps
    /// its own bytes. The rule is the writer's, shared by the clipboard road
    /// and the pasted-path road, so neither can disagree about it.
    #[test]
    fn pastes_in_one_millisecond_take_distinct_names() {
        let ws = temp_workspace("paste-same-millisecond");
        let dir = ws.root().join(".mush/paste");
        fs::create_dir_all(&dir).unwrap();

        let (first, mut file) = create_paste_file(&dir, 1_700_000_000_000, "image/png").unwrap();
        file.write_all(&png(0)).unwrap();
        let (second, mut file) = create_paste_file(&dir, 1_700_000_000_000, "image/png").unwrap();
        file.write_all(&png(4)).unwrap();
        let (third, _file) = create_paste_file(&dir, 1_700_000_000_000, "image/png").unwrap();

        assert_eq!(first, "pasted-1700000000000.png");
        assert_eq!(second, "pasted-1700000000000-2.png");
        assert_eq!(third, "pasted-1700000000000-3.png");
        assert_eq!(
            fs::read(dir.join(&first)).unwrap(),
            png(0),
            "the first keeps its bytes"
        );
        assert_eq!(
            fs::read(dir.join(&second)).unwrap(),
            png(4),
            "and the second its own"
        );
    }

    /// The paste directory is a bound, not a pile: a picture an earlier run
    /// left behind — older than this run and older than a day — is gone the
    /// next time anything is pasted, while the picture a live transcript names
    /// is never touched (finding B13: "5 pastes leave 5 files", and the
    /// directory lived in `.mush/.gitignore`, so only `du` ever said so).
    #[test]
    fn the_paste_directory_is_pruned_but_not_under_a_live_image() {
        let day = PASTE_MAX_AGE_MILLIS;
        let now = now_millis();
        let mut ws = temp_workspace("paste-prune");
        // A run that began three days ago, as a long session's workspace is.
        ws.opened = now - 3 * day;
        let dir = paste_dir(ws.root());
        fs::create_dir_all(&dir).unwrap();

        // Two pastes from a run that ended before this one began: older than
        // the run and older than a day, so the prune's candidates.
        let (old_a, mut file) = create_paste_file(&dir, now - 5 * day, "image/png").unwrap();
        file.write_all(&png(0)).unwrap();
        let (old_b, mut file) = create_paste_file(&dir, now - 5 * day - 1, "image/png").unwrap();
        file.write_all(&png(4)).unwrap();
        // The live one: this run pasted it on its first day, and its
        // transcript still names the path. It is *older than a day* on purpose
        // — the run's start, not the age, is what protects it.
        let (live_old, mut file) = create_paste_file(&dir, now - 2 * day, "image/png").unwrap();
        file.write_all(&png(8)).unwrap();

        // The paste the write road is making now, and the prune it runs.
        let live = ws.save_pasted_image(png(12), false).unwrap();

        assert!(!dir.join(&old_a).exists(), "{old_a} is older than the run");
        assert!(!dir.join(&old_b).exists(), "and {old_b} with it");
        assert!(
            dir.join(&live_old).exists(),
            "the live transcript's picture stays, however old it is: {live_old}"
        );
        let name = live.path.rsplit('/').next().unwrap();
        assert!(
            dir.join(name).exists(),
            "and the paste just written is never a candidate: {name}"
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            2,
            "the directory holds the run's pictures and nothing older"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A refusal a paste that cannot be written is told names the file where it
    /// would have been — `.mush/paste/<name>` — not `.mush/<name>`: the message
    /// is what the human acts on, and the two write failures used to point
    /// beside the directory. One spelling ([`PASTE_REL`]) serves the join that
    /// makes the directory, the refusal and the path an [`Image`] carries.
    #[test]
    fn a_paste_that_cannot_be_written_names_the_paste_directory() {
        let ws = temp_workspace("paste-spelling");
        // `.mush/paste` is a file, so the directory cannot be made and a file
        // inside it cannot be opened.
        fs::create_dir_all(ws.root().join(".mush")).unwrap();
        fs::write(ws.root().join(PASTE_REL), "not a directory").unwrap();

        let refused = ws.save_pasted_image(png(4), false).unwrap_err();
        assert!(refused.contains("cannot create .mush/paste: "), "{refused}");

        let refused = create_paste_file(&ws.root().join(PASTE_REL), 1_700_000_000_000, "image/png")
            .unwrap_err();
        assert!(
            refused.contains("cannot write .mush/paste/pasted-1700000000000.png: "),
            "{refused}"
        );

        assert_eq!(paste_rel("shot.png"), ".mush/paste/shot.png");
        assert_eq!(paste_dir(ws.root()), ws.root().join(PASTE_REL));
    }

    /// An image past the cap is a refusal, not the text fallback, whichever
    /// way it was named: a batch holding one says the read's own sentence and
    /// attaches nothing — the app lands the whole paste as text.
    #[test]
    fn a_paste_member_past_the_cap_is_refused() {
        let ws = temp_workspace("paste-many-big");
        fs::write(ws.root().join("small.png"), png(0)).unwrap();
        fs::write(ws.root().join("big.png"), png(IMAGE_FILE_CAP as usize)).unwrap();

        let refused = ws
            .pasted_images("small.png big.png")
            .expect_err("the big member is refused");
        assert!(
            refused.contains("big.png"),
            "the member is named: {refused}"
        );
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(refused.contains("convert"), "the downscale road: {refused}");
    }

    /// A FIFO named like an image is not an image, and is never opened:
    /// `File::open` on a FIFO with no writer blocks until one appears, so the
    /// old order — open, sniff, decide — froze the pane on the human's paste
    /// and parked an actor on the model's read, on a name as innocent as
    /// `x.png`. Both roads answer from the metadata now, and the watchdog is
    /// the test: the old code never answers, and a hang must fail here rather
    /// than hang the suite.
    #[test]
    fn a_fifo_named_like_an_image_is_never_opened() {
        let ws = temp_workspace("fifo");
        let fifo = ws.root().join("x.png");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(
            made.is_ok_and(|status| status.success()),
            "this test needs `mkfifo` to build the shape it pins"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let (paste_ws, read_ws) = (ws.clone(), ws.clone());
        std::thread::spawn(move || {
            let _ = tx.send((
                paste_ws.pasted_image("x.png"),
                read_ws.read_window("x.png", 1, 10, 1024),
            ));
        });
        let (image, text) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a FIFO must answer, not hold an open until a writer appears");
        assert!(
            image.unwrap().is_none(),
            "a FIFO is not an image, so the paste is the text it is"
        );
        assert!(text.is_err(), "and there are no bytes to read: {text:?}");
    }

    /// `read_file`'s own regular-file guard, pinned apart from the image road
    /// that happened to exercise it: a path that is not a regular file is
    /// refused from the metadata — `"{rel} is not a regular file — cannot read
    /// it"` — before anything is opened, the same shape of check
    /// [`Self::image_at`] makes. The FIFO test above reads `read_window`, so
    /// deleting this guard would leave `read_file` free to open a FIFO with no
    /// writer and park an actor forever; the watchdog is the test here too,
    /// because the old code never answers and a hang must fail rather than hang
    /// the suite. A directory named like a file is the cheap half of the same
    /// rule.
    #[test]
    fn read_file_refuses_a_path_that_is_not_a_regular_file() {
        let ws = temp_workspace("read-file-guard");
        let fifo = ws.root().join("x.log");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(
            made.is_ok_and(|status| status.success()),
            "this test needs `mkfifo` to build the shape it pins"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let read_ws = ws.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_ws.read_file("x.log"));
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a FIFO must be refused, not held open until a writer appears")
            .unwrap_err();
        assert!(
            refused.contains("x.log is not a regular file"),
            "the guard's own sentence: {refused}"
        );

        fs::create_dir_all(ws.root().join("notes.txt")).unwrap();
        let refused = ws.read_file("notes.txt").unwrap_err();
        assert!(
            refused.contains("notes.txt is not a regular file"),
            "and a directory is not read either: {refused}"
        );
    }

    /// A write changes the bytes and nothing else: the mode is a fact of the
    /// file, not of the temp file the rename carries it from. This is finding
    /// B1's whole failure — every `write_file` rebuilt the inode at `0600`, so a
    /// `0755` script stopped running and `git` recorded the mode change — and
    /// the two edges the repair draws: a new file is made the way the box makes
    /// one (`0666 & !umask`, measured against `fs::write`'s own file rather
    /// than spelled as `0644`), and a file with no owner-write bit is a refusal
    /// the model can read, naming the mode it found.
    #[test]
    fn a_write_keeps_the_files_mode() {
        let ws = temp_workspace("write-mode");
        let run = ws.root().join("run.sh");
        let data = ws.root().join("data.txt");
        fs::write(&run, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&data, "a\n").unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o644)).unwrap();

        ws.write_file("run.sh", "#!/bin/sh\necho hi\n").unwrap();
        ws.write_file("data.txt", "b\n").unwrap();
        assert_eq!(mode_of(&run), 0o755, "the executable bit survives");
        assert_eq!(mode_of(&data), 0o644, "group and other readability survive");

        // A new file is the box's, not mush's: `fs::write` makes one out of the
        // same `0666 & !umask`, so the oracle is measured here, not spelled.
        fs::write(ws.root().join("oracle.txt"), "x\n").unwrap();
        ws.write_file("new.txt", "y\n").unwrap();
        assert_eq!(
            mode_of(&ws.root().join("new.txt")),
            mode_of(&ws.root().join("oracle.txt")),
            "a new file is the way the rest of the box makes one"
        );

        // The read-only bit is not overridden: the door refuses, names the mode
        // and leaves the file as it was.
        fs::set_permissions(&run, fs::Permissions::from_mode(0o444)).unwrap();
        let refused = ws.write_file("run.sh", "replaced\n").unwrap_err();
        assert!(refused.contains("0444"), "the mode is named: {refused}");
        assert!(refused.contains("owner-write"), "{refused}");
        assert_eq!(fs::read_to_string(&run).unwrap(), "#!/bin/sh\necho hi\n");
        assert_eq!(mode_of(&run), 0o444);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The other half of [`Fresh`]: a store mush writes for itself — the
    /// session, the key-bearing home config — is not the box's file, so a new
    /// one is `0600` whatever the human's umask says. `atomic_write` is the one
    /// road both audiences travel, and this pins the one decision they differ
    /// on; the session and userconfig tests pin their own road's call.
    #[test]
    fn a_private_store_is_made_0600() {
        let ws = temp_workspace("fresh-private");
        let path = ws.root().join("session.json");
        atomic_write(&path, b"{}", Fresh::Private).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "mush's own file is the owner's alone"
        );

        // An existing file keeps whatever it has, private or not: `Fresh`
        // decides new files only, and a mode a file already has is not the
        // umask's business.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_write(&path, b"{\"a\":1}", Fresh::Private).unwrap();
        assert_eq!(mode_of(&path), 0o640);
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// One numbering rule for the copy kept beside a file mush cannot use: the
    /// first free `.bak` name, then `.bak.2`, …, and the shared bound is what
    /// stops it. The session store and the home config both take this road
    /// ([`crate::session::keep_unreadable`], [`crate::userconfig`]), so the two
    /// cannot disagree about which copy is the second accident; each keeps its
    /// own reason for the copy in its own doc.
    #[test]
    fn a_backup_name_is_the_first_free_one_beside_the_file() {
        let root = Scratch::new("backup");
        let file = root.join("thing.json");
        fs::write(&file, "x").unwrap();

        assert_eq!(backup_name(&file).unwrap(), root.join("thing.json.bak"));
        fs::write(root.join("thing.json.bak"), "kept").unwrap();
        assert_eq!(backup_name(&file).unwrap(), root.join("thing.json.bak.2"));

        // Every name the bound allows, taken: the answer is the refusal, not a
        // hundred-and-first name.
        for step in 1..=BACKUP_TRIES {
            let name = if step == 1 {
                "thing.json.bak".to_string()
            } else {
                format!("thing.json.bak.{step}")
            };
            fs::write(root.join(name), "kept").unwrap();
        }
        let refused = backup_name(&file).unwrap_err();
        assert!(refused.contains("every backup name beside"), "{refused}");
        assert!(refused.contains("thing.json"), "{refused}");
        let _ = fs::remove_dir_all(&root);
    }

    /// The one fact a rename cannot keep, pinned as the fact it is: a hard link
    /// is another *name* for the inode, and `rename` replaces the name — so the
    /// two names fork, the other keeps the old bytes, and they stop being one
    /// inode. This is accepted, not fixed: the alternative, copying into the
    /// existing inode, would give up the atomic rename that promises a reader
    /// never sees a half-written file, and [`atomic_write`]'s doc says so. A
    /// *symlink* is the case a rename can and does follow
    /// (`an_edit_follows_a_symlink_to_its_target`, in the TUI crate's tests).
    #[test]
    fn a_hard_link_forks_under_the_rename() {
        use std::os::unix::fs::MetadataExt;

        let ws = temp_workspace("hard-link");
        let left = ws.root().join("left.txt");
        let right = ws.root().join("right.txt");
        fs::write(&left, "one\n").unwrap();
        fs::hard_link(&left, &right).unwrap();

        ws.write_file("left.txt", "two\n").unwrap();
        assert_eq!(fs::read_to_string(&left).unwrap(), "two\n");
        assert_eq!(
            fs::read_to_string(&right).unwrap(),
            "one\n",
            "the twin keeps the old bytes"
        );
        let a = fs::metadata(&left).unwrap();
        let b = fs::metadata(&right).unwrap();
        assert_ne!(a.ino(), b.ino(), "and the two names fork");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A name that is not a file is not content, and a rename replaces the name
    /// whatever it is: the workspace's own attach socket dies this way (finding
    /// B3 — every `mush read`/`agents`/`edit` in that directory answered "no
    /// mush is running" until mush restarted) and a FIFO goes the same way.
    /// Both doors refuse, naming the type: the tool's `write_file` and the
    /// `atomic_write` that `session::save` and the home config use, because a
    /// caller that does not pass through the tool's door must not rename over
    /// one either.
    #[test]
    fn a_write_will_not_replace_a_socket_or_a_fifo() {
        use std::os::unix::net::UnixListener;

        let ws = temp_workspace("write-type");
        fs::create_dir_all(ws.root().join(".mush")).unwrap();
        let sock = ws.root().join(".mush/mush.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let refused = ws
            .write_file(".mush/mush.sock", "not a socket any more\n")
            .unwrap_err();
        assert!(refused.contains("socket"), "the type is named: {refused}");
        assert!(fs::symlink_metadata(&sock).unwrap().file_type().is_socket());
        assert!(
            atomic_write(&sock, b"x", Fresh::Box).is_err(),
            "the guard is kept beside the rename too"
        );
        // A socket's inode is not the fact that matters: a client must still be
        // able to connect, because that is what mush's attach road does.
        std::os::unix::net::UnixStream::connect(&sock)
            .expect("the attach socket must still accept a client");

        let fifo = ws.root().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(
            made.is_ok_and(|status| status.success()),
            "this test needs `mkfifo` to build the shape it pins"
        );
        let refused = ws.write_file("pipe", "content\n").unwrap_err();
        assert!(refused.contains("FIFO"), "the type is named: {refused}");
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());

        // And an ordinary file and a brand-new name still write.
        ws.write_file("plain.txt", "a\n").unwrap();
        ws.write_file("fresh.txt", "b\n").unwrap();
        assert_eq!(ws.read_file("fresh.txt").unwrap(), "b\n");
        drop(listener);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The store's own files are refused by *name*, on top of the shape check
    /// above: the lock (finding E2) and the conversation — with the `.bak`,
    /// `.bak.2`, … copies `keep_unreadable` sets aside — are regular files, so
    /// a socket/FIFO refusal cannot see them, and a rename over one of them is
    /// how one tool call ("clean up stale locks", "reset .mush") puts a second
    /// mush on the store or loses the chat. The names are the store's own list
    /// ([`session::STORE_FILES`]), and everything else under `.mush/` is a
    /// name like any other: a note, a paste.
    #[test]
    fn a_write_will_not_replace_the_stores_own_files() {
        use std::os::unix::fs::symlink;

        let ws = temp_workspace("write-store");
        session::ensure_mush_dir(ws.root()).unwrap();
        for name in [
            ".mush/lock",
            ".mush/session.json",
            ".mush/session.json.bak",
            ".mush/session.json.bak.2",
        ] {
            let refused = ws.write_file(name, "replaced\n").unwrap_err();
            assert!(
                refused.contains(name),
                "the refusal names the file: {refused}"
            );
            assert!(refused.contains("refusing to replace it"), "{refused}");
        }
        // A link that resolves to a store file is a write to that file whatever
        // it is called: the real path is what the refusal is asked about.
        fs::write(ws.root().join(".mush/session.json"), "{}").unwrap();
        symlink(
            ws.root().join(".mush/session.json"),
            ws.root().join(".mush/notes"),
        )
        .unwrap();
        let refused = ws.write_file(".mush/notes", "replaced\n").unwrap_err();
        assert!(refused.contains(".mush/notes"), "{refused}");
        assert!(refused.contains("conversation"), "{refused}");
        // The positive twin: names that are not the store's still write, in
        // mush's directory and under one of its subdirectories.
        ws.write_file(".mush/note.txt", "a note\n").unwrap();
        ws.write_file(".mush/paste/lock", "a pasted picture named lock\n")
            .unwrap();
        assert_eq!(
            ws.read_file(".mush/paste/lock").unwrap(),
            "a pasted picture named lock\n"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// Every road that touches the filesystem asks what really lies under a
    /// name (finding B4): `resolve` is lexical, so a link the root *contains*
    /// used to carry reads, writes, listings and searches out of the workspace
    /// (`write_file("out/written.txt")` landed in the link's directory). The
    /// outside directory is the proof of the refusal, and the positive twin
    /// rides in the same test: a link to a file *inside* the root still reads,
    /// a write through it lands in the target and keeps the link a link, and a
    /// symlinked directory is not followed — the listing's own claim, which its
    /// start used to break.
    #[test]
    fn a_link_inside_the_root_cannot_leave_it() {
        use std::os::unix::fs::symlink;

        let outside = Scratch::new("test-root-link-outside");
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(outside.join("secret.txt"), "SEKRIT\n").unwrap();
        fs::write(outside.join("sub/deep.txt"), "DEEP\n").unwrap();

        let ws = temp_workspace("root-link");
        symlink(&outside, ws.root().join("out")).unwrap();
        symlink(&outside, ws.root().join("sub-out")).unwrap();
        fs::write(ws.root().join("inside.txt"), "INSIDE\n").unwrap();
        symlink(ws.root().join("inside.txt"), ws.root().join("link.txt")).unwrap();

        let mut refusals = vec![
            ws.read_file("out/secret.txt").unwrap_err(),
            ws.write_file("out/written.txt", "ESCAPED\n").unwrap_err(),
            ws.list_files("out", 100).unwrap_err(),
        ];
        // `Matches` carries no `Debug`, so the search's refusal is matched out
        // by hand rather than unwrapped.
        refusals.push(match ws.search("DEEP", "out", 100, 0) {
            Err(refused) => refused,
            Ok(found) => panic!(
                "a search through a link out of the root must be refused, not run: {:?}",
                found.rows
            ),
        });
        for refused in refusals {
            assert!(
                refused.contains("outside the workspace"),
                "names the escape: {refused}"
            );
        }
        assert!(
            !outside.join("written.txt").exists(),
            "nothing landed outside"
        );
        let listed = ws.list_files("", 100).unwrap().0;
        assert!(
            !listed.iter().any(|file| file.contains("secret")),
            "a symlinked child directory is still not followed: {listed:?}"
        );

        // The positive twin: a link to a file inside the root keeps working.
        assert_eq!(ws.read_file("link.txt").unwrap(), "INSIDE\n");
        assert_eq!(
            ws.list_files("link.txt", 10).unwrap().0,
            vec!["link.txt".to_string()],
            "naming a link to a file answers about that file, under its name"
        );
        ws.write_file("link.txt", "EDITED\n").unwrap();
        assert!(
            fs::symlink_metadata(ws.root().join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the write went through the link and left it a link"
        );
        assert_eq!(
            fs::read_to_string(ws.root().join("inside.txt")).unwrap(),
            "EDITED\n"
        );

        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A file far past the cap is refused from what the stat said, not by being
    /// read. The 8 GiB here is sparse — a few blocks on disk, cheap on ext4 and
    /// tmpfs — so the old code's read-the-whole-file first would have allocated
    /// gigabytes to say what the metadata already said: a probe on a 128 MiB
    /// sparse png drove the test process's peak RSS from 3,172 kB to 134,136 kB
    /// before the refusal.
    #[test]
    fn a_sparse_file_far_past_the_cap_is_refused_without_a_whole_read() {
        let ws = temp_workspace("sparse-big");
        let path = ws.root().join("huge.png");
        let mut file = fs::File::create(&path).unwrap();
        file.set_len(8 * 1024 * 1024 * 1024).unwrap();
        file.write_all(&png(0)).unwrap();
        drop(file);

        let refused = ws.read_image("huge.png").unwrap_err();
        assert!(
            refused.contains("8589934592 bytes"),
            "the sentence names the size the stat saw: {refused}"
        );
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
    }

    /// A file bigger than the image cap is opened and refused with its own
    /// size: the number is the file's length — the stat's — and not a buffer's,
    /// and the head sniff ran first, so an over-cap file that is not an image
    /// stays text ([`Self::image_at`]'s head decides that). The audit found no
    /// test that opened one.
    #[test]
    fn a_file_bigger_than_the_cap_is_refused_with_the_size_the_stat_saw() {
        let ws = temp_workspace("over-cap-open");
        let bytes = png(IMAGE_FILE_CAP as usize + 4096);
        fs::write(ws.root().join("big.png"), &bytes).unwrap();

        let refused = ws.read_image("big.png").unwrap_err();
        assert!(
            refused.contains(&format!("of {} bytes", bytes.len())),
            "the file's own length is named: {refused}"
        );
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
    }

    /// The other half of the same rule: when the length in hand is a buffer's
    /// and not the picture's, the refusal names the cap and no number. The
    /// branch that reaches this is the growth race [`Self::image_at`]'s bounded
    /// read guards — a file that grew between the stat and the read — which a
    /// test cannot stage, because the stat and the read are one function; so
    /// the sentence is pinned directly, and the known-size branch beside it
    /// keeps its exact number.
    #[test]
    fn a_size_nobody_knows_is_not_named_in_the_refusal() {
        let unknown = image_too_big("shots/a.png", "image/png", None);
        assert!(unknown.contains("past the 2 MB cap"), "{unknown}");
        assert!(unknown.contains("its size is not known"), "{unknown}");
        assert!(
            !unknown.contains(" bytes"),
            "no length is claimed: {unknown}"
        );
        assert!(unknown.contains("convert"), "the road stays: {unknown}");

        let known = image_too_big("shots/a.png", "image/png", Some(3_000_000));
        assert!(known.contains("of 3000000 bytes"), "{known}");
    }

    /// The whole-read cap is a different ruler from the image cap, and every
    /// whole read is checked against it from the stat: `read_window` and
    /// `read_file` are one read ([`Workspace::whole_read`]), so they refuse with
    /// the *same* sentence, naming the road that does work (`run_command`) — and
    /// the count `write_file` asks for is bounded the same way
    /// ([`LineCount::More`]). The 32 MiB + 4 KiB here are sparse — a few blocks
    /// on disk — so the pin costs nothing to hold.
    ///
    /// A file past the cap with no read bit makes the "before the bytes are
    /// read" claim falsifiable: the cap sentence can only come from the stat,
    /// because an open of this file fails. A read-first implementation answers
    /// with the permission error and this test fails.
    #[test]
    fn a_file_bigger_than_the_read_cap_is_refused_before_any_read() {
        let ws = temp_workspace("over-read-cap");
        let path = ws.root().join("huge.log");
        let file = fs::File::create(&path).unwrap();
        file.set_len(READ_FILE_CAP + 4096).unwrap();
        drop(file);

        let window = ws.read_window("huge.log", 1, 10, 4_000).unwrap_err();
        assert!(
            window.contains(&format!("{} bytes", READ_FILE_CAP + 4096)),
            "the file's own length is named: {window}"
        );
        assert!(window.contains("past the 32 MB cap"), "{window}");
        assert!(
            window.contains("run_command"),
            "the road that works: {window}"
        );
        let whole = ws.read_file("huge.log").unwrap_err();
        assert_eq!(whole, window, "one cap, one sentence, whichever road asked");
        assert_eq!(
            ws.line_count("huge.log"),
            LineCount::More,
            "the count is not read over the cap either"
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let refused = ws.read_file("huge.log").unwrap_err();
        assert_eq!(
            refused, window,
            "the cap refusal comes from the stat, not from an open of the file"
        );
        assert_eq!(ws.line_count("huge.log"), LineCount::More);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(ws.root());
    }

    /// `outline` against every shape a path can have, and the file truths that
    /// keep it honest: a path that cannot be read is refused in the *window
    /// road's own words* (one whole read, so the two roads cannot disagree
    /// about what a path is), and a file with nothing to sketch answers with a
    /// sentence rather than a refusal — a markdown file, a config and an empty
    /// file are normal files, and "no definitions" is a fact, not a failure.
    ///
    /// The non-Rust answers are pinned here because they are the textual rule's
    /// declared cost. Prose with the word `fn` *inside* a sentence finds
    /// nothing, and a TOML `type = "lib"` finds nothing (the keyword must be
    /// followed by a name, not by `=`); but a markdown code fence whose line
    /// *opens* with `fn sample() {}` is a row, because the rule reads lines and
    /// the answer says on its own first line that it is not a compiler's. A
    /// Python file is the other half of that reading: the predicate takes a line
    /// and no path, so its keywords are a union over the world's languages, and
    /// a `def` is a row where a `fn` is one.
    #[test]
    fn the_outline_road_answers_every_shape_a_path_can_have() {
        let ws = temp_workspace("outline-shapes");

        // A path that is not there, a directory and a binary file: the read
        // refuses, and the sentences are the window road's own.
        assert!(ws
            .outline("no/such.rs")
            .unwrap_err()
            .contains("cannot read"));
        fs::create_dir_all(ws.root().join("dir")).unwrap();
        assert!(ws
            .outline("dir")
            .unwrap_err()
            .contains("not a regular file"));
        fs::write(ws.root().join("blob.bin"), b"text before \x00 after\n").unwrap();
        assert!(ws.outline("blob.bin").unwrap_err().contains("binary"));

        // Past the read cap, refused from the stat before any read — the
        // sparse file the window road's own cap test uses.
        let path = ws.root().join("huge.log");
        fs::File::create(&path)
            .unwrap()
            .set_len(READ_FILE_CAP + 4096)
            .unwrap();
        let refused = ws.outline("huge.log").unwrap_err();
        assert!(refused.contains("past the 32 MB cap"), "{refused}");
        assert!(refused.contains("run_command"), "{refused}");

        // An empty file has nothing to sketch, and says the file's own word.
        fs::write(ws.root().join("empty.rs"), "").unwrap();
        assert_eq!(
            ws.outline("empty.rs").unwrap().render(4_000, ""),
            "empty.rs is empty — there are no definitions to outline"
        );

        // A Rust file: the rows are the declarations' own lines.
        fs::write(
            ws.root().join("lib.rs"),
            "//! docs\npub fn a() {}\n\nstruct B;\n",
        )
        .unwrap();
        assert_eq!(
            ws.outline("lib.rs").unwrap().render(4_000, ""),
            "lib.rs — 4 lines; 2 definitions (textual, many languages, best-effort — not a \
             compiler's answer)\n\n  2  pub fn a() {}\n  4  struct B;"
        );

        // A Python file: the rule reads a line and no path, and its keywords
        // are a union over languages — `class` and `def` are this file's
        // declarations exactly as `struct` and `fn` are Rust's.
        fs::write(
            ws.root().join("handlers.py"),
            "import json\n\n\nclass Handler:\n    def handle(self, event):\n        return event\n",
        )
        .unwrap();
        assert_eq!(
            ws.outline("handlers.py")
                .unwrap()
                .definitions()
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["class Handler:", "    def handle(self, event):"]
        );

        // A markdown file: prose finds nothing, and the answer names the road
        // that still shows the text. A fenced line that *opens* like Rust does
        // find something — the rule is textual, and the header says so.
        fs::write(ws.root().join("NOTES.md"), "# Notes\nWe call fn things.\n").unwrap();
        assert_eq!(
            ws.outline("NOTES.md").unwrap().render(4_000, ""),
            "NOTES.md — 2 lines; no definitions (textual, many languages, best-effort — not a \
             compiler's answer); read_file shows the text"
        );
        fs::write(
            ws.root().join("SNIPPET.md"),
            "# Sample\n\n```rust\nfn sample() {}\n```\n",
        )
        .unwrap();
        let fenced = ws.outline("SNIPPET.md").unwrap().render(4_000, "");
        assert!(fenced.contains("1 definition"), "{fenced}");
        assert!(fenced.contains("  4  fn sample() {}"), "{fenced}");

        // A config: `type = "lib"` is not a Rust declaration, because the
        // keyword is followed by `=` and not by a name.
        fs::write(
            ws.root().join("Cargo.toml"),
            "[package]\nname = \"x\"\ntype = \"lib\"\n",
        )
        .unwrap();
        assert!(ws.outline("Cargo.toml").unwrap().is_empty());

        // A CRLF file: the rows are the lines' own text — an ending is not a
        // line's text — and the same sentence the window road writes says so.
        fs::write(ws.root().join("crlf.rs"), "fn a() {}\r\nfn b() {}\r\n").unwrap();
        let crlf = ws.outline("crlf.rs").unwrap().render(4_000, "");
        assert!(crlf.contains("  1  fn a() {}"), "{crlf}");
        assert!(crlf.contains("  2  fn b() {}"), "{crlf}");
        assert!(
            crlf.contains(CRLF_NOTE),
            "the endings are said, not hidden: {crlf}"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The room is the caller's and the count is the file's: the road that knows
    /// the cap it will render at keeps that room's rows and one more — the proof
    /// — and answers with the file's own count plus a sentence naming what it
    /// left. The read and the refusals are the same road's, room or no room.
    #[test]
    fn the_outline_road_keeps_the_room_its_caller_gives_it() {
        let ws = temp_workspace("outline-room");
        let body: String = (1..=500).map(|n| format!("fn item_{n}() {{}}\n")).collect();
        fs::write(ws.root().join("many.rs"), &body).unwrap();

        // No room named: this crate's ceiling for a result the model reads,
        // which is past every row this file has — so the answer is whole and
        // owes no note at all.
        let roomy = ws.outline("many.rs").unwrap();
        assert_eq!(roomy.total(), 500);
        assert_eq!(roomy.definitions().len(), 500);
        let whole = roomy.render(1_000_000, "");
        assert!(whole.contains("  500  fn item_500() {}"), "{whole}");
        assert!(!whole.contains("[mush: only the first"), "{whole}");

        // The caller's own room: three rows and the proof, the count of the
        // file in the header, and the note naming the rows the answer is not.
        let narrow = ws.outline_within("many.rs", 3).unwrap();
        assert_eq!(narrow.total(), 500);
        assert_eq!(narrow.definitions().len(), 4, "the room and the proof row");
        assert!(
            narrow.header().contains("500 definitions"),
            "{}",
            narrow.header()
        );
        let answer = narrow.render(4_000, "");
        assert!(answer.contains("  3  fn item_3() {}"), "{answer}");
        assert!(
            !answer.contains("fn item_4()"),
            "the proof is not a row: {answer}"
        );
        assert!(
            answer.contains("only the first 3 of 500 definitions are shown"),
            "{answer}"
        );

        // Nothing about the read changed with the room: the same refusals, in
        // the window road's own words.
        assert!(ws
            .outline_within("no/such.rs", 3)
            .unwrap_err()
            .contains("cannot read"));
        fs::write(ws.root().join("blob.bin"), b"text before \x00 after\n").unwrap();
        assert!(ws
            .outline_within("blob.bin", 3)
            .unwrap_err()
            .contains("binary"));
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The cap is the transport's, not the sniffer's: a file past it that is
    /// not an image is still text. The head decides that before any whole read
    /// — the cap must not turn "this is not a picture" into a refusal, or a
    /// long log with a `PNG`-looking start would stop being readable.
    #[test]
    fn an_over_cap_file_that_is_not_an_image_is_still_text() {
        let ws = temp_workspace("over-cap-text");
        let body = "not a picture\n".repeat(200_000);
        assert!(body.len() as u64 > IMAGE_FILE_CAP, "past the cap");
        fs::write(ws.root().join("notes.txt"), &body).unwrap();

        assert!(ws.read_image("notes.txt").unwrap().is_none());
        assert!(ws.pasted_image("notes.txt").unwrap().is_none());
        assert_eq!(ws.read_file("notes.txt").unwrap(), body);
    }

    /// A paste that is text, in every shape that is not an image's: prose, a
    /// paragraph, a path that is not there, a directory, a file that is not a
    /// picture, and a name that escapes the workspace. All of them are
    /// `Ok(None)` — the paste is inserted as the words it is.
    #[test]
    fn a_paste_that_is_not_an_image_is_text() {
        let ws = temp_workspace("paste-text");
        fs::write(ws.root().join("notes.txt"), "hello").unwrap();
        fs::create_dir_all(ws.root().join("shots")).unwrap();

        for paste in [
            "",
            "   \n",
            "hello world",
            "what about the tests?",
            "line one\nline two",
            "notes.txt",
            "missing.png",
            "/no/such/file.png",
            "../secret.png",
            "shots",
            // `~` is not expanded, and the workspace has no such file: a
            // relative name that is not there is text like any other.
            "~/shot.png",
        ] {
            assert!(
                ws.pasted_image(paste).unwrap().is_none(),
                "{paste:?} is text, not an image"
            );
        }
    }

    /// An image past the cap is an image that cannot ride, whichever way it
    /// was named: the refusal is a refusal, not the text fallback, and it names
    /// the one road a big picture has.
    #[test]
    fn an_image_past_the_cap_is_refused_with_a_downscale() {
        let ws = temp_workspace("paste-big");
        fs::write(ws.root().join("big.png"), png(IMAGE_FILE_CAP as usize)).unwrap();
        let refused = ws.pasted_image("big.png").unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(refused.contains("convert"), "the downscale: {refused}");
    }

    /// A clipboard image is written under `.mush/paste/` — which ignores
    /// itself, so a screenshot cannot dirty the tree — and the bytes come back
    /// whole: the file is what a later trim's placeholder names.
    #[test]
    fn a_clipboard_image_is_saved_under_mush_paste() {
        let ws = temp_workspace("clipboard");
        let image = ws.save_pasted_image(png(4), false).unwrap();
        assert_eq!(image.mime, "image/png");
        assert!(image.path.starts_with(".mush/paste/pasted-"), "{image:?}");
        assert!(image.path.ends_with(".png"), "{image:?}");
        assert_eq!(
            fs::read(ws.root().join(&image.path)).unwrap(),
            image.bytes,
            "the saved file is the bytes that were pasted"
        );
        assert_eq!(
            fs::read_to_string(ws.root().join(".mush/.gitignore")).unwrap(),
            "*\n",
            "`.mush` ignores itself, so the paste is invisible to git"
        );

        // The jpeg extension is the spelling a browser reads, and the other
        // three are the mime's own name.
        let jpeg = ws
            .save_pasted_image(vec![0xff, 0xd8, 0xff, 0xe0, 0x00], false)
            .unwrap();
        assert!(jpeg.path.ends_with(".jpg"), "{jpeg:?}");

        // Words are not an image, and an image past the cap names the
        // clipboard's own road: save it, downscale it, copy the smaller one.
        let refused = ws.save_pasted_image(b"hello".to_vec(), false).unwrap_err();
        assert!(refused.contains("not a png"), "{refused}");
        let refused = ws
            .save_pasted_image(png(IMAGE_FILE_CAP as usize), false)
            .unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(
            refused.contains("wl-paste"),
            "the clipboard road: {refused}"
        );
        assert!(refused.contains("convert"), "and the downscale: {refused}");
    }

    /// An outside image past the cap is refused the same way an inside one is —
    /// with the read tool's sentence naming the human's file — and the refusal
    /// comes before any copy: `.mush/paste/` never appears for a picture that
    /// cannot ride.
    #[test]
    fn an_outside_image_past_the_cap_is_refused_before_any_copy() {
        let ws = temp_workspace("paste-outside-big");
        let outside = outside_image("paste-outside-big", &png(IMAGE_FILE_CAP as usize));

        let refused = ws.pasted_image(&outside.display().to_string()).unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(
            refused.contains(&outside.display().to_string()),
            "the refusal names the human's file: {refused}"
        );
        assert!(!ws.root().join(".mush").exists(), "nothing was copied");
    }

    /// A copy that cannot be written is a refusal, not a broken attachment: the
    /// `Err` names the human's file and the problem, so the app can put the
    /// path in the box as text and say why — nothing half-written is left, and
    /// the original is untouched.
    #[test]
    fn a_copy_that_cannot_be_written_is_refused_and_attaches_nothing() {
        let ws = temp_workspace("paste-refused");
        // `.mush` is a file, so nothing can be created under it.
        fs::write(ws.root().join(".mush"), "not a directory").unwrap();
        let outside = outside_image("paste-refused", &png(4));

        let refused = ws.pasted_image(&outside.display().to_string()).unwrap_err();
        assert!(refused.contains("cannot copy"), "{refused}");
        assert!(
            refused.contains(&outside.display().to_string()),
            "the sentence names the file: {refused}"
        );
        assert!(
            refused.contains("cannot create .mush"),
            "and the problem: {refused}"
        );
        assert!(
            !ws.root().join(".mush/paste").exists(),
            "no broken image and no half-written copy"
        );
        assert_eq!(
            fs::read(&outside).unwrap(),
            png(4),
            "the human's file is untouched"
        );
    }

    /// The human's ruling, end to end at the level this crate owns: a picture
    /// pasted from *outside* the root — their screenshots live where they live —
    /// must survive a restart. Attach, save (the payload is shed to the
    /// placeholder), load, then read the placeholder's *own* path back through
    /// the workspace's image reader: the model gets the picture again. The
    /// human's path, which the old rule kept, is the one name that cannot do
    /// this.
    ///
    /// What this does not reach: the app painting the placeholder or
    /// re-attaching on resume; those roads are the app's, and this is the
    /// workspace/session seam they rest on.
    #[test]
    fn a_picture_pasted_from_outside_is_read_again_after_a_restart() {
        use crate::message::Message;

        let ws = temp_workspace("paste-restart");
        let original = png_of(640, 480, 32);
        let outside = outside_image("paste-restart", &original);
        let image = ws
            .pasted_image(&outside.display().to_string())
            .unwrap()
            .expect("an outside image is attached");

        let mut session = session::Session {
            model: "a-model".to_string(),
            messages: vec![Message::user_with_images("look at this", vec![image])],
            ..Default::default()
        };
        session.save(ws.root()).unwrap();

        // The restart: no bytes in the file, one placeholder naming the copy.
        let loaded = session::Session::load(ws.root()).unwrap();
        assert!(loaded.messages[0].images.is_empty(), "no bytes come back");
        let line = loaded.messages[0].text().to_string();
        let path = line
            .split("[image: ")
            .nth(1)
            .and_then(|rest| rest.split(" (").next())
            .unwrap_or_else(|| panic!("the placeholder names a path: {line}"))
            .to_string();
        assert!(path.starts_with(".mush/paste/pasted-"), "{line}");
        assert!(
            ws.resolve(&path).is_ok(),
            "the model's own tools resolve the placeholder's path: {path}"
        );
        assert!(
            ws.resolve(&outside.display().to_string()).is_err(),
            "the human's own path is one the model may not resolve"
        );

        let reread = ws.read_image(&path).unwrap().unwrap();
        assert_eq!(reread.mime, "image/png");
        assert_eq!(reread.pixels, Some((640, 480)));
        assert_eq!(reread.bytes, original, "the model gets the very picture");
    }

    /// The whole file or a refusal: `edit_file` is the one caller left, and an
    /// exact replacement needs every byte it is replacing. Two things are
    /// refused, for two different reasons: a binary file (a NUL byte) is not
    /// text at all, and a non-UTF-8 file ([`not_utf8`], the test beside this
    /// one) is text this read cannot decode without rewriting it — the lossy
    /// decode is what made the old edit road rewrite a Latin-1 file (finding
    /// B6). Everything else comes back whole within the cap.
    #[test]
    fn read_refuses_a_binary_file_and_reads_the_rest_whole() {
        let ws = temp_workspace("binary");
        fs::write(ws.root().join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        let refused = ws.read_file("blob.bin").unwrap_err();
        assert!(refused.contains("binary"), "{refused}");
        // And nothing is cut: a file well past any window a model would want
        // comes back in full, because the caller is an editor.
        let long = "x".repeat(64 * 1024);
        ws.write_file("long.txt", &long).unwrap();
        assert_eq!(ws.read_file("long.txt").unwrap(), long);
    }

    /// A CRLF file's window and its edit road tell one story: the window says
    /// the file's lines end with CRLF, because a line's *text* is not the
    /// file's bytes when an ending is there; an edit inside one line lands and
    /// leaves every ending alone; and an edit that spells a line break — the
    /// copy that could never match, or the `new_string` that would insert LF
    /// lines — is refused in words, naming the endings and the road that
    /// changes them (finding B7).
    #[test]
    fn a_crlf_file_is_read_and_edited_consistently() {
        let ws = temp_workspace("crlf-edit");
        fs::write(ws.root().join("win.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();

        let window = ws.read_window("win.txt", 1, 10, 4_000).unwrap();
        assert!(
            window.contains("CRLF"),
            "the read says the file's line endings: {window:?}"
        );

        // A line's own text — what the window shows — is exactly what the edit
        // road matches, and the file's other endings are untouched.
        let edited = crate::tools::edit_text(
            &ws.read_file("win.txt").unwrap(),
            "gamma",
            "delta",
            false,
            "win.txt",
        )
        .unwrap();
        assert_eq!(edited, "alpha\r\nbeta\r\ndelta\r\n");
        ws.write_file("win.txt", &edited).unwrap();
        assert_eq!(
            fs::read(ws.root().join("win.txt")).unwrap(),
            b"alpha\r\nbeta\r\ndelta\r\n",
            "the one-line edit left every CRLF standing"
        );

        // The copy the window used to invite: two of its lines, which is an
        // `old_string` the file's bytes can never hold. The refusal names the
        // line endings and the road that still does the work.
        let refused = crate::tools::edit_text(&edited, "alpha\nbeta", "one\ntwo", false, "win.txt")
            .unwrap_err();
        assert!(refused.contains("CRLF"), "{refused}");
        assert!(refused.contains("run_command"), "{refused}");

        // And the one-line edit whose `new_string` would insert LF lines into
        // the CRLF file is refused for the same reason, as is an edit that
        // spells the ending itself.
        for (old, new) in [("alpha", "A\nB"), ("alpha\r", "x"), ("alpha", "x\r")] {
            let refused = crate::tools::edit_text(&edited, old, new, false, "win.txt").unwrap_err();
            assert!(refused.contains("CRLF"), "{old:?} -> {new:?}: {refused}");
        }

        // The read still says so after the edit, and the window of a *mixed*
        // file says nothing: its bytes can be edited exactly, and are.
        assert!(ws
            .read_window("win.txt", 1, 10, 4_000)
            .unwrap()
            .contains("CRLF"));
        fs::write(ws.root().join("mixed.txt"), "a\nb\r\nc\n").unwrap();
        let mixed = ws.read_window("mixed.txt", 1, 10, 4_000).unwrap();
        assert!(!mixed.contains("CRLF"), "{mixed}");
        assert_eq!(
            crate::tools::edit_text("a\nb\r\nc\n", "a\nb", "A\nB", false, "mixed.txt").unwrap(),
            "A\nB\r\nc\n"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A search result is the file's line, not a display's: the trailing
    /// spaces, the control byte and the escape sequence all come back, and so
    /// does the `\r` of a CRLF ending — the bytes the file holds at that line
    /// number. A line past the cap is cut on a character boundary and the cut
    /// is said, never silently shortened (finding B8).
    #[test]
    fn a_searched_line_is_the_files_own_bytes() {
        let ws = temp_workspace("search-bytes");
        let line = "before\rneedle \x1b[31m \x07  ";
        fs::write(ws.root().join("raw.txt"), format!("{line}\n")).unwrap();
        assert_eq!(
            ws.read_file("raw.txt").unwrap(),
            format!("{line}\n"),
            "the strict read and the search line are the same bytes"
        );
        let found = ws.search("needle", ".", 10, 0).unwrap();
        assert_eq!(
            found.rows,
            vec![format!("raw.txt:1: {line}")],
            "no sanitizing, no trim_end: the line comes back byte for byte"
        );

        // The `\r` of a CRLF ending is the file's byte too.
        fs::write(ws.root().join("crlf.txt"), "needle\r\nnext\r\n").unwrap();
        let found = ws.search("needle", "crlf.txt", 10, 0).unwrap();
        assert_eq!(found.rows, vec!["crlf.txt:1: needle\r".to_string()]);

        // A line past the cap is cut on a character boundary and marked, never
        // silently shortened.
        let long = format!("needle{}", "x".repeat(2 * MATCH_LINE_CAP));
        ws.write_file("long.txt", &format!("{long}\n")).unwrap();
        let found = ws.search("needle", "long.txt", 10, 0).unwrap();
        let reported = &found.rows[0];
        assert!(
            reported.starts_with(&format!("long.txt:1: {}", &long[..MATCH_LINE_CAP])),
            "{reported}"
        );
        assert!(reported.contains("line cut at"), "{reported}");
        assert!(reported.len() < long.len(), "the cut is real");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// `pattern` is a regex and not a literal, and the escape rule is the cost
    /// the schema states: `a(1)` is the group matching `a1`, so the model that
    /// meant the file's `a(1)` literally gets a miss until it writes `a\(1\)`.
    /// The literal this replaced could not ask for "any digit" at all.
    #[test]
    fn a_pattern_is_a_regex_and_a_metacharacter_is_escaped_for_it() {
        let ws = temp_workspace("search-regex");
        fs::write(ws.root().join("a.txt"), "a(1)\na1\n").unwrap();
        let grouped = ws.search("a(1)", "", 10, 0).unwrap();
        assert_eq!(grouped.rows, vec!["a.txt:2: a1".to_string()]);
        let escaped = ws.search(r"a\(1\)", "", 10, 0).unwrap();
        assert_eq!(escaped.rows, vec!["a.txt:1: a(1)".to_string()]);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// Case-insensitivity is the pattern's own `(?i)` — the flag argument the
    /// call used to carry is gone — and the fold is the engine's, which is
    /// ASCII-only, not the Unicode `to_lowercase` the old flag ran.
    #[test]
    fn case_insensitivity_is_the_patterns_own_i_flag() {
        let ws = temp_workspace("search-ignore-case");
        fs::write(ws.root().join("a.txt"), "NEEDLE\nneedle\n").unwrap();
        let found = ws.search("(?i)needle", "", 10, 0).unwrap();
        assert_eq!(
            found.rows,
            vec!["a.txt:1: NEEDLE".to_string(), "a.txt:2: needle".to_string()]
        );
        let exact = ws.search("needle", "", 10, 0).unwrap();
        assert_eq!(exact.rows, vec!["a.txt:2: needle".to_string()]);

        // The fold a model might expect from the old flag is not there: `(?i)é`
        // is not `É`, because `regex-lite` folds ASCII only.
        fs::write(ws.root().join("unicode.txt"), "É\n").unwrap();
        let unicode = ws.search("(?i)é", "unicode.txt", 10, 0).unwrap();
        assert!(unicode.rows.is_empty(), "{:?}", unicode.rows);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// `\b` is a word boundary, and the engine's word characters are ASCII:
    /// `\bheld\b` finds the word and not `beheld`, while a boundary at a `β`
    /// is not one — the limit the schema names rather than lets a model
    /// discover.
    #[test]
    fn a_word_boundary_is_ascii() {
        let ws = temp_workspace("search-word-boundary");
        fs::write(ws.root().join("a.txt"), "held\nbeheld\nβββ\n").unwrap();
        let found = ws.search(r"\bheld\b", "", 10, 0).unwrap();
        assert_eq!(found.rows, vec!["a.txt:1: held".to_string()]);
        let unicode = ws.search(r"\bβββ\b", "", 10, 0).unwrap();
        assert!(unicode.rows.is_empty(), "{:?}", unicode.rows);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The haystack is one line, not the whole file: `^`/`$` anchor a line and
    /// a pattern holding `\n` can never match, which is what a `path:line:
    /// text` row needs — and why the walk is per line instead of the file's
    /// whole text.
    #[test]
    fn the_match_is_one_line_and_the_anchors_are_a_lines() {
        let ws = temp_workspace("search-per-line");
        fs::write(ws.root().join("a.txt"), "one\ntwo\n").unwrap();
        let anchored = ws.search("^two$", "", 10, 0).unwrap();
        assert_eq!(anchored.rows, vec!["a.txt:2: two".to_string()]);
        let across = ws.search("one\ntwo", "", 10, 0).unwrap();
        assert!(across.rows.is_empty(), "{:?}", across.rows);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The cap's contract survives a pattern that matches every line: the
    /// first `limit` rows come back, `more` is set, and the walk stops at the
    /// line that proved there was more rather than walking the rest.
    #[test]
    fn a_pattern_that_matches_every_line_still_respects_the_cap() {
        let ws = temp_workspace("search-cap-everything");
        fs::write(ws.root().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let found = ws.search(".*", "", 2, 0).unwrap();
        assert_eq!(
            found.rows,
            vec!["a.txt:1: one".to_string(), "a.txt:2: two".to_string()]
        );
        assert!(found.more, "the third line is the proof of more");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A pattern the engine refuses is the call's refusal, not a "no match":
    /// the engine's own words travel back so the pattern can be fixed, and the
    /// walk never runs — there are no rows to show for an argument that never
    /// parsed.
    #[test]
    fn a_bad_pattern_is_refused_with_the_engines_own_words() {
        let ws = temp_workspace("search-bad-pattern");
        fs::write(ws.root().join("a.txt"), "needle\n").unwrap();
        // `Matches` carries no `Debug`, so the refusal is matched out by hand
        // rather than unwrapped.
        let refused = match ws.search("needle(", "", 10, 0) {
            Err(refused) => refused,
            Ok(found) => panic!("a bad pattern must be refused, not run: {:?}", found.rows),
        };
        assert!(refused.contains("`pattern`"), "{refused}");
        assert!(
            refused.contains("found open group without closing ')'"),
            "the engine's own words: {refused}"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// An empty pattern is refused, like [`Workspace::usages`]'s empty symbol:
    /// the empty regex matches every line, so a patternless call would answer
    /// with the cap's worth of the workspace instead of a search.
    #[test]
    fn an_empty_pattern_is_refused() {
        let ws = temp_workspace("search-empty-pattern");
        fs::write(ws.root().join("a.txt"), "one\ntwo\n").unwrap();
        let refused = match ws.search("", "", 10, 0) {
            Err(refused) => refused,
            Ok(found) => panic!(
                "an empty pattern must be refused, not run: {:?}",
                found.rows
            ),
        };
        assert_eq!(refused, "`pattern` must not be empty");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// `context` is the neighbours, and the answer is `rg -C`'s: a match with
    /// `context = 2` shows exactly the two lines either side of it, numbered as
    /// the file numbers them. The match keeps the `path:line: text` shape and a
    /// neighbour is `path-line- text` — the two separators are the whole
    /// grammar, and every model has read it.
    #[test]
    fn a_match_with_context_shows_the_lines_either_side() {
        let ws = temp_workspace("search-context");
        fs::write(ws.root().join("a.txt"), "one\ntwo\nNEEDLE\nthree\nfour\n").unwrap();
        let found = ws.search("NEEDLE", "a.txt", 10, 2).unwrap();
        assert_eq!(
            found.rows,
            vec![
                "a.txt-1- one".to_string(),
                "a.txt-2- two".to_string(),
                "a.txt:3: NEEDLE".to_string(),
                "a.txt-4- three".to_string(),
                "a.txt-5- four".to_string(),
            ],
            "the file's lines 1..5, its own numbers, one row shape each"
        );
        assert!(!found.more);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// Two matches whose windows overlap are one window: three lines apart with
    /// `context = 2` shares the lines between them, and each line comes back
    /// once — the answer is a run of the file, not a window per match with the
    /// shared rows said twice.
    #[test]
    fn two_nearby_matches_share_one_window_and_no_line_twice() {
        let ws = temp_workspace("search-context-merge");
        fs::write(
            ws.root().join("a.txt"),
            "one\ntwo\nHIT\nfour\nHIT\nsix\nseven\n",
        )
        .unwrap();
        let found = ws.search("HIT", "a.txt", 10, 2).unwrap();
        assert_eq!(
            found.rows,
            vec![
                "a.txt-1- one".to_string(),
                "a.txt-2- two".to_string(),
                "a.txt:3: HIT".to_string(),
                "a.txt-4- four".to_string(),
                "a.txt:5: HIT".to_string(),
                "a.txt-6- six".to_string(),
                "a.txt-7- seven".to_string(),
            ],
            "windows [1..5] and [3..7] are one run of [1..7]"
        );
        for row in &found.rows {
            assert_eq!(
                found.rows.iter().filter(|other| *other == row).count(),
                1,
                "a line is shown once: {row}"
            );
        }
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A window is clipped to the file: a match on the first line opens at line
    /// 1 and a match on the last line closes at it. There is no line 0 and no
    /// line past the end for a window to reach for.
    #[test]
    fn a_window_never_runs_past_the_files_ends() {
        let ws = temp_workspace("search-context-ends");
        fs::write(ws.root().join("top.txt"), "NEEDLE\none\ntwo\n").unwrap();
        let top = ws.search("NEEDLE", "top.txt", 10, 2).unwrap();
        assert_eq!(
            top.rows,
            vec![
                "top.txt:1: NEEDLE".to_string(),
                "top.txt-2- one".to_string(),
                "top.txt-3- two".to_string(),
            ]
        );
        fs::write(ws.root().join("bottom.txt"), "one\ntwo\nNEEDLE\n").unwrap();
        let bottom = ws.search("NEEDLE", "bottom.txt", 10, 2).unwrap();
        assert_eq!(
            bottom.rows,
            vec![
                "bottom.txt-1- one".to_string(),
                "bottom.txt-2- two".to_string(),
                "bottom.txt:3: NEEDLE".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// `context = 0` is the answer this tool has always given, byte for byte:
    /// every row is a match row and nothing stands between two matches. A
    /// neighbour is the next answer, so zero is a real choice and not an
    /// accidental default.
    #[test]
    fn context_zero_is_the_answer_the_tool_has_always_given() {
        let ws = temp_workspace("search-context-zero");
        fs::write(ws.root().join("a.txt"), "one\nNEEDLE\ntwo\nNEEDLE\nfour\n").unwrap();
        let zero = ws.search("NEEDLE", "a.txt", 10, 0).unwrap();
        assert_eq!(
            zero.rows,
            vec!["a.txt:2: NEEDLE".to_string(), "a.txt:4: NEEDLE".to_string()]
        );
        assert!(
            zero.rows
                .iter()
                .all(|row| row.contains(":2: ") || row.contains(":4: ")),
            "no context row at all: {:?}",
            zero.rows
        );
        let one = ws.search("NEEDLE", "a.txt", 10, 1).unwrap();
        assert_ne!(one.rows, zero.rows);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A huge `context` is clamped to [`SEARCH_CONTEXT_MAX`] and answered, not
    /// refused: a thousand is a real request — "show me the whole
    /// neighbourhood" — and the ten either side that fit is its honest answer.
    /// The clamp is the constant and the constant is the answer: the two calls
    /// agree byte for byte and the window is exactly the clamped width.
    #[test]
    fn a_huge_context_is_clamped_to_the_named_maximum() {
        let ws = temp_workspace("search-context-clamp");
        let mut lines: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
        lines[15] = "NEEDLE".to_string();
        fs::write(ws.root().join("a.txt"), format!("{}\n", lines.join("\n"))).unwrap();
        let asked = ws.search("NEEDLE", "a.txt", 100, 1000).unwrap();
        let clamped = ws
            .search("NEEDLE", "a.txt", 100, SEARCH_CONTEXT_MAX)
            .unwrap();
        assert_eq!(asked.rows, clamped.rows);
        assert_eq!(
            asked.rows.len(),
            2 * SEARCH_CONTEXT_MAX + 1,
            "the match and its ten either side: {:?}",
            asked.rows
        );
        assert_eq!(asked.rows.first().unwrap(), "a.txt-6- line 6");
        assert_eq!(asked.rows[SEARCH_CONTEXT_MAX], "a.txt:16: NEEDLE");
        assert_eq!(asked.rows.last().unwrap(), "a.txt-26- line 26");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The cap counts **rows**, so a context cannot blow past it: four rows
    /// fit, the fifth is the proof, and `more` says there was one — the second
    /// match is not shown, and the window it opens is not pretended closed. The
    /// rows the answer does hold are the answer's first four, not a whole
    /// window and not a count of matches.
    #[test]
    fn the_cap_counts_rows_so_context_cannot_blow_past_it() {
        let ws = temp_workspace("search-context-cap");
        let mut lines: Vec<String> = (1..=20).map(|n| format!("line {n}")).collect();
        lines[0] = "NEEDLE".to_string();
        lines[9] = "NEEDLE".to_string();
        fs::write(ws.root().join("a.txt"), format!("{}\n", lines.join("\n"))).unwrap();
        let found = ws.search("NEEDLE", "a.txt", 4, 2).unwrap();
        assert_eq!(
            found.rows,
            vec![
                "a.txt:1: NEEDLE".to_string(),
                "a.txt-2- line 2".to_string(),
                "a.txt-3- line 3".to_string(),
                "a.txt-8- line 8".to_string(),
            ],
            "the first four rows of the whole answer"
        );
        assert!(found.more, "the second match is the proof of more");
        // The room is the answer's and not the file's: a wider cap shows the
        // second window whole and clears `more`.
        let wider = ws.search("NEEDLE", "a.txt", 20, 2).unwrap();
        assert_eq!(wider.rows.len(), 8, "3 rows then 5: {:?}", wider.rows);
        assert!(!wider.more);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The two shapes are told apart by the row's own punctuation: a match row
    /// is `path:line: text` and a context row is `path-line- text`, so the
    /// digest's own parse of a row names a file for a match and for a context
    /// row names nothing — a reader that keeps only match rows keeps exactly
    /// the matches.
    #[test]
    fn a_context_row_is_told_from_a_match_row_by_its_own_shape() {
        let ws = temp_workspace("search-context-shapes");
        fs::write(ws.root().join("a.txt"), "one\nNEEDLE\ntwo\n").unwrap();
        let found = ws.search("NEEDLE", "a.txt", 10, 1).unwrap();
        assert_eq!(found.rows.len(), 3);
        // The digest's `match_file` rule, run here: the first `:` followed by
        // digits and another `:` names the file a match row is in. A context
        // row's separators are dashes, so it parses as no row at all.
        fn match_file(row: &str) -> Option<&str> {
            for (at, byte) in row.bytes().enumerate() {
                if byte != b':' {
                    continue;
                }
                let rest = &row[at + 1..];
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits > 0 && rest.as_bytes().get(digits) == Some(&b':') {
                    return Some(&row[..at]);
                }
            }
            None
        }
        assert_eq!(match_file(&found.rows[0]), None, "{}", found.rows[0]);
        assert_eq!(
            match_file(&found.rows[1]),
            Some("a.txt"),
            "{}",
            found.rows[1]
        );
        assert_eq!(match_file(&found.rows[2]), None, "{}", found.rows[2]);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A cut context row names the road that prints *it* whole: the pattern
    /// alone (`rg -n`) prints a match line and never a neighbour, so the marker
    /// carries the window — `rg -n -C 2` — and a model sent to the wrong road
    /// would come back without the line it asked to see.
    #[test]
    fn a_cut_context_row_names_the_context_road() {
        let ws = temp_workspace("search-context-cut");
        let long = format!("{}NEEDLE", "x".repeat(2 * MATCH_LINE_CAP));
        fs::write(ws.root().join("a.txt"), format!("{long}\nNEEDLE\n")).unwrap();
        let found = ws.search("^NEEDLE$", "a.txt", 10, 1).unwrap();
        assert!(
            found.rows[0].contains("`rg -n -C 1`"),
            "the neighbour's road is its window: {:?}",
            found.rows
        );
        assert_eq!(found.rows[1], "a.txt:2: NEEDLE");
        // A match row's cut still names the plain road, unchanged.
        let long_match = format!("NEEDLE{}", "x".repeat(2 * MATCH_LINE_CAP));
        fs::write(ws.root().join("b.txt"), format!("{long_match}\n")).unwrap();
        let found = ws.search("NEEDLE", "b.txt", 10, 0).unwrap();
        assert!(
            found.rows[0].contains("`rg -n`") && !found.rows[0].contains("-C"),
            "a match row's marker is the one it has always carried: {:?}",
            found.rows
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// `usages`' walk: a word at a boundary and never a substring, grouped by
    /// file with the declaration-looking rows first — and it is the *search's*
    /// walk, so build output and mush's own state contribute no row, and the
    /// counters carry what the walk would not read instead of dropping it
    /// silently.
    #[test]
    fn a_usage_walk_groups_by_file_and_leads_with_the_declaration() {
        let ws = temp_workspace("usages");
        fs::write(
            ws.root().join("a.rs"),
            "// held\nlet a = held;\nfn held(x: usize) -> usize { x }\nlet beheld = 1;\n",
        )
        .unwrap();
        fs::write(ws.root().join("b.txt"), "held, held\n").unwrap();
        fs::create_dir_all(ws.root().join("target")).unwrap();
        fs::write(ws.root().join("target/gen.rs"), "held\n").unwrap();
        fs::create_dir_all(ws.root().join(".mush")).unwrap();
        fs::write(ws.root().join(".mush/gate.log"), "held\n").unwrap();
        fs::write(ws.root().join("blob.bin"), b"held\0\0").unwrap();
        // A name the model's road cannot carry: a row under it would be a dead
        // end, so the file is counted and not searched.
        fs::write(ws.root().join("bad\nname.txt"), "held\n").unwrap();

        let found = ws.usages("held", 100).unwrap();
        assert_eq!(
            found
                .groups
                .iter()
                .map(|group| group.file.as_str())
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.txt"],
            "the walk's own order, and no row from a skipped directory"
        );
        assert_eq!(
            found.groups[0]
                .rows
                .iter()
                .map(|row| row.line)
                .collect::<Vec<_>>(),
            vec![3, 1, 2],
            "the declaration line first, then the mentions in file order"
        );
        assert_eq!(found.groups[0].declarations(), vec![3]);
        assert!(found.groups[0].rows[0].definition);
        assert_eq!(
            found.groups[1]
                .rows
                .iter()
                .map(|row| row.line)
                .collect::<Vec<_>>(),
            vec![1],
            "one row per line, however many times the word is on it"
        );
        assert!(found.groups[1].declarations().is_empty());
        // `beheld` is a substring and not a row; the binary file and the
        // unnavigable name are the walk's own counters, not silence.
        assert_eq!(found.hits(), 4);
        assert_eq!(found.scanned, 2);
        assert_eq!(found.skipped, 1);
        assert_eq!(found.unnamed, 1);
        assert!(!found.more);

        // Case is part of the spelling, and the empty symbol is refused at the
        // door rather than walked as a match at every position.
        assert!(ws.usages("Held", 100).unwrap().groups.is_empty());
        assert!(ws
            .usages("", 100)
            .unwrap_err()
            .contains("must not be empty"));
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The cap stops the walk at the first row it cannot keep — the row is the
    /// whole proof that there was more — and `scanned`/`skipped` are what let a
    /// miss say how much it did read, instead of answering as if it had read
    /// everything.
    #[test]
    fn a_usage_walk_stops_at_the_cap_and_counts_what_it_could_not_read() {
        let ws = temp_workspace("usages-cap");
        for n in 0..3 {
            fs::write(
                ws.root().join(format!("f{n}.txt")),
                format!("held {n}\nheld again\n"),
            )
            .unwrap();
        }
        fs::write(ws.root().join("blob.bin"), b"held\0").unwrap();

        let capped = ws.usages("held", 3).unwrap();
        assert_eq!(capped.hits(), 3, "the cap kept exactly its rows");
        assert!(capped.more, "the row it could not keep was seen");
        assert_eq!(
            capped
                .groups
                .iter()
                .map(|group| (group.file.as_str(), group.rows.len()))
                .collect::<Vec<_>>(),
            vec![("f0.txt", 2), ("f1.txt", 1)],
            "the walk stopped inside `f1.txt`, and `f2.txt` was never reached"
        );
        assert_eq!(capped.scanned, 2);
        assert_eq!(capped.skipped, 1, "the blob was counted before the cap");

        // With room for everything, the same walk answers the same rows and no
        // `more`: the cap changed the answer, not the rule.
        let whole = ws.usages("held", 100).unwrap();
        assert!(!whole.more);
        assert_eq!(whole.hits(), 6);
        assert_eq!(whole.groups.len(), 3);
        assert_eq!(whole.scanned, 3);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A UTF-8 BOM is a signature, not a character: the roads that show a file
    /// read past it, so a `.cs` or `.ps1` a Windows editor wrote is outlined
    /// from the declarations it spells instead of being missed on its first
    /// line. The byte roads keep it — the edit read returns the file whole,
    /// and a search line is the file's own bytes (finding B8).
    #[test]
    fn a_bom_is_a_signature_and_the_reader_roads_read_past_it() {
        let ws = temp_workspace("bom");
        fs::write(
            ws.root().join("bom.rs"),
            "\u{feff}fn held() {}\na = held;\n",
        )
        .unwrap();

        // The outline reads the declaration the file spells, not `\u{feff}fn`,
        // and the mention under it is not a second one.
        let outline = ws.outline("bom.rs").unwrap().render(4_000, "");
        assert!(outline.contains("1 definition"), "{outline}");
        assert!(outline.contains("  1  fn held() {}"), "{outline}");

        // The usage walk leads with the same line as a declaration, and the
        // row's text is the line `fn` opens.
        let found = ws.usages("held", 10).unwrap();
        assert!(found.groups[0].rows[0].definition);
        assert_eq!(found.groups[0].rows[0].line, 1);
        assert_eq!(found.groups[0].rows[0].text, "fn held() {}");

        // The window is the same reader's road: the signature is dropped there
        // too, so a line copied out of it is the line the outline sketched.
        let window = ws.read_window("bom.rs", 1, 1, 4_000).unwrap();
        assert!(window.starts_with("fn held() {}"), "{window:?}");

        // The byte roads keep every byte.
        assert!(ws.read_file("bom.rs").unwrap().starts_with('\u{feff}'));
        let searched = ws.search("held", "bom.rs", 10, 0).unwrap();
        assert!(
            searched.rows[0].starts_with("bom.rs:1: \u{feff}fn held() {}"),
            "{:?}",
            searched.rows
        );

        // A file that is nothing but a signature holds no text: the readers
        // say so, and the edit road still hands back the three bytes.
        fs::write(ws.root().join("only_bom.txt"), "\u{feff}").unwrap();
        assert_eq!(
            ws.outline("only_bom.txt").unwrap().render(4_000, ""),
            "only_bom.txt is empty — there are no definitions to outline"
        );
        assert_eq!(
            ws.read_window("only_bom.txt", 1, 10, 4_000).unwrap().text,
            "only_bom.txt is empty"
        );
        assert_eq!(ws.read_file("only_bom.txt").unwrap(), "\u{feff}");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The number a usage row names is the *reader's* line — `str::lines`,
    /// counted at `\n` — so a CRLF ending is not in the row's text, a lone
    /// carriage return is, a last line without a line feed is still a line,
    /// and a `\n\r` pair puts the `\r` at the head of the next row. The bytes
    /// road still reads every ending back through the road that writes them.
    #[test]
    fn a_usage_row_is_the_readers_line_whatever_the_endings_are() {
        let ws = temp_workspace("usages-endings");
        fs::write(ws.root().join("crlf.txt"), "held\r\nheld\r\n").unwrap();
        fs::write(ws.root().join("lone.txt"), "held\r\nheld\r").unwrap();
        fs::write(ws.root().join("pair.txt"), "held\n\rheld\n").unwrap();
        fs::write(ws.root().join("final.txt"), "held").unwrap();
        fs::write(ws.root().join("mixed.txt"), "held\r\nheld\n").unwrap();

        let found = ws.usages("held", 100).unwrap();
        let rows = |name: &str| -> Vec<(usize, String)> {
            found
                .groups
                .iter()
                .find(|group| group.file == name)
                .unwrap_or_else(|| panic!("{name} has no group"))
                .rows
                .iter()
                .map(|row| (row.line, row.text.clone()))
                .collect()
        };
        assert_eq!(
            rows("crlf.txt"),
            vec![(1, "held".to_string()), (2, "held".to_string())],
            "a CRLF ending is not a row's text"
        );
        assert_eq!(
            rows("lone.txt"),
            vec![(1, "held".to_string()), (2, "held\r".to_string())],
            "a lone `\\r` is"
        );
        assert_eq!(
            rows("pair.txt"),
            vec![(1, "held".to_string()), (2, "\rheld".to_string())],
            "`\\n\\r` splits at the `\\n`"
        );
        assert_eq!(rows("final.txt"), vec![(1, "held".to_string())]);
        assert_eq!(
            rows("mixed.txt"),
            vec![(1, "held".to_string()), (2, "held".to_string())]
        );
        assert_eq!(
            ws.read_file("crlf.txt").unwrap(),
            "held\r\nheld\r\n",
            "the edit road still holds every ending"
        );

        // A needle holding the ending is not a row in a CRLF file — the ending
        // is not the line's text — while the file whose last line really ends
        // with a lone `\r` answers with that line.
        let ending = ws.usages("held\r", 100).unwrap();
        assert_eq!(ending.groups.len(), 1);
        assert_eq!(ending.groups[0].file, "lone.txt");

        // The search road is the bytes road: it shows the lines as the file
        // holds them, ending and all (`text::file_lines`, finding B8).
        assert_eq!(
            ws.search("held", "crlf.txt", 10, 0).unwrap().rows,
            vec!["crlf.txt:1: held\r", "crlf.txt:2: held\r"]
        );
        assert_eq!(
            ws.search("held", "pair.txt", 10, 0).unwrap().rows,
            vec!["pair.txt:1: held", "pair.txt:2: \rheld"],
            "a `\\n\\r` pair leaves the `\\r` at the head of the next line"
        );
        assert_eq!(
            ws.search("held", "final.txt", 10, 0).unwrap().rows,
            vec!["final.txt:1: held"],
            "a last line without a line feed is a line"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// One minified line is one row and not a result: the usage row is cut at
    /// the outline's width with the cut said, the outline answers inside the
    /// cap it was given, and a file past [`SEARCH_FILE_CAP`] is skipped by the
    /// walk that has one — counted, not silently dropped.
    #[test]
    fn one_giant_line_is_cut_by_every_reader_and_past_the_search_cap_is_skipped() {
        let ws = temp_workspace("giant-line");
        let giant = format!("fn held() {{ let x = \"{}\"; }}", "x".repeat(200_000));
        fs::write(ws.root().join("giant.rs"), format!("{giant}\n")).unwrap();
        let too_big = format!("held \"{}\";", "x".repeat(SEARCH_FILE_CAP as usize));
        fs::write(ws.root().join("big.txt"), format!("{too_big}\n")).unwrap();
        assert!(
            too_big.len() as u64 > SEARCH_FILE_CAP,
            "the second file is past the cap"
        );

        let started = Instant::now();
        let found = ws.usages("held", 10).unwrap();
        assert_eq!(
            found
                .groups
                .iter()
                .map(|group| group.file.as_str())
                .collect::<Vec<_>>(),
            vec!["giant.rs"],
            "the file past the cap contributes no row"
        );
        assert_eq!(found.hits(), 1);
        let row = &found.groups[0].rows[0];
        assert_eq!(row.line, 1);
        assert!(row.definition, "a `fn` declaration with a 200 KB body");
        assert!(row.text.ends_with('…'));
        assert!(row.text.len() <= crate::outline::ROW_WIDTH + '…'.len_utf8());
        assert!(giant.starts_with(row.text.trim_end_matches('…')));
        assert_eq!(found.scanned, 1);
        assert_eq!(found.skipped, 1, "the cap is counted, not silent");
        // The search road has the same cap and the same count.
        let searched = ws.search("held", ".", 10, 0).unwrap();
        assert_eq!(searched.rows.len(), 1);
        assert_eq!(searched.skipped, 1);

        let rendered = ws.outline("giant.rs").unwrap().render(1_000, "");
        assert!(rendered.contains("1 definition"), "{rendered}");
        assert!(
            rendered.len() <= 1_000,
            "{} bytes for a 1,000-byte cap",
            rendered.len()
        );
        for line in rendered.lines().filter(|line| line.starts_with("  ")) {
            assert!(
                line.len() <= crate::outline::ROW_WIDTH + 16,
                "a row is bounded: {} bytes",
                line.len()
            );
        }
        // The 2 MB file is still *text* to the outline, whose own cap is the
        // 32 MB whole-read cap: it answers with no definitions and no refusal.
        let big = ws.outline("big.txt").unwrap().render(1_000, "");
        assert!(big.contains("no definitions"), "{big}");
        assert!(big.len() <= 1_000);

        let elapsed = started.elapsed();
        eprintln!("giant line: {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the walk of two files took {elapsed:?}"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// A NUL byte is a binary file whatever surrounds it: a file that is 98%
    /// text with one stray NUL has no rows and no outline, and both walks
    /// *count* it — `skipped` for the usage walk, a refusal naming the file for
    /// the read roads — rather than answering the miss a model would read as
    /// "the symbol is not there". The counter is the honest sentence, and
    /// `run_command` (`rg`, `strings`) is the road around it.
    #[test]
    fn a_stray_nul_is_binary_whatever_the_rest_of_the_file_is() {
        let ws = temp_workspace("stray-nul");
        let mut bytes = "held\n".repeat(40).into_bytes();
        bytes.push(0);
        bytes.extend_from_slice(&"held\n".repeat(40).into_bytes());
        fs::write(ws.root().join("mostly.txt"), &bytes).unwrap();

        let found = ws.usages("held", 10).unwrap();
        assert!(
            found.groups.is_empty(),
            "no row from a file the walk would not read: {found:?}"
        );
        assert_eq!(found.scanned, 0);
        assert_eq!(found.skipped, 1);
        assert!(!found.more);

        let refused = ws.outline("mostly.txt").unwrap_err();
        assert!(refused.contains("binary"), "{refused}");
        let found = ws.search("held", ".", 10, 0).unwrap();
        assert!(found.rows.is_empty());
        assert_eq!(found.skipped, 1);

        // The NUL-and-nothing-else shape is the same fact.
        fs::write(ws.root().join("fffe.bin"), b"\xff\xfe\x00").unwrap();
        let found = ws.usages("held", 10).unwrap();
        assert_eq!(found.skipped, 2);
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The walk's decode is lossy like the window's, so a Latin-1 file still
    /// has rows — and every row names the line the bytes hold: a replacement
    /// character never was a `\n`, so line 2 of the decoded text is line 2 of
    /// the bytes, and the strict edit road still refuses the same file rather
    /// than rewriting the bytes it cannot decode (finding B6).
    #[test]
    fn a_lossy_row_names_the_line_the_files_bytes_hold() {
        let ws = temp_workspace("lossy-rows");
        let bytes = b"caf\xe9 = held;\nna\xefve = held;\n";
        fs::write(ws.root().join("latin.txt"), bytes).unwrap();

        let found = ws.usages("held", 10).unwrap();
        assert_eq!(found.hits(), 2);
        assert_eq!(
            found.groups[0]
                .rows
                .iter()
                .map(|row| (row.line, row.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "caf\u{fffd} = held;"), (2, "na\u{fffd}ve = held;")],
            "each row names its line and shows what the lossy decode held"
        );
        assert_eq!(found.scanned, 1);
        assert!(!found.groups[0].rows[0].definition);

        assert_eq!(fs::read(ws.root().join("latin.txt")).unwrap(), bytes);
        assert!(ws
            .read_file("latin.txt")
            .unwrap_err()
            .contains("not valid UTF-8"));

        // Not-UTF-8 *and* holding a NUL is the binary skip, not a lossy
        // decode: a NUL is not text in any encoding this shows.
        fs::write(ws.root().join("blob.bin"), b"\xff\xfe\x00").unwrap();
        let found = ws.usages("held", 10).unwrap();
        assert_eq!((found.scanned, found.skipped), (1, 1));
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The needles a human might type and a model might emit: a symbol with a
    /// line break is refused at the door — no line holds one, so the walk
    /// could only prove the miss it would spend the whole tree proving — while
    /// a whitespace-only needle, a needle longer than every line, a needle
    /// that is an identifier prefix, and a one-character non-word needle all
    /// walk to their honest answer: rows or a miss that says what was read,
    /// never a panic and never a false hit.
    #[test]
    fn hostile_needles_walk_to_an_honest_answer() {
        let ws = temp_workspace("hostile-needles");
        fs::write(ws.root().join("a.txt"), "held is a word\nheld_x is not\n").unwrap();
        fs::write(ws.root().join("b.txt"), "a - b\n").unwrap();

        let refused = ws.usages("held\nheld", 10).unwrap_err();
        assert!(refused.contains("line break"), "{refused}");
        assert!(refused.contains("rg -U"), "{refused}");

        // A prefix is not the word: `held` never answers for `held_x`, and
        // `held_x` answers for its own line.
        let prefix = ws.usages("held_x", 10).unwrap();
        assert_eq!(prefix.hits(), 1);
        assert_eq!(prefix.groups[0].rows[0].line, 2);
        for symbol in [" ", "\t", "a needle longer than any line in this workspace"] {
            let found = ws.usages(symbol, 10).unwrap();
            assert_eq!(found.hits(), 0, "{symbol:?}");
            assert_eq!(found.scanned, 2, "{symbol:?}: a miss says what it read");
            assert_eq!((found.skipped, found.unnamed), (0, 0), "{symbol:?}");
        }
        // The one-character non-word needle finds the `a - b` line and not the
        // hyphen inside a word.
        let dash = ws.usages("-", 10).unwrap();
        assert_eq!(dash.groups.len(), 1);
        assert_eq!(dash.groups[0].file, "b.txt");
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The walk's cap is a walk's, not an answer's end: five thousand files
    /// each holding one row, asked for a hundred, must stop at the file whose
    /// row did not fit — `scanned` says 101 names were opened, not 5,000 — and
    /// the counters must add up to the files it did visit.
    #[test]
    fn a_usage_walk_stops_at_the_cap_across_five_thousand_files() {
        let ws = temp_workspace("usages-pool");
        for n in 0..5_000 {
            fs::write(ws.root().join(format!("f{n:04}.txt")), "held\n").unwrap();
        }

        let started = Instant::now();
        let found = ws.usages("held", 100).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(found.hits(), 100, "the cap kept exactly its rows");
        assert!(found.more, "the row it could not keep was seen");
        assert_eq!(found.groups.len(), 100);
        assert_eq!(
            (found.scanned, found.skipped, found.unnamed),
            (101, 0, 0),
            "the walk stopped at the file whose row it could not keep"
        );
        eprintln!("usages pool: 100 rows over 5,000 files in {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the walk of 101 names took {elapsed:?}"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The degenerate files a workspace really holds: an empty file, a file of
    /// one blank line, a file of only comments, a file with no final line feed,
    /// and a file of a hundred thousand very short lines. Each answers a
    /// sentence or an honest miss — never a panic — and the walk of the
    /// short-line file is bounded by the answer's room, not the file's lines.
    #[test]
    fn degenerate_files_answer_a_sentence_and_not_a_panic() {
        let ws = temp_workspace("degenerate");
        fs::write(ws.root().join("empty.rs"), "").unwrap();
        fs::write(ws.root().join("blank.rs"), "\n").unwrap();
        fs::write(
            ws.root().join("comments.rs"),
            "// held\n/* fn held() {} */\n",
        )
        .unwrap();
        fs::write(ws.root().join("nonl.rs"), "fn held() {}").unwrap();
        fs::write(ws.root().join("short.txt"), "held\n".repeat(100_000)).unwrap();

        assert_eq!(
            ws.outline("empty.rs").unwrap().render(4_000, ""),
            "empty.rs is empty — there are no definitions to outline"
        );
        assert_eq!(
            ws.outline("blank.rs").unwrap().render(4_000, ""),
            "blank.rs — 1 line; no definitions (textual, many languages, best-effort — not a \
             compiler's answer); read_file shows the text"
        );

        // A comment holds the word, so it is a usage row; it is never a
        // declaration, and the outline of a comments-only file has none.
        let found = ws.usages("held", 100).unwrap();
        let comments = found
            .groups
            .iter()
            .find(|group| group.file == "comments.rs")
            .expect("the comments file has rows");
        assert_eq!(comments.rows.len(), 2);
        assert!(
            comments.rows.iter().all(|row| !row.definition),
            "a comment line is never a declaration: {:?}",
            comments.rows
        );
        let outlined = ws.outline("comments.rs").unwrap().render(4_000, "");
        assert!(outlined.contains("no definitions"), "{outlined}");

        // The last line without a line feed is a line, number and all.
        let nonl = found
            .groups
            .iter()
            .find(|group| group.file == "nonl.rs")
            .expect("the no-final-newline file has a row");
        assert_eq!(nonl.rows[0].line, 1);
        assert_eq!(ws.outline("nonl.rs").unwrap().definitions().len(), 1);

        // A directory holds no file: it neither answers nor counts, and asking
        // the outline for one is the read road's refusal, not an empty answer.
        fs::create_dir(ws.root().join("empty_dir")).unwrap();
        assert!(ws
            .outline("empty_dir")
            .unwrap_err()
            .contains("not a regular file"));

        // A hundred thousand very short lines: the answer is ten rows and the
        // proof of a further one, in bounded time.
        let started = Instant::now();
        let found = ws.usages("held", 10).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(found.hits(), 10);
        assert!(found.more);
        assert_eq!(found.scanned, 5, "all five files were read before the cap");
        eprintln!("degenerate corpus: five files, 100,000 short lines, {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the walk took {elapsed:?}"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The corpus no checkout has: a scratch root holding every shape a real
    /// file can hand a reader — minified lines, CRLF, a lone carriage return, a
    /// BOM, Latin-1 bytes, escape sequences, combining marks, a zero-width
    /// joiner, a stray NUL, a file past the search cap, a hundred thousand
    /// short lines, an empty file and a blank one. The property test over both
    /// tools and five needles then asserts the invariants that must hold on all
    /// of them: every row names a line that really holds the word, every row is
    /// that line cut and never rearranged, rows ascend, no row holds a line
    /// break, a row is bounded by its tool's width, and every answer respects
    /// its cap and counts every file it visited.
    #[test]
    fn every_row_of_the_generated_corpus_is_bounded_and_names_its_line() {
        let ws = temp_workspace("generated-corpus");
        let files = generated_corpus(&ws);
        let started = Instant::now();

        for symbol in ["held", "fn", "-", "$", "é"] {
            // The bounded answers, at three rooms: nothing, one row, and a
            // handful. The walk must never hand back more than the room, and
            // must stop exactly on its cap when it says there was more.
            for room in [0usize, 1, 7] {
                let found = ws.usages(symbol, room).unwrap();
                check_usage_rows(&files, symbol, room, &found);
            }
            // The unbounded answer is the whole corpus: every row of every
            // file the walk could read, and every file in exactly one counter.
            let expected: usize = files
                .iter()
                .filter(|(_, bytes)| bytes.len() as u64 <= SEARCH_FILE_CAP && !bytes.contains(&0))
                .map(|(_, bytes)| crate::usages::rows(symbol, &reader_text(bytes)).len())
                .sum();
            let whole = ws.usages(symbol, usize::MAX).unwrap();
            check_usage_rows(&files, symbol, usize::MAX, &whole);
            assert!(!whole.more, "{symbol:?}: no room to cut, so no cut");
            assert_eq!(
                whole.hits(),
                expected,
                "{symbol:?}: every row of every scanned file"
            );
            assert_eq!(
                whole.scanned + whole.skipped + whole.unnamed,
                files.len(),
                "{symbol:?}: the counters are the walk's own file count"
            );
        }

        for (name, _) in &files {
            match ws.outline(name) {
                // The one file a read road refuses: a NUL is binary.
                Err(refusal) => {
                    assert_eq!(name, "nul.bin", "{name}: {refusal}");
                    assert!(refusal.contains("binary"), "{name}: {refusal}");
                }
                Ok(outline) => check_outline_rows(&files, name, &outline),
            }
        }

        let elapsed = started.elapsed();
        eprintln!("generated corpus: {} files, {elapsed:?}", files.len());
        assert!(
            elapsed < Duration::from_secs(30),
            "the corpus sweep took {elapsed:?}"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The corpus the property test walks, written into `ws`: the shapes a real
    /// workspace can hand a reader, from a file with no bytes to a file past
    /// the search cap, each one returned with its bytes so the checks can read
    /// the same file the walk read.
    fn generated_corpus(ws: &Workspace) -> Vec<(String, Vec<u8>)> {
        let files: Vec<(String, Vec<u8>)> = vec![
            ("empty.rs".into(), Vec::new()),
            ("blank.rs".into(), b"\n".to_vec()),
            ("nonl.rs".into(), b"fn held() {}".to_vec()),
            (
                "crlf.rs".into(),
                b"fn held() {}\r\nlet a = held;\r\n".to_vec(),
            ),
            (
                "lone_cr.rs".into(),
                b"let held = 1;\rlet b = held;\r\n".to_vec(),
            ),
            (
                "bom.rs".into(),
                "\u{feff}fn held() {}\nlet a = held;\n".as_bytes().to_vec(),
            ),
            (
                "bom_crlf.cs".into(),
                "\u{feff}fn held() {}\r\nlet a = held;\r\n"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "latin1.rs".into(),
                b"caf\xe9 = held;\nna\xefve = held;\n".to_vec(),
            ),
            ("accent.txt".into(), "é held é\n".as_bytes().to_vec()),
            ("dollar.js".into(), b"let x = $; held\n".to_vec()),
            ("dash.txt".into(), b"a - b\nheld\n".to_vec()),
            (
                "controls.rs".into(),
                b"let held = \"\x1b[31m\";\t// held\n".to_vec(),
            ),
            (
                "combining.txt".into(),
                format!("held{}\n", "\u{0301}".repeat(1_000)).into_bytes(),
            ),
            (
                "zwj.txt".into(),
                "let held = \"\u{200d}\";\n".as_bytes().to_vec(),
            ),
            (
                "giant.js".into(),
                format!("let held = \"{}\";\n", "x".repeat(200_000)).into_bytes(),
            ),
            (
                // The mention sits past the row's width: the row names the
                // line — the number is the claim — and its text is the line's
                // cut, which may not reach the word. `read_file {offset}` is
                // the road that shows the whole line.
                "late_mention.txt".into(),
                format!("{} held\n", "x".repeat(50_000)).into_bytes(),
            ),
            ("comments.rs".into(), b"// held\n/* held */\n".to_vec()),
            (
                "short_lines.txt".into(),
                "held\n".repeat(100_000).into_bytes(),
            ),
            ("nul.bin".into(), b"held before\x00held after\n".to_vec()),
            (
                "over_cap.txt".into(),
                format!("held {}\n", "x".repeat(SEARCH_FILE_CAP as usize)).into_bytes(),
            ),
        ];
        for (name, bytes) in &files {
            fs::write(ws.root().join(name), bytes).unwrap();
        }
        files
    }

    /// The text a *reader's* road sees in these bytes: the lossy decode the
    /// walk makes, minus the leading BOM ([`text::strip_bom`]: a signature, not
    /// a line's text).
    fn reader_text(bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        text::strip_bom(&text).to_string()
    }

    /// [`reader_text`]'s lines, which is what both tools' rows are lines of.
    fn reader_lines(bytes: &[u8]) -> Vec<String> {
        reader_text(bytes).lines().map(str::to_string).collect()
    }

    /// The property, on one `usages` answer: every row names a real line of its
    /// file, that line really holds the word at a boundary, the row's text is
    /// that line cut and never rearranged, the declaration flag is the outline
    /// rule's own, the two halves ascend, no row holds a line break, a row is
    /// bounded by [`crate::outline::ROW_WIDTH`] (the outline's documented
    /// over-long-qualifier exception aside, which is the one shape whose cut
    /// may overrun the width to keep the word that made the row), and the
    /// answer is inside its cap.
    fn check_usage_rows(files: &[(String, Vec<u8>)], symbol: &str, limit: usize, found: &Usages) {
        assert!(
            found.hits() <= limit,
            "{symbol:?}: {} rows over a cap of {limit}",
            found.hits()
        );
        assert!(
            !found.more || found.hits() == limit,
            "{symbol:?}: a cut answer stops exactly on its cap"
        );
        assert!(
            found.scanned + found.skipped + found.unnamed <= files.len(),
            "{symbol:?}: more counters than files"
        );
        let mut seen = 0usize;
        for group in &found.groups {
            let (_, bytes) = files
                .iter()
                .find(|(name, _)| *name == group.file)
                .unwrap_or_else(|| panic!("a group for {} and no such file", group.file));
            let lines = reader_lines(bytes);
            let mut last_definition = 0usize;
            let mut last_mention = 0usize;
            let mut mention_seen = false;
            for row in &group.rows {
                seen += 1;
                assert!(
                    row.line >= 1 && row.line <= lines.len(),
                    "{}:{}: line {} of a file with {} lines",
                    group.file,
                    row.line,
                    row.line,
                    lines.len()
                );
                let line = &lines[row.line - 1];
                assert!(
                    crate::usages::is_usage(line, symbol),
                    "{}:{}: the row names a line that does not hold `{symbol}` at a boundary: \
                     {line:?}",
                    group.file,
                    row.line
                );
                assert_eq!(
                    row.definition,
                    crate::outline::is_declaration(line),
                    "{}:{}: the declaration flag is the outline rule's",
                    group.file,
                    row.line
                );
                let body = row.text.strip_suffix('…').unwrap_or(&row.text);
                assert!(
                    line.starts_with(body),
                    "{}:{}: the row is not that line, cut: {:?}",
                    group.file,
                    row.line,
                    row.text
                );
                if !row.text.ends_with('…') {
                    assert_eq!(
                        row.text, *line,
                        "{}:{}: an uncut row is the line itself",
                        group.file, row.line
                    );
                }
                assert!(
                    !row.text.contains('\n'),
                    "{}:{}: a row holds a line break",
                    group.file,
                    row.line
                );
                assert!(
                    row.text.len() <= crate::outline::ROW_WIDTH + '…'.len_utf8() || row.definition,
                    "{}:{}: a non-declaration row over the width: {} bytes",
                    group.file,
                    row.line,
                    row.text.len()
                );
                if row.definition {
                    assert!(
                        crate::outline::is_declaration(&row.text),
                        "{}:{}: a declaration row may never be cut through the word that made \
                         it one: {:?}",
                        group.file,
                        row.line,
                        row.text
                    );
                }
                if row.definition {
                    assert!(
                        !mention_seen,
                        "{}:{}: a declaration row after a plain mention",
                        group.file, row.line
                    );
                    assert!(
                        row.line > last_definition,
                        "{}:{}: declaration rows must ascend",
                        group.file,
                        row.line
                    );
                    last_definition = row.line;
                } else {
                    mention_seen = true;
                    assert!(
                        row.line > last_mention,
                        "{}:{}: mention rows must ascend",
                        group.file,
                        row.line
                    );
                    last_mention = row.line;
                }
            }
        }
        assert_eq!(
            seen,
            found.hits(),
            "{symbol:?}: the header's count is the rows"
        );
    }

    /// The same property on one `outline`: the answer fits the cap it was
    /// given, every rendered row is `  line  text` for a real line of the file,
    /// the text is that line cut, the line really is a declaration, the row
    /// re-matches the rule it came from (the outline's hard invariant), rows
    /// ascend, and an answer no cap cut shows every definition it counted.
    ///
    /// The smallest cap is comfortably longer than `CRLF_NOTE`: a cap under
    /// that sentence's own length is the outline's header-only edge — a bare
    /// header with no rows — which this property does not pin and does not
    /// pretend is a row's business.
    fn check_outline_rows(files: &[(String, Vec<u8>)], name: &str, outline: &Outline) {
        let (_, bytes) = files
            .iter()
            .find(|(file, _)| file == name)
            .unwrap_or_else(|| panic!("{name} is not a corpus file"));
        let lines = reader_lines(bytes);
        for cap in [4_000usize, 1_000, 700] {
            let rendered = outline.render(cap, "");
            assert!(
                rendered.len() <= cap + 220,
                "{name}: {} bytes for a cap of {cap}",
                rendered.len()
            );
            if cap >= 1_000 {
                assert!(
                    rendered.len() <= cap,
                    "{name}: {} bytes for a cap of {cap}",
                    rendered.len()
                );
            }
            let mut rows = 0usize;
            let mut last = 0usize;
            for line in rendered.lines() {
                let Some(rest) = line.strip_prefix("  ") else {
                    continue;
                };
                let (number, text) = rest
                    .split_once("  ")
                    .unwrap_or_else(|| panic!("{name}: a row without a line number: {line:?}"));
                let number: usize = number
                    .parse()
                    .unwrap_or_else(|_| panic!("{name}: {line:?}"));
                rows += 1;
                assert!(
                    number > last,
                    "{name}: line {number} after {last} — rows must ascend"
                );
                last = number;
                assert!(
                    number >= 1 && number <= lines.len(),
                    "{name}:{number}: line {number} of a file with {} lines",
                    lines.len()
                );
                let body = text.strip_suffix('…').unwrap_or(text);
                assert!(
                    lines[number - 1].starts_with(body),
                    "{name}:{number}: the row is not that line, cut: {text:?}"
                );
                assert!(
                    crate::outline::is_declaration(&lines[number - 1]),
                    "{name}:{number}: the line is not a declaration: {:?}",
                    lines[number - 1]
                );
                assert!(
                    crate::outline::is_declaration(text),
                    "{name}:{number}: a row may never lie — {text:?} does not re-match the rule"
                );
                assert!(
                    !text.contains('\n'),
                    "{name}:{number}: a row holds a line break"
                );
                assert!(
                    text.len() <= crate::outline::ROW_WIDTH + '…'.len_utf8()
                        || crate::outline::is_declaration(text),
                    "{name}:{number}: {} bytes",
                    text.len()
                );
            }
            assert!(
                rows <= outline.definitions().len(),
                "{name}: more rows shown than the outline holds"
            );
            if !rendered.contains("[mush: only the first")
                && !rendered.contains("output truncated at")
            {
                assert_eq!(
                    rows,
                    outline.definitions().len(),
                    "{name}: an uncut answer shows every row it counted"
                );
            }
        }
    }

    /// A reader that stats a file before it reads it still bounds the read:
    /// [`read_bounded`] hands back at most `cap + 1` bytes, so a file that grew
    /// behind the stat is caught by the length rather than loaded whole. This
    /// is the bound `whole_read` and `image_at` keep, and the one `search`'s
    /// read kept only from the stat (finding: the drift the dedup report named
    /// at workspace.rs:1068 vs :509, :878).
    #[test]
    fn a_bounded_read_stops_at_the_cap() {
        let ws = temp_workspace("read-bounded");
        let path = ws.root().join("grew.txt");
        fs::write(&path, vec![b'x'; 4096]).unwrap();

        let bytes = read_bounded(&path, 1024).unwrap();
        assert_eq!(bytes.len(), 1025, "cap + 1 is what says `past the cap`");
        assert_eq!(
            read_bounded(&path, 4096).unwrap().len(),
            4096,
            "a file inside the cap is read whole"
        );
        let _ = fs::remove_dir_all(ws.root());
    }

    /// The edit road's decode is strict: a non-UTF-8 file is refused with the
    /// offset, the encoding problem and the road that can still change it
    /// (`iconv` through `run_command`) — every byte would otherwise be rewritten
    /// as U+FFFD in the lines the model never touched (finding B6, measured on
    /// `caf\xe9 = 1\nna\xefve = 2\n`). The window and search roads keep showing
    /// the same file lossily on purpose: a shown read is not a written one, and
    /// a refusal there would hide the file from the only tools that can diagnose
    /// it.
    #[test]
    fn a_non_utf8_file_is_refused_whole_and_shown_in_a_window() {
        let ws = temp_workspace("non-utf8");
        let bytes = b"caf\xe9 = 1\nna\xefve = 2\n";
        fs::write(ws.root().join("latin.txt"), bytes).unwrap();

        let refused = ws.read_file("latin.txt").unwrap_err();
        assert!(
            refused.contains("latin.txt"),
            "the file is named: {refused}"
        );
        assert!(refused.contains("not valid UTF-8"), "{refused}");
        assert!(
            refused.contains("offset 3"),
            "where the decode stopped: {refused}"
        );
        assert!(
            refused.contains("iconv") && refused.contains("run_command"),
            "the roads that still work: {refused}"
        );
        assert_eq!(
            fs::read(ws.root().join("latin.txt")).unwrap(),
            bytes,
            "the refusal touched nothing"
        );

        let shown = ws.read_window("latin.txt", 1, 10, 4_000).unwrap();
        assert!(
            shown.starts_with("caf\u{fffd} = 1\nna\u{fffd}ve = 2"),
            "the window still shows the file, lossily: {shown}"
        );
        let found = ws.search("caf", "", 10, 0).unwrap();
        assert!(
            found
                .rows
                .iter()
                .any(|line| line.starts_with("latin.txt:1:")),
            "search still searches it, lossily: {:?}",
            found.rows
        );

        // The positive twin: valid UTF-8 with multi-byte characters reads whole.
        let utf8 = "café = 1\nnaïve = 2\n";
        fs::write(ws.root().join("utf8.txt"), utf8).unwrap();
        assert_eq!(ws.read_file("utf8.txt").unwrap(), utf8);
        assert_eq!(ws.line_count("utf8.txt"), LineCount::Lines(2));
        let _ = fs::remove_dir_all(ws.root());
    }

    #[test]
    fn truncation_keeps_short_text_intact() {
        assert_eq!(truncate_for_model("short".to_string(), 100), "short");
        assert_eq!(truncate_for_model(String::new(), 0), "");
        let cut = truncate_for_model("abcdef".to_string(), 3);
        assert!(cut.starts_with("abc"), "{cut}");
        assert!(
            cut.contains("output truncated at 3 bytes") && cut.contains("rerun it narrower"),
            "a partial result says so and what to do: {cut}"
        );
    }

    /// A tail keeps the last bytes and says so at the front — the opposite end
    /// from `truncate_for_model`, because what a log died of is at the bottom.
    #[test]
    fn a_tail_keeps_the_end_and_marks_the_cut_at_the_front() {
        assert_eq!(tail_for_model("short", 100), "short");
        let tail = tail_for_model("abcdef", 3);
        assert!(tail.ends_with("def"), "{tail}");
        assert!(
            tail.starts_with("[mush: output truncated at 3 bytes"),
            "{tail}"
        );
        assert!(tail.contains("rerun it narrower"), "{tail}");
        // The cut lands on a char boundary, never inside a character.
        let tail = tail_for_model("éééééé", 5);
        assert!(tail.ends_with("é"), "{tail}");
        assert!(tail.starts_with("[mush: output truncated"), "{tail}");
    }
}
