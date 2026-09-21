//! Workspace filesystem access: safe paths, reads, listings, search, atomic
//! writes.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

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

/// The longest file `search` opens. A pattern that matches inside a 200 MB log
/// is a match the model does not need and a walk that takes minutes; `run_command`
/// is the road to that file.
pub const SEARCH_FILE_CAP: u64 = 2 * 1024 * 1024;

/// How much of one matching line `search` shows, so one minified line cannot
/// spend the whole result.
const MATCH_LINE_CAP: usize = 240;

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
    /// file or refuse the edit. The read tools are back ([`Self::read_window`])
    /// because the shell cannot serve them behind a machine lock, so the cut
    /// lives there and this stays the whole-file read.
    pub fn read_file(&self, rel: &str) -> Result<String, String> {
        let path = self.resolve(rel)?;
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if bytes.contains(&0) {
            return Err(format!("{rel} looks like a binary file"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
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
