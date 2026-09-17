//! The text-only half of the agent's tools.
//!
//! Every tool result is produced here, so the executor and the schemas cannot
//! drift apart. The bytes always come from disk: mush holds no open file, so
//! there is no second copy for a tool result to disagree with.

use serde_json::Value;

use crate::workspace::Workspace;

/// Every tool the model may call, in schema order. `prompt::tool_schemas` is
/// tested against this list, so a schema and its executor cannot drift.
pub const TOOL_NAMES: [&str; 9] = [
    "list_files",
    "read_file",
    "write_file",
    "edit_file",
    "run_command",
    "spawn_agent",
    "wait_agents",
    "agent_status",
    "agent_control",
];

/// The tools that exist for delegation only. A leaf agent (at `MAX_DEPTH`)
/// does not receive them, which is what bounds the tree.
pub const ORCHESTRATION_TOOLS: [&str; 4] = [
    "spawn_agent",
    "wait_agents",
    "agent_status",
    "agent_control",
];

/// A required string argument.
pub fn arg_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing `{key}`"))
}

/// The directory an optional `path` argument names, as a listing prefix.
pub fn arg_prefix(args: &Value) -> &str {
    args.get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
}

/// The result text of `list_files`: one path per line, or an empty answer the
/// model can act on.
pub fn list_result(ws: &Workspace, args: &Value, limit: usize) -> Result<String, String> {
    let prefix = arg_prefix(args);
    let files: Vec<String> = ws
        .list_files(limit)
        .into_iter()
        .filter(|file| {
            prefix.is_empty() || prefix == "." || file.starts_with(&format!("{prefix}/"))
        })
        .collect();
    if files.is_empty() {
        Ok(format!(
            "no files under `{}`",
            if prefix.is_empty() { "." } else { prefix }
        ))
    } else {
        Ok(files.join("\n"))
    }
}

/// The text `edit_file` produces: `current` with exactly one occurrence of
/// `old` replaced by `new`. Refusing to guess is the point — a missing or
/// ambiguous match is an error the model can correct, and a wrong edit is
/// impossible.
pub fn edit_text(current: &str, old: &str, new: &str, rel: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string must not be empty".to_string());
    }
    match current.matches(old).count() {
        0 => Err(format!("old_string not found in {rel}")),
        1 => Ok(current.replacen(old, new, 1)),
        count => Err(format!(
            "old_string appears {count} times in {rel}; include more context to make it unique"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_workspace(name: &str) -> Workspace {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("mush-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Workspace::new(&dir).unwrap()
    }

    #[test]
    fn arg_string_demands_a_string() {
        assert_eq!(
            arg_string(&json!({"path": "a.rs"}), "path").unwrap(),
            "a.rs"
        );
        assert!(arg_string(&json!({"path": 7}), "path").is_err());
        assert!(arg_string(&json!({}), "path").is_err());
    }

    #[test]
    fn list_result_filters_by_prefix() {
        use std::fs;
        let ws = temp_workspace("list");
        fs::create_dir_all(ws.root().join("src")).unwrap();
        fs::write(ws.root().join("top.rs"), "x").unwrap();
        fs::write(ws.root().join("src/deep.rs"), "x").unwrap();

        let all = list_result(&ws, &json!({}), 100).unwrap();
        assert!(
            all.contains("top.rs") && all.contains("src/deep.rs"),
            "{all}"
        );

        let src = list_result(&ws, &json!({"path": "./src/"}), 100).unwrap();
        assert_eq!(src, "src/deep.rs");

        let none = list_result(&ws, &json!({"path": "missing"}), 100).unwrap();
        assert!(none.starts_with("no files under"), "{none}");
        let _ = fs::remove_dir_all(ws.root());
    }

    #[test]
    fn edit_text_replaces_only_an_unambiguous_match() {
        let file = "let a = 1;\nlet b = 2;\n";
        assert_eq!(
            edit_text(file, "let b = 2;", "let b = 3;", "f.rs").unwrap(),
            "let a = 1;\nlet b = 3;\n"
        );

        let missing = edit_text(file, "let c", "x", "f.rs").unwrap_err();
        assert!(missing.contains("not found"), "{missing}");

        let ambiguous = edit_text(file, "let ", "const ", "f.rs").unwrap_err();
        assert!(ambiguous.contains("2 times"), "{ambiguous}");

        let empty = edit_text(file, "", "x", "f.rs").unwrap_err();
        assert!(empty.contains("must not be empty"), "{empty}");
    }
}
