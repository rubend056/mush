//! The system prompts and the tool schemas.
//!
//! These things *are* the agent contract. They are kept deliberately small: a
//! model only has to know how to edit, run, and delegate — the shell does the
//! reading and writing, and mush handles the rest. Leaf agents (at `MAX_DEPTH`)
//! simply don't receive `spawn_agent`, which is how deep chains stay bounded.
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
- Work inside the workspace: paths are workspace-relative (\"src/main.rs\", not an absolute \
path), and a command runs with its cwd at the workspace root. Never touch paths outside the \
workspace.\n\
- Use `run_command` to read/write/rewrite files (`sed -n '1,200p' file`, `rg pattern`) \
- To edit use `edit_file` with the exact text it replaces, and refuses a match that is missing or not unique.\n\
- When you are done finish with a concise summary of what you did.
- Don't forget to have fun :)";

/// What is true of the machine for every agent, root or leaf. One home, read by
/// both prompts; the child pays for every word here on every request.
const MACHINE: &str = "\
The machine is shared (CPU, ports, /tmp — a worktree isolates files, nothing else):\n\
- A long command detaches into a job instead of dying: run_command answers \"[still running — detached \
as #c2]\" and the command keeps its own process group. The tools' schemas say what starts one, and \
what reads, waits on or stops it.\n\
- exclusive=true owns the machine for timing- or port-sensitive work (a benchmark, a profiler, a fixed \
port): a sibling's command queues behind it and is refused if the lock outlasts that (`#N holds \
the machine`) — and then a subagent's `wait` is the road back: it blocks until the machine is free \
(however many waits that takes — the schema says what each one hands over), and one more call runs. \
Never retry a refused call in a loop. The root is exempt from a lock it did not take: it works \
beside the holder, told when it did, only its own exclusive claim is refused, and its `wait` does \
not block on the lock.";

/// The delegation policy, for every agent that has the orchestration tools:
/// the root and any subagent below `MAX_DEPTH`. It used to live only in the
/// root's prompt, so a depth-1 orchestrator could spawn with no idea its brief
/// had to be self-contained (audit row 6).
const DELEGATION: &str = "\
Delegation:\n\
- spawn_agent(brief, title, base?) starts a subagent with no memory of this conversation: the brief \
must carry every fact, file, and the exact deliverable; title is three words naming it in the tree.\n\
- base gives the child its own worktree and branch forked from that ref, so siblings with bases run in \
parallel; without one the child works in this workspace, and only one such child may run at a time. \
Decide up front, or wait for the running one first. (The check can only fail after the brief \
exists, so decide before writing it.)\n\
- A subagent runs until it stops calling tools, so a brief is bounded by the work, not a turn count: \
split by what is independent, not by how long you think it takes.\n\
- Delegate independent, large, or context-heavy subtasks; do single edits and lookups yourself. Prefer \
a few big delegations over many small ones.\n\
- Ending your turn while children still run is fine: they keep working and a finish wakes you with its \
\"#N done: summary\". wait is optional — use it when you want the results now (its schema says what it \
hands over).";

/// The opening a blank brief leaves: the child's first user message and the
/// transcript's first line, so the model and the human read the same words.
pub const BEGIN_TASK: &str = "Begin the task now.";

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
/// isolated agent's worktree is its cwd already, and `edit_file` refuses an
/// absolute path, so naming it in a command is a mistake the prompt should not
/// invite.
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

/// JSON-Schema tool definitions in the OpenAI `tools` format, keyed by the tool
/// they describe: a schema without an executor cannot be written down.
pub fn tool_schemas() -> Vec<Value> {
    vec![
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
            "Run a shell command in the workspace root. The result is capped to fit the context window; a capped result says so — rerun it narrower (rg, head, a smaller path) to see the rest.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Sh command." },
                    "detach": { "type": "boolean", "description": "Return at once; it keeps running as a job (a server, a watch) and you are told when it finishes. A command that outlives 60s detaches by itself; a job is killed after 4h." },
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
                    "title": { "type": "string", "description": "A 3 word description of this agent's brief." },
                    "base": { "type": "string", "description": "Branch, tag or commit for the child's own worktree and branch. Without one the child shares this workspace." }
                },
                "required": ["brief", "title"]
            }),
        ),
        tool(
            ToolName::Status,
            "List your children and your jobs in one place: each child's state and title or branch, each \
             job's state, age and command. A listing, not a delivery \u{2014} wait hands results over.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            ToolName::Control,
            "Stop or message one thing you own: a child agent (`2`) or a job (`c2`), as status names it. \
             `message` is agent-only \u{2014} it steers a child, resuming one at rest, and needs `text`. \
             Stopping a child is not finishing: it keeps its context and work.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The target as status lists it: `2` for child agent #2, `c2` for job #c2." },
                    "action": { "type": "string", "enum": ["stop", "message"] },
                    "text": { "type": "string", "description": "The message, when action is message. Agent-only \u{2014} a job cannot be messaged." }
                },
                "required": ["id", "action"]
            }),
        ),
        tool(
            ToolName::Wait,
            "Block until nothing you own is still running \u{2014} every child and every job \u{2014} then \
             answer with one digest: a result you have not read comes in full, an already-read one as a \
             line. A result nobody has read is handed over first, whatever the machine is doing; \
             otherwise a subagent's wait also waits while another agent holds the machine, which is how \
             a command refused with `#N holds the machine` is retried. Returns at once when you have \
             nothing to wait for \u{2014} no children, no jobs, no other agent holding the machine; \
             gives up after 10 minutes and names what is still running; a message to you ends the wait \
             early and says so.",
            json!({ "type": "object", "properties": {} }),
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

    /// Depth is bounded by what a leaf can see: the one delegation tool is gone
    /// and the workspace and job tools stay.
    #[test]
    fn a_leaf_keeps_the_workspace_and_job_tools() {
        let leaf = leaf_tool_schemas();
        let names: Vec<&str> = leaf
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 5);
        for kept in ["edit_file", "run_command", "status", "control", "wait"] {
            assert!(names.contains(&kept), "a leaf loses {kept}");
        }
        assert!(!names.contains(&"spawn_agent"), "{names:?}");
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

    /// `wait` takes no arguments: it is "everything I own has finished", and a
    /// parameter is exactly the reasoning (ids? all? timeout?) H15 found a
    /// model getting wrong. The two parameterless listings take none either.
    #[test]
    fn the_control_and_wait_schemas_are_parameterless_where_they_promise() {
        let schema = |tool: &str| {
            tool_schemas()
                .into_iter()
                .find(|schema| schema["function"]["name"] == tool)
                .expect("the tool has a schema")["function"]["parameters"]
                .clone()
        };
        for tool in ["wait", "status"] {
            assert_eq!(
                schema(tool)["properties"].as_object().map(|p| p.len()),
                Some(0),
                "{tool} takes no arguments"
            );
        }
        // `control` names its target as `status` prints it, and `message` says
        // it is the agent-only action.
        let control = schema("control");
        assert_eq!(control["properties"]["id"]["type"], "string");
        assert_eq!(control["properties"]["action"]["enum"][1], "message");
        assert!(control["properties"]["text"]["description"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("agent-only"));
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
        assert!(prompt.contains(RULES));
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

    /// The lock refusal has a road back that is not a retry — and it is the
    /// machine block that owns it (`wait` spans the hold) and `wait`'s schema
    /// that owns what the call covers. The two cannot disagree: the refusal in
    /// `jobs.rs` sends the model to a wait that its own schema describes.
    ///
    /// The wait is scoped to a subagent, because the root's is not this wait:
    /// the root works beside a holder, so a blocking wait for a lock it never
    /// took is exactly what its exemption spares it.
    #[test]
    fn the_prompts_say_how_a_refused_command_gets_retried() {
        let root = system_prompt("/tmp/ws");
        assert!(
            root.contains("a subagent's `wait` is the road back")
                && root.contains("it blocks until the machine is free"),
            "the road back is the machine block's fact: {root}"
        );
        assert!(
            root.contains("Never retry a refused call in a loop"),
            "{root}"
        );
        // The exemption is said where the root reads the lock's rule, so the
        // orchestrator does not sit out a lock it never took — and the wait is
        // scoped in the same breath, so it does not read the subagent's wait as
        // its own.
        assert!(root.contains("The root is exempt"), "{root}");
        assert!(
            root.contains("its `wait` does not block on the lock"),
            "{root}"
        );
        // The block must not promise that *one* wait frees the lock: a wait
        // with an unread result to hand over comes back first, holding the
        // lock — what the call hands over, and in what order, is the schema's
        // fact, and the block points at it rather than spelling it twice.
        assert!(
            root.contains(
                "however many waits that takes — the schema says what each one hands over"
            ),
            "{root}"
        );
        assert!(
            subagent_prompt("/tmp/ws", 1, true, true)
                .contains("a subagent's `wait` is the road back"),
            "a subagent is refused, so it is the one that needs the wait"
        );

        // And `wait`'s own schema says what the call now covers: the machine,
        // its 10-minute cap, and the early release a message causes (the
        // three facts audit item 2 found the context did not state).
        let wait = tool_schemas()
            .into_iter()
            .find(|schema| schema["function"]["name"] == "wait")
            .expect("wait has a schema");
        let description = wait["function"]["description"].as_str().unwrap();
        assert!(
            description.contains("another agent holds the machine"),
            "{description}"
        );
        assert!(
            description.contains("handed over first"),
            "an unread result outranks the machine wait: {description}"
        );
        assert!(description.contains("10 minutes"), "{description}");
        assert!(description.contains("ends the wait early"), "{description}");
        // And what "nothing to wait for" means, since the immediate return is
        // the one clause a locked-out agent could read as "the wait the refusal
        // named comes straight back".
        assert!(
            description.contains("no children, no jobs, no other agent holding the machine"),
            "{description}"
        );
    }

    #[test]
    fn compaction_message_carries_the_summary() {
        let message = compaction_message("did the thing");
        assert!(message.contains("Context compacted"));
        assert!(message.trim_end().ends_with("did the thing"));
    }
}
