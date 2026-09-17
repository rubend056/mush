//! The text-only half of the agent's tools.
//!
//! Every tool result is produced here, so the executor and the schemas cannot
//! drift apart. The bytes always come from disk: mush holds no open file, so
//! there is no second copy for a tool result to disagree with.

use std::fmt;

use serde_json::Value;

use crate::workspace::Workspace;

/// Every tool the model may call.
///
/// The schemas, the dispatcher and the prompt all name tools through this enum,
/// so adding a tool is a compile error in every place that has to know about it
/// instead of a string that silently never matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolName {
    ListFiles,
    ReadFile,
    WriteFile,
    EditFile,
    RunCommand,
    SpawnAgent,
    WaitAgents,
    AgentStatus,
    AgentControl,
    CommandStatus,
    CommandControl,
    WaitCommands,
}

impl ToolName {
    /// Every tool, in schema order. `prompt::tool_schemas` is tested against
    /// this list, so a schema and its executor cannot drift.
    pub const ALL: [ToolName; 12] = [
        ToolName::ListFiles,
        ToolName::ReadFile,
        ToolName::WriteFile,
        ToolName::EditFile,
        ToolName::RunCommand,
        ToolName::SpawnAgent,
        ToolName::WaitAgents,
        ToolName::AgentStatus,
        ToolName::AgentControl,
        ToolName::CommandStatus,
        ToolName::CommandControl,
        ToolName::WaitCommands,
    ];

    /// The tools that exist for delegation only. A leaf agent (at `MAX_DEPTH`)
    /// does not receive them, which is what bounds the tree. A *job's* tools are
    /// workspace tools: an agent with no children can still start one, so they
    /// are not in this list.
    pub const ORCHESTRATION: [ToolName; 7] = [
        ToolName::SpawnAgent,
        ToolName::WaitAgents,
        ToolName::AgentStatus,
        ToolName::AgentControl,
        ToolName::CommandStatus,
        ToolName::CommandControl,
        ToolName::WaitCommands,
    ];

    /// The name the model calls this tool by.
    pub const fn as_str(self) -> &'static str {
        match self {
            ToolName::ListFiles => "list_files",
            ToolName::ReadFile => "read_file",
            ToolName::WriteFile => "write_file",
            ToolName::EditFile => "edit_file",
            ToolName::RunCommand => "run_command",
            ToolName::SpawnAgent => "spawn_agent",
            ToolName::WaitAgents => "wait_agents",
            ToolName::AgentStatus => "agent_status",
            ToolName::AgentControl => "agent_control",
            ToolName::CommandStatus => "command_status",
            ToolName::CommandControl => "command_control",
            ToolName::WaitCommands => "wait_commands",
        }
    }

    /// The tool a name refers to, or `None` when the model invented one.
    pub fn parse(name: &str) -> Option<ToolName> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == name)
    }

    /// Whether this tool exists only for delegation.
    pub fn is_orchestration(self) -> bool {
        Self::ORCHESTRATION.contains(&self)
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The names of a list of tools, in the order given.
const fn names<const N: usize>(tools: [ToolName; N]) -> [&'static str; N] {
    let mut out = [""; N];
    let mut index = 0;
    while index < N {
        out[index] = tools[index].as_str();
        index += 1;
    }
    out
}

/// Every tool name, in schema order. Derived from [`ToolName::ALL`], so the two
/// cannot disagree.
pub const TOOL_NAMES: [&str; 12] = names(ToolName::ALL);

/// The names of the delegation-only tools.
pub const ORCHESTRATION_TOOLS: [&str; 7] = names(ToolName::ORCHESTRATION);

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

/// One replacement in a batch. `replace_all` is what a rename needs: the same
/// pattern several times in a file is otherwise refused as ambiguous.
#[derive(Clone, Debug)]
pub struct Edit {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

/// The text `edit_file` produces: `current` with exactly one occurrence of
/// `old` replaced by `new`. Refusing to guess is the point — a missing or
/// ambiguous match is an error the model can correct, and a wrong edit is
/// impossible.
pub fn edit_text(current: &str, old: &str, new: &str, rel: &str) -> Result<String, String> {
    apply_one(current, old, new, false, rel, None)
}

/// Several edits applied in order, in memory, as one change.
///
/// All or nothing: the value is built up and returned whole, so an edit that
/// does not match leaves the input untouched and a caller that writes only on
/// `Ok` cannot leave a file half-changed. Applying them here rather than as
/// separate tool calls also makes the batch one round trip instead of one per
/// edit, and guarantees the edits see each other's results in the order given.
pub fn edit_text_many(current: &str, edits: &[Edit], rel: &str) -> Result<String, String> {
    let mut text = current.to_string();
    for (index, edit) in edits.iter().enumerate() {
        text = apply_one(
            &text,
            &edit.old,
            &edit.new,
            edit.replace_all,
            rel,
            Some(index + 1),
        )?;
    }
    Ok(text)
}

fn apply_one(
    current: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    rel: &str,
    which: Option<usize>,
) -> Result<String, String> {
    // Naming *which* edit failed matters in a batch: the model has to know what
    // to fix, and "old_string not found" alone is ambiguous.
    let at = match which {
        Some(index) => format!("edit {index}: "),
        None => String::new(),
    };
    if old.is_empty() {
        return Err(format!("{at}old_string must not be empty"));
    }
    match current.matches(old).count() {
        0 => Err(format!("{at}old_string not found in {rel}")),
        1 => Ok(current.replacen(old, new, 1)),
        _ if replace_all => Ok(current.replace(old, new)),
        count => Err(format!(
            "{at}old_string appears {count} times in {rel}; include more context to make it \
             unique, or set replace_all to change every occurrence"
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

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
        }
    }

    /// A batch lands whole or not at all: an edit that cannot apply leaves the
    /// input untouched, so a caller that writes only on `Ok` cannot leave a file
    /// half-changed.
    #[test]
    fn a_batch_is_all_or_nothing() {
        let file = "let a = 1;\nlet b = 2;\n";
        let applied = edit_text_many(
            file,
            &[edit("a = 1", "a = 10"), edit("b = 2", "b = 20")],
            "f.rs",
        )
        .unwrap();
        assert_eq!(applied, "let a = 10;\nlet b = 20;\n");

        // The second edit cannot match, so the whole batch fails and the first
        // edit is *not* returned as a partial result.
        let error = edit_text_many(
            file,
            &[edit("a = 1", "a = 10"), edit("nothing here", "x")],
            "f.rs",
        )
        .unwrap_err();
        assert!(
            error.contains("edit 2"),
            "the failing edit is named: {error}"
        );
        assert!(error.contains("not found"), "{error}");
        // The input itself is untouched, which is what makes the batch atomic.
        assert_eq!(file, "let a = 1;\nlet b = 2;\n");
    }

    /// A rename is the case a single-pair edit cannot express: the same text
    /// several times.
    #[test]
    fn replace_all_changes_every_occurrence() {
        let file = "old_name();\nold_name(arg);\n";
        let plain = edit_text(file, "old_name", "new_name", "f.rs").unwrap_err();
        assert!(plain.contains("2 times"), "{plain}");

        let all = Edit {
            old: "old_name".to_string(),
            new: "new_name".to_string(),
            replace_all: true,
        };
        assert_eq!(
            edit_text_many(file, &[all], "f.rs").unwrap(),
            "new_name();\nnew_name(arg);\n"
        );
    }

    /// One name per variant, and parsing it back gives the same tool: the
    /// schema table and the dispatcher are two views of one list.
    #[test]
    fn every_tool_name_round_trips() {
        for tool in ToolName::ALL {
            assert_eq!(ToolName::parse(tool.as_str()), Some(tool));
            assert_eq!(tool.to_string(), tool.as_str());
        }
        assert_eq!(ToolName::parse("list_files"), Some(ToolName::ListFiles));
        assert_eq!(ToolName::parse("nonsense"), None);

        // The names derive from the enum, in the same order.
        let all: Vec<&str> = ToolName::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(all, TOOL_NAMES.to_vec());
        let orchestration: Vec<&str> = ToolName::ORCHESTRATION.iter().map(|t| t.as_str()).collect();
        assert_eq!(orchestration, ORCHESTRATION_TOOLS.to_vec());
        assert!(ToolName::ALL
            .iter()
            .skip(5)
            .all(|tool| tool.is_orchestration()));
        // A job is a second-class actor, so its tools travel with the agent
        // tools: a leaf at `MAX_DEPTH` does not get them, and a leaf's job still
        // reports to it by waking it.
        assert!(ToolName::CommandStatus.is_orchestration());
        assert!(ToolName::CommandControl.is_orchestration());
        assert!(ToolName::WaitCommands.is_orchestration());
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
