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

/// The largest image a read will hand to a model, in bytes. An image's real
/// cost is its pixels and the tokens they become — a 4 MB screenshot is a page
/// of context on most endpoints — so the cap is a size a picture is worth and
/// not the whole-read cap: past it the refusal names a downscale, which is the
/// one road that makes the picture readable at all.
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

    /// Read a text file whole. Binary files are refused.
    ///
    /// It used to take a `cap`, keep the head of a long file and mark the cut
    /// with a sentence of its own — then the six-tool cut took the file tools
    /// away and this became `edit_file`'s private read, which must see the whole
    /// file or refuse the edit. The read tools are back ([`Self::read_window`],
    /// [`Self::read_image`]) because the shell cannot serve them behind a
    /// machine lock, so the cut lives there and this stays the whole-file read.
    pub fn read_file(&self, rel: &str) -> Result<String, String> {
        let path = self.resolve(rel)?;
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if bytes.contains(&0) {
            return Err(format!("{rel} looks like a binary file"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Read `rel` as an image, when it is one: the mime its own first bytes
    /// name, and the bytes whole — or `None` when the file is not an image, so
    /// the caller reads it as text.
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
    /// `Ok(None)` = "this is text, not an image".
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
    /// existing, not being a file at all, and not being an image are all
    /// `Ok(None)`: the paste is inserted as the text it is. Past
    /// [`IMAGE_FILE_CAP`] it is an image that cannot ride, and the `Err` is the
    /// read tool's own refusal sentence — one file refused at either door is
    /// refused with the same words.
    pub fn pasted_image(&self, paste: &str) -> Result<Option<Image>, String> {
        let Some(name) = pasted_name(paste) else {
            return Ok(None);
        };
        let path = if Path::new(&name).is_absolute() {
            PathBuf::from(&name)
        } else {
            match self.resolve(&name) {
                Ok(path) => path,
                // An escape or a `..` is not a name this door opens; the paste
                // goes in the box as the words it is.
                Err(_) => return Ok(None),
            }
        };
        // The name the placeholder keeps: workspace-relative for a file inside
        // the root (however the human spelled it), and the absolute path as
        // given for one outside — a name that still means the file after the
        // bytes are shed.
        let label = self.rel(&path);
        self.image_at(&path, &label)
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
    /// overwrite the first.
    ///
    /// `Err` is "these bytes are an image, and they cannot ride": bytes that
    /// sniff as no image at all, or one past [`IMAGE_FILE_CAP`], whose refusal
    /// names the clipboard's own road (`wl-paste -t image/png > shot.png`,
    /// then a `convert` downscale) because that is the only one a clipboard
    /// image has.
    pub fn save_pasted_image(&self, bytes: Vec<u8>) -> Result<Image, String> {
        let Some(mime) = image_mime(&bytes) else {
            return Err(
                "the clipboard bytes are not a png, jpeg, gif or webp image — copy the picture \
                 itself, or paste the path of an image file"
                    .to_string(),
            );
        };
        let size = bytes.len() as u64;
        if size > IMAGE_FILE_CAP {
            let cap = IMAGE_FILE_CAP / (1024 * 1024);
            let format = mime.strip_prefix("image/").unwrap_or(mime);
            return Err(format!(
                "the clipboard image is a {format} of {size} bytes — past the {cap} MB cap on an \
                 image. Save it to a file and downscale it (`wl-paste -t image/png > shot.png`, \
                 then `convert shot.png -resize 50% small.png`), then copy the smaller one"
            ));
        }
        session::ensure_mush_dir(self.root())
            .map_err(|e| format!("cannot create {}: {e}", session::MUSH_DIR))?;
        let dir = self.root().join(session::MUSH_DIR).join("paste");
        fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}/paste: {e}", session::MUSH_DIR))?;
        let name = format!("pasted-{}.{}", now_millis(), pasted_extension(mime));
        let path = dir.join(&name);
        fs::write(&path, &bytes)
            .map_err(|e| format!("cannot write {}/{name}: {e}", session::MUSH_DIR))?;
        Ok(Image {
            path: format!("{}/paste/{name}", session::MUSH_DIR),
            mime: mime.to_string(),
            bytes,
        })
    }

    /// The image at `path`, named `name` in the refusal and in the returned
    /// [`Image`]: the mime its own first bytes name, and the bytes whole — or
    /// `Ok(None)` when they are not an image's, so a caller reads the file as
    /// text (or, in [`Self::pasted_image`]'s case, as the words the paste is).
    ///
    /// One reader for the two doors a picture comes in by — the model's
    /// `read_file` and the human's paste — because a cap, a sniff and a
    /// refusal sentence that differed between them would be the same decision
    /// made twice, and the copies would drift. The head is read before the
    /// whole file so a 200 MB blob that is not an image is not loaded to find
    /// that out; the file is only whole here once its own bytes said "image".
    fn image_at(&self, path: &Path, name: &str) -> Result<Option<Image>, String> {
        let mut head = [0u8; 16];
        let Ok(mut file) = fs::File::open(path) else {
            // Not there, or not openable: not an image, so the caller's other
            // reading of the path is the answer.
            return Ok(None);
        };
        // A directory opens but does not read (or does not open at all,
        // depending on the platform): either way it is `Ok(None)`, not an
        // image.
        let Ok(read) = file.read(&mut head) else {
            return Ok(None);
        };
        let Some(mime) = image_mime(&head[..read]) else {
            return Ok(None);
        };
        let bytes = fs::read(path).map_err(|e| format!("cannot read {name}: {e}"))?;
        let size = bytes.len() as u64;
        if size > IMAGE_FILE_CAP {
            return Err(image_too_big(name, mime, size));
        }
        Ok(Some(Image {
            path: name.to_string(),
            mime: mime.to_string(),
            bytes,
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
fn image_too_big(name: &str, mime: &str, size: u64) -> String {
    let cap = IMAGE_FILE_CAP / (1024 * 1024);
    let format = mime.strip_prefix("image/").unwrap_or(mime);
    format!(
        "{name} is a {format} image of {size} bytes — past the {cap} MB cap on an image. \
         Downscale it with run_command (`convert {name} -resize 50% small.png`) and read that"
    )
}

/// The name a paste may be, or `None` when the paste is text. The whole parse
/// of a pasted path lives here so [`Workspace::pasted_image`] reads as its
/// rules rather than as their arithmetic; see there for why each rule is what
/// it is.
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

        // An absolute path outside the root keeps the path as the human gave
        // it: there is no workspace name for it.
        let outside = std::env::temp_dir().join(format!("mush-paste-{}", std::process::id()));
        fs::write(&outside, png(0)).unwrap();
        let paste = outside.display().to_string();
        let image = ws.pasted_image(&paste).unwrap().unwrap();
        assert_eq!(image.path, paste);

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
        let image = ws.save_pasted_image(png(4)).unwrap();
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
            .save_pasted_image(vec![0xff, 0xd8, 0xff, 0xe0, 0x00])
            .unwrap();
        assert!(jpeg.path.ends_with(".jpg"), "{jpeg:?}");

        // Words are not an image, and an image past the cap names the
        // clipboard's own road: save it, downscale it, copy the smaller one.
        let refused = ws.save_pasted_image(b"hello".to_vec()).unwrap_err();
        assert!(refused.contains("not a png"), "{refused}");
        let refused = ws
            .save_pasted_image(png(IMAGE_FILE_CAP as usize))
            .unwrap_err();
        assert!(refused.contains("past the 2 MB cap"), "{refused}");
        assert!(
            refused.contains("wl-paste"),
            "the clipboard road: {refused}"
        );
        assert!(refused.contains("convert"), "and the downscale: {refused}");
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
