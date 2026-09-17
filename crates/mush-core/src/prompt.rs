//! The system prompts and the tool schemas.
//!
//! These things *are* the agent contract. They are kept deliberately small:
//! a model only has to know how to read, edit, run, and delegate — mush
//! handles the rest. Leaf agents (at `MAX_DEPTH`) simply don't receive the
//! orchestration tools, which is how deep chains stay bounded.

use serde_json::{json, Value};

use crate::tools::ToolName;

/// The whole root-agent system prompt. If this grows much, something else went
/// wrong.
pub fn system_prompt(root: &str) -> String {
    format!(
        "You are mush, a coding agent working in the workspace at {root}.\n\
         \n\
         Use the tools to inspect and change files. Rules:\n\
         - Read a file before you edit it.\n\
         - Prefer edit_file for small, surgical changes; use write_file only for new files or full rewrites.\n\
         - Every tool already works inside the workspace: paths are workspace-relative (\"src/main.rs\", \
         not an absolute path) and run_command already runs there with its cwd at the workspace root. \
         Never prefix a command with `cd`.\n\
         - Do the work instead of describing it. Keep replies short.\n\
         - Never touch paths outside the workspace.\n\
         - When the task is done, stop calling tools and reply with a one-sentence summary.\n\
         \n\
         Delegation:\n\
         - spawn_agent(brief, isolated?) starts a subagent that has NO memory of this conversation: \
         the brief must carry every fact, file, and the exact deliverable.\n\
         - An isolated subagent works in its own copy of the repository (its own git worktree and branch); \
         a shared one works in this workspace, so only one of those may run at a time. Decide up front: \
         pass isolated=true for siblings that should run in parallel, or wait_agents for the running one \
         first. (The check can only fail after the brief exists, so decide before writing it.)\n\
         - A subagent runs until it stops calling tools, so a brief is bounded by the work, not a turn \
         count: split by what is independent, not by how long you think it takes.\n\
         - Delegate independent, large, or context-heavy subtasks; do single edits and lookups yourself. \
         Prefer a few big delegations over many small ones.\n\
         - wait_agents blocks until a child finishes and returns its summary; agent_status lists your \
         children; agent_control stops or messages one. wait_agents with all=true waits for every \
         child instead of the first.\n\
         - Ending your turn while children still run is fine: they keep working and you are woken with \
         their \"#N done: summary\" results as each finishes. Use wait_agents when you need a result \
         before you continue.\n\
         \n\
         The machine is shared (CPU, ports, /tmp — a worktree isolates files, nothing else):\n\
         - A long command detaches instead of dying: run_command answers \"[still running — detached as \
         #c2]\" and you are told when it finishes. Pass detach=true for a server; any command that \
         outlives 60s detaches by itself. command_status lists your jobs, wait_commands waits for one, \
         command_control with action \\\"stop\\\" ends one.\n\
         - Pass exclusive=true for anything timing- or port-sensitive (a benchmark, a profiler, a \
         fixed port): it owns the machine while it runs, and a sibling's command is refused with \
         \"#3 holds the machine; retry when it finishes\" — so wait, don't interleave. The lock is \
         between agents; it cannot see the human's own build or an unrelated process."
    )
}

/// The system prompt for a delegated subagent: who it is, the workspace it
/// works in, and the rules. The task itself is not embedded here — it arrives
/// as the first user message, mirroring the root's system+user shape.
/// `root` is what a human reading a log would recognise as this agent's
/// workspace. It is deliberately *not* offered as something to type: an
/// isolated agent's worktree is its cwd already, and an absolute path is
/// refused by every file tool, so naming it in a command is a mistake the
/// prompt should not invite.
pub fn subagent_prompt(root: &str, depth: usize, isolated: bool) -> String {
    let workspace = if isolated {
        format!(
            "You have your own copy of the repository — a git worktree at {root}, on your own branch. It \
             is your workspace root: edit in it, run your tests in it. Your changes stay on your branch \
             until your parent reviews and merges them."
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
         - Every tool already works inside your workspace: paths are workspace-relative (\"src/main.rs\"), \
         never absolute, and run_command already runs there with its cwd at the workspace root. \
         Never prefix a command with `cd`.\n\
         - Never touch paths outside your workspace.\n\
         - Do the work instead of describing it. Keep replies short.\n\
         - Run until you are done: a run ends when you stop calling tools, not at a turn count, so do the \
         whole task.\n\
         - The machine is shared with your siblings (CPU, ports, /tmp): a long command detaches into a \
         job you are told about (command_status, wait_commands, command_control), and run_command with \
         exclusive=true owns the machine for timing- or port-sensitive work.\n\
         - Finish with a concise summary of what you changed."
    )
}

/// The user message that replaces a compacted transcript. The actor builds it
/// and the UI mirrors it, so both continue from exactly the same words.
pub fn compaction_message(summary: &str) -> String {
    format!("Context compacted — continue the task from this summary:\n{summary}")
}

fn tool(name: ToolName, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name.as_str(),
            "description": description,
            "parameters": parameters,
        }
    })
}

/// JSON-Schema tool definitions in the OpenAI `tools` format, keyed by the tool
/// they describe: a schema without an executor cannot be written down.
pub fn tool_schemas() -> Vec<Value> {
    vec![
        tool(
            ToolName::ListFiles,
            "List files, optionally under a subdirectory.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative directory, default the root." }
                }
            }),
        ),
        tool(
            ToolName::ReadFile,
            "Read a text file (may be truncated).",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Workspace-relative file path." } },
                "required": ["path"]
            }),
        ),
        tool(
            ToolName::WriteFile,
            "Create or replace a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string", "description": "The complete new content." }
                },
                "required": ["path", "content"]
            }),
        ),
        tool(
            ToolName::EditFile,
            "Replace text: one old_string/new_string, or `edits` for several replacements at once. A batch lands all-or-nothing in one call, so prefer it for multi-part changes. Ambiguous matches are refused unless replace_all is set.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Exact text, must occur exactly once. Omit when using `edits`." },
                    "new_string": { "type": "string", "description": "Replacement. Omit when using `edits`." },
                    "edits": {
                        "type": "array",
                        "description": "Replacements, applied in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string", "description": "Exact text." },
                                "new_string": { "type": "string", "description": "Replacement." },
                                "replace_all": { "type": "boolean", "description": "Change every occurrence (a rename). Default false refuses an ambiguous match." }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path"]
            }),
        ),
        tool(
            ToolName::RunCommand,
            "Run a shell command in the workspace root — no `cd` needed. A long one becomes a job you are told about.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command (run with sh -c)." },
                    "detach": { "type": "boolean", "description": "Return at once; it keeps running as a job (a server, a watch) and you are told when it finishes. A command that outlives 60s detaches by itself." },
                    "exclusive": { "type": "boolean", "description": "Own the machine while it runs: benchmarks, profiling, a fixed port. Siblings are refused, not interleaved." }
                },
                "required": ["command"]
            }),
        ),
        tool(
            ToolName::SpawnAgent,
            "Delegate a self-contained task to a subagent with no memory here: the brief must carry all context, the deliverable, and the expected output. Only one non-isolated subagent runs at a time. Returns its id.",
            json!({
                "type": "object",
                "properties": {
                    "brief": { "type": "string", "description": "Self-contained task for the subagent." },
                    "isolated": { "type": "boolean", "description": "Own git worktree. Required to run siblings in parallel: only one non-isolated subagent runs at a time. Default false." }
                },
                "required": ["brief"]
            }),
        ),
        tool(
            ToolName::WaitAgents,
            "Block until a child finishes, or the timeout expires; returns its id and summary.",
            json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "integer" }, "description": "Child ids to wait for; empty means all." },
                    "timeout": { "type": "integer", "description": "Seconds to wait; 0 waits forever. Default 600." },
                    "all": { "type": "boolean", "description": "Every result, not the first." }
                }
            }),
        ),
        tool(
            ToolName::AgentStatus,
            "Describe your children: running, finished, failed, or stopped (idle until messaged).",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            ToolName::AgentControl,
            "Stop a child or message it. Stopping is not finishing: it keeps its context and work, and a later message resumes it.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "action": { "type": "string", "enum": ["stop", "message"] },
                    "text": { "type": "string", "description": "Text, when action is message." }
                },
                "required": ["id", "action"]
            }),
        ),
        tool(
            ToolName::CommandStatus,
            "List your jobs: what is running, how long, and the end of what it wrote.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            ToolName::CommandControl,
            "Stop one of your jobs.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "The number in `#c2`." },
                    "action": { "type": "string", "enum": ["stop"] }
                },
                "required": ["id", "action"]
            }),
        ),
        tool(
            ToolName::WaitCommands,
            "Block until a job finishes, or the timeout expires; returns its exit status and the end of its output.",
            json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "integer" }, "description": "Job ids; empty means all of yours." },
                    "timeout": { "type": "integer", "description": "Seconds; 0 waits forever. Default 600." },
                    "all": { "type": "boolean", "description": "Every result, not the first." }
                }
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
            let name = schema["function"]["name"].as_str().unwrap_or("");
            // A schema whose name is not a tool is a bug this filter cannot
            // hide: `schemas_match_the_tool_names` asserts the list matches.
            !ToolName::parse(name).is_some_and(ToolName::is_orchestration)
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
        assert!(
            prompt.contains("your own copy of the repository"),
            "{prompt}"
        );
        assert!(prompt.contains("worktree at /tmp/wt/3"), "{prompt}");
        assert!(!prompt.contains("shared workspace"));
    }

    /// Every tool already runs in the workspace, so a command never needs a
    /// `cd` — and an absolute path is refused by the file tools, so inviting one
    /// is inviting a failure. Both prompts say so, and neither tells the model
    /// to size a brief against a turn budget (a run ends when the model stops
    /// calling tools).
    #[test]
    fn the_prompts_forbid_cd_and_promise_no_turn_budget() {
        for prompt in [
            system_prompt("/tmp/ws"),
            subagent_prompt("/tmp/ws", 1, true),
        ] {
            let lower = prompt.to_lowercase();
            assert!(
                lower.contains("never prefix a command with `cd`"),
                "{prompt}"
            );
            assert!(lower.contains("workspace-relative"), "{prompt}");
            assert!(
                !lower.contains("bounded number of turns") && !lower.contains("turn budget"),
                "no turn budget exists to size a brief against: {prompt}"
            );
        }
        // The tool description the model reads says the same thing.
        let command = tool_schemas()
            .into_iter()
            .find(|schema| schema["function"]["name"] == "run_command")
            .expect("run_command has a schema");
        let description = command["function"]["description"].as_str().unwrap();
        assert!(description.contains("no `cd` needed"), "{description}");
    }

    /// The prompt says what the tools promise: that a long command detaches
    /// rather than being killed, and that `exclusive` buys the whole machine.
    /// A model that does not know this writes `timeout`-shaped commentary in
    /// every summary instead of using the tools.
    #[test]
    fn the_prompts_say_the_machine_is_shared() {
        let root = system_prompt("/tmp/ws");
        assert!(root.contains("The machine is shared"), "{root}");
        assert!(root.contains("exclusive=true"), "{root}");
        assert!(
            root.contains("holds the machine"),
            "the refusal a sibling reads is quoted, so the model recognises it: {root}"
        );
        assert!(root.contains("worktree isolates files"), "{root}");
        // A subagent gets the same facts in one bullet.
        let child = subagent_prompt("/tmp/ws", 1, true);
        assert!(child.contains("machine is shared"), "{child}");
        assert!(child.contains("exclusive=true"), "{child}");
    }

    #[test]
    fn compaction_message_carries_the_summary() {
        let message = compaction_message("did the thing");
        assert!(message.contains("Context compacted"));
        assert!(message.trim_end().ends_with("did the thing"));
    }
}
