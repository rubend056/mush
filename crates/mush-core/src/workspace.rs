//! Workspace filesystem access: safe paths, listings, reads, atomic writes.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;
use walkdir::WalkDir;

/// Directories that are never worth showing or walking into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".mush",
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

/// Whether a walk should skip this entry: hidden names always, build/VCS
/// directories by name (their contents are never part of the workspace).
fn skipped(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || (entry.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()))
}

/// A single workspace root. All agent file access goes through here, which is
/// what keeps a runaway model inside the directory the human opened.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
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

    pub fn exists(&self, rel: &str) -> bool {
        self.resolve(rel).map(|p| p.exists()).unwrap_or(false)
    }

    /// List workspace-relative file paths, sorted. Hidden files and build/VCS
    /// directories are skipped so a listing stays useful, and symlinks are not
    /// followed (a link out of the workspace is not workspace content).
    pub fn list_files(&self, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        let walk = WalkDir::new(&self.root)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !skipped(entry));
        for entry in walk {
            // A per-entry error (a racing delete, a permission wall) skips
            // that entry, not the rest of the walk.
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            out.push(self.rel(entry.path()));
            if out.len() >= limit {
                break;
            }
        }
        out.sort();
        out
    }

    /// Read a text file, capping the returned bytes. Binary files are refused.
    pub fn read_file(&self, rel: &str, cap: usize) -> Result<String, String> {
        let path = self.resolve(rel)?;
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if bytes.contains(&0) {
            return Err(format!("{rel} looks like a binary file"));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(truncate_for_model(text, cap))
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
/// cut, so a partial result can never be mistaken for the whole file.
/// `usize::MAX` keeps everything.
pub fn truncate_for_model(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n\n[mush: output truncated]");
    text
}

/// Cap text handed to a model from the *end*, marking the cut at the front.
///
/// A side effect of an unfinished command is a tail: the tests it printed last,
/// the error it died on, the panic at the bottom of the log. `truncate_for_model`
/// keeps the head instead, which is right for a file the model is about to edit
/// and wrong for a log it is about to read. Keeping the tail is also what makes a
/// crash legible at all — the interesting bytes are the ones written just before
/// it stopped.
pub fn tail_for_model(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut cut = text.len() - cap;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[mush: output truncated]\n\n{}", &text[cut..])
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
        assert_eq!(ws.read_file("src/lib.rs", 1024).unwrap(), "fn a() {}\n");
        assert!(!ws.exists("src/missing.rs"));
    }

    #[test]
    fn read_truncates_at_char_boundary() {
        let ws = temp_workspace("truncate");
        ws.write_file("a.txt", "éééééééééé").unwrap();
        let text = ws.read_file("a.txt", 5).unwrap();
        assert!(text.starts_with("é"));
        assert!(text.ends_with("[mush: output truncated]"));
    }

    #[test]
    fn truncation_keeps_short_text_intact() {
        assert_eq!(truncate_for_model("short".to_string(), 100), "short");
        assert_eq!(truncate_for_model(String::new(), 0), "");
        assert_eq!(
            truncate_for_model("abcdef".to_string(), 3),
            "abc\n\n[mush: output truncated]"
        );
    }

    /// A tail keeps the last bytes and says so at the front — the opposite end
    /// from `truncate_for_model`, because what a log died of is at the bottom.
    #[test]
    fn a_tail_keeps_the_end_and_marks_the_cut_at_the_front() {
        assert_eq!(tail_for_model("short", 100), "short");
        assert_eq!(
            tail_for_model("abcdef", 3),
            "[mush: output truncated]\n\ndef"
        );
        // The cut lands on a char boundary, never inside a character.
        let tail = tail_for_model("éééééé", 5);
        assert!(tail.ends_with("é"), "{tail}");
        assert!(tail.starts_with("[mush: output truncated]"), "{tail}");
    }

    #[test]
    fn listing_skips_hidden_and_build_dirs() {
        let ws = temp_workspace("listing");
        fs::create_dir_all(ws.root().join(".git")).unwrap();
        fs::create_dir_all(ws.root().join("target")).unwrap();
        fs::write(ws.root().join(".git/config"), "x").unwrap();
        fs::write(ws.root().join("target/out"), "x").unwrap();
        fs::write(ws.root().join(".hidden"), "x").unwrap();
        fs::write(ws.root().join("main.rs"), "x").unwrap();
        assert_eq!(ws.list_files(100), vec!["main.rs".to_string()]);
    }
}
