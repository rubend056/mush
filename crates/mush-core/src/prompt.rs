//! The system prompts and the tool schemas.
//!
//! These things *are* the agent contract. They are kept deliberately small: a
//! model only has to know how to read, edit, run, and delegate — mush handles
//! the rest. Leaf agents (at `MAX_DEPTH`) simply don't receive the
//! orchestration tools, which is how deep chains stay bounded.
//!
//! Ownership, so nothing is said twice: the prompts own *how to work* (the
//! rules, the delegation policy, what the machine is like); each schema owns
//! *the call* (its arguments, their defaults, what comes back). A rule with two
//! homes is a rule that drifts — `docs/findings.md` §8's class.

use serde_json::{json, Value};

use crate::tools::ToolName;

/// How to work, for every agent at every depth. One home: the root and a
/// subagent used to spell these rules twice, and the copies had drifted.
const RULES: &str = "\
Rules:\n\
- Every tool already works inside the workspace: paths are workspace-relative (\"src/main.rs\", not an \
absolute path) and run_command/edit/read already runs there with its cwd at the workspace root.\n\
- Never touch paths outside the workspace.\n\
- When you are done finish with a concise summary of what you did.";

/// What is true of the machine for every agent, root or leaf. One home, read by
/// both prompts; the child pays for every word here on every request.
const MACHINE: &str = "\
The machine is shared (CPU, ports, /tmp — a worktree isolates files, nothing else):\n\
- A long command detaches into a job instead of dying: run_command answers \"[still running — detached \
as #c2]\", detach=true asks for one at once, and any command that outlives 60s does it by itself. \
command_status lists your jobs, wait_commands waits for one, command_control stops one.\n\
- exclusive=true owns the machine for timing- or port-sensitive work (a benchmark, a profiler, a fixed \
port): a sibling's command queues behind it and is refused if the lock outlasts the wait (`#N holds the \
machine`) — do not retry in a loop.";

/// The delegation policy, for every agent that has the orchestration tools:
/// the root and any subagent below `MAX_DEPTH`. It used to live only in the
/// root's prompt, so a depth-1 orchestrator could spawn with no idea its brief
/// had to be self-contained (audit row 6).
const DELEGATION: &str = "\
Delegation:\n\
- spawn_agent(brief, isolated?, base?) starts a subagent with no memory of this conversation: the brief \
must carry every fact, file, and the exact deliverable.\n\
- An isolated subagent works in its own copy of the repository (its own git worktree and branch); a \
shared one works in this workspace, so only one of those may run at a time. Decide up front: pass \
isolated=true for siblings that should run in parallel, or wait_agents for the running one first. (The \
check can only fail after the brief exists, so decide before writing it.)\n\
- A subagent runs until it stops calling tools, so a brief is bounded by the work, not a turn count: \
split by what is independent, not by how long you think it takes.\n\
- Delegate independent, large, or context-heavy subtasks; do single edits and lookups yourself. Prefer \
a few big delegations over many small ones.\n\
- wait_agents blocks until a child finishes: no ids means the *first* finish, all=true every child; its \
answer hands over an unread result's full summary, and an already-read one comes back as a digest. \
Ending your turn while children still run is fine: they keep working and you are woken with their \
\"#N done: summary\" results as each finishes.";

/// The whole root-agent system prompt. If this grows much, something else went
/// wrong.
pub fn system_prompt(root: &str) -> String {
    format!(
        "You are mush, a coding agent working in the workspace at {root}.\n\
         \n\
         {RULES}\n\
         \n\
         {DELEGATION}\n\
         \n\
         {MACHINE}"
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
///
/// `delegates` is whether this agent gets the orchestration tools (depth below
/// `MAX_DEPTH`): the policy reads exactly when the tools are there (audit row
/// 6).
pub fn subagent_prompt(root: &str, depth: usize, isolated: bool, delegates: bool) -> String {
    let workspace = if isolated {
        format!(
            "You work at `{root}`, a worktree of your own branch. It \
             is your workspace root."
        )
    } else {
        format!("Your workspace is `{root}`.")
    };
    let policy = if delegates {
        format!("\n\n{DELEGATION}")
    } else {
        String::new()
    };
    format!(
        "You are a mush subagent at depth {depth}, working for a parent agent.\n\
         \n\
         {workspace}\n\
         \n\
         {RULES}{policy}\n\
         \n\
         {MACHINE}"
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

/// The three arguments both waits take, written once: they promise the same
/// release rule and the same defaults. Only `ids` may differ.
fn wait_parameters(ids: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "ids": { "type": "array", "items": { "type": "integer" }, "description": ids },
            "timeout": { "type": "integer", "description": "Seconds to wait; 0 waits forever. Default 600." },
            "all": { "type": "boolean", "description": "Every result, not just the first." }
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
            "Read a file.",
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
            "Run a shell command in the workspace root.",
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
            "Delegate a self-contained task to a subagent. Returns its id.",
            json!({
                "type": "object",
                "properties": {
                    "brief": { "type": "string" },
                    "isolated": { "type": "boolean", "description": "Own git worktree. Default false." },
                    "base": { "type": "string", "description": "Branch, tag or commit an isolated worktree forks from. Default: this agent's branch. Requires isolated=true." }
                },
                "required": ["brief"]
            }),
        ),
        tool(
            ToolName::WaitAgents,
            "Block until a child finishes. Returns an unread result's summary; an already-read one as \
             a marked digest.",
            wait_parameters("Child ids. Empty means any child: the first finish."),
        ),
        tool(
            ToolName::AgentStatus,
            "List your children and their state: a one-line digest per result, unread ones marked. A \
             listing, not a delivery \u{2014} wait_agents hands summaries over.",
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
            wait_parameters("Job ids; empty means all of yours."),
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
    use crate::tools::{ORCHESTRATION_TOOLS, TOOL_NAMES};

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

        // A leaf keeps the workspace tools — the job tools among them, since a
        // leaf may run a build in the background while it edits — and none of
        // the delegation tools.
        let leaf = leaf_tool_schemas();
        let leaf_names: Vec<String> = leaf
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap().to_string())
            .collect();
        let workspace: Vec<&str> = TOOL_NAMES
            .iter()
            .copied()
            .filter(|name| !ORCHESTRATION_TOOLS.contains(name))
            .collect();
        assert_eq!(leaf_names, workspace);
    }

    /// Depth is bounded by what a leaf can see: the delegation tools are gone
    /// and the job tools stay.
    #[test]
    fn a_leaf_keeps_the_workspace_and_job_tools() {
        let leaf = leaf_tool_schemas();
        let names: Vec<&str> = leaf
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 8);
        for kept in [
            "edit_file",
            "command_status",
            "command_control",
            "wait_commands",
        ] {
            assert!(names.contains(&kept), "a leaf loses {kept}");
        }
    }

    /// The root's schemas must fit the tokens `Config::history_budget`
    /// reserves for them, or every request quietly overshoots the window. The
    /// measurement is printed, so a shrink is as visible as a growth.
    #[test]
    fn schemas_fit_the_budget_reserve() {
        let bytes = serde_json::to_string(&tool_schemas()).unwrap().len();
        eprintln!("root schemas: {bytes} bytes");
        assert!(
            bytes <= crate::config::SCHEMA_TOKENS * 3,
            "root schemas grew to {bytes} bytes — raise Config::SCHEMA_TOKENS"
        );
    }

    /// The two waits take the same three arguments: `wait_parameters` writes
    /// them once, and only `ids` may differ.
    #[test]
    fn the_two_wait_tools_take_the_same_arguments() {
        let prop = |tool: &str, key: &str| {
            tool_schemas()
                .into_iter()
                .find(|schema| schema["function"]["name"] == tool)
                .expect("the wait tool has a schema")["function"]["parameters"]["properties"][key]
                .clone()
        };
        for key in ["timeout", "all"] {
            assert_eq!(
                prop("wait_agents", key),
                prop("wait_commands", key),
                "{key} is one promise, not two"
            );
        }
        assert_ne!(prop("wait_agents", "ids"), prop("wait_commands", "ids"));
    }

    #[test]
    fn subagent_prompt_keeps_role_and_depth_out_of_the_task() {
        let prompt = subagent_prompt("/tmp/x", 2, false, true);
        assert!(prompt.contains("depth 2"));
        assert!(prompt.contains("mush subagent"));
        // The task is a user message, never part of the system prompt.
        assert!(!prompt.contains("PARENT TASK"));
        assert!(!prompt.contains("port the parser"));
        // Subagents get the same workspace rules as the root, from one block.
        assert!(prompt.contains("Rules:"));
        assert!(prompt.contains("Every tool already works inside the workspace"));
        assert!(prompt.contains("`/tmp/x`"));
    }

    /// The delegation policy is readable exactly by the agents that have the
    /// orchestration tools: a depth-1 subagent can spawn, so it must know its
    /// brief carries everything; a leaf cannot, so it is not told to (audit row
    /// 6).
    #[test]
    fn only_a_delegating_subagent_reads_the_delegation_policy() {
        assert!(subagent_prompt("/tmp/ws", 1, false, true).contains("Delegation:"));
        assert!(!subagent_prompt("/tmp/ws", 3, false, false).contains("Delegation:"));
    }

    #[test]
    fn subagent_prompt_names_an_isolated_worktree() {
        let prompt = subagent_prompt("/tmp/wt/3", 1, true, false);
        assert!(prompt.contains("worktree of your own branch"), "{prompt}");
        assert!(prompt.contains("`/tmp/wt/3`"), "{prompt}");
        // A shared child gets the shared-workspace sentence instead.
        let shared = subagent_prompt("/tmp/wt/3", 1, false, false);
        assert!(!shared.contains("worktree of your own branch"), "{shared}");
    }

    /// Every tool already runs in the workspace with its cwd at the root, so
    /// paths are workspace-relative and nothing invites an absolute one. The
    /// rule lives in the shared prompt block and is not restated in
    /// `run_command`'s schema: one home, so the two cannot drift (the schema
    /// says what the call is, the prompt how to work).
    #[test]
    fn the_prompts_keep_commands_in_the_workspace_root() {
        for prompt in [
            system_prompt("/tmp/ws"),
            subagent_prompt("/tmp/ws", 1, true, true),
            subagent_prompt("/tmp/ws", 3, false, false),
        ] {
            let lower = prompt.to_lowercase();
            assert!(lower.contains("workspace-relative"), "{prompt}");
            assert!(lower.contains("cwd at the workspace root"), "{prompt}");
            assert!(lower.contains("never touch paths outside"), "{prompt}");
            assert!(
                !lower.contains("bounded number of turns") && !lower.contains("turn budget"),
                "no turn budget exists to size a brief against: {prompt}"
            );
        }
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
        // A subagent gets the same facts, from the same block.
        let child = subagent_prompt("/tmp/ws", 1, true, false);
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
