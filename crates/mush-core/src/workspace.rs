//! Workspace filesystem access: safe paths, reads, listings, search, atomic
//! writes.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::message::Image;
use crate::session;
use crate::text;

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

    /// Workspace-relative display path for an absolute path.
    pub fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Read a text file whole, for the one road that writes back what it reads:
    /// `edit_file`, whose exact replacement needs every byte it is replacing.
    /// Binary files are refused, and so is anything that is not a regular file:
    /// `fs::read` on a FIFO blocks until a writer appears — an actor parked
    /// forever on a name as innocent as `x.png` — and a device may never end at
    /// all. The metadata answers what the path *is* before anything is opened,
    /// the same shape of check [`Self::image_at`] makes, so the two roads
    /// cannot disagree about which paths can be read. The name is resolved for
    /// real first ([`Self::real_path`]), because a link the root contains must
    /// not make this read a file outside it. The read itself is bounded by
    /// [`READ_FILE_CAP`] and checked from the stat before a byte is read (see
    /// [`Self::whole_read`]): past the cap the refusal is [`over_read_cap`]'s,
    /// the one sentence [`Self::read_window`] gives too, naming `run_command`
    /// as the road that works.
    ///
    /// The decode is **strict**: bytes that are not valid UTF-8 are refused
    /// ([`not_utf8`]), because this read's text is an edit's source *and* its
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
                String::from_utf8_lossy(&bytes).into_owned(),
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
    /// ([`Self::real_path`]) so a link inside the root cannot make the model's
    /// read open a picture outside it; the human's paste road reaches
    /// [`Self::image_at`] without this check, because the human already has
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
    /// [`Self::write_pasted_image`], shared with the copy
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
    ///
    /// The window's lines are a *reader's* lines: [`str::lines`] drops the
    /// `\r` of a CRLF ending, so what is copied out of a CRLF file's window is
    /// a line's text, never its bytes. That is said rather than hidden — a file
    /// whose lines all end with CRLF gets a sentence saying so, and [`edit`]
    /// refuses an edit whose strings hold a line break or a `\r` in such a
    /// file: a single-line edit lands byte for byte, and the road for anything
    /// across lines is `run_command` (`sed -i`, `perl -pi`) or `write_file`
    /// (finding B7). The two used to be silent and disagreed — a copied
    /// multi-line `old_string` could never match, and a one-line edit that did
    /// match inserted LF lines into the CRLF file.
    ///
    /// [`edit`]: crate::tools::edit_text
    ///
    /// The cap is the same whole-read cap as everywhere else ([`READ_FILE_CAP`],
    /// checked from the stat by [`Self::whole_read`] before a byte is read): this
    /// road cannot get past it, and the refusal is [`over_read_cap`]'s one
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
    ) -> Result<String, String> {
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
        if text::is_crlf(&text) {
            out.push_str(
                "\n[mush: the file's lines end with CRLF — the \\r is not shown in a line; an \
                 edit whose old_string or new_string holds a line break or a \\r is refused, a \
                 line's own text still edits exactly, and run_command (`sed -i`, `perl -pi`) or \
                 write_file is the road for anything across lines]",
            );
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
        Ok(out)
    }

    /// Every file under `rel` (default the workspace root), workspace-relative
    /// and sorted, with the first `limit` and whether there were more. Build and
    /// VCS directories are skipped ([`SKIP_DIRS`]); a symlinked directory is not
    /// followed, so a listing cannot leave the workspace.
    ///
    /// That claim is why the name is checked for real before the walk
    /// ([`Self::real_path`]): `out -> /tmp/elsewhere` is a name inside the root
    /// whose listing used to be the outside directory's. The walk itself still
    /// runs on the name the model gave, so a link to a *file* inside the root
    /// answers about that file under the name it was asked about, while a link
    /// to a directory is left alone like every other symlinked directory.
    pub fn list_files(&self, rel: &str, limit: usize) -> Result<(Vec<String>, bool), String> {
        let start = self.resolve(rel)?;
        self.real_path(&start, rel)?;
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
    /// (a NUL byte) and files past [`SEARCH_FILE_CAP`] are skipped, and a
    /// matching line is cut to [`MATCH_LINE_CAP`] so one minified file cannot
    /// spend the result.
    ///
    /// The decode is **lossy on purpose**, like [`Self::read_window`]'s and for
    /// the same reason: a search only shows what it found, so a file in another
    /// encoding is searched as U+FFFD rather than skipped — a Latin-1 config the
    /// model can still find a symbol in is worth more than a miss the model
    /// cannot see through. A file that is not valid UTF-8 is not "binary" here;
    /// the skip counter is for the files (blobs, NUL-bearing) that have no text
    /// to search at all.
    ///
    /// What it skipped is counted and travels back with the matches
    /// ([`Matches::skipped`]): a search that says "no match" while it never
    /// opened a file is a false negative a model will act on.
    ///
    /// Like the listing, the name is checked for real before the walk
    /// ([`Self::real_path`]): a link inside the root cannot make the search
    /// read files outside it, and the files it does read are the ones under
    /// the name the model gave.
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
        self.real_path(&start, rel)?;
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
    ///
    /// The name is resolved to what it really is before anything is made (see
    /// [`Self::real_path`]): a write through a symlink lands in the file the
    /// link points at and the link stays a link, while a name whose real path
    /// leaves the root is refused rather than followed. What the name *is*
    /// decides the rest: a socket, a FIFO or a device is refused rather than
    /// renamed over ([`entry_for_write`]), and a file with no owner-write bit
    /// is refused with the mode it has, so a `0444` file the human marked
    /// read-only is a sentence the model can read instead of an override it
    /// cannot see. The mode refusal lives here, at the model's door, and not in
    /// [`atomic_write`], which `session::save` and the human's own
    /// `config.json` writer also use: they may replace a file whatever its
    /// mode, but a model may not. The type refusal is shared with
    /// [`atomic_write`], because a rename over a socket destroys it whatever
    /// door it came through.
    ///
    /// The store's own files are refused *by name* ([`Self::store_file_refusal`])
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
    /// Show them as U+FFFD: what is read this way is shown, never written back.
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
/// The name is resolved to what it really is first ([`entry_for_write`]): a
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

    fn temp_workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("mush-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Workspace::new(&dir).unwrap()
    }

    /// A path's permission bits as a human reads them (`0755`): the low twelve,
    /// without the file-type bits `Permissions::mode` also carries.
    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
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

        let outside = std::env::temp_dir().join(format!(
            "mush-test-root-link-outside-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&outside);
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
        refusals.push(match ws.search("DEEP", "out", false, 100) {
            Err(refused) => refused,
            Ok(found) => panic!(
                "a search through a link out of the root must be refused, not run: {:?}",
                found.matches
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
        let found = ws.search("caf", "", false, 10).unwrap();
        assert!(
            found
                .matches
                .iter()
                .any(|line| line.starts_with("latin.txt:1:")),
            "search still searches it, lossily: {:?}",
            found.matches
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
