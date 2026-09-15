//! Agent actors.
//!
//! Each agent is its own thread owning one transcript; parents and children
//! talk *directly* through mailboxes, while the UI observes everything through
//! id-tagged events. This is what makes parallel subagent chains possible:
//! an orchestrator can spawn N children, wait for whichever finishes first,
//! nudge or stop individual agents, and descendants can spawn their own.
//!
//! File access still honors the single-owner rule: agents working in the main
//! workspace round-trip file tools through the UI thread (live buffers), while
//! isolated agents edit their own git worktree directly on disk.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use serde_json::{json, Value};

use mush_core::message::{ChatRequest, ChatResponse};
use mush_core::{prompt, Config, Message, Workspace, CMD_CAP, CMD_TIMEOUT_SECS, READ_CAP};

use crate::app::{Msg, ToolCallRequest};
use crate::http;

/// Safety valve: how many model turns one run may take.
const MAX_TURNS: usize = 24;
/// Approximate context budget before old turns are dropped.
const HISTORY_BUDGET: usize = 60_000;
/// A tool that never comes back must not wedge the agent forever.
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// How deep subagent chains may go (0 = root agent only).
pub const MAX_DEPTH: usize = 3;
/// Hard ceiling on simultaneously running agents across the whole tree.
const MAX_AGENTS: u64 = 16;
/// Default `wait_agents` timeout in seconds; 0 means forever.
const WAIT_TIMEOUT_SECS: u64 = 600;

/// Commands sent into an agent actor's mailbox.
pub enum AgentMsg {
    /// Adopt these messages and run. The actor keeps the transcript, so later
    /// nudges continue the same conversation.
    Run(Vec<Message>),
    /// Append a user message; if idle, run again.
    Nudge(String),
    /// Cooperative cancel of the current run (idle actors exit on this).
    Stop,
    /// A child sent this parent its final summary.
    ChildDone { id: u64, summary: String },
}

/// Events streamed to the UI thread, tagged with the emitting agent's id.
#[derive(Debug)]
pub enum AgentEvent {
    /// A child actor now exists (sent by its parent), with the handle to steer it.
    Spawned {
        child: u64,
        parent: u64,
        brief: String,
        depth: usize,
        branch: Option<String>,
        cmd: Sender<AgentMsg>,
    },
    Status(String),
    Message(Message),
    Resync,
    Done,
    Error(String),
}

/// Shared by every actor: config, the UI channel, and the budgets.
pub struct AgentCtx {
    /// Shared so a runtime `/provider` / `/url` / `/model` / `/key` applies to
    /// every agent immediately.
    pub cfg: Arc<Mutex<Config>>,
    pub tx: Sender<Msg>,
    /// The main workspace root; agents whose root differs are isolated.
    pub root: PathBuf,
    pub ids: Arc<AtomicU64>,
    pub live: Arc<AtomicU64>,
}

/// Per-actor state that survives across runs (children, summaries, branch).
struct ActorState {
    my_tx: Sender<AgentMsg>,
    branch: Option<String>,
    children: HashMap<u64, Sender<AgentMsg>>,
    running: HashSet<u64>,
    completed: HashMap<u64, String>,
    /// Completions already handed to the model (via wait_agents or delivery).
    delivered: HashSet<u64>,
}

/// Start the root actor. Returns its mailbox and the shared config.
pub fn spawn(cfg: Config, tx: Sender<Msg>, root: PathBuf) -> (Sender<AgentMsg>, Arc<Mutex<Config>>) {
    let shared = Arc::new(Mutex::new(cfg));
    let ctx = Arc::new(AgentCtx {
        cfg: shared.clone(),
        tx,
        root,
        // Root agent is id 0; children start at 1.
        ids: Arc::new(AtomicU64::new(1)),
        live: Arc::new(AtomicU64::new(0)),
    });
    let ws = Workspace::new(&ctx.root).expect("workspace root must exist");
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    let (done, _) = bounded::<String>(1);
    // The root has no parent. Its children report into its own mailbox; its
    // own completion goes to a dead channel so it can never wake itself up.
    let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    drop(dead_rx);
    spawn_actor(ctx, 0, 0, ws, None, Vec::new(), false, cmd_tx.clone(), dead_tx, done, cmd_rx);
    (cmd_tx, shared)
}

fn spawn_actor(
    ctx: Arc<AgentCtx>,
    id: u64,
    depth: usize,
    ws: Workspace,
    branch: Option<String>,
    initial: Vec<Message>,
    // Children start running immediately; the root waits for its first Run.
    start_immediately: bool,
    // This actor's own mailbox, where its children report completions.
    my_tx: Sender<AgentMsg>,
    // Where this actor reports its own completion to its parent. Dead for the
    // root, which has no parent.
    parent_tx: Sender<AgentMsg>,
    done: Sender<String>,
    rx: Receiver<AgentMsg>,
) {
    let builder = std::thread::Builder::new().name(format!("mush-agent-{id}"));
    if let Err(error) = builder.spawn(move || {
        actor_main(ctx, id, depth, ws, branch, initial, start_immediately, my_tx, parent_tx, done, rx)
    }) {
        eprintln!("mush: could not start agent {id}: {error}");
    }
}

fn actor_main(
    ctx: Arc<AgentCtx>,
    id: u64,
    depth: usize,
    ws: Workspace,
    branch: Option<String>,
    mut transcript: Vec<Message>,
    start_immediately: bool,
    my_tx: Sender<AgentMsg>,
    parent_tx: Sender<AgentMsg>,
    done: Sender<String>,
    rx: Receiver<AgentMsg>,
) {
    let mut state = ActorState {
        my_tx,
        branch,
        children: HashMap::new(),
        running: HashSet::new(),
        completed: HashMap::new(),
        delivered: HashSet::new(),
    };
    let mut running = start_immediately;
    loop {
        if !running {
            match rx.recv() {
                Ok(AgentMsg::Run(messages)) => {
                    transcript = messages;
                    running = true;
                }
                Ok(AgentMsg::Nudge(text)) => {
                    transcript.push(Message::user(text));
                    running = true;
                }
                Ok(AgentMsg::Stop) | Err(_) => return,
                Ok(AgentMsg::ChildDone { id, summary }) => {
                    // The parent ended (or napped) while a child still ran:
                    // waking it with the completion(s) restarts its run with
                    // the results folded in as user messages.
                    transcript.push(Message::user(format!("#{id} done: {summary}")));
                    while let Ok(command) = rx.try_recv() {
                        match command {
                            AgentMsg::ChildDone { id, summary } => {
                                transcript.push(Message::user(format!("#{id} done: {summary}")));
                            }
                            AgentMsg::Nudge(text) => transcript.push(Message::user(text)),
                            // Leave Stop/Run in the mailbox: the run_loop's
                            // drain applies them (cancel / ignore).
                            AgentMsg::Stop | AgentMsg::Run(_) => break,
                        }
                    }
                    running = true;
                }
            }
        }
        if running {
            let cancel = Arc::new(AtomicBool::new(false));
            ctx.live.fetch_add(1, Ordering::SeqCst);
            let result = run_loop(&ctx, id, depth, &ws, &mut state, &mut transcript, &rx, &cancel);
            ctx.live.fetch_sub(1, Ordering::SeqCst);
            let (summary, error) = match result {
                Ok(Some(text)) => (text, None),
                Ok(None) => ("(finished)".to_string(), None),
                Err(error) => (error.clone(), Some(error)),
            };
            let _ = done.send(summary.clone());
            let _ = parent_tx.send(AgentMsg::ChildDone { id, summary });
            let event = match error {
                Some(error) => AgentEvent::Error(error),
                None => AgentEvent::Done,
            };
            let _ = ctx.tx.send(Msg::Agent { id, event });
            running = false;
        }
    }
}

/// One run: model turns → tool calls → results, until the model answers.
fn run_loop(
    ctx: &Arc<AgentCtx>,
    id: u64,
    depth: usize,
    ws: &Workspace,
    state: &mut ActorState,
    messages: &mut Vec<Message>,
    rx: &Receiver<AgentMsg>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let tools = if depth >= MAX_DEPTH {
        prompt::leaf_tool_schemas()
    } else {
        prompt::tool_schemas()
    };

    for _ in 0..MAX_TURNS {
        drain_mailbox(rx, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        trim_history(messages);

        let cfg = ctx
            .cfg
            .lock()
            .map(|config| config.clone())
            .map_err(|_| "shared configuration poisoned".to_string())?;

        let mut request = ChatRequest {
            model: &cfg.model,
            messages,
            tools: &tools,
            // `auto` keeps models that ignore tools working: they simply answer.
            tool_choice: "auto",
            stream: false,
            temperature: 0.2,
            max_tokens: 2048,
            thinking: None,
            reasoning_effort: None,
        };
        // Provider-specific knobs (DeepSeek thinking mode), only for providers
        // that advertise them; other endpoints see a plain request.
        if cfg.thinking_enabled() {
            request.thinking = Some(json!({ "type": "enabled" }));
        }
        if let Some(effort) = cfg.reasoning_effort() {
            request.reasoning_effort = Some(effort.to_string());
        }

        let body = match serde_json::to_string(&request) {
            Ok(body) => body,
            Err(error) => return Err(format!("could not encode request: {error}")),
        };

        let response = match http::post_json(&cfg.chat_url(), &body, cfg.api_key.as_deref()) {
            Ok(response) => response,
            Err(error) => {
                return Err(format!("cannot reach {}: {error}", cfg.base_url));
            }
        };

        if response.status != 200 {
            let parsed = serde_json::from_str::<ChatResponse>(&response.body).ok();
            let detail = parsed
                .and_then(|r| r.error.map(|e| e.message))
                .unwrap_or_else(|| truncate(&response.body, 600));
            return Err(format!("model returned HTTP {}: {detail}", response.status));
        }

        let parsed = match serde_json::from_str::<ChatResponse>(&response.body) {
            Ok(parsed) => parsed,
            Err(error) => return Err(format!("could not parse model response: {error}")),
        };

        let Some(choice) = parsed.choices.into_iter().next() else {
            return Err("model returned no choices".to_string());
        };

        let assistant = choice.message;
        let tool_calls = assistant.tool_calls().to_vec();
        let content = assistant.text().trim().to_string();

        messages.push(assistant.clone());
        let _ = ctx.tx.send(Msg::Agent { id, event: AgentEvent::Message(assistant) });

        if tool_calls.is_empty() {
            if content.is_empty() {
                let _ = ctx
                    .tx
                    .send(Msg::Agent { id, event: AgentEvent::Status("model produced an empty reply".into()) });
            }
            // Children may have finished while we were working without being
            // waited on: deliver their summaries and keep going instead of
            // ending. (Completions that arrive after this run returns wake the
            // idle actor instead — see actor_main.)
            let pending: Vec<(u64, String)> = state
                .completed
                .iter()
                .filter(|(child, _)| !state.delivered.contains(child))
                .map(|(child, summary)| (*child, summary.clone()))
                .collect();
            if !pending.is_empty() {
                for (child, summary) in &pending {
                    messages.push(Message::user(format!("#{child} done: {summary}")));
                    state.delivered.insert(*child);
                }
                continue;
            }
            return Ok(if content.is_empty() { None } else { Some(content) });
        }

        for call in tool_calls {
            drain_mailbox(rx, cancel, messages, state);
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".to_string());
            }
            let name = call.function.name.clone();
            let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);

            let _ = ctx.tx.send(Msg::Agent {
                id,
                event: AgentEvent::Status(format!("{} {}", name, summarize(&args))),
            });

            let result = exec_tool(ctx, id, depth, ws, state, rx, cancel, &name, &args, messages);

            // Shell commands and edits in the main workspace can move files
            // behind the editor's back; ask the UI to resync clean buffers.
            if name == "run_command" || (ws.root() == ctx.root && matches!(name.as_str(), "write_file" | "edit_file")) {
                let _ = ctx.tx.send(Msg::Agent { id, event: AgentEvent::Resync });
            }

            let output = match result {
                Ok(output) => output,
                Err(error) => format!("error: {error}"),
            };
            let tool_message = Message::tool(call.id.clone(), output);
            messages.push(tool_message.clone());
            let _ = ctx.tx.send(Msg::Agent { id, event: AgentEvent::Message(tool_message) });
        }
    }

    Err(format!("stopped after {MAX_TURNS} turns without finishing"))
}

/// Fold pending mailbox commands into the current run: nudges become user
/// messages, stops set the cancel flag, child completions update the registry.
fn drain_mailbox(
    rx: &Receiver<AgentMsg>,
    cancel: &AtomicBool,
    messages: &mut Vec<Message>,
    state: &mut ActorState,
) {
    while let Ok(command) = rx.try_recv() {
        match command {
            AgentMsg::Nudge(text) => messages.push(Message::user(text)),
            AgentMsg::Stop => cancel.store(true, Ordering::SeqCst),
            AgentMsg::ChildDone { id, summary } => {
                state.running.remove(&id);
                state.completed.insert(id, summary);
            }
            AgentMsg::Run(_) => {}
        }
    }
}

fn exec_tool(
    ctx: &Arc<AgentCtx>,
    id: u64,
    depth: usize,
    ws: &Workspace,
    state: &mut ActorState,
    rx: &Receiver<AgentMsg>,
    cancel: &AtomicBool,
    name: &str,
    args: &Value,
    messages: &mut Vec<Message>,
) -> Result<String, String> {
    match name {
        "run_command" => run_command(args, ws.root()),
        "spawn_agent" => spawn_tool(ctx, id, depth, ws, state, args),
        "wait_agents" => wait_tool(ctx, state, rx, cancel, messages, args),
        "agent_status" => status_tool(state, args),
        "agent_control" => control_tool(state, args),
        _ if ws.root() == ctx.root => forward_to_ui(&ctx.tx, name, args),
        _ => direct_tool(ws, name, args),
    }
}

fn spawn_tool(
    ctx: &Arc<AgentCtx>,
    parent: u64,
    depth: usize,
    ws: &Workspace,
    state: &mut ActorState,
    args: &Value,
) -> Result<String, String> {
    if depth >= MAX_DEPTH {
        return Err(format!("cannot spawn: depth {depth} is the limit ({MAX_DEPTH})"));
    }
    if ctx.live.load(Ordering::SeqCst) >= MAX_AGENTS {
        return Err(format!("too many agents running (max {MAX_AGENTS})"));
    }
    let brief = arg_string(args, "brief")?;
    let isolated = args.get("isolated").and_then(Value::as_bool).unwrap_or(false);
    if !isolated && !state.running.is_empty() {
        return Err(
            "a sibling agent already runs in this shared workspace; set isolated=true \
             (its own git worktree) to work in parallel"
                .to_string(),
        );
    }

    let id = ctx.ids.fetch_add(1, Ordering::SeqCst);
    let (child_ws, branch, note) = if isolated {
        match create_worktree(&ctx.root, id, state.branch.as_deref()) {
            Some((path, branch)) => (
                Workspace::new(&path).map_err(|e| format!("cannot open worktree: {e}"))?,
                Some(branch),
                String::new(),
            ),
            None => (
                ws.clone(),
                None,
                " (isolated unavailable: needs a git repo and the git binary; running in place)"
                    .to_string(),
            ),
        }
    } else {
        (ws.clone(), None, String::new())
    };

    let (done_tx, _done_rx) = bounded::<String>(1);
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    let system = prompt::subagent_prompt(&child_ws.root_str(), depth + 1, &format!("{brief}{note}"));
    // Mirror the root's message shape (system + user); some servers' chat
    // templates reject a system-only first request.
    let initial = vec![Message::system(system), Message::user("Begin the task now.")];
    // The child's own mailbox is where grandchildren report; the parent's
    // mailbox is where this child reports its completion.
    let parent_tx = state.my_tx.clone();
    spawn_actor(
        ctx.clone(),
        id,
        depth + 1,
        child_ws,
        branch.clone(),
        initial,
        true,
        cmd_tx.clone(),
        parent_tx,
        done_tx,
        cmd_rx,
    );

    state.children.insert(id, cmd_tx.clone());
    state.running.insert(id);
    let _ = ctx.tx.send(Msg::Agent {
        id: parent,
        event: AgentEvent::Spawned {
            child: id,
            parent,
            brief: brief.clone(),
            depth: depth + 1,
            branch,
            cmd: cmd_tx,
        },
    });
    Ok(format!("spawned agent #{id}"))
}

fn wait_tool(
    _ctx: &Arc<AgentCtx>,
    state: &mut ActorState,
    rx: &Receiver<AgentMsg>,
    cancel: &AtomicBool,
    messages: &mut Vec<Message>,
    args: &Value,
) -> Result<String, String> {
    let ids: Vec<u64> = args
        .get("ids")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let candidates: Vec<u64> = if ids.is_empty() {
        state.children.keys().copied().collect()
    } else {
        ids
    };
    if candidates.is_empty() {
        return Ok("no child agents to wait for".to_string());
    }
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(WAIT_TIMEOUT_SECS);
    let deadline = (timeout > 0).then(|| Instant::now() + Duration::from_secs(timeout));

    loop {
        drain_mailbox(rx, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        for id in &candidates {
            if let Some(summary) = state.completed.get(id) {
                state.delivered.insert(*id);
                return Ok(format!("#{id} done: {summary}"));
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Ok("wait timed out — your agents are still running".to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn status_tool(state: &ActorState, _args: &Value) -> Result<String, String> {
    if state.children.is_empty() {
        return Ok("no child agents".to_string());
    }
    let mut lines = Vec::new();
    let mut ids: Vec<u64> = state.children.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        if let Some(summary) = state.completed.get(&id) {
            lines.push(format!("#{id} ✓ {summary}"));
        } else {
            lines.push(format!("#{id} ◐ running"));
        }
    }
    Ok(lines.join("\n"))
}

fn control_tool(state: &mut ActorState, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing `id`".to_string())?;
    let action = arg_string(args, "action")?;
    let Some(cmd) = state.children.get(&id) else {
        return Err(format!("no such child agent #{id}"));
    };
    match action.as_str() {
        "stop" => {
            let _ = cmd.send(AgentMsg::Stop);
            Ok(format!("stopping agent #{id}"))
        }
        "message" => {
            let text = arg_string(args, "text")?;
            let _ = cmd.send(AgentMsg::Nudge(text));
            Ok(format!("messaged agent #{id}"))
        }
        other => Err(format!("unknown action `{other}` (stop or message)")),
    }
}

/// A private git worktree for an isolated child: `.mush/wt/<id>` on branch
/// `mush/<id>`, based on the parent's branch (or HEAD).
fn create_worktree(main_root: &Path, id: u64, base_branch: Option<&str>) -> Option<(PathBuf, String)> {
    if !main_root.join(".git").exists() {
        return None;
    }
    let worktree = main_root.join(format!(".mush/wt/{id}"));
    let branch = format!("mush/{id}");
    let base = base_branch.unwrap_or("HEAD");
    let status = Command::new("git")
        .arg("-C")
        .arg(main_root)
        .args(["worktree", "add", "-b", &branch, worktree.to_str()?, base])
        .status()
        .ok()?;
    if status.success() {
        Some((worktree, branch))
    } else {
        None
    }
}

/// File tools for the main workspace must run on the UI thread: only it knows
/// about live buffers.
fn forward_to_ui(tx: &Sender<Msg>, name: &str, args: &Value) -> Result<String, String> {
    let (reply_tx, reply_rx) = bounded(1);
    let request = ToolCallRequest {
        name: name.to_string(),
        args: args.clone(),
        reply: reply_tx,
    };
    if tx.send(Msg::Tool(request)).is_err() {
        return Err("mush is shutting down".to_string());
    }
    match reply_rx.recv_timeout(TOOL_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(format!("{name} did not complete in time")),
    }
}

/// Same five tools, executed directly against a workspace the UI never sees
/// (an isolated agent's worktree): plain disk I/O, no live buffers.
fn direct_tool(ws: &Workspace, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "list_files" => {
            let requested = args.get("path").and_then(Value::as_str).unwrap_or("");
            let prefix = requested.trim().trim_start_matches("./").trim_end_matches('/');
            let files: Vec<String> = ws
                .list_files(4_000)
                .into_iter()
                .filter(|file| prefix.is_empty() || prefix == "." || file.starts_with(&format!("{prefix}/")))
                .collect();
            if files.is_empty() {
                Ok(format!("no files under `{}`", if prefix.is_empty() { "." } else { prefix }))
            } else {
                Ok(files.join("\n"))
            }
        }
        "read_file" => {
            let rel = arg_string(args, "path")?;
            ws.read_file(&rel, READ_CAP)
        }
        "write_file" => {
            let rel = arg_string(args, "path")?;
            let content = arg_string(args, "content")?;
            ws.write_file(&rel, &content)?;
            Ok(format!("wrote {rel}"))
        }
        "edit_file" => {
            let rel = arg_string(args, "path")?;
            let old = arg_string(args, "old_string")?;
            let new = arg_string(args, "new_string")?;
            if old.is_empty() {
                return Err("old_string must not be empty".to_string());
            }
            let current = ws.read_file(&rel, usize::MAX)?;
            match current.matches(old.as_str()).count() {
                0 => Err(format!("old_string not found in {rel}")),
                1 => {
                    let updated = current.replacen(old.as_str(), new.as_str(), 1);
                    ws.write_file(&rel, &updated)?;
                    Ok(format!("edited {rel}"))
                }
                count => Err(format!(
                    "old_string appears {count} times in {rel}; include more context to make it unique"
                )),
            }
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn run_command(args: &Value, root: &Path) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `command`".to_string())?;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run command: {e}"))?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let out_reader = std::thread::spawn(move || drain(&mut stdout, CMD_CAP));
    let err_reader = std::thread::spawn(move || drain(&mut stderr, CMD_CAP));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => return Err(format!("could not wait for command: {error}")),
        }
        if started.elapsed() > Duration::from_secs(CMD_TIMEOUT_SECS) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(40));
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    let mut report = format!("$ {command}\n");
    if !stdout.trim().is_empty() {
        report.push_str(stdout.trim_end());
        report.push('\n');
    }
    if !stderr.trim().is_empty() {
        report.push_str("--- stderr ---\n");
        report.push_str(stderr.trim_end());
        report.push('\n');
    }
    match status {
        Some(status) => report.push_str(&format!("[exit {}]", status.code().unwrap_or(-1))),
        None => report.push_str(&format!("[timed out after {CMD_TIMEOUT_SECS}s]")),
    }
    Ok(report)
}

/// Read a pipe to the end, keeping only the first `cap` bytes. We must keep
/// reading after the cap or the child can block on a full pipe.
fn drain<R: Read>(reader: &mut R, cap: usize) -> String {
    let mut kept = Vec::new();
    let mut scratch = [0u8; 8192];
    loop {
        match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < cap {
                    let take = (cap - kept.len()).min(n);
                    kept.extend_from_slice(&scratch[..take]);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Drop the oldest turns until the conversation fits the budget. Trimming at a
/// user message keeps assistant/tool pairs intact, which servers validate.
fn trim_history(messages: &mut Vec<Message>) {
    loop {
        let total: usize = messages.iter().map(Message::weight).sum();
        if total <= HISTORY_BUDGET {
            return;
        }
        let user_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "user")
            .map(|(i, _)| i)
            .collect();
        if user_indices.len() < 2 {
            return;
        }
        let keep_from = user_indices[1];
        // `messages[0]` is the system prompt; never drop it, and never stall.
        if keep_from <= 1 {
            return;
        }
        messages.drain(1..keep_from);
    }
}

fn summarize(args: &Value) -> String {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        return path.to_string();
    }
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        return truncate(command, 60);
    }
    if let Some(brief) = args.get("brief").and_then(Value::as_str) {
        return truncate(brief, 40);
    }
    String::new()
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn arg_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing `{key}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_prefers_paths_then_commands_then_briefs() {
        assert_eq!(summarize(&json!({"path": "a.rs"})), "a.rs");
        assert_eq!(summarize(&json!({"command": "ls -la"})), "ls -la");
        assert_eq!(summarize(&json!({"brief": "fix the parser"})), "fix the parser");
        assert_eq!(summarize(&json!({})), "");
    }

    #[test]
    fn trim_history_keeps_recent_turns_and_pairs() {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        for i in 0..200 {
            messages.push(Message::assistant(format!("reply {i} {}", "x".repeat(500))));
            messages.push(Message::tool(format!("call{i}"), "result"));
            messages.push(Message::user(format!("again {i}")));
        }
        trim_history(&mut messages);
        assert_eq!(messages[0].role, "system");
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= HISTORY_BUDGET);
        // The first kept entry must be a user message so pairs stay valid.
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn drain_caps_output_but_consumes_the_pipe() {
        let data = vec![b'a'; 100_000];
        let mut cursor = std::io::Cursor::new(data);
        let kept = drain(&mut cursor, 10);
        assert_eq!(kept.len(), 10);
        assert_eq!(cursor.position(), 100_000);
    }

    /// The full orchestration path, headless: root spawns an isolated child,
    /// the child writes into its own worktree, the parent waits and collects
    /// the summary. The model is `scripts/mock_llm.py` — deterministic.
    #[test]
    #[ignore = "needs python3 + git; spawns a local mock model server"]
    fn isolated_subagent_writes_its_worktree() {
        use std::fs;

        const PORT: u16 = 18_731;
        let mut mock = start_mock(PORT);

        // A real git repo so `create_worktree` has something to branch from.
        let root = init_git_repo("iso");

        let cfg = Config::new(format!("http://127.0.0.1:{PORT}"), "mock", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let (root_tx, _shared) = spawn(cfg, tx, root.clone());

        let messages = vec![
            Message::system(prompt::system_prompt(root.to_str().unwrap())),
            Message::user("delegate: create iso.txt via an isolated subagent".to_string()),
        ];
        root_tx.send(AgentMsg::Run(messages)).unwrap();

        // The isolated child must write into `.mush/wt/1/`, not the main root.
        let target = root.join(".mush/wt/1/iso.txt");
        let mut found = None;
        for _ in 0..300 {
            if target.exists() {
                found = fs::read_to_string(&target).ok();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // The mock ends the root's first turn while the child still runs, so
        // the orchestrator must be woken by the child's completion: expect the
        // root to run twice (3 Done events total: root, child, woken root).
        let done_events = count_done_events(&rx, 3);
        let _ = mock.kill();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(found.as_deref(), Some("isolated work"));
        assert_eq!(done_events, 3, "root must be woken when its child finishes");
    }

    /// Root -> child -> grandchild, each isolated: the grandchild's file must
    /// land in `.mush/wt/2/`, branched off the child's worktree (`mush/2`
    /// based on `mush/1`), and the summaries bubble up through wait_agents.
    #[test]
    #[ignore = "needs python3 + git; spawns a local mock model server"]
    fn deep_chain_writes_nested_worktrees() {
        use std::fs;

        const PORT: u16 = 18_732;
        let mut mock = start_mock(PORT);
        let root = init_git_repo("chain");

        let cfg = Config::new(format!("http://127.0.0.1:{PORT}"), "mock", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let (root_tx, _shared) = spawn(cfg, tx, root.clone());

        let messages = vec![
            Message::system(prompt::system_prompt(root.to_str().unwrap())),
            Message::user("CHAIN: delegate the file creation through two levels of subagents".to_string()),
        ];
        root_tx.send(AgentMsg::Run(messages)).unwrap();

        // The grandchild (agent #2) writes into its own worktree.
        let target = root.join(".mush/wt/2/deep.txt");
        let mut found = None;
        for _ in 0..600 {
            if target.exists() {
                found = fs::read_to_string(&target).ok();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Every level ran exactly once and reported: root, child, grandchild.
        let done_events = count_done_events(&rx, 3);

        // Nested worktree: mush/2 must branch off mush/1 (both sit on the
        // branch point; nothing has been committed yet), and the child's
        // worktree must NOT contain the grandchild's file.
        let grandchild_head = git_rev_parse(&root, "mush/2");
        let child_head = git_rev_parse(&root, "mush/1");
        let child_has_file = root.join(".mush/wt/1/deep.txt").exists();

        let _ = mock.kill();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(found.as_deref(), Some("deep work"), "grandchild must write its own worktree");
        assert_eq!(done_events, 3, "each level must run and finish once");
        assert_eq!(
            grandchild_head.as_deref(),
            child_head.as_deref(),
            "mush/2 must branch off mush/1"
        );
        assert!(!child_has_file, "the child's worktree must stay clean of grandchild work");
    }

    /// Start the scripted mock model server and wait until it answers.
    fn start_mock(port: u16) -> std::process::Child {
        use std::process::Command;

        const MOCK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/mock_llm.py");
        let mock = Command::new("python3")
            .arg(MOCK)
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("mock server starts");

        let probe = format!(
            "import urllib.request;urllib.request.urlopen(\
             'http://127.0.0.1:{port}/v1/models', timeout=0.2)"
        );
        let mut ready = false;
        for _ in 0..100 {
            let status = Command::new("python3")
                .args(["-c", &probe])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if status {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(ready, "mock model server (port {port}) did not come up");
        mock
    }

    /// A scratch git repo with one initial commit, ready for worktrees. The
    /// label keeps parallel tests from sharing a directory.
    fn init_git_repo(label: &str) -> PathBuf {
        use std::fs;
        use std::process::Command;

        let root = std::env::temp_dir()
            .join(format!("mush-chain-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git").arg("-C").arg(&root).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        root
    }

    fn git_rev_parse(root: &Path, rev: &str) -> Option<String> {
        use std::process::Command;

        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", rev])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Drain the event channel until `target` Done events have been seen.
    fn count_done_events(rx: &Receiver<Msg>, target: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut done_events = 0usize;
        while done_events < target && Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                if matches!(msg, Msg::Agent { event: AgentEvent::Done, .. }) {
                    done_events += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done_events
    }
}