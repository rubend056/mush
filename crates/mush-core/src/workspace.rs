//! Workspace filesystem access: safe paths, reads, listings, search, atomic
//! writes.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::message::Image;
use crate::session;
use crate::text;
use tempfile::NamedTempFile;

/// Directories a walk never descends into: VCS metadata and build output, whose
/// contents are never the workspace's work. Hidden names are *not* skipped —
/// `.github/`, `.gitignore` and `.env.example` are exactly the files an agent is
/// asked about (audit of the prompt vs behaviour, row 8) — and neither is
/// `.mush`, whose session file a model may well be asked to look at.
const SKIP_DIRS: &[&str] = &[
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

/// The largest file a read tool will open whole. Past it the memory a read
/// costs is the agent's problem rather than the file's, and the sentence says
/// so; a window of a 200 MB log is `run_command`'s job (`tail`, `sed -n`).
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
/// ruler against the other and got both cases wrong.
pub const IMAGE_FILE_CAP: u64 = 2 * 1024 * 1024;

/// The longest file `search` opens. A pattern that matches inside a 200 MB log
/// is a match the model does not need and a walk that takes minutes; `run_command`
/// is the road to that file.
pub const SEARCH_FILE_CAP: u64 = 2 * 1024 * 1024;

/// How much of one matching line `search` shows, so one minified line cannot
/// spend the whole result.
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
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

/// What a search found: the matching lines, whether the cap cut the list short,
/// and how many files it never opened (binary, or past [`SEARCH_FILE_CAP`]).
///
/// The third field is the one that keeps "no match" honest. A model reads a
/// miss as "the symbol does not exist", so a search that skipped a file must
/// say so — the count is what the tool tells it instead of a false negative.
pub struct Matches {
    pub matches: Vec<String>,
    pub more: bool,
    pub skipped: usize,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root)?,
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

    /// Workspace-relative display path for an absolute path.
    pub fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Read a text file whole. Binary files are refused, and so is anything
    /// that is not a regular file: `fs::read` on a FIFO blocks until a writer
    /// appears — an actor parked forever on a name as innocent as `x.png` — and
    /// a device may never end at all. The metadata answers what the path *is*
    /// before anything is opened, the same shape of check [`Self::image_at`]
    /// makes, so the two roads cannot disagree about which paths can be read.
    ///
    /// It used to take a `cap`, keep the head of a long file and mark the cut
    /// with a sentence of its own — then the six-tool cut took the file tools
    /// away and this became `edit_file`'s private read, which must see the whole
    /// file or refuse the edit. The read tools are back ([`Self::read_window`],
    /// [`Self::read_image`]) because the shell cannot serve them behind a
    /// machine lock, so the cut lives there and this stays the whole-file read.
    pub fn read_file(&self, rel: &str) -> Result<String, String> {
        let path = self.resolve(rel)?;
        let meta = fs::metadata(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{rel} is not a regular file — cannot read it"));
        }
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if bytes.contains(&0) {
            return Err(format!("{rel} looks like a binary file"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
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
    /// are lines and an image has none.
    pub fn read_image(&self, rel: &str) -> Result<Option<Image>, String> {
        let path = self.resolve(rel)?;
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
    /// is copied into `.mush/paste/` through [`Self::write_pasted_image`], the
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
    /// `.mush/paste/` through [`Self::write_pasted_image`], so every picture in
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
            .map_err(|e| format!("cannot copy {label} into .mush/paste: {e}"))
    }

    /// Write bytes that came from outside the workspace (the clipboard) into
    /// `.mush/paste/` and hand back the image that names them.
    ///
    /// A clipboard image has no path — the clipboard is a buffer, not a file —
    /// and [`Image`] carries one: it is the name a shed payload's placeholder
    /// keeps, so the bytes are given one here. `.mush/` ignores itself via its
    /// own `.gitignore` ([`session::ensure_mush_dir`]), so a pasted screenshot
    /// cannot dirty the tree, and the file is named for the moment it was
    /// pasted rather than for the clipboard, which would let a second paste
    /// overwrite the first. The write itself is [`Self::write_pasted_image`],
    /// shared with the copy [`Self::pasted_image`] makes of a file outside the
    /// root, so the two roads cannot drift in where the bytes land or what the
    /// image is called.
    ///
    /// `Err` is "these bytes are an image, and they cannot ride": bytes that
    /// sniff as no image at all, or one past [`IMAGE_FILE_CAP`], whose refusal
    /// names the clipboard's own road (`wl-paste -t image/png > shot.png`,
    /// then a `convert` downscale) because that is the only one a clipboard
    /// image has.
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
    /// The one write of that directory, shared by the two roads bytes from
    /// outside the workspace arrive by: the clipboard
    /// ([`Self::save_pasted_image`]), which has no file behind the bytes, and
    /// a paste naming a file outside the root ([`Self::pasted_image`]), which
    /// the model's own tools cannot reach. Two writes would be two spellings of
    /// one rule — the directory, the `pasted-<unix millis>.<png|jpg|gif|webp>`
    /// name and the [`Image`] that points at it — so there is one. `.mush/`
    /// ignores itself via its own `.gitignore` ([`session::ensure_mush_dir`]),
    /// so neither paste can dirty the tree, and the name carries the moment it
    /// was pasted rather than the clipboard or the source file; a name already
    /// taken moves to `-2`, `-3`, …, so neither a second paste nor the next
    /// picture of one batch can overwrite the one before it.
    ///
    /// Both callers have already refused a payload that is no image and one
    /// past [`IMAGE_FILE_CAP`], each with the sentence its own road can act on
    /// (the clipboard names `wl-paste`, a file names the `convert` downscale);
    /// what must not differ is where under-cap bytes land. `Err` here is "the
    /// copy cannot be written": the IO problem the directory or the file named.
    fn write_pasted_image(&self, bytes: Vec<u8>, mime: &str) -> Result<Image, String> {
        session::ensure_mush_dir(self.root())
            .map_err(|e| format!("cannot create {}: {e}", session::MUSH_DIR))?;
        let dir = self.root().join(session::MUSH_DIR).join("paste");
        fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}/paste: {e}", session::MUSH_DIR))?;
        let (name, mut file) = create_paste_file(&dir, now_millis(), mime)?;
        file.write_all(&bytes)
            .map_err(|e| format!("cannot write {}/{name}: {e}", session::MUSH_DIR))?;
        drop(file);
        let pixels = image_dimensions(mime, &bytes);
        Ok(Image {
            path: format!("{}/paste/{name}", session::MUSH_DIR),
            mime: mime.to_string(),
            bytes,
            pixels,
        })
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
        Ok(Some(Image {
            path: name.to_string(),
            mime: mime.to_string(),
            bytes,
            pixels,
        }))
    }

    /// A window of a text file, as the model reads it: `limit` lines from
    /// 1-based `offset`, cut to `cap` bytes, then one sentence if there is a
    /// rest — how much of the file this was and the `offset` that reads on.
    ///
    /// No line numbers are printed beside the text, on purpose: a model copies
    /// what it reads into `edit_file`'s `old_string`, and a numbered line is a
    /// string that cannot match. The range is named once, in the trailing
    /// sentence.
    pub fn read_window(
        &self,
        rel: &str,
        offset: usize,
        limit: usize,
        cap: usize,
    ) -> Result<String, String> {
        let path = self.resolve(rel)?;
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > READ_FILE_CAP {
                return Err(format!(
                    "{rel} is {} bytes — past the {} MB cap on a whole read, and a window cannot \
                     get past it (the file is opened whole first): read part of it with \
                     run_command (`sed -n '1,200p' {rel}`)",
                    meta.len(),
                    READ_FILE_CAP / (1024 * 1024)
                ));
            }
        }
        let text = self.read_file(rel)?;
        let total = text.lines().count();
        let offset = offset.max(1);
        if limit == 0 {
            return Err(format!(
                "`limit` must be at least 1 line — {rel} has {total}"
            ));
        }
        if total == 0 {
            return Ok(format!("{rel} is empty"));
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
        Ok(out)
    }

    /// Every file under `rel` (default the workspace root), workspace-relative
    /// and sorted, with the first `limit` and whether there were more. Build and
    /// VCS directories are skipped ([`SKIP_DIRS`]); a symlinked directory is not
    /// followed, so a listing cannot leave the workspace.
    pub fn list_files(&self, rel: &str, limit: usize) -> Result<(Vec<String>, bool), String> {
        let start = self.resolve(rel)?;
        // "Empty" and "not there" are different facts, and a listing that
        // answers `no files` for a path that does not exist is a lie the model
        // cannot see through.
        if fs::symlink_metadata(&start).is_err() {
            return Err(format!("no such path: `{rel}`"));
        }
        let mut found = Vec::new();
        self.walk(&start, &mut |path: &Path| {
            found.push(self.rel(path));
            true
        });
        found.sort();
        let truncated = found.len() > limit;
        found.truncate(limit);
        Ok((found, truncated))
    }

    /// Every line under `rel` containing `pattern` — a literal string, not a
    /// regex — as `path:line: text`, capped at `limit` matches plus the fact
    /// that there were more.
    ///
    /// Literal on purpose: a regex engine is a dependency and a search that
    /// runs one is the `rg` the shell already has, while this tool exists for
    /// the one case the shell cannot serve (a held machine lock). Binary files
    /// and files past [`SEARCH_FILE_CAP`] are skipped, and a matching line is
    /// cut to [`MATCH_LINE_CAP`] so one minified file cannot spend the result.
    ///
    /// What it skipped is counted and travels back with the matches
    /// ([`Matches::skipped`]): a search that says "no match" while it never
    /// opened a file is a false negative a model will act on.
    pub fn search(
        &self,
        pattern: &str,
        rel: &str,
        ignore_case: bool,
        limit: usize,
    ) -> Result<Matches, String> {
        if pattern.is_empty() {
            return Err("`pattern` must not be empty".to_string());
        }
        let start = self.resolve(rel)?;
        if fs::symlink_metadata(&start).is_err() {
            return Err(format!("no such path: `{rel}`"));
        }
        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };
        let mut matches = Vec::new();
        let mut more = false;
        let mut skipped = 0usize;
        self.walk(&start, &mut |path: &Path| {
            let Ok(meta) = fs::metadata(path) else {
                skipped += 1;
                return true;
            };
            if meta.len() > SEARCH_FILE_CAP {
                skipped += 1;
                return true;
            }
            let Ok(bytes) = fs::read(path) else {
                skipped += 1;
                return true;
            };
            if bytes.contains(&0) {
                skipped += 1;
                return true;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (number, line) in text.lines().enumerate() {
                let haystack = if ignore_case {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if !haystack.contains(&needle) {
                    continue;
                }
                if matches.len() == limit {
                    more = true;
                    return false;
                }
                matches.push(format!(
                    "{}:{}: {}",
                    self.rel(path),
                    number + 1,
                    text::truncate(line.trim_end(), MATCH_LINE_CAP)
                ));
            }
            true
        });
        Ok(Matches {
            matches,
            more,
            skipped,
        })
    }

    /// Walk every file under `start` — files only, [`SKIP_DIRS`] by name, no
    /// symlinked directories — calling `visit` until it answers `false`. A
    /// `start` that is itself a file visits that one file, so "list this path"
    /// and "search this path" answer about the file the model named instead of
    /// claiming there is nothing there.
    ///
    /// One walker for the listing and the search: a second one is a second
    /// answer to "what is a workspace file", and the two drift.
    fn walk(&self, start: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
        if start.is_file() {
            visit(start);
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
                    if !SKIP_DIRS.contains(&name.as_ref()) {
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
    pub fn write_file(&self, rel: &str, content: &str) -> Result<(), String> {
        let path = self.resolve(rel)?;
        if path == self.root {
            return Err("refusing to write to the workspace root".to_string());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", self.rel(parent)))?;
        }
        atomic_write(&path, content.as_bytes()).map_err(|e| format!("cannot write {rel}: {e}"))
    }
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
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

/// Create the file a paste's bytes go in under `dir` (`.mush/paste/`), and
/// hand back the name it took: `pasted-<unix millis>.<ext>`, or
/// `pasted-<millis>-2.<ext>` and on when that name is already there.
///
/// A paste of four pictures is four of these in the same millisecond, and
/// every image must keep the bytes that rode with it — a name taken is not a
/// name to overwrite. `create_new` is what makes the test and the create one
/// step, so two pastes can never land in one file however they interleave.
/// `millis` is a parameter rather than `now_millis()` inside because the
/// naming rule is a fact a test can pin: same millisecond, second name.
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
            Err(e) => return Err(format!("cannot write {}/{name}: {e}", session::MUSH_DIR)),
        }
    }
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

/// Write via a same-directory temp file plus `rename`, so readers never observe
/// a half-written file and a crash cannot corrupt the original.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("mush-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Workspace::new(&dir).unwrap()
    }

    /// A picture file *outside* every test workspace — the paste road's own
    /// case: a name the human may give and the model's tools may not resolve.
    /// The test's name and the process id keep two tests from sharing (and so
    /// overwriting) one outside file, and a stale one from an earlier run is
    /// replaced.
    fn outside_image(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mush-test-outside-{name}-{}.png",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        fs::write(&path, bytes).unwrap();
        path
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
        // And the rule folds `\` to `/`: a path built on Windows, or a name a
        // model wrote, reads with the one separator — which the second rule
        // did not do.
        assert_eq!(ws.rel(&ws.root().join("a\\b")), "a/b");
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
        let outside: Vec<PathBuf> = originals
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

        let session = session::Session {
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
    /// exact replacement needs every byte it is replacing. A binary file is the
    /// one thing refused, because lossy UTF-8 would rewrite it.
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
