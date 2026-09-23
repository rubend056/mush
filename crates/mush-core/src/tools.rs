//! The tool vocabulary the model and the dispatcher share.
//!
//! [`ToolName`] is the one list of what exists. The text half of an edit
//! (`edit_text`, `edit_text_many`) lives here too: every edit result is produced
//! by the same code the executor calls, so a schema and its behaviour cannot
//! drift apart.

use std::fmt;

use serde_json::Value;

use crate::text;

/// Every tool the model may call.
///
/// Ten, and the count has a history worth keeping. Six of them were a *cut*:
/// `list_files`, `read_file` and `write_file` went, because the shell lists,
/// reads and writes a workspace better than a bespoke tool could — `rg`, `sed
/// -n '1,200p'`, `ls -la`, `mkdir -p && cat > f` — and `edit_file` stayed for
/// the one property the shell cannot offer, an exact-and-unique replacement.
/// Two facts brought the three back, and neither is taste: the machine lock
/// refuses *every* `run_command` while another agent holds it, so "the shell can
/// do it" is false exactly when an agent is blind; and the shell cannot carry
/// bytes that are not text, so an image had no road at all. `list_files`,
/// `read_file` and `write_file` work beside a lock and take an image;
/// `search` is the same argument for finding a line.
///
/// The schemas, the dispatcher and the prompt all name tools through this enum,
/// so adding a tool is a compile error in every place that has to know about it
/// instead of a string that silently never matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolName {
    EditFile,
    ReadFile,
    WriteFile,
    ListFiles,
    Search,
    RunCommand,
    SpawnAgent,
    Status,
    Control,
    Wait,
}

impl ToolName {
    /// Every tool, in schema order: the file work first, then the shell, then
    /// what an agent manages. `prompt::tool_schemas` is tested against this
    /// list, so a schema and its executor cannot drift.
    pub const ALL: [ToolName; 10] = [
        ToolName::EditFile,
        ToolName::ReadFile,
        ToolName::WriteFile,
        ToolName::ListFiles,
        ToolName::Search,
        ToolName::RunCommand,
        ToolName::SpawnAgent,
        ToolName::Status,
        ToolName::Control,
        ToolName::Wait,
    ];

    /// The tools that exist for delegation only. A leaf agent (at `MAX_DEPTH`)
    /// does not receive them, which is what bounds the tree. `status`, `control`
    /// and `wait` are *job* tools too — an agent with no children can still
    /// start a command and manage it — so they stay in a leaf's set.
    pub const ORCHESTRATION: [ToolName; 1] = [ToolName::SpawnAgent];

    /// The name the model calls this tool by.
    pub const fn as_str(self) -> &'static str {
        match self {
            ToolName::EditFile => "edit_file",
            ToolName::ReadFile => "read_file",
            ToolName::WriteFile => "write_file",
            ToolName::ListFiles => "list_files",
            ToolName::Search => "search",
            ToolName::RunCommand => "run_command",
            ToolName::SpawnAgent => "spawn_agent",
            ToolName::Status => "status",
            ToolName::Control => "control",
            ToolName::Wait => "wait",
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
pub const TOOL_NAMES: [&str; 10] = names(ToolName::ALL);

/// The names of the delegation-only tools.
pub const ORCHESTRATION_TOOLS: [&str; 1] = names(ToolName::ORCHESTRATION);

/// A required string argument. A value that is present but not a string is
/// refused with the shape it should have had: "missing" is a true sentence
/// about a field that is not there and a false one about a field the model sent
/// as a number, and a model told its `command` is missing will add a second one
/// before it ever changes the type.
pub fn arg_string(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        None | Some(Value::Null) => Err(format!("missing `{key}`")),
        Some(Value::String(text)) => Ok(text.to_string()),
        Some(other) => Err(format!("`{key}` must be a string; got {other}")),
    }
}

/// An optional string argument, with `null` read as absent — the JSON way of
/// saying nothing, deliberately, so `{base: null}` still means "no base" while
/// any other non-string is refused rather than read as one.
///
/// A fallback here is the most expensive default in the tool set: a `base` that
/// reads as "no base" drops the child's worktree and puts its edits in the
/// parent's checkout (finding F12), and a `title` that reads as "no title"
/// drops the row's name — both answer a question nobody asked, exactly the trap
/// [`arg_usize`] documents.
pub fn arg_string_opt(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.to_string())),
        Some(other) => Err(format!("`{key}` must be a string; got {other}")),
    }
}

/// An optional whole number argument, defaulted. A value that is present but
/// not a number is refused rather than defaulted: silently reading line 1 when
/// the model asked for a window is how a read answers a question nobody asked.
pub fn arg_usize(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| format!("`{key}` must be a whole number")),
    }
}

/// An optional boolean argument, defaulted.
pub fn arg_bool(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("`{key}` must be true or false")),
    }
}

/// An optional path argument, defaulted to the workspace root.
///
/// A value that is present but not a string is refused, like every other typed
/// argument: `list_files({path: 7})` used to read as "the root" and answer a
/// question nobody asked, which is exactly the trap [`arg_usize`] documents.
pub fn arg_path(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(path)) => Ok(path.trim().to_string()),
        Some(_) => Err(format!("`{key}` must be a string")),
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

/// Read `edit_file`'s `edits`: the one shape. A list of `{old_string,
/// new_string, replace_all?}` — and a lone entry given as an object is read as
/// the list of one it means, because a model that sends the shape it meant
/// should not spend a turn being told about brackets.
///
/// There used to be a second, top-level `old_string`/`new_string` pair beside
/// this, and the two spellings drifted exactly as a fact with two homes does:
/// the schema declared `replace_all` only inside the list, while the code
/// honoured a top-level one (H20 item 3). One shape, one parse.
pub fn edits_arg(args: &Value) -> Result<Vec<Edit>, String> {
    let entries = match args.get("edits") {
        None | Some(Value::Null) => {
            return Err(
                "missing `edits`: a list of {old_string, new_string, replace_all?}".to_string(),
            )
        }
        Some(Value::Array(list)) => list.clone(),
        Some(entry @ Value::Object(_)) => vec![entry.clone()],
        Some(_) => return Err("`edits` must be a list of edits".to_string()),
    };
    if entries.is_empty() {
        return Err("`edits` is empty — nothing to change".to_string());
    }
    let mut edits = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let at = |what: String| format!("edit {}: {what}", index + 1);
        // The required strings go through [`arg_string`], so a value present
        // with the wrong type is told its shape rather than called missing; the
        // flag is refused the same way, never defaulted — a `replace_all`
        // someone set to `"true"` must not silently change one occurrence
        // (finding A7's class: a wrongly-typed argument is refused, not
        // defaulted).
        let old = arg_string(entry, "old_string").map_err(at)?;
        let new = arg_string(entry, "new_string").map_err(at)?;
        let replace_all = arg_bool(entry, "replace_all", false).map_err(at)?;
        edits.push(Edit {
            old,
            new,
            replace_all,
        });
    }
    Ok(edits)
}

/// The text `edit_file` produces for a single pair: `current` with exactly one
/// occurrence of `old` replaced by `new` — or with *every* occurrence replaced
/// when `replace_all` is set, which is the same promise a batch entry's flag
/// makes. Refusing to guess is the point — a missing or ambiguous match is an
/// error the model can correct, and a wrong edit is impossible.
///
/// A file whose lines all end with CRLF is edited by its own bytes or not at
/// all. [`Workspace::read_window`] does not show the `\r` (it is an ending, not
/// a line's text), so an edit that spells a line break — or a `\r` — cannot be
/// trusted to be the file's own ending: a bare-LF `old_string` copied from the
/// window could never match, and a `new_string` holding one would land LF lines
/// inside a CRLF file, which `git diff` shows as a whole-file change the next
/// time anything normalizes the endings. So an edit whose `old` or `new` holds
/// `\n` or `\r` is refused in words on such a file, naming the endings and the
/// roads that can do the work (`run_command`, `write_file`); an edit that stays
/// inside a line lands byte for byte and touches no ending (finding B7). A
/// *mixed* file is not a CRLF file and is left to the byte-exact rule — see
/// [`crate::text::is_crlf`].
///
/// [`Workspace::read_window`]: crate::workspace::Workspace::read_window
pub fn edit_text(
    current: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    rel: &str,
) -> Result<String, String> {
    apply_one(current, old, new, replace_all, rel, None)
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
    // Why a CRLF file refuses a line-crossing edit is [`edit_text`]'s doc; the
    // check is here because this is where `current` and both strings are in
    // hand. It comes before the match count so the model is told the real
    // reason instead of "old_string not found" — a bare-LF `old` can never
    // match a file that has none (finding B7).
    if text::is_crlf(current) && (old.contains(['\n', '\r']) || new.contains(['\n', '\r'])) {
        return Err(format!(
            "{at}{rel}'s lines end with CRLF — an old_string or new_string holding a line break \
             or a \\r cannot be applied; a line's own text edits exactly, and run_command \
             (`sed -i`, `perl -pi`) or write_file is the road for anything across lines"
        ));
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

    #[test]
    fn arg_string_demands_a_string() {
        assert_eq!(
            arg_string(&json!({"path": "a.rs"}), "path").unwrap(),
            "a.rs"
        );
        assert!(arg_string(&json!({}), "path").is_err());
        // A value that is there with the wrong type is not a missing one: the
        // sentence names the shape the model has to fix (finding A7's class).
        assert_eq!(
            arg_string(&json!({"path": 7}), "path").unwrap_err(),
            "`path` must be a string; got 7"
        );
        assert_eq!(
            arg_string(&json!({"path": null}), "path").unwrap_err(),
            "missing `path`"
        );
    }

    /// An optional string reads `null` as absent and refuses every other wrong
    /// shape: the JSON way of saying nothing is not a licence to default a
    /// value the model sent (finding A7/F12).
    #[test]
    fn arg_string_opt_reads_null_as_absent_and_refuses_the_rest() {
        assert_eq!(arg_string_opt(&json!({}), "base").unwrap(), None);
        assert_eq!(
            arg_string_opt(&json!({"base": null}), "base").unwrap(),
            None
        );
        assert_eq!(
            arg_string_opt(&json!({"base": "main"}), "base").unwrap(),
            Some("main".to_string())
        );
        for wrong in [json!(7), json!(["main"]), json!(true)] {
            assert_eq!(
                arg_string_opt(&json!({ "base": wrong.clone() }), "base").unwrap_err(),
                format!("`base` must be a string; got {wrong}")
            );
        }
    }

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
        }
    }

    /// The batch's own fields are under the same rule as every typed argument:
    /// a value present with the wrong shape is refused, never defaulted — a
    /// `replace_all` of `"true"` must not quietly change one occurrence when
    /// the model asked for all of them (finding A7's class).
    #[test]
    fn a_wrongly_typed_edit_field_is_refused_never_defaulted() {
        let wrong = edits_arg(&json!({
            "edits": [{ "old_string": "a", "new_string": "b", "replace_all": "true" }]
        }))
        .unwrap_err();
        assert_eq!(wrong, "edit 1: `replace_all` must be true or false");

        let wrong = edits_arg(&json!({
            "edits": [{ "old_string": 7, "new_string": "b" }]
        }))
        .unwrap_err();
        assert_eq!(wrong, "edit 1: `old_string` must be a string; got 7");

        let missing = edits_arg(&json!({"edits": [{"new_string": "b"}]})).unwrap_err();
        assert_eq!(missing, "edit 1: missing `old_string`");
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
        let plain = edit_text(file, "old_name", "new_name", false, "f.rs").unwrap_err();
        assert!(plain.contains("2 times"), "{plain}");
        // The same flag reaches the single-pair path, not just a batch entry:
        // the schema's sentence offers it and the refusal above tells the model
        // to set it, so the call that *has* set it must change every occurrence.
        assert_eq!(
            edit_text(file, "old_name", "new_name", true, "f.rs").unwrap(),
            "new_name();\nnew_name(arg);\n"
        );

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
    /// schema table and the dispatcher are two views of one list. Ten names —
    /// the three the six-tool cut deleted are back (a held machine lock and
    /// images are two roads the shell cannot serve), and `search` came with
    /// them.
    #[test]
    fn every_tool_name_round_trips() {
        for tool in ToolName::ALL {
            assert_eq!(ToolName::parse(tool.as_str()), Some(tool));
            assert_eq!(tool.to_string(), tool.as_str());
        }
        assert_eq!(ToolName::parse("edit_file"), Some(ToolName::EditFile));
        assert_eq!(ToolName::parse("read_file"), Some(ToolName::ReadFile));
        assert_eq!(ToolName::parse("write_file"), Some(ToolName::WriteFile));
        assert_eq!(ToolName::parse("list_files"), Some(ToolName::ListFiles));
        assert_eq!(ToolName::parse("search"), Some(ToolName::Search));
        assert_eq!(ToolName::parse("wait"), Some(ToolName::Wait));
        assert_eq!(ToolName::parse("nonsense"), None);
        assert_eq!(ToolName::parse("read"), None);
        assert_eq!(ToolName::parse("grep"), None);
        assert_eq!(ToolName::parse("wait_agents"), None);
        assert_eq!(ToolName::parse("agent_status"), None);
        assert_eq!(ToolName::parse("agent_control"), None);
        assert_eq!(ToolName::parse("command_status"), None);
        assert_eq!(ToolName::parse("command_control"), None);
        assert_eq!(ToolName::parse("wait_commands"), None);

        // The names derive from the enum, in the same order.
        let all: Vec<&str> = ToolName::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(all, TOOL_NAMES.to_vec());
        assert_eq!(TOOL_NAMES.len(), 10);
        let orchestration: Vec<&str> = ToolName::ORCHESTRATION.iter().map(|t| t.as_str()).collect();
        assert_eq!(orchestration, ORCHESTRATION_TOOLS.to_vec());
        // Only delegation bounds a tree. `status`, `control` and `wait` are how
        // an agent manages the *jobs* it started too, so they are not
        // orchestration and a leaf receives them (the subagent prompt names
        // them).
        assert!(ToolName::SpawnAgent.is_orchestration());
        assert!(!ToolName::Status.is_orchestration());
        assert!(!ToolName::Control.is_orchestration());
        assert!(!ToolName::Wait.is_orchestration());
    }

    #[test]
    fn edit_text_replaces_only_an_unambiguous_match() {
        let file = "let a = 1;\nlet b = 2;\n";
        assert_eq!(
            edit_text(file, "let b = 2;", "let b = 3;", false, "f.rs").unwrap(),
            "let a = 1;\nlet b = 3;\n"
        );

        let missing = edit_text(file, "let c", "x", false, "f.rs").unwrap_err();
        assert!(missing.contains("not found"), "{missing}");

        let ambiguous = edit_text(file, "let ", "const ", false, "f.rs").unwrap_err();
        assert!(ambiguous.contains("2 times"), "{ambiguous}");

        let empty = edit_text(file, "", "x", false, "f.rs").unwrap_err();
        assert!(empty.contains("must not be empty"), "{empty}");
    }
}
