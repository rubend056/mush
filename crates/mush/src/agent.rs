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
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde_json::{json, Value};

use mush_core::message::{ChatRequest, ChatResponse};
use mush_core::workspace::truncate_for_model;
use mush_core::{
    prompt, tools, Config, Message, Workspace, CMD_CAP, CMD_TIMEOUT_SECS, LIST_LIMIT, READ_CAP,
};

use crate::app::{Msg, ToolCallRequest};
use crate::http;

/// Safety valve: how many model turns one run may take.
const MAX_TURNS: usize = 24;
/// A tool that never comes back must not wedge the agent forever.
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// How deep subagent chains may go (0 = root agent only).
pub const MAX_DEPTH: usize = 3;
/// Hard ceiling on simultaneously running agents across the whole tree.
const MAX_AGENTS: u64 = 16;
/// Default `wait_agents` timeout in seconds; 0 means forever.
const WAIT_TIMEOUT_SECS: u64 = 600;
/// Why a cancelled run ends. The UI shows this one as a status, not a failure.
pub const CANCELLED: &str = "cancelled";

/// Commands sent into an agent actor's mailbox.
pub enum AgentMsg {
    /// Adopt these messages and run. The actor keeps the transcript, so later
    /// nudges continue the same conversation.
    Run(Vec<Message>),
    /// Append a user message; if idle, run again.
    Nudge(String),
    /// Cancel the current run. An idle agent ignores it — Stop cancels work,
    /// it does not end an agent.
    Stop,
    /// End this actor for good (`/new`, Ctrl-N). A `Stop` cannot do this: an
    /// actor holds its own mailbox open, so it never learns that everyone else
    /// let go — it has to be told.
    Shutdown,
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
    /// A run began — including one the UI did not ask for, because an idle
    /// agent was woken by a child's result. Keeps `busy` and the tree honest.
    /// `cancel` is this run's flag, and the UI keeps a clone: it is the one the
    /// HTTP reader polls, so a Stop reaches a model call that has not answered.
    Running {
        cancel: Arc<AtomicBool>,
    },
    Status(String),
    Message(Message),
    Resync,
    Done,
    Error(String),
    /// The transcript was folded into a summary (context compaction); the
    /// conversation is now `[system, user(summary)]`.
    Compact {
        summary: String,
    },
}

/// Shared by every actor: config, the UI channel, and the budgets.
pub struct AgentCtx {
    /// Shared so a runtime `/provider` / `/url` / `/model` / `/key` applies to
    /// every agent immediately.
    pub cfg: Arc<Mutex<Config>>,
    pub tx: Sender<Msg>,
    /// Which conversation this tree belongs to. The UI drops events stamped
    /// with another one: after `/new`, an abandoned actor can still be
    /// finishing a request, and its events must not land in the new chat.
    pub conversation: u64,
    /// The main workspace root; agents whose root differs are isolated.
    pub root: PathBuf,
    pub ids: Arc<AtomicU64>,
    pub live: Arc<AtomicU64>,
}

impl AgentCtx {
    /// Send an id-tagged event to the UI, stamped with this conversation.
    fn emit(&self, id: u64, event: AgentEvent) {
        let _ = self.tx.send(Msg::Agent {
            conversation: self.conversation,
            id,
            event,
        });
    }
}

/// Per-actor state that survives across runs (children, summaries).
#[derive(Default)]
struct ActorState {
    children: HashMap<u64, Sender<AgentMsg>>,
    running: HashSet<u64>,
    completed: HashMap<u64, String>,
    /// Completions already handed to the model (via wait_agents or delivery).
    delivered: HashSet<u64>,
    /// Commands parked while a blocking tool call was in flight; folded in at
    /// the next message boundary (see `drain_signals`).
    deferred: Vec<AgentMsg>,
    /// A `Shutdown` arrived: stop the run and end this actor.
    shutdown: bool,
}

/// One agent: its identity, its workspace, and the mailboxes it is wired to.
/// Passed by reference through a run, which keeps the loop functions small.
struct Actor {
    ctx: Arc<AgentCtx>,
    id: u64,
    depth: usize,
    ws: Workspace,
    /// The isolated worktree branch this agent works on, if any; children
    /// branch from it so nested work is not lost.
    branch: Option<String>,
    /// The task this agent was given (empty for the root). Used as the commit
    /// subject when an isolated agent's run ends.
    brief: String,
    /// Its own mailbox — where its children report their completions.
    my_tx: Sender<AgentMsg>,
    /// Where it reports its own completion to its parent. Dead for the root,
    /// which has no parent.
    parent_tx: Sender<AgentMsg>,
    rx: Receiver<AgentMsg>,
}

/// The UI's handle on the root actor of one conversation.
pub struct RootHandle {
    /// The root's mailbox.
    pub tx: Sender<AgentMsg>,
    /// The config cell every actor in this tree reads, so a runtime `/model`
    /// reaches them all.
    pub cfg: Arc<Mutex<Config>>,
    /// Identifies this conversation in events; see `AgentCtx::conversation`.
    pub conversation: u64,
}

/// Start the root actor.
pub fn spawn(cfg: Config, tx: Sender<Msg>, root: PathBuf) -> RootHandle {
    // One conversation per `/new`, so stale events can be told apart.
    static CONVERSATIONS: AtomicU64 = AtomicU64::new(1);
    let conversation = CONVERSATIONS.fetch_add(1, Ordering::SeqCst);
    let shared = Arc::new(Mutex::new(cfg));
    let ctx = Arc::new(AgentCtx {
        cfg: shared.clone(),
        tx,
        conversation,
        root,
        // Root agent is id 0; children start at 1.
        ids: Arc::new(AtomicU64::new(1)),
        live: Arc::new(AtomicU64::new(0)),
    });
    let ws = Workspace::new(&ctx.root).expect("workspace root must exist");
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    // The root has no parent. Its children report into its own mailbox; its
    // own completion goes to a dead channel so it can never wake itself up.
    let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    drop(dead_rx);
    let actor = Actor {
        ctx,
        id: 0,
        depth: 0,
        ws,
        branch: None,
        brief: String::new(),
        my_tx: cmd_tx.clone(),
        parent_tx: dead_tx,
        rx: cmd_rx,
    };
    start(actor, Vec::new(), false);
    RootHandle {
        tx: cmd_tx,
        cfg: shared,
        conversation,
    }
}

/// Run an actor on its own thread. A thread that cannot start is reported as
/// that agent's result, so a parent waiting on it is never left waiting.
fn start(actor: Actor, initial: Vec<Message>, start_immediately: bool) {
    let id = actor.id;
    let ctx = actor.ctx.clone();
    let parent_tx = actor.parent_tx.clone();
    let builder = std::thread::Builder::new().name(format!("mush-agent-{id}"));
    if let Err(error) = builder.spawn(move || actor_main(actor, initial, start_immediately)) {
        let summary = format!("error: agent #{id} could not start ({error})");
        ctx.emit(id, AgentEvent::Error(summary.clone()));
        let _ = parent_tx.send(AgentMsg::ChildDone { id, summary });
    }
}

fn actor_main(actor: Actor, mut transcript: Vec<Message>, start_immediately: bool) {
    let mut state = ActorState::default();
    // Children are handed a task and start at once; the root waits to be asked.
    let mut ready = start_immediately;
    loop {
        if !wait_for_work(&actor, &mut state, &mut transcript, ready) {
            return;
        }
        ready = false;
        let cancel = Arc::new(AtomicBool::new(false));
        // Say so up front: the UI did not necessarily ask for this run (a nap
        // ends with a wake-up), and the tree must show it running. The flag
        // travels with the event so the human can stop a run that is blocked
        // waiting for a model reply.
        actor.ctx.emit(
            actor.id,
            AgentEvent::Running {
                cancel: cancel.clone(),
            },
        );
        actor.ctx.live.fetch_add(1, Ordering::SeqCst);
        let result = run_loop(&actor, &mut state, &mut transcript, &cancel);
        actor.ctx.live.fetch_sub(1, Ordering::SeqCst);
        // An isolated agent's branch *is* the deliverable mush documents for it
        // (`/diff`, `/merge`, `/discard`), so its work is committed here instead
        // of being left as untracked files in the worktree. Before the parent is
        // told, so a diff or merge it triggers already sees the work.
        if let Some(branch) = actor.branch.clone() {
            match commit_worktree(actor.ws.root(), actor.id, &actor.brief) {
                Ok(Some(revision)) => {
                    actor.ctx.emit(
                        actor.id,
                        AgentEvent::Status(format!("committed {revision} on {branch}")),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    actor.ctx.emit(
                        actor.id,
                        AgentEvent::Status(format!("could not commit the worktree: {error}")),
                    );
                }
            }
        }
        let (summary, error) = match result {
            Ok(Some(text)) => (text, None),
            Ok(None) => ("(finished)".to_string(), None),
            Err(error) => (error.clone(), Some(error)),
        };
        let _ = actor.parent_tx.send(AgentMsg::ChildDone {
            id: actor.id,
            summary,
        });
        match error {
            Some(error) => actor.ctx.emit(actor.id, AgentEvent::Error(error)),
            None => actor.ctx.emit(actor.id, AgentEvent::Done),
        }
        // A Shutdown arrived while this run was winding down: it is over, and
        // so is this actor.
        if state.shutdown {
            return;
        }
    }
}

/// Wait for the next run, folding every already-queued command into the
/// transcript first so a batch of completions costs one run, not one each.
/// `ready` skips the blocking wait when the actor was started with a task.
/// Returns `false` when this actor should end.
fn wait_for_work(
    actor: &Actor,
    state: &mut ActorState,
    transcript: &mut Vec<Message>,
    ready: bool,
) -> bool {
    if !ready {
        // Idle: block until there is something to do. A stray Stop carries no
        // work, so it just means waiting again.
        loop {
            match actor.rx.recv() {
                // Every handle to this agent is gone; so is any reason to live.
                Err(_) => return false,
                Ok(command) => match absorb(state, transcript, command) {
                    Fold::End => return false,
                    Fold::Run => break,
                    Fold::Idle => continue,
                },
            }
        }
    }
    // Fold in whatever else is already queued, so a batch of completions costs
    // one run instead of one run each.
    loop {
        match actor.rx.try_recv() {
            Err(_) => return true,
            Ok(command) => match absorb(state, transcript, command) {
                Fold::End => return false,
                Fold::Run | Fold::Idle => {}
            },
        }
    }
}

/// What a command means for an actor that is not running.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Fold {
    /// Folded in; stay idle.
    Idle,
    /// There is work to do.
    Run,
    /// End this actor.
    End,
}

/// Fold one mailbox command into the actor's transcript.
fn absorb(state: &mut ActorState, transcript: &mut Vec<Message>, command: AgentMsg) -> Fold {
    match command {
        // An idle agent has nothing to cancel, so a Stop is a no-op here.
        // Ending an agent is what Shutdown is for.
        AgentMsg::Stop => Fold::Idle,
        AgentMsg::Shutdown => Fold::End,
        AgentMsg::Run(messages) => {
            // The UI's transcript is newer than ours; it wins. Mark as already
            // delivered whatever completion lines it carries (the model reads
            // them there), so the pending-completion step below cannot inject
            // the same news twice; anything it cannot know about is still
            // ours to announce.
            *transcript = messages;
            // The human may have typed while a tool batch was running, which
            // puts their words between an assistant's calls and their results;
            // strict servers reject that shape.
            repair_tool_pairs(transcript);
            let announced: Vec<u64> = state
                .completed
                .keys()
                .filter(|id| {
                    let line = format!("#{id} done:");
                    transcript
                        .iter()
                        .any(|message| message.text().contains(&line))
                })
                .copied()
                .collect();
            state.delivered = announced.into_iter().collect();
            Fold::Run
        }
        AgentMsg::Nudge(text) => {
            transcript.push(Message::user(text));
            Fold::Run
        }
        AgentMsg::ChildDone { id, summary } => {
            // The parent ended (or napped) while a child still ran: waking it
            // with the completion restarts its run with the result folded in,
            // so an early End is not a lost result, it is a nap. The
            // completion counts as delivered because the model is about to
            // read it in this very run.
            let cancelled = summary == CANCELLED;
            let line = note_completion(state, id, summary);
            transcript.push(Message::user(line));
            state.delivered.insert(id);
            // A cancelled child is the human's doing, not news that warrants
            // waking a napping parent into a fresh (paid) run: the line is in
            // the transcript for whenever the parent runs next.
            if cancelled {
                Fold::Idle
            } else {
                Fold::Run
            }
        }
    }
}

/// One run: model turns → tool calls → results, until the model answers.
fn run_loop(
    actor: &Actor,
    state: &mut ActorState,
    messages: &mut Vec<Message>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let tools = if actor.depth >= MAX_DEPTH {
        prompt::leaf_tool_schemas()
    } else {
        prompt::tool_schemas()
    };

    for _ in 0..MAX_TURNS {
        drain_mailbox(&actor.rx, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }

        let cfg = actor
            .ctx
            .cfg
            .lock()
            .map(|config| config.clone())
            .map_err(|_| "shared configuration poisoned".to_string())?;

        let budget = cfg.history_budget();
        // Approaching the context window: fold the conversation into a summary
        // instead of dropping old turns, so long-running tasks keep their
        // state. The summarize request re-sends the history, so only fire
        // while it still fits; beyond that, trimming stays the last resort.
        let history: usize = messages.iter().map(Message::weight).sum();
        if history > budget * 3 / 4 && history <= budget {
            compact_history(actor, &cfg, messages, cancel, state)?;
        }
        // Keep the whole request inside the endpoint's context window.
        trim_history(messages, budget);

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

        let response = match http::post_json(&cfg.chat_url(), &body, cfg.api_key.as_deref(), cancel)
        {
            Ok(response) => response,
            // The reader stops the moment the human cancels; that is a
            // cancellation, not a failure to reach the endpoint.
            Err(_) if cancel.load(Ordering::SeqCst) => return Err(CANCELLED.to_string()),
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

        let assistant = sanitize_tool_calls(choice.message);
        let tool_calls = assistant.tool_calls().to_vec();
        let content = assistant.text().trim().to_string();

        // A Stop (cancel, new chat) or Shutdown may have arrived while the
        // request was in flight: drop the stale reply instead of delivering it
        // into a fresh conversation; the run ends as cancelled. A nudge is
        // parked rather than folded: it arrived after the model wrote this
        // reply, so the transcript must not read as if the model had seen it.
        drain_signals(actor, cancel, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }

        messages.push(assistant.clone());
        actor.ctx.emit(actor.id, AgentEvent::Message(assistant));

        if tool_calls.is_empty() {
            if content.is_empty() {
                actor.ctx.emit(
                    actor.id,
                    AgentEvent::Status("model produced an empty reply".into()),
                );
            }
            // Parked nudges belong after the reply; the human wrote them while
            // it was in flight, so the model has not answered them yet.
            let before = messages.len();
            drain_mailbox(&actor.rx, cancel, messages, state);
            if cancel.load(Ordering::SeqCst) {
                return Err(CANCELLED.to_string());
            }
            let steered = messages.len() > before;
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
                    let line = note_completion(state, *child, summary.clone());
                    messages.push(Message::user(line));
                    state.delivered.insert(*child);
                }
                continue;
            }
            // Answer the steering instead of ending the run without it: the
            // model has not seen those words yet.
            if steered {
                continue;
            }
            return Ok(if content.is_empty() {
                None
            } else {
                Some(content)
            });
        }

        // Every call in a batch must be answered, or the transcript keeps an
        // assistant message whose tool calls dangle — which most servers then
        // reject for the rest of the conversation.
        for (index, call) in tool_calls.iter().enumerate() {
            // Keep watching for a Stop/Shutdown between calls, and answer the
            // rest of the batch before leaving: a cancellation must not leave
            // unanswered tool calls behind.
            drain_signals(actor, cancel, state);
            if cancel.load(Ordering::SeqCst) {
                for skipped in &tool_calls[index..] {
                    let message = Message::tool(skipped.id.clone(), format!("error: {CANCELLED}"));
                    messages.push(message.clone());
                    actor.ctx.emit(actor.id, AgentEvent::Message(message));
                }
                return Err(CANCELLED.to_string());
            }

            let name = call.function.name.clone();
            let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);

            actor.ctx.emit(
                actor.id,
                AgentEvent::Status(format!("{} {}", name, summarize(&args))),
            );

            let result = exec_tool(actor, state, &name, &args, cancel);

            // Shell commands and edits in the main workspace can move files
            // behind the editor's back; ask the UI to resync clean buffers.
            if name == "run_command"
                || (actor.ws.root() == actor.ctx.root
                    && matches!(name.as_str(), "write_file" | "edit_file"))
            {
                actor.ctx.emit(actor.id, AgentEvent::Resync);
            }

            let output = match result {
                Ok(output) => output,
                Err(error) => format!("error: {error}"),
            };
            let tool_message = Message::tool(call.id.clone(), output);
            messages.push(tool_message.clone());
            actor.ctx.emit(actor.id, AgentEvent::Message(tool_message));
        }
        // Fold mailbox commands in at the message boundary, and honour a
        // cancellation now that every call has a result.
        drain_mailbox(&actor.rx, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }
    }

    Err(format!("stopped after {MAX_TURNS} turns without finishing"))
}

/// The instruction appended when the transcript nears the context window.
const COMPACT_INSTRUCTION: &str = "\
The conversation is approaching the context limit. Summarize everything \
important so far — the original task, the work done, files created or \
changed, open issues, and the current state. This summary replaces the \
conversation, so include every fact the task still depends on. Reply with \
just the summary.";

/// Fold the transcript into a summary: ask the model to condense it, then
/// replace the conversation with `[system, user(summary)]` — the summary is
/// the new opening task message, which trimming protects. Does nothing when
/// the model could not produce a summary; trimming is the fallback.
fn compact_history(
    actor: &Actor,
    cfg: &Config,
    messages: &mut Vec<Message>,
    cancel: &AtomicBool,
    state: &mut ActorState,
) -> Result<(), String> {
    if !matches!(messages.first(), Some(message) if message.role == "system") {
        return Ok(());
    }
    // Nothing left to fold: system + one user message is already minimal
    // (usually a previous summary), so compacting again would just cost a
    // request and re-summarize the summary.
    if messages.len() <= 2 {
        return Ok(());
    }
    let _ = actor.ctx.tx.send(Msg::Agent {
        conversation: actor.ctx.conversation,
        id: actor.id,
        event: AgentEvent::Status("context nearly full — summarizing…".to_string()),
    });

    // Fold pending nudges/completions in first; a Stop cancels the run.
    drain_mailbox(&actor.rx, cancel, messages, state);
    if cancel.load(Ordering::SeqCst) {
        return Err(CANCELLED.to_string());
    }

    let mut ask = messages.clone();
    ask.push(Message::user(COMPACT_INSTRUCTION));
    // A plain, tool-free request: the summary, nothing else.
    let request = ChatRequest {
        model: &cfg.model,
        messages: &ask,
        tools: &[],
        tool_choice: "none",
        stream: false,
        temperature: 0.2,
        max_tokens: 1024,
        thinking: if cfg.thinking_enabled() {
            Some(json!({ "type": "enabled" }))
        } else {
            None
        },
        reasoning_effort: cfg.reasoning_effort().map(str::to_string),
    };
    let body = match serde_json::to_string(&request) {
        Ok(body) => body,
        Err(error) => return Err(format!("could not encode request: {error}")),
    };
    let response = match http::post_json(&cfg.chat_url(), &body, cfg.api_key.as_deref(), cancel) {
        Ok(response) => response,
        // A cancelled run is already ending; do not report a network failure.
        Err(_) if cancel.load(Ordering::SeqCst) => return Err(CANCELLED.to_string()),
        // The run will fail on its real request anyway; surface it.
        Err(error) => return Err(format!("cannot reach {}: {error}", cfg.base_url)),
    };
    if response.status != 200 {
        return Ok(());
    }
    let parsed = match serde_json::from_str::<ChatResponse>(&response.body) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(()),
    };
    let summary = parsed
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.text().trim().to_string())
        .unwrap_or_default();
    if summary.is_empty() {
        return Ok(());
    }

    let system = messages[0].clone();
    *messages = vec![system, Message::user(prompt::compaction_message(&summary))];
    actor.ctx.emit(
        actor.id,
        AgentEvent::Compact {
            summary: summary.clone(),
        },
    );
    Ok(())
}

/// Fold in only what may appear between tool calls: cancellation, shutdown, and
/// child completions. Nudges and new transcripts are *parked* for the next
/// message boundary — a user message between an assistant's tool calls and their
/// results makes strict servers reject the whole conversation. They are parked
/// in the actor's own state, never put back in the mailbox: that is the queue
/// this function is draining, so re-sending would spin forever.
fn drain_signals(actor: &Actor, cancel: &AtomicBool, state: &mut ActorState) {
    for command in actor.rx.try_iter() {
        match command {
            AgentMsg::Stop => cancel.store(true, Ordering::SeqCst),
            AgentMsg::Shutdown => {
                cancel.store(true, Ordering::SeqCst);
                state.shutdown = true;
            }
            AgentMsg::ChildDone { id, summary } => {
                note_completion(state, id, summary);
            }
            parked => state.deferred.push(parked),
        }
    }
}

/// Fold pending mailbox commands into the current run: nudges become user
/// messages, stops set the cancel flag, child completions update the registry.
/// Everything parked by `drain_signals` goes in first, in order.
fn drain_mailbox(
    rx: &Receiver<AgentMsg>,
    cancel: &AtomicBool,
    messages: &mut Vec<Message>,
    state: &mut ActorState,
) {
    let parked = std::mem::take(&mut state.deferred);
    for command in parked.into_iter().chain(rx.try_iter()) {
        match command {
            AgentMsg::Nudge(text) => messages.push(Message::user(text)),
            AgentMsg::Stop => cancel.store(true, Ordering::SeqCst),
            AgentMsg::Shutdown => {
                // Cancel now, and remember: the run ends, and so does the actor.
                cancel.store(true, Ordering::SeqCst);
                state.shutdown = true;
            }
            AgentMsg::ChildDone { id, summary } => {
                note_completion(state, id, summary);
            }
            // The UI sends a whole transcript when it believes we are idle.
            // We are mid-run, so the only new information is the message the
            // human just typed — fold that in rather than dropping input the
            // UI has already echoed. The full transcript re-syncs at the next
            // idle Run.
            AgentMsg::Run(transcript) => {
                if let Some(message) = transcript.last() {
                    if message.role == "user" {
                        messages.push(message.clone());
                    }
                }
            }
        }
    }
}

/// Record a child's completion and return the line the model reads. A fresh
/// completion also supersedes any earlier delivery of the same child.
fn note_completion(state: &mut ActorState, id: u64, summary: String) -> String {
    state.running.remove(&id);
    state.completed.insert(id, summary.clone());
    state.delivered.remove(&id);
    format!("#{id} done: {summary}")
}

fn exec_tool(
    actor: &Actor,
    state: &mut ActorState,
    name: &str,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, String> {
    match name {
        "run_command" => run_command(actor, state, args, cancel),
        "spawn_agent" => spawn_tool(actor, state, args),
        "wait_agents" => wait_tool(actor, state, cancel, args),
        "agent_status" => status_tool(state),
        "agent_control" => control_tool(state, args),
        _ if actor.ws.root() == actor.ctx.root => forward_to_ui(&actor.ctx, name, args),
        _ => direct_tool(&actor.ws, name, args),
    }
}

fn spawn_tool(actor: &Actor, state: &mut ActorState, args: &Value) -> Result<String, String> {
    let ctx = &actor.ctx;
    let (parent, depth) = (actor.id, actor.depth);
    if depth >= MAX_DEPTH {
        return Err(format!(
            "cannot spawn: depth {depth} is the limit ({MAX_DEPTH})"
        ));
    }
    if ctx.live.load(Ordering::SeqCst) >= MAX_AGENTS {
        return Err(format!("too many agents running (max {MAX_AGENTS})"));
    }
    let brief = tools::arg_string(args, "brief")?;
    let isolated = args
        .get("isolated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !isolated && !state.running.is_empty() {
        return Err(
            "a sibling agent already runs in this shared workspace; set isolated=true \
             (its own git worktree) to work in parallel"
                .to_string(),
        );
    }

    let id = ctx.ids.fetch_add(1, Ordering::SeqCst);
    let (child_ws, branch, note) = if isolated {
        match create_worktree(&ctx.root, id, actor.branch.as_deref()) {
            Ok((path, branch)) => match Workspace::new(&path) {
                Ok(child_ws) => (child_ws, Some(branch), String::new()),
                // Isolation is best-effort: degrade to the shared workspace
                // rather than fail the delegation outright.
                Err(error) => (
                    actor.ws.clone(),
                    None,
                    format!(" (isolated unavailable: {error}; running in place)"),
                ),
            },
            Err(reason) => (
                actor.ws.clone(),
                None,
                format!(" (isolated unavailable: {reason}; running in place)"),
            ),
        }
    } else {
        (actor.ws.clone(), None, String::new())
    };

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    // The UI hears about the child before any of its events can arrive, so
    // every later event has a node to land on.
    ctx.emit(
        parent,
        AgentEvent::Spawned {
            child: id,
            parent,
            brief: brief.clone(),
            depth: depth + 1,
            branch: branch.clone(),
            cmd: cmd_tx.clone(),
        },
    );

    // Who the child is goes in the system prompt; the parent's task is the
    // first user message, mirroring the root's system+user shape. Some
    // servers' chat templates also reject a system-only first request.
    let whoami = prompt::subagent_prompt(&child_ws.root_str(), depth + 1, branch.is_some());
    let brief_text = format!("{brief}{note}");
    let initial = if brief_text.trim().is_empty() {
        vec![
            Message::system(whoami),
            Message::user("Begin the task now."),
        ]
    } else {
        vec![Message::system(whoami), Message::user(brief_text)]
    };
    // The child's own mailbox is where grandchildren report; the parent's
    // mailbox is where this child reports its completion.
    let child = Actor {
        ctx: ctx.clone(),
        id,
        depth: depth + 1,
        ws: child_ws,
        branch,
        brief: brief.clone(),
        my_tx: cmd_tx.clone(),
        parent_tx: actor.my_tx.clone(),
        rx: cmd_rx,
    };
    start(child, initial, true);

    state.children.insert(id, cmd_tx);
    state.running.insert(id);
    Ok(format!("spawned agent #{id}"))
}

fn wait_tool(
    actor: &Actor,
    state: &mut ActorState,
    cancel: &AtomicBool,
    args: &Value,
) -> Result<String, String> {
    let ids: Vec<u64> = args
        .get("ids")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let candidates: Vec<u64> = if ids.is_empty() {
        let mut children: Vec<u64> = state.children.keys().copied().collect();
        children.sort_unstable();
        children
    } else {
        ids
    };
    if candidates.is_empty() {
        return Ok("no child agents to wait for".to_string());
    }
    // Waiting on an id that was never spawned would block for the whole
    // timeout and then claim the agents are still running. Say so instead.
    let unknown: Vec<String> = candidates
        .iter()
        .filter(|id| !state.children.contains_key(id))
        .map(|id| format!("#{id}"))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "no such child agent(s): {} — agent_status lists yours",
            unknown.join(", ")
        ));
    }
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_TIMEOUT_SECS);
    // A model-supplied timeout must never overflow the clock; an
    // unrepresentable one just means "forever" (0 means that too).
    let deadline = (timeout > 0)
        .then(|| Instant::now().checked_add(Duration::from_secs(timeout)))
        .flatten();

    loop {
        // This is the one tool that blocks for minutes, so it is also the one
        // that must notice a cancellation (and a child's result) promptly.
        drain_signals(actor, cancel, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
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

fn status_tool(state: &ActorState) -> Result<String, String> {
    if state.children.is_empty() {
        return Ok("no child agents".to_string());
    }
    let mut lines = Vec::new();
    let mut ids: Vec<u64> = state.children.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        if let Some(summary) = state.completed.get(&id) {
            // A cancelled child was stopped, not finished.
            let mark = if summary == CANCELLED { "✗" } else { "✓" };
            lines.push(format!("#{id} {mark} {summary}"));
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
    let action = tools::arg_string(args, "action")?;
    let Some(cmd) = state.children.get(&id) else {
        return Err(format!("no such child agent #{id}"));
    };
    // A dead mailbox means the child is gone; saying "stopping" anyway would
    // have the model wait on a result that can never arrive.
    let sent = match action.as_str() {
        "stop" => cmd.send(AgentMsg::Stop).map_err(|_| (id, "stop")),
        "message" => {
            let text = tools::arg_string(args, "text")?;
            cmd.send(AgentMsg::Nudge(text)).map_err(|_| (id, "message"))
        }
        other => return Err(format!("unknown action `{other}` (stop or message)")),
    };
    match sent {
        Ok(()) if action == "stop" => Ok(format!("stopping agent #{id}")),
        Ok(()) => Ok(format!("messaged agent #{id}")),
        Err((id, _)) => Err(format!("agent #{id} is gone")),
    }
}

/// Commit whatever an isolated agent left in its worktree, so the branch that
/// `/diff`, `/merge`, and `/discard` name actually carries the work. Returns the
/// short revision when something was committed, `None` when the run changed
/// nothing.
///
/// The identity and the message are supplied here (`-c user.name=…`,
/// `--no-verify`) so a commit never depends on the human's git configuration and
/// never runs their hooks. The index belongs to this worktree, so committing
/// here cannot contend with the human's own git commands in the main checkout.
fn commit_worktree(root: &Path, id: u64, brief: &str) -> Result<Option<String>, String> {
    let status = git_output(root, &["status", "--porcelain"])?;
    if status.is_empty() {
        return Ok(None);
    }
    git_output(root, &["add", "-A"])?;
    let subject = format!("mush #{id}: {}", truncate(brief, 60));
    git_output(
        root,
        &[
            "-c",
            "user.name=mush",
            "-c",
            "user.email=mush@local",
            "commit",
            "--no-verify",
            "-qm",
            &subject,
        ],
    )?;
    Ok(Some(git_output(root, &["rev-parse", "--short", "HEAD"])?))
}

/// Run a git command in `dir` and return its trimmed stdout. Git never inherits
/// our stdout — the TUI owns the terminal.
fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|_| "git binary unavailable".to_string())?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("git {} failed", args.first().unwrap_or(&""))
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A private git worktree for an isolated child: `.mush/wt/<id>` on branch
/// `mush/<id>`, based on the parent's branch (or HEAD). Returns the reason
/// when isolation is impossible so callers can degrade transparently.
fn create_worktree(
    main_root: &Path,
    id: u64,
    base_branch: Option<&str>,
) -> Result<(PathBuf, String), String> {
    if !main_root.join(".git").exists() {
        return Err("not a git repository".to_string());
    }
    if base_branch.is_none() {
        // `output()`, not `status()`: the TUI owns the terminal, so git must
        // never inherit our stdout.
        let head = Command::new("git")
            .arg("-C")
            .arg(main_root)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .map_err(|_| "git binary unavailable".to_string())?;
        if !head.status.success() {
            return Err("the repo has no commits yet — commit first or drop isolated".to_string());
        }
    }
    let worktree = main_root.join(format!(".mush/wt/{id}"));
    let branch = format!("mush/{id}");
    let base = base_branch.unwrap_or("HEAD");
    let output = Command::new("git")
        .arg("-C")
        .arg(main_root)
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            worktree.to_str().unwrap_or(""),
            base,
        ])
        .output()
        .map_err(|_| "git binary unavailable".to_string())?;
    if output.status.success() {
        Ok((worktree, branch))
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if detail.is_empty() {
            "git worktree add failed".to_string()
        } else {
            detail
        })
    }
}

/// File tools for the main workspace must run on the UI thread: only it knows
/// about live buffers.
fn forward_to_ui(ctx: &AgentCtx, name: &str, args: &Value) -> Result<String, String> {
    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
    let request = ToolCallRequest {
        name: name.to_string(),
        args: args.clone(),
        reply: reply_tx,
    };
    let sent = ctx.tx.send(Msg::Tool {
        conversation: ctx.conversation,
        request,
    });
    if sent.is_err() {
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
        "list_files" => tools::list_result(ws, args, LIST_LIMIT),
        "read_file" => {
            let rel = tools::arg_string(args, "path")?;
            ws.read_file(&rel, READ_CAP)
        }
        "write_file" => {
            let rel = tools::arg_string(args, "path")?;
            let content = tools::arg_string(args, "content")?;
            ws.write_file(&rel, &content)?;
            Ok(format!("wrote {rel}"))
        }
        "edit_file" => {
            let rel = tools::arg_string(args, "path")?;
            let old = tools::arg_string(args, "old_string")?;
            let new = tools::arg_string(args, "new_string")?;
            let current = ws.read_file(&rel, usize::MAX)?;
            let updated = tools::edit_text(&current, &old, &new, &rel)?;
            ws.write_file(&rel, &updated)?;
            Ok(format!("edited {rel}"))
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn run_command(
    actor: &Actor,
    state: &mut ActorState,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `command`".to_string())?;
    run_shell(
        command,
        actor.ws.root(),
        Duration::from_secs(CMD_TIMEOUT_SECS),
        cancel,
        actor,
        state,
    )
}

/// Hard ceiling on what one command may write to its scratch files. The model
/// only ever sees the first `CMD_CAP` bytes, so a command that gets here is not
/// communicating, it is running away — and it must not fill the disk. The size
/// is checked every few milliseconds (see `wait_bounded`), so a fast writer can
/// overshoot by a few tens of MB before the kill lands.
const CMD_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

/// Why a command stopped running.
enum Ended {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    TooMuchOutput,
}

/// Run a shell command in `root` and return a report the model can read.
///
/// Output goes to scratch files rather than pipes on purpose: a pipe is only
/// complete once *every* process holding it exits, so a command that leaves a
/// background job behind (`npm run dev &`) would otherwise pin this thread
/// forever — past the timeout and past any cancellation. Files can be read
/// whenever we stop waiting, so the timeout is a real bound.
fn run_shell(
    command: &str,
    root: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    actor: &Actor,
    state: &mut ActorState,
) -> Result<String, String> {
    let out = Scratch::new("out")?;
    let err = Scratch::new("err")?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out.writer()?))
        .stderr(Stdio::from(err.writer()?));
    // Its own process group, so a signal aimed at mush never lands on a build
    // and cleanup can target everything the command started.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run command: {e}"))?;

    let ended = wait_bounded(&mut child, timeout, cancel, &out, &err, actor, state)?;
    let stdout = out.read(CMD_CAP);
    let stderr = err.read(CMD_CAP);

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
    match ended {
        Ended::Exited(status) => {
            report.push_str(&format!("[exit {}]", status.code().unwrap_or(-1)))
        }
        Ended::TimedOut => report.push_str(&format!("[timed out after {}s]", timeout.as_secs())),
        Ended::Cancelled => report.push_str("[cancelled]"),
        Ended::TooMuchOutput => report.push_str(&format!(
            "[killed: output passed {CMD_OUTPUT_LIMIT} bytes; the first {CMD_CAP} are above]"
        )),
    }
    Ok(report)
}

/// Wait for a child, stopping it when the timeout, a cancellation, or the
/// output limit arrives first.
fn wait_bounded(
    child: &mut Child,
    timeout: Duration,
    cancel: &AtomicBool,
    out: &Scratch,
    err: &Scratch,
    actor: &Actor,
    state: &mut ActorState,
) -> Result<Ended, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Ended::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                // Never leave a half-reaped child (or a running process)
                // behind on an error path.
                kill_command(child);
                return Err(format!("could not wait for command: {error}"));
            }
        }
        // A Stop or Shutdown has to reach a command *while* it runs, or Ctrl-C
        // would wait for the command to finish (up to CMD_TIMEOUT_SECS). The
        // mailbox is polled here only for signals: nudges are parked for the
        // next message boundary, never folded in mid-batch.
        drain_signals(actor, cancel, state);
        let ended = if started.elapsed() > timeout {
            Some(Ended::TimedOut)
        } else if cancel.load(Ordering::SeqCst) {
            Some(Ended::Cancelled)
        } else if out.size() + err.size() > CMD_OUTPUT_LIMIT {
            Some(Ended::TooMuchOutput)
        } else {
            None
        };
        if let Some(ended) = ended {
            kill_command(child);
            return Ok(ended);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Stop a command and everything it started. The direct child is always
/// killed; its process group catches background jobs it left behind (and, on
/// Unix, keeps them from filling the scratch file forever).
fn kill_command(child: &mut Child) {
    let group = child.id();
    let _ = child.kill();
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", &format!("-{group}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.wait();
}

/// A command's output file, removed when it is dropped.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(kind: &str) -> Result<Self, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        // The timestamp keeps a reused pid from colliding with a file left
        // behind by a crashed run (which would make `create_new` fail and the
        // command never run).
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "mush-cmd-{}-{stamp}-{unique}-{kind}",
            std::process::id()
        ));
        Ok(Self { path })
    }

    /// A write handle for the child to inherit. `create_new` plus `0600`
    /// keeps a guessable name in a shared temp directory from being a way to
    /// redirect or read what a command prints.
    fn writer(&self) -> Result<File, String> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&self.path)
            .map_err(|error| format!("cannot create {}: {error}", self.path.display()))
    }

    /// What was written, capped for the model. Reads one byte past the cap so
    /// a truncated result is marked as such.
    fn read(&self, cap: usize) -> String {
        let mut bytes = Vec::new();
        if let Ok(file) = File::open(&self.path) {
            let _ = file.take(cap as u64 + 1).read_to_end(&mut bytes);
        }
        truncate_for_model(String::from_utf8_lossy(&bytes).into_owned(), cap)
    }

    fn size(&self) -> u64 {
        fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A transcript adopted from the UI can interleave the human's steering with a
/// tool batch (they typed while the tools ran) or hold calls whose run was
/// interrupted before it recorded a result. Strict servers reject both shapes,
/// so pull every batch's results back beside the assistant message that asked
/// for them, then answer whatever is still missing.
fn repair_tool_pairs(messages: &mut Vec<Message>) {
    let mut index = 0;
    while index < messages.len() {
        let calls: Vec<String> = messages[index]
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        if calls.is_empty() {
            index += 1;
            continue;
        }
        // Results belong immediately after their assistant message. Anything
        // else that sat in between (a nudge, usually) keeps its order after the
        // batch — which is where the actor folded it at runtime.
        let mut cursor = index + 1;
        let mut insert_at = index + 1;
        while cursor < messages.len() && messages[cursor].role != "assistant" {
            let answers_a_call = messages[cursor].role == "tool"
                && messages[cursor]
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| calls.iter().any(|call| call == id));
            if answers_a_call {
                if cursor != insert_at {
                    let result = messages.remove(cursor);
                    messages.insert(insert_at, result);
                }
                cursor += 1;
                insert_at += 1;
            } else {
                cursor += 1;
            }
        }
        // A call with no result (the run was interrupted mid-batch) would
        // dangle forever; give it an explicit error the model can act on.
        for id in &calls {
            let answered = messages[index + 1..insert_at]
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id.as_str()));
            if !answered {
                messages.insert(
                    insert_at,
                    Message::tool(
                        id.clone(),
                        "error: no result was recorded for this call (the run was interrupted)",
                    ),
                );
                insert_at += 1;
            }
        }
        index = insert_at;
    }
}

/// A model occasionally emits `tool_call` arguments that are not valid JSON.
/// Sending that message back into history verbatim makes some servers reject
/// the whole request with a parse error; rewrite invalid arguments to `{}` so
/// the tool executor returns a clear per-call error instead.
fn sanitize_tool_calls(mut message: Message) -> Message {
    let Some(calls) = message.tool_calls.as_mut() else {
        return message;
    };
    for call in calls {
        // Arguments must be a JSON object; a bare string passes JSON parsing
        // but makes servers reject the message outright.
        if !matches!(
            serde_json::from_str::<Value>(&call.function.arguments),
            Ok(Value::Object(_))
        ) {
            call.function.arguments = "{}".to_string();
        }
    }
    message
}

/// Drop the oldest turns until the conversation fits the budget. Trimming at a
/// user message keeps assistant/tool pairs intact, which servers validate.
/// The budget comes from the endpoint's context window.
fn trim_history(messages: &mut Vec<Message>, budget: usize) {
    loop {
        let total: usize = messages.iter().map(Message::weight).sum();
        if total <= budget {
            return;
        }
        let user_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "user")
            .map(|(i, _)| i)
            .collect();
        // `messages[0]` is the system prompt and `messages[1]` the opening
        // task — for subagents that is the parent's brief, which must survive
        // trimming. System + task + the newest turn is the minimum shape.
        if user_indices.len() < 3 {
            return;
        }
        // Drop the oldest full turn: everything after the task message up to
        // the third user message, cutting at user boundaries so pairs stay
        // valid. Guarded so the drain can never be a no-op (which would spin
        // here forever) on a transcript that does not start with system+user.
        let keep_from = user_indices[2];
        if keep_from <= 2 {
            return;
        }
        messages.drain(2..keep_from);
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

#[cfg(test)]
mod tests {
    use super::*;
    use mush_core::{FunctionCall, ToolCall};
    use serde_json::json;

    #[test]
    fn summarize_prefers_paths_then_commands_then_briefs() {
        assert_eq!(summarize(&json!({"path": "a.rs"})), "a.rs");
        assert_eq!(summarize(&json!({"command": "ls -la"})), "ls -la");
        assert_eq!(
            summarize(&json!({"brief": "fix the parser"})),
            "fix the parser"
        );
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
        // Mirror the 8K-context default budget from Config::history_budget.
        trim_history(&mut messages, 15_000);
        assert_eq!(messages[0].role, "system");
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= 15_000);
        // The first kept entry must be a user message so pairs stay valid.
        assert_eq!(messages[1].role, "user");
    }

    /// A transcript that does not open with `system, user` must not make the
    /// trimmer spin: the guard returns instead of draining nothing forever.
    #[test]
    fn trim_history_terminates_on_a_system_less_transcript() {
        let mut messages = vec![Message::user("a"), Message::user("b"), Message::user("c")];
        trim_history(&mut messages, 0);
        assert_eq!(
            messages.len(),
            3,
            "nothing can be trimmed without a pair to keep"
        );
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }
    }

    fn assistant_calling(ids: &[&str]) -> Message {
        let mut message = Message::assistant("working");
        message.tool_calls = Some(ids.iter().map(|id| call(id)).collect());
        message
    }

    fn roles(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.role.as_str())
            .collect()
    }

    /// The human typed while a tool batch was running: the UI's transcript puts
    /// their words between the assistant's calls and the results. Adopting it
    /// verbatim would make strict servers reject every later request.
    #[test]
    fn steering_inside_a_tool_batch_moves_after_the_results() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a", "b"]),
            Message::user("actually, also do X"),
            Message::tool("a", "result a"),
            Message::tool("b", "result b"),
            Message::assistant("done"),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(
            roles(&messages),
            [
                "system",
                "user",
                "assistant",
                "tool",
                "tool",
                "user",
                "assistant"
            ]
        );
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("a"));
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("b"));
        assert_eq!(messages[5].text(), "actually, also do X");
    }

    /// Quitting during a batch can persist an assistant message whose tool
    /// calls never got results; the next request must still be answerable.
    #[test]
    fn a_dangling_tool_call_is_answered_with_an_error() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a", "b"]),
            Message::user("carry on"),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(
            roles(&messages),
            ["system", "user", "assistant", "tool", "tool", "user"]
        );
        assert!(messages[3].text().starts_with("error:"));
        assert!(messages[4].text().starts_with("error:"));
    }

    /// An already-valid transcript must come out byte-for-byte unchanged.
    #[test]
    fn repair_leaves_a_valid_transcript_alone() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::tool("a", "result"),
            Message::user("next"),
        ];
        let before = serde_json::to_string(&messages).unwrap();
        repair_tool_pairs(&mut messages);
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);
    }

    /// A stopped child is not a finished one; agent_status must say so, or the
    /// parent treats a cancelled result as a successful one.
    #[test]
    fn agent_status_marks_a_cancelled_child() {
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx.clone());
        state.completed.insert(1, CANCELLED.to_string());
        state.children.insert(2, tx);
        state.completed.insert(2, "did the thing".to_string());

        let lines = status_tool(&state).unwrap();
        assert!(lines.contains("#1 ✗ cancelled"), "{lines}");
        assert!(lines.contains("#2 ✓ did the thing"), "{lines}");
    }

    /// The repair must be wired into the one path that adopts a transcript
    /// wholesale, or it never runs where it matters.
    #[test]
    fn adopting_a_ui_transcript_repairs_tool_pairs() {
        let (actor, _mailbox) = test_actor("adopt");
        let mut state = ActorState::default();
        let mut messages = Vec::new();
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::user("steer"),
            Message::tool("a", "result"),
        ];
        let folded = absorb(&mut state, &mut messages, AgentMsg::Run(fresh));
        assert!(matches!(folded, Fold::Run));
        assert_eq!(
            roles(&messages),
            ["system", "user", "assistant", "tool", "user"]
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A completion that the incoming transcript already carries (as a
    /// `wait_agents` result, say) is not news: announcing it again would spend
    /// a turn repeating what the model just read.
    #[test]
    fn adopting_a_transcript_that_announces_a_completion_keeps_it_delivered() {
        let (actor, _mailbox) = test_actor("delivered-yes");
        let mut state = ActorState::default();
        let mut messages = Vec::new();
        state.completed.insert(1, "did the thing".to_string());
        state.delivered.insert(1);
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::tool("a", "#1 done: did the thing"),
        ];

        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert!(
            state.delivered.contains(&1),
            "the model reads it in the transcript, so it is already delivered"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    #[test]
    fn sanitize_repairs_invalid_tool_call_json() {
        let mut message = Message::assistant("here you go");
        message.tool_calls = Some(vec![
            ToolCall {
                id: "a".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\": \"ok.rs\"}".into(),
                },
            },
            ToolCall {
                id: "b".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "edit_file".into(),
                    arguments: "\"Please retry with a smaller context\"".into(),
                },
            },
        ]);
        let repaired = sanitize_tool_calls(message);
        let calls = repaired.tool_calls();
        assert_eq!(calls[0].function.arguments, "{\"path\": \"ok.rs\"}");
        assert_eq!(calls[1].function.arguments, "{}");
    }

    /// A background job inherits the command's output file, so leaving one
    /// behind must not hold the tool (and the agent) hostage.
    #[test]
    fn a_background_job_does_not_hold_the_tool_hostage() {
        let (actor, _mailbox) = test_actor("background");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30 & echo started",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(report.contains("started"), "{report}");
        assert!(report.contains("[exit 0]"), "{report}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The timeout is a real bound, and the report says what happened.
    #[test]
    fn a_command_that_runs_forever_is_killed_on_time() {
        let (actor, _mailbox) = test_actor("timeout");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30",
            &std::env::temp_dir(),
            Duration::from_millis(200),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(report.contains("timed out"), "{report}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Ctrl-C reaches a command that is still running; the tool returns at once
    /// and says why. A plain flag is enough: `run_shell` polls the mailbox
    /// itself, so a real Stop lands the same way.
    #[test]
    fn a_running_command_can_be_cancelled() {
        let (actor, _mailbox) = test_actor("cancel");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(report.contains("[cancelled]"), "{report}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A Stop that arrives while a command runs is noticed by the command, not
    /// left for the end of the batch.
    #[test]
    fn a_stop_in_the_mailbox_interrupts_a_running_command() {
        let (actor, mailbox) = test_actor("stop-command");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        mailbox.send(AgentMsg::Stop).unwrap();
        let report = run_shell(
            "echo starting; sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(report.contains("[cancelled]"), "{report}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command that writes without end is stopped at the disk limit rather
    /// than filling the filesystem — the model only ever sees the first chunk.
    #[test]
    fn a_runaway_writer_is_stopped_at_the_output_limit() {
        let (actor, _mailbox) = test_actor("runaway");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let report = run_shell(
            "yes mush",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(report.contains("output passed"), "{report}");
        assert!(report.len() < CMD_CAP * 2, "report grew: {}", report.len());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Output longer than the cap is truncated and marked, and the command
    /// still finishes (nothing blocks on a full pipe).
    #[test]
    fn long_output_is_capped_and_marked() {
        let (actor, _mailbox) = test_actor("long-output");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let report = run_shell(
            "yes mush | head -c 40000",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(
            report.contains("[mush: output truncated]"),
            "cap was not marked"
        );
        assert!(
            report.len() < CMD_CAP * 2,
            "report grew past the cap: {}",
            report.len()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A standalone actor over a scratch workspace, for exercising the mailbox
    /// plumbing with no model, no UI, and no threads.
    fn test_actor(label: &str) -> (Actor, Sender<AgentMsg>) {
        let root = std::env::temp_dir().join(format!("mush-actor-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let (ui_tx, ui_rx) = crossbeam_channel::unbounded::<Msg>();
        std::mem::forget(ui_rx);
        let ctx = Arc::new(AgentCtx {
            cfg: Arc::new(Mutex::new(Config::new("http://127.0.0.1:1", "test", None))),
            tx: ui_tx.clone(),
            conversation: 1,
            root: root.clone(),
            ids: Arc::new(AtomicU64::new(1)),
            live: Arc::new(AtomicU64::new(0)),
        });
        let (my_tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(dead_rx);
        let actor = Actor {
            ctx,
            id: 7,
            depth: 0,
            ws: Workspace::new(&root).unwrap(),
            branch: None,
            brief: String::new(),
            my_tx: my_tx.clone(),
            parent_tx: dead_tx,
            rx,
        };
        (actor, my_tx)
    }

    /// The bug this guards: re-queuing a parked nudge into the actor's own
    /// mailbox spins forever, because `my_tx` *is* the queue being drained.
    #[test]
    fn drain_signals_parks_nudges_for_the_next_boundary() {
        let (actor, mailbox) = test_actor("signals");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        mailbox.send(AgentMsg::Nudge("steer left".into())).unwrap();
        mailbox.send(AgentMsg::Stop).unwrap();

        drain_signals(&actor, &cancel, &mut state);
        assert!(cancel.load(Ordering::SeqCst), "a Stop is honoured at once");
        assert_eq!(state.deferred.len(), 1, "the nudge is parked, not dropped");
        assert!(
            actor.rx.try_recv().is_err(),
            "nothing may be put back in the queue we are draining"
        );

        // The next message boundary folds it in, in order.
        let mut messages = vec![Message::assistant("working")];
        drain_mailbox(&actor.rx, &cancel, &mut messages, &mut state);
        assert!(state.deferred.is_empty());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "steer left");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// `Stop` cancels work and is a no-op for an idle agent; only `Shutdown`
    /// ends one — which is what keeps `/new` from leaving an orphan root
    /// behind that still answers to agent #0.
    #[test]
    fn stop_cancels_but_shutdown_ends() {
        let (actor, mailbox) = test_actor("shutdown");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        mailbox.send(AgentMsg::Stop).unwrap();
        let cancel = AtomicBool::new(false);
        drain_mailbox(&actor.rx, &cancel, &mut messages, &mut state);
        assert!(cancel.load(Ordering::SeqCst), "a Stop cancels the run");
        assert!(!state.shutdown, "a Stop must not end the actor");

        mailbox.send(AgentMsg::Shutdown).unwrap();
        drain_mailbox(&actor.rx, &cancel, &mut messages, &mut state);
        assert!(
            state.shutdown,
            "a Shutdown ends the actor once the run stops"
        );

        // Idle: the same split, expressed as what the actor should do next.
        let mut state = ActorState::default();
        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Stop),
            Fold::Idle
        ));
        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Shutdown),
            Fold::End
        ));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A completion that the model has not read yet must be delivered when the
    /// UI's transcript replaces the actor's, or the result is lost for good.
    #[test]
    fn replacing_the_transcript_puts_completions_back_on_the_delivery_list() {
        let (actor, _mailbox) = test_actor("deliver");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];
        state.completed.insert(1, "did the thing".to_string());
        state.delivered.insert(1);

        let fresh = vec![Message::system("you are mush"), Message::user("carry on")];
        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert!(
            !state.delivered.contains(&1),
            "the new transcript has no #1 done line, so it is undelivered again"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The full orchestration path, headless: root spawns an isolated child,
    /// the child writes into its own worktree, the parent waits and collects
    /// the summary. The model is `scripts/mock_llm.py` — deterministic.
    #[test]
    #[ignore = "needs python3 + git; spawns a local mock model server"]
    fn isolated_subagent_writes_its_worktree() {
        use std::fs;

        const PORT: u16 = 18_731;
        let mock = start_mock(PORT);

        // A real git repo so `create_worktree` has something to branch from.
        let root = init_git_repo("iso");

        let cfg = Config::new(format!("http://127.0.0.1:{PORT}"), "mock", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let root_tx = spawn(cfg, tx, root.clone()).tx;

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
        // The run's end commits the worktree, so the branch mush advertises for
        // the child (and tells the human to diff and merge) carries the file.
        let branch_files =
            git_output(&root, &["diff", "--name-only", "HEAD...mush/1"]).unwrap_or_default();
        // …and nothing is left behind as an uncommitted change.
        let worktree_status = git_output(&root.join(".mush/wt/1"), &["status", "--porcelain"])
            .unwrap_or_else(|error| error);
        // The three commands mush prints must now do what they say: merge the
        // work back, then let go of the worktree and the branch.
        let merged = git_output(
            &root,
            &[
                "-c",
                "user.name=mush",
                "-c",
                "user.email=mush@local",
                "merge",
                "--no-edit",
                "mush/1",
            ],
        );
        let merged_into_workspace = root.join("iso.txt").exists();
        let removed = git_output(&root, &["worktree", "remove", ".mush/wt/1"]);
        let deleted = git_output(&root, &["branch", "-D", "mush/1"]);
        stop_mock(mock);
        let _ = fs::remove_dir_all(&root);
        assert_eq!(found.as_deref(), Some("isolated work"));
        assert_eq!(done_events, 3, "root must be woken when its child finishes");
        assert!(
            branch_files.contains("iso.txt"),
            "the branch must carry the child's work, got {branch_files:?}"
        );
        assert!(
            worktree_status.is_empty(),
            "the worktree must be left clean, got {worktree_status:?}"
        );
        assert!(merged.is_ok(), "/merge must merge: {merged:?}");
        assert!(
            merged_into_workspace,
            "after /merge the file must be in the human's workspace"
        );
        assert!(
            removed.is_ok(),
            "/discard must remove the worktree: {removed:?}"
        );
        assert!(
            deleted.is_ok(),
            "/discard must delete the branch: {deleted:?}"
        );
    }

    /// Root -> child -> grandchild, each isolated: the grandchild's file must
    /// land in `.mush/wt/2/` on a branch that carries it, branched off the
    /// child's worktree (`mush/2` based on `mush/1`), and the summaries bubble up
    /// through wait_agents.
    #[test]
    #[ignore = "needs python3 + git; spawns a local mock model server"]
    fn deep_chain_writes_nested_worktrees() {
        use std::fs;

        const PORT: u16 = 18_732;
        let mock = start_mock(PORT);
        let root = init_git_repo("chain");

        let cfg = Config::new(format!("http://127.0.0.1:{PORT}"), "mock", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let root_tx = spawn(cfg, tx, root.clone()).tx;

        let messages = vec![
            Message::system(prompt::system_prompt(root.to_str().unwrap())),
            Message::user(
                "CHAIN: delegate the file creation through two levels of subagents".to_string(),
            ),
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

        // Nested worktree, now with real history: the grandchild's branch
        // carries its work and is based on the child's branch, which — because
        // the child only delegated — still sits at the branch point. And the
        // child's worktree must NOT contain the grandchild's file.
        let grandchild_files =
            git_output(&root, &["diff", "--name-only", "HEAD...mush/2"]).unwrap_or_default();
        let child_head = git_rev_parse(&root, "mush/1");
        let base_head = git_rev_parse(&root, "HEAD");
        let branched_from_child =
            git_output(&root, &["merge-base", "--is-ancestor", "mush/1", "mush/2"]).is_ok();
        let child_has_file = root.join(".mush/wt/1/deep.txt").exists();

        stop_mock(mock);
        let _ = fs::remove_dir_all(&root);
        assert_eq!(
            found.as_deref(),
            Some("deep work"),
            "grandchild must write its own worktree"
        );
        assert_eq!(done_events, 3, "each level must run and finish once");
        assert!(
            grandchild_files.contains("deep.txt"),
            "mush/2 must carry the grandchild's work, got {grandchild_files:?}"
        );
        assert_eq!(
            child_head.as_deref(),
            base_head.as_deref(),
            "the child delegated, so its own branch stays at the branch point"
        );
        assert!(
            branched_from_child,
            "mush/2 must be based on mush/1 (nested, not re-rooted)"
        );
        assert!(
            !child_has_file,
            "the child's worktree must stay clean of grandchild work"
        );
    }

    /// A transcript that fills the (tiny, configured) context window must be
    /// folded into a summary — not dropped — and the run continues from it.
    #[test]
    #[ignore = "needs python3 + git; spawns a local mock model server"]
    fn compaction_folds_overflowing_history_into_a_summary() {
        use std::fs;

        const PORT: u16 = 18_733;
        let mock = start_mock(PORT);
        let root = init_git_repo("compact");

        let mut cfg = Config::new(format!("http://127.0.0.1:{PORT}"), "mock", None);
        // Tiny window: history_budget() = 3 * (ctx - 3348) bytes = 1956.
        cfg.context_tokens = 4_000;
        let budget = cfg.history_budget();

        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let root_tx = spawn(cfg, tx, root.clone()).tx;

        // ~1.6 KB of history: above 3/4 of the budget but still fitting, so
        // compaction must trigger instead of trimming.
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("create a file via subagents".to_string()),
        ];
        for i in 0..5 {
            messages.push(Message::assistant(format!("reply {i} {}", "x".repeat(280))));
            messages.push(Message::user(format!("again {i}")));
        }
        let total: usize = messages.iter().map(Message::weight).sum();
        assert!(
            total > budget * 3 / 4 && total <= budget,
            "test transcript must sit in the compaction window (total {total}, budget {budget})"
        );
        root_tx.send(AgentMsg::Run(messages)).unwrap();

        // Expect a Compact event, then the run continuing (the mock resumes
        // the DEFAULT scenario from the summary and spawns the iso child).
        let mut compact = 0usize;
        let mut done = 0usize;
        let deadline = Instant::now() + Duration::from_secs(10);
        while (compact == 0 || done == 0) && Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Agent { event, .. } = msg {
                    if matches!(event, AgentEvent::Compact { .. }) {
                        compact += 1;
                    }
                    if matches!(event, AgentEvent::Done) {
                        done += 1;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let iso = root.join(".mush/wt/1/iso.txt");
        let iso_ok =
            iso.exists() && fs::read_to_string(&iso).ok().as_deref() == Some("isolated work");

        stop_mock(mock);
        let _ = fs::remove_dir_all(&root);
        assert!(compact >= 1, "history must be compacted into a summary");
        assert!(done >= 1, "the run must finish after compaction");
        assert!(
            iso_ok,
            "the task must survive compaction (iso.txt in the worktree)"
        );
    }

    /// The human types while the model is answering: the nudge must land after
    /// that reply and be answered, never silently swallowed when the run would
    /// otherwise end. The mock holds its first reply open — writing a marker,
    /// so the nudge is provably in flight — and answers "steered" only once it
    /// receives the nudge.
    #[test]
    #[ignore = "needs python3; spawns a local mock model server"]
    fn a_nudge_that_arrives_mid_reply_is_answered() {
        use std::fs;

        const PORT: u16 = 18_734;
        let root = std::env::temp_dir().join(format!("mush-steer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("in-flight");
        let mock = start_mock_with(PORT, &[marker.to_str().unwrap()]);

        let cfg = Config::new(format!("http://127.0.0.1:{PORT}"), "mock", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let root_tx = spawn(cfg, tx, root.clone()).tx;

        let messages = vec![
            Message::system("you are mush"),
            Message::user("STEER: answer this, then whatever else I say".to_string()),
        ];
        root_tx.send(AgentMsg::Run(messages)).unwrap();

        // Only nudge once the mock has the reply in hand.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "the first request never reached the mock");
        root_tx
            .send(AgentMsg::Nudge("STEERME".to_string()))
            .unwrap();

        let (mut first, mut steered) = (false, false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(first && steered) && Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Agent {
                    event: AgentEvent::Message(message),
                    ..
                } = msg
                {
                    match message.text().trim() {
                        "first reply" => first = true,
                        "steered" => steered = true,
                        _ => {}
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        stop_mock(mock);
        let _ = fs::remove_dir_all(&root);
        assert!(first, "the first reply should arrive while the nudge waits");
        assert!(steered, "the nudge must be answered, not swallowed");
    }

    /// Start the scripted mock model server and wait until it answers.
    fn start_mock(port: u16) -> Child {
        start_mock_with(port, &[])
    }

    /// Same, with extra script arguments (the STEER scenario takes a marker
    /// path so the test knows when its first reply is in flight).
    fn start_mock_with(port: u16, extra: &[&str]) -> Child {
        use std::process::Command;

        const MOCK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/mock_llm.py");
        let mock = Command::new("python3")
            .arg(MOCK)
            .arg(port.to_string())
            .args(extra)
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
                .stdout(Stdio::null())
                .stderr(Stdio::null())
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

    /// Kill the mock server and reap it, so the suite leaves no zombies.
    fn stop_mock(mut mock: Child) {
        let _ = mock.kill();
        let _ = mock.wait();
    }

    /// A scratch git repo with one initial commit, ready for worktrees. The
    /// label keeps parallel tests from sharing a directory.
    fn init_git_repo(label: &str) -> PathBuf {
        use std::fs;
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("mush-chain-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
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
                if matches!(
                    msg,
                    Msg::Agent {
                        event: AgentEvent::Done,
                        ..
                    }
                ) {
                    done_events += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done_events
    }
}
