//! The system prompts and the tool schemas.
//!
//! These things *are* the agent contract. They are kept deliberately small:
//! a model only has to know how to read, edit, run, and delegate — mush
//! handles the rest. Leaf agents (at `MAX_DEPTH`) simply don't receive the
//! orchestration tools, which is how deep chains stay bounded.

use serde_json::{json, Value};

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
         - spawn_agent(brief, isolated?) delegates to a subagent that starts with NO memory of this conversation; \
         include every fact, file, and the exact deliverable in the brief.\n\
         - Delegate independent, large, or context-heavy subtasks; do single edits and lookups yourself.\n\
         - wait_agents blocks until a child finishes and returns its summary. agent_control stops or messages a child; \
         agent_status lists them.\n\
         - Ending your turn while children still run is allowed: they keep working, and you are woken with their \
         \"#N done: summary\" results as they finish. mush shows \"waiting on N subagents\" until then. Prefer \
         wait_agents for children you depend on before reporting done.\n\
         - Prefer a few big delegations over many small ones."
    )
}

/// The system prompt for a delegated subagent: amnesia by design, plus the
/// parent's self-contained brief.
pub fn subagent_prompt(root: &str, depth: usize, brief: &str) -> String {
    format!(
        "You are a mush subagent at depth {depth} working for a parent agent in the workspace at {root}.\n\
         You have no memory of your parent's conversation; the entire task is below. Same tools as always.\n\
         Do the work, keep replies short, and finish with a concise summary of what you changed.\n\
         \n\
         PARENT TASK:\n\
         {brief}"
    )
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
            "Delegate a self-contained task to a subagent. The subagent starts with no memory of this conversation, so the brief must contain all context, the exact deliverable, and the expected output. Returns the new agent's id.",
            json!({
                "type": "object",
                "properties": {
                    "brief": { "type": "string", "description": "Self-contained task for the subagent." },
                    "isolated": { "type": "boolean", "description": "Run the subagent in its own git worktree so parallel agents never collide. Default false." }
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
            "Describe your child agents: id, running or finished with summary.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "agent_control",
            "Stop a child agent, or message it (a nudge appears in its conversation as a user message).",
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
    let orchestration = ["spawn_agent", "wait_agents", "agent_status", "agent_control"];
    tool_schemas()
        .into_iter()
        .filter(|schema| {
            !orchestration.contains(&schema["function"]["name"].as_str().unwrap_or(""))
        })
        .collect()
}

/// Names of the workspace tools, for validation and display.
pub fn tool_names() -> [&'static str; 9] {
    [
        "list_files",
        "read_file",
        "write_file",
        "edit_file",
        "run_command",
        "spawn_agent",
        "wait_agents",
        "agent_status",
        "agent_control",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn subagent_prompt_embeds_the_brief() {
        let prompt = subagent_prompt("/tmp/x", 2, "port the parser");
        assert!(prompt.contains("depth 2"));
        assert!(prompt.contains("port the parser"));
    }
}
