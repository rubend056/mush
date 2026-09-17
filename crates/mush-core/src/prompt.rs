//! The system prompts and the tool schemas.
//!
//! These things *are* the agent contract. They are kept deliberately small:
//! a model only has to know how to read, edit, run, and delegate — mush
//! handles the rest. Leaf agents (at `MAX_DEPTH`) simply don't receive the
//! orchestration tools, which is how deep chains stay bounded.

use serde_json::{json, Value};

use crate::tools::ORCHESTRATION_TOOLS;

/// The whole root-agent system prompt. If this grows much, something else went
/// wrong.
pub fn system_prompt(root: &str) -> String {
    format!(
        "You are mush, a coding agent working in the workspace at {root}.\n\
         \n\
         Use the tools to inspect and change files. Rules:\n\
         - Read a file before you edit it.\n\
         - Prefer edit_file for small, surgical changes; use write_file only for new files or full rewrites.\n\
         - Do the work instead of describing it. Keep replies short.\n\
         - Never touch paths outside the workspace.\n\
         - When the task is done, stop calling tools and reply with a one-sentence summary.\n\
         \n\
         Delegation:\n\
         - spawn_agent(brief, isolated?) starts a subagent that has NO memory of this conversation: \
         the brief must carry every fact, file, and the exact deliverable.\n\
         - A subagent gets a bounded number of turns and the spawn result names it. Size the brief \
         so the work fits in that budget: a brief too big for its budget ends mid-task, not early.\n\
         - Only one non-isolated subagent may run at a time in a shared workspace. Decide up front: \
         pass isolated=true for siblings that should run in parallel, or wait_agents for the running \
         one first. (The check can only fail after the brief exists, so decide before writing it.)\n\
         - Delegate independent, large, or context-heavy subtasks; do single edits and lookups yourself. \
         Prefer a few big delegations over many small ones.\n\
         - wait_agents blocks until a child finishes and returns its summary; agent_status lists your \
         children; agent_control stops or messages one.\n\
         - Ending your turn while children still run is fine: they keep working and you are woken with \
         their \"#N done: summary\" results as each finishes. Use wait_agents when you need a result \
         before you continue."
    )
}

/// The system prompt for a delegated subagent: who it is, the workspace it
/// works in, and the rules. The task itself is not embedded here — it arrives
/// as the first user message, mirroring the root's system+user shape.
pub fn subagent_prompt(root: &str, depth: usize, isolated: bool) -> String {
    let workspace = if isolated {
        format!(
            "Your workspace is an isolated git worktree at {root}; your changes stay on its branch \
             until they are reviewed and merged."
        )
    } else {
        format!("Your workspace is the shared workspace at {root}.")
    };
    format!(
        "You are a mush subagent at depth {depth}, working for a parent agent.\n\
         You have no memory of your parent's conversation; your task arrives as the next user message.\n\
         {workspace}\n\
         Rules:\n\
         - Read a file before you edit it.\n\
         - Prefer edit_file for small, surgical changes; use write_file only for new files or full rewrites.\n\
         - Never touch paths outside your workspace.\n\
         - Do the work instead of describing it. Keep replies short.\n\
         - Finish with a concise summary of what you changed."
    )
}

/// The user message that replaces a compacted transcript. The actor builds it
/// and the UI mirrors it, so both continue from exactly the same words.
pub fn compaction_message(summary: &str) -> String {
    format!("Context compacted — continue the task from this summary:\n{summary}")
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

/// JSON-Schema tool definitions in the OpenAI `tools` format.
pub fn tool_schemas() -> Vec<Value> {
    vec![
        tool(
            "list_files",
            "List files in the workspace, optionally under a subdirectory.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative directory. Defaults to the root." }
                }
            }),
        ),
        tool(
            "read_file",
            "Read a text file. The result may be truncated for very large files.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Workspace-relative file path." } },
                "required": ["path"]
            }),
        ),
        tool(
            "write_file",
            "Create or fully replace a file with the given content.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string", "description": "The complete new file content." }
                },
                "required": ["path", "content"]
            }),
        ),
        tool(
            "edit_file",
            "Replace one exact occurrence of old_string with new_string in a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Exact text to find. Must occur exactly once." },
                    "new_string": { "type": "string", "description": "Replacement text." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        tool(
            "run_command",
            "Run a shell command in the workspace root and return its output.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command (run with sh -c)." }
                },
                "required": ["command"]
            }),
        ),
        tool(
            "spawn_agent",
            "Delegate a self-contained task to a subagent. It has no memory of this conversation, so the brief must carry all context, the exact deliverable, and the expected output. Only one non-isolated subagent may run at a time. Returns its id.",
            json!({
                "type": "object",
                "properties": {
                    "brief": { "type": "string", "description": "Self-contained task for the subagent." },
                    "isolated": { "type": "boolean", "description": "Run in its own git worktree (.mush/wt/<id>, branch mush/<id>). Required to run siblings in parallel: a non-isolated subagent shares this workspace and only one may run at a time. Default false." }
                },
                "required": ["brief"]
            }),
        ),
        tool(
            "wait_agents",
            "Block until any of your child agents finishes (or the timeout expires) and return its id and final summary.",
            json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "integer" }, "description": "Child agent ids to wait for; empty means all children." },
                    "timeout": { "type": "integer", "description": "Seconds to wait; 0 waits forever. Default 600." }
                }
            }),
        ),
        tool(
            "agent_status",
            "Describe your children: running, finished with a summary, failed, or stopped (no result; idle until you message it again).",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "agent_control",
            "Stop a child, or message it (a nudge appears in its conversation as a user message). Stopping is not finishing: the child keeps its context and its work in progress, and a later message resumes it.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "action": { "type": "string", "enum": ["stop", "message"] },
                    "text": { "type": "string", "description": "Message content when action is message." }
                },
                "required": ["id", "action"]
            }),
        ),
    ]
}

/// The tool set for leaf agents: everything, minus the orchestration tools,
/// so depth is bounded by what the model can see.
pub fn leaf_tool_schemas() -> Vec<Value> {
    tool_schemas()
        .into_iter()
        .filter(|schema| {
            !ORCHESTRATION_TOOLS.contains(&schema["function"]["name"].as_str().unwrap_or(""))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TOOL_NAMES;

    /// The schemas and the executors are two lists that must agree; this is
    /// the test that keeps them in step (and in order).
    #[test]
    fn schemas_match_the_tool_names() {
        let schemas = tool_schemas();
        let names: Vec<String> = schemas
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, TOOL_NAMES.to_vec());

        // A leaf keeps exactly the workspace tools.
        let leaf = leaf_tool_schemas();
        let leaf_names: Vec<String> = leaf
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(leaf_names, TOOL_NAMES[..5].to_vec());
    }

    #[test]
    fn leaf_schemas_omit_orchestration() {
        let leaf = leaf_tool_schemas();
        let names: Vec<&str> = leaf
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 5);
        assert!(names.contains(&"edit_file"));
        assert!(!names.contains(&"spawn_agent"));
        assert!(!names.contains(&"wait_agents"));
    }

    /// The root's schemas must fit the tokens `Config::history_budget`
    /// reserves for them, or every request quietly overshoots the window.
    #[test]
    fn schemas_fit_the_budget_reserve() {
        let bytes = serde_json::to_string(&tool_schemas()).unwrap().len();
        assert!(
            bytes <= crate::config::SCHEMA_TOKENS * 3,
            "root schemas grew to {bytes} bytes — raise Config::SCHEMA_TOKENS"
        );
    }

    #[test]
    fn subagent_prompt_keeps_role_and_depth_out_of_the_task() {
        let prompt = subagent_prompt("/tmp/x", 2, false);
        assert!(prompt.contains("depth 2"));
        assert!(prompt.contains("mush subagent"));
        // The task is a user message, never part of the system prompt.
        assert!(!prompt.contains("PARENT TASK"));
        assert!(!prompt.contains("port the parser"));
        // Subagents get the same workspace rules as the root.
        assert!(prompt.contains("Read a file before you edit it"));
        assert!(prompt.contains("Never touch paths outside"));
        assert!(prompt.contains("shared workspace at /tmp/x"));
    }

    #[test]
    fn subagent_prompt_names_an_isolated_worktree() {
        let prompt = subagent_prompt("/tmp/wt/3", 1, true);
        assert!(prompt.contains("isolated git worktree at /tmp/wt/3"));
        assert!(!prompt.contains("shared workspace"));
    }

    #[test]
    fn compaction_message_carries_the_summary() {
        let message = compaction_message("did the thing");
        assert!(message.contains("Context compacted"));
        assert!(message.trim_end().ends_with("did the thing"));
    }
}
