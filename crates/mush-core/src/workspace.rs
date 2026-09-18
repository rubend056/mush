//! Workspace filesystem access: safe paths, reads, atomic writes.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::text;
use tempfile::NamedTempFile;

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

    /// Read a text file whole. Binary files are refused.
    ///
    /// It used to take a `cap`, keep the head of a long file and mark the cut
    /// with a sentence of its own — but the file tools that read that way are
    /// gone (the six-tool cut), and its one caller is `edit_file`, which must
    /// see the whole file or refuse the edit. A too-big result is the command
    /// result's problem now, and `truncate_for_model` is the one place that
    /// says so.
    pub fn read_file(&self, rel: &str) -> Result<String, String> {
        let path = self.resolve(rel)?;
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        if bytes.contains(&0) {
            return Err(format!("{rel} looks like a binary file"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
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
        assert!(!ws.exists("src/missing.rs"));
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
