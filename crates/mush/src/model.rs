//! The model seam: one trait, the real client, and a scripted fake.
//!
//! Every chat completion mush asks for goes through [`ModelClient`]. `http.rs`
//! below it does framing, read slices, caps and TLS and nothing else; this
//! module owns what a *model call* is — encoding the request, reading the
//! reply, and saying what went wrong in terms the run loop can act on (a
//! cancellation, a refusal, a status the endpoint chose).
//!
//! The seam is one method wide on purpose. A queue of scripted replies is a
//! complete implementation, so `run_loop`, compaction, the learned-context
//! retry and a cancellation mid-reply are all assertable in process — no
//! socket, no thread, no sleep. And a whole tree shares one client, so a
//! scripted model can serve a parent, its children and their children: each
//! reply may say which request it is the answer to, because a child's first
//! request races its parent's next one.
//!
//! A model call is also where the retry lives: [`retrying`] repeats a request
//! whose *wire* failed — the transport breaking, or a reply whose framing broke
//! before its body was read — and never one the endpoint answered. The loop is
//! driven by the caller, because the caller is the layer that holds the clock
//! the backoff waits on, the cancel flag the human's Stop sets, and the agent
//! whose transcript the retry is announced in.

use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mush_core::message::{ChatRequest, ChatResponse};

use crate::app::ConfigHandle;
use crate::clock::Clock;
use crate::http;

/// Why a chat call produced no reply.
///
/// The variants are separate so the *caller* can say what happened instead of
/// guessing from a string: a cancellation is not a failure to reach the
/// endpoint, and a refusal (a reply past the body cap, a malformed status or
/// chunk line) is not a connection error either — that distinction is what
/// tells the human whether to check the URL or the size of the reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// The human cancelled (Ctrl-C, `control stop`, Ctrl-N) while the
    /// reply was in flight.
    Cancelled,
    /// The request could not be encoded.
    Encode(String),
    /// The endpoint could not be reached, and asking again would ask the same
    /// broken question: a URL mush cannot parse, a name that does not resolve,
    /// a TLS handshake that failed, a config cell that would not be read.
    Unreachable(String),
    /// The wire failed under a request the endpoint never answered — or a reply
    /// that was still coming in was cut off before its body framed itself: a
    /// connection reset or refused, an unexpected end of stream, a connect or
    /// read timeout, a chunked body whose stream ended inside it. One of the two
    /// failures [`retrying`] may ask again: nothing of the reply was handed to
    /// the caller, and a completion has no effect on the endpoint's state, so
    /// asking again cannot duplicate work. What a retry on a body cut in half
    /// costs is tokens, and it saves the run (finding B23).
    Transport(String),
    /// The reply's framing broke before its body could be read: a status line
    /// or `Content-Length` that is not one, a chunk size that is not hex, a
    /// chunk terminator the framing did not promise. [`http`] raises these, and
    /// they are *not* an answer the endpoint chose — nothing of the reply was
    /// handed over, so asking again cannot duplicate anything a run has read.
    /// The remedy is a fresh connection, never the one that carried the broken
    /// frame: `http` drops every failing connection rather than returning it to
    /// the pool, so the retry cannot read the same leftover framing again. This
    /// is also why a retry is safe if the break was *ours* — a kept connection
    /// whose previous body was not fully consumed cannot exist, and the one
    /// that produced the error is gone (finding B27).
    Framing(String),
    /// The endpoint answered, but its reply was refused before it could be
    /// read: a body past `http`'s cap. A malformed status line or chunk line is
    /// *not* here — those are [`ModelError::Framing`], a reply that broke on the
    /// way in rather than an answer.
    Refused(String),
    /// The endpoint answered with a status other than 200. `body` is what it
    /// said, verbatim: the caller reads the endpoint's own complaint out of it
    /// (and, for a context-limit complaint, the window it names).
    Status { status: u16, body: String },
    /// The reply body was not a chat completion.
    Malformed(String),
}

/// One method: ask the model, get its reply or the reason there is none.
///
/// `Send + Sync` because one client is shared by every agent in a tree: a
/// child's run and its parent's go through the same value, which is also what
/// lets a scripted client serve a whole tree.
///
/// The one contract beyond the signature: a call that returns `Err` has handed
/// the caller *nothing of a reply*. [`retrying`] reads that rule when it decides
/// what may be asked again — a repeat can only duplicate work if part of the
/// first reply was already used — so a client that ever streams a partial body
/// must report a later failure as something that is never retried. `HttpModel`
/// satisfies it by construction: it makes a `Response` only from a body that
/// framed itself (finding B27).
pub trait ModelClient: Send + Sync {
    /// `cancel` is the flag the human's Stop sets; the call must notice it
    /// while it waits, not only once the endpoint has answered.
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<ChatResponse, ModelError>;
}

/// The real endpoint: the config cell the actors already share, and the
/// transport below it.
pub struct HttpModel {
    cfg: ConfigHandle,
}

impl HttpModel {
    pub fn new(cfg: ConfigHandle) -> Self {
        Self { cfg }
    }
}

impl ModelClient for HttpModel {
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<ChatResponse, ModelError> {
        // One snapshot per request, not a lock held across the call.
        let cfg = self.cfg.config().map_err(ModelError::Unreachable)?;

        let body = serde_json::to_string(request)
            .map_err(|error| ModelError::Encode(error.to_string()))?;

        let response = match http::post_json(&cfg.chat_url(), &body, cfg.api_key.as_deref(), cancel)
        {
            Ok(response) => response,
            // The reader stops the moment the human cancels; that is a
            // cancellation, not a failure to reach the endpoint. The flag is
            // checked first, so a request that failed *because* of the cancel
            // is reported as one however the socket reported it.
            Err(_) if cancel.load(Ordering::SeqCst) => return Err(ModelError::Cancelled),
            // The reply's framing broke before its body was read: nothing of
            // the reply exists for this caller to have used — a `Response` is
            // only ever made from a body that framed itself — and `http`
            // dropped the connection that carried it. Its own class, because
            // `InvalidData` alone also covers the answers mush *refuses* (a
            // body past the cap), and those are never retried (finding B27).
            Err(error) if http::is_framing(&error) => {
                return Err(ModelError::Framing(error.to_string()))
            }
            // A refusal is not a connection failure: the endpoint answered.
            Err(error) if error.kind() == ErrorKind::InvalidData => {
                return Err(ModelError::Refused(error.to_string()));
            }
            // Which of the two a failed call is decides whether asking again
            // is honest, so the *kind* travels with the message instead of
            // being flattened into a string no caller can classify.
            Err(error) if transport(&error) => {
                return Err(ModelError::Transport(error.to_string()))
            }
            Err(error) => return Err(ModelError::Unreachable(error.to_string())),
        };

        // Any other status is the endpoint's own verdict on the request, and
        // the body is the only place its reason lives. Handed back verbatim:
        // what to do about it (learn the window and retry, or fail) is the
        // caller's decision, not the transport's.
        if response.status != 200 {
            return Err(ModelError::Status {
                status: response.status,
                body: response.body,
            });
        }

        serde_json::from_str::<ChatResponse>(&response.body)
            .map_err(|error| ModelError::Malformed(error.to_string()))
    }
}

/// Whether `error` is the wire failing rather than the endpoint answering: a
/// connection reset or refused, an end of stream where a reply should have
/// been, a connect or a read that timed out.
///
/// What is *not* here is as deliberate: `InvalidData` is a refusal `http.rs`
/// already classified, and `Interrupted` is how the cancel flag is reported. A
/// reply whose *framing* broke is its own class too ([`ModelError::Framing`]):
/// it is retried, but through the marker `http.rs` raises for it rather than
/// through this kind, which would also sweep a body past the cap into the retry
/// net. A signal that interrupted a read or a write — the human resizing the
/// terminal — was already made again inside `http.rs`, so the interrupt itself
/// never reaches this classifier (finding B25); the only `Interrupted` that
/// arrives here is a decision, and a cancellation is never retried.
/// `NotFound`/`InvalidInput`/`Other` are a name that does not resolve, a URL
/// that cannot be parsed and a TLS handshake that failed — misconfiguration,
/// where a retry only repeats the mistake.
fn transport(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
            | ErrorKind::WouldBlock
    )
}

/// How many times one model call is attempted: the first try and two retries.
/// A fourth would be a loop wearing a policy's clothes.
pub const RETRY_ATTEMPTS: usize = 3;
/// What the first backoff waits, doubling for the retry after it. Short on
/// purpose: the hiccup this exists for clears in a moment, and a human who is
/// being told what is happening does not need mush to wait a minute to be sure.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// The longest a backoff sleeps before the cancel flag is read again. Short
/// enough that Ctrl-C during a pause lands at once, and the same shape as the
/// poll in `agent.rs`'s waits (docs/mush.md §5.5).
const BACKOFF_SLICE: Duration = Duration::from_millis(50);

/// One model call, with the bounded retry a transport hiccup deserves (finding
/// B23: three agents in one session died mid-work on `Connection reset by
/// peer`, which was the network and not the endpoint).
///
/// `attempt` is one whole try — for the real client, one `http::post_json` —
/// and it must obey the [`ModelClient`] contract: an `Err` means the caller got
/// nothing of the reply, which is what makes a repeat safe. This decides
/// whether a failed try is worth another, waits the backoff on the run's own
/// [`Clock`], and hands every retry to `announce`, so the human reads
/// `Connection reset by peer (os error 104) — retrying (2/3)` in the transcript
/// instead of watching a spinner that looks stuck.
///
/// Two failures are repeated: a [`ModelError::Transport`] one, and a
/// [`ModelError::Framing`] one. Both mean the reply was never handed to the
/// caller — `http.rs` makes a `Response` only from a body that framed itself —
/// so asking again cannot duplicate anything the run has read, and a completion
/// has no effect on the endpoint's state, so it cannot replay a write the way
/// re-sending a `POST` that changed something would. What a retry on a body cut
/// in half costs is tokens — and it saves the run.
///
/// A framing error is retried on a *fresh* connection by construction: `http.rs`
/// never returns a connection whose body did not read to its own end to the
/// pool, and it drops the one that carried the broken frame, so the second
/// attempt cannot be handed the leftover framing that may have produced the
/// first (finding B27). That is the reason a retry is the honest answer here and
/// not a bug hidden behind one: if the stray empty chunk-size line was ours,
/// the connection that held it is gone; if it was the peer's, one fresh ask is
/// the cheapest way to find out.
///
/// Everything else is returned at once, unchanged: a cancellation, a status the
/// endpoint chose (4xx *and* 5xx: an answer is not a hiccup, and the 400 that
/// teaches mush a smaller window already has exactly one retry of its own in
/// the run loop), a refusal (a body past the cap), and a body that arrived and
/// did not parse are all answers.
///
/// Worst case: [`RETRY_ATTEMPTS`] attempts, each bounded by the transport below
/// it (`http.rs`: 5 s to connect, 30 s to write, a 600 s read deadline — and
/// `http.rs` may itself replace a kept connection that died unheard, once,
/// inside one attempt), plus 1.5 s of backoff. So roughly half an hour for an
/// endpoint that stalls three times and loses every time, and a few hundred
/// milliseconds for the reset this is here for. The one unbounded step is the
/// one already documented: resolving a host has no timeout (docs/mush.md §8),
/// and that is not a retry's to fix.
pub fn retrying<T>(
    clock: &dyn Clock,
    cancel: &AtomicBool,
    announce: impl Fn(&str),
    mut attempt: impl FnMut() -> Result<T, ModelError>,
) -> Result<T, ModelError> {
    let mut tries = 1;
    loop {
        // A Stop outranks everything: before the first attempt, and between
        // every pair of them.
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::Cancelled);
        }
        let error = match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        // A failure the wire produced, not one the endpoint answered: the
        // transport broke, or the reply's framing did. Anything else — a
        // cancellation, the endpoint's own verdict, a refusal, a body that did
        // not parse — is an answer, and is never asked for twice.
        let message = match &error {
            ModelError::Transport(message) | ModelError::Framing(message) => message.clone(),
            _ => return Err(error),
        };
        if tries == RETRY_ATTEMPTS {
            // The endpoint's own message, plus the fact that it is not the
            // first time it was heard: a retry layer that replaced it with a
            // bare "unreachable after retries" would hide the only detail the
            // human can act on.
            return Err(error.after_attempts());
        }
        announce(&format!(
            "{message} — retrying ({}/{RETRY_ATTEMPTS})",
            tries + 1
        ));
        wait(clock, cancel, tries)?;
        tries += 1;
    }
}

/// The pause before the retry after `tries` attempts, in slices the cancel flag
/// is read between.
///
/// One long `sleep` would make Ctrl-C wait out the whole backoff, which is the
/// one thing a cancellation may never do; and a `Cancelled` returned from here
/// is the run stopping, not the wire failing.
fn wait(clock: &dyn Clock, cancel: &AtomicBool, tries: usize) -> Result<(), ModelError> {
    let mut left = RETRY_BACKOFF * 2u32.pow(tries as u32 - 1);
    while !left.is_zero() {
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::Cancelled);
        }
        let slice = left.min(BACKOFF_SLICE);
        clock.sleep(slice);
        left -= slice;
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(ModelError::Cancelled);
    }
    Ok(())
}

impl ModelError {
    /// The same failure, saying how many times it was asked. Only the two
    /// retryable classes carry a count; every other error is returned from
    /// [`retrying`] before this is reached.
    fn after_attempts(self) -> Self {
        let say = |message: String| format!("{message} — {RETRY_ATTEMPTS} attempts failed");
        match self {
            ModelError::Transport(message) => ModelError::Transport(say(message)),
            ModelError::Framing(message) => ModelError::Framing(say(message)),
            other => other,
        }
    }
}

/// A scripted model, for tests: a queue of replies, and a log of the requests
/// that consumed them.
///
/// Nothing here opens a socket, starts a thread or sleeps, so a whole
/// `run_loop` — tool calls, the learned-context retry, a cancellation —
/// is asserted in process.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crossbeam_channel::{Receiver, Sender};

    use mush_core::message::{
        ChatRequest, ChatResponse, Choice, FunctionCall, Message, ToolCall, Usage,
    };
    use serde_json::Value;

    use super::{ModelClient, ModelError};

    /// The request the caller made, copied out of the borrow it held — the
    /// caller's transcript does not outlive the call, so a test that wants to
    /// assert "the second request carried the tool result" reads it here.
    #[derive(Clone, Debug)]
    pub struct Asked {
        pub model: String,
        pub messages: Vec<Message>,
        pub tools: usize,
        /// The schemas themselves, not just how many there were. They are the
        /// head of the rendered prompt, so whether two requests share a
        /// cacheable prefix is a question about these bytes — a count of zero
        /// and a count of eight are not the comparison a test needs.
        pub tool_schemas: Vec<Value>,
        /// How the request asked for tools to be used. Also part of "is this the
        /// same request as the one before it": a fold that switches to `none`
        /// is asking the model something else.
        pub tool_choice: String,
        /// What the request asked about the provider's thinking mode, as the
        /// endpoint would read it: a test asserts the config's knobs reach
        /// this, and that nothing stated sends no field at all.
        pub thinking: Option<Value>,
        /// The same for `reasoning_effort`.
        pub reasoning_effort: Option<String>,
        /// The reply cap the request carried, in both of the names it can travel
        /// under. Keeping the two apart is the point: a fold that sent
        /// `max_tokens` to an endpoint configured for `max_completion_tokens` was
        /// a request the endpoint refused, and a test can only tell which field
        /// was used if `Asked` records them separately.
        pub max_tokens: u32,
        pub max_completion_tokens: Option<u32>,
    }

    impl Asked {
        /// Who the model was told it is working for: the depth of a subagent,
        /// or `None` for a root. Read off the identity line of the system
        /// prompt, which is the one thing that differs between the actors of a
        /// tree when they ask at the same time.
        pub fn depth(&self) -> Option<usize> {
            let after = self.system().split_once("mush subagent at depth ")?.1;
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        }

        /// Whether any message the model was given contains `needle` — what
        /// the script needs to know to answer as if it had read the transcript.
        pub fn saw(&self, needle: &str) -> bool {
            self.messages
                .iter()
                .any(|message| message.text().contains(needle))
        }

        fn system(&self) -> &str {
            self.messages.first().map(Message::text).unwrap_or_default()
        }
    }

    /// A reply the test releases. A call that would answer with a held reply
    /// announces itself and then waits, so a test can do something while the
    /// reply is in flight — type a nudge, let a parent's turn end — with no
    /// sleep and no socket.
    pub struct Gate {
        arrived: (Sender<()>, Receiver<()>),
        released: (Sender<()>, Receiver<()>),
    }

    impl Gate {
        pub fn new() -> Self {
            Self {
                arrived: crossbeam_channel::unbounded(),
                released: crossbeam_channel::unbounded(),
            }
        }

        /// Wait for a held call to be in flight. Bounded, so a call that never
        /// arrives fails the test instead of hanging it.
        pub fn wait_until_asked(&self, timeout: Duration) -> bool {
            self.arrived.1.recv_timeout(timeout).is_ok()
        }

        /// Let the held reply through.
        pub fn release(&self) {
            let _ = self.released.0.send(());
        }

        fn hold(&self) {
            let _ = self.arrived.0.send(());
            // Bounded too: a test that never releases holds up one actor
            // thread, not the suite.
            let _ = self.released.1.recv_timeout(Duration::from_secs(10));
        }
    }

    /// What answers a request: a reply, or a reply held open until the test
    /// says so.
    enum Answer {
        Now(Result<ChatResponse, ModelError>),
        Held(Arc<Gate>, Result<ChatResponse, ModelError>),
    }

    /// What a request has to look like for a scripted reply to be its answer.
    type Matches = Box<dyn Fn(&Asked) -> bool + Send + Sync>;

    /// One scripted answer, and what a request has to look like to get it.
    struct Rule {
        when: Option<Matches>,
        answer: Answer,
    }

    /// The model that answers whatever the test says, in order.
    #[derive(Default)]
    pub struct Scripted {
        rules: Mutex<VecDeque<Rule>>,
        asked: Mutex<Vec<Asked>>,
        /// The matcher and the gate the *next* scripted reply is written with.
        when: Option<Matches>,
        hold: Option<Arc<Gate>>,
    }

    impl Scripted {
        pub fn new() -> Self {
            Self::default()
        }

        /// The next reply answers only a request this matcher accepts.
        ///
        /// A tree asks concurrently, so a script for more than one actor has to
        /// say who each reply is for: matching is on the request — which system
        /// prompt, what the transcript already holds — not on arrival order.
        /// A reply with no matcher answers whatever is asked next, which is all
        /// a single-actor script ever needs.
        pub fn when(mut self, matches: impl Fn(&Asked) -> bool + Send + Sync + 'static) -> Self {
            self.when = Some(Box::new(matches));
            self
        }

        /// The next reply is held until the test releases `gate`: the call is
        /// announced first, so the test never has to race it or sleep on it.
        pub fn held(mut self, gate: Arc<Gate>) -> Self {
            self.hold = Some(gate);
            self
        }

        /// The next reply is this text, finished.
        pub fn says(mut self, content: &str) -> Self {
            self.script(Ok(reply(Message::assistant(content), "stop")));
            self
        }

        /// The next reply asks for these calls before it can answer.
        pub fn calls(mut self, calls: Vec<ToolCall>) -> Self {
            self.script(Ok(reply(
                Message {
                    role: "assistant".into(),
                    tool_calls: Some(calls),
                    ..Default::default()
                },
                "tool_calls",
            )));
            self
        }

        /// The next reply is this message, finished for this reason: how a
        /// server that refuses (`finish_reason: content_filter`) or ends a
        /// reply in a way mush does not know answers.
        pub fn finishing(mut self, message: Message, finish_reason: &str) -> Self {
            self.script(Ok(reply(message, finish_reason)));
            self
        }

        /// The next reply is cut off at the token cap: the endpoint stopped it
        /// mid-answer (`finish_reason: length`), which is what a model does when
        /// it tries to write a whole file in one call.
        pub fn cut_off(mut self, content: &str) -> Self {
            self.script(Ok(reply(Message::assistant(content), "length")));
            self
        }

        /// The same, but the cut lands inside a tool call: the arguments are
        /// whatever JSON survived, which is exactly what must never be run.
        pub fn cut_off_call(mut self, name: &str, arguments: &str) -> Self {
            self.script(Ok(reply(
                Message {
                    role: "assistant".into(),
                    tool_calls: Some(vec![ToolCall {
                        id: "cut".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: name.into(),
                            arguments: arguments.into(),
                        },
                    }]),
                    ..Default::default()
                },
                "length",
            )));
            self
        }

        /// The next call fails the way an endpoint does: a status other than
        /// 200, and its own complaint as the body.
        pub fn fails_with(mut self, status: u16, body: &str) -> Self {
            self.script(Err(ModelError::Status {
                status,
                body: body.to_string(),
            }));
            self
        }

        /// The next call fails with exactly this error: how a test drives a
        /// failure class the retry policy has an opinion about — a transport
        /// hiccup, a refusal, a body that did not parse — with no socket to
        /// produce one.
        pub fn fails(mut self, error: ModelError) -> Self {
            self.script(Err(error));
            self
        }

        /// The next call fails the way the wire does: the request never reached
        /// an endpoint with an opinion about it (finding B23).
        pub fn fails_transport(mut self, message: &str) -> Self {
            self.script(Err(ModelError::Transport(message.to_string())));
            self
        }

        /// The next call is cancelled mid-reply. The flag the reader polls is
        /// set as well as the error being returned, because that is what the
        /// real client does: a caller that trusts the flag sees the same thing.
        pub fn cancels(mut self) -> Self {
            self.script(Err(ModelError::Cancelled));
            self
        }

        /// The reply just scripted carries the endpoint's own token counts, the
        /// way a server that reports `usage` calls do — and a server that does
        /// not is every other scripted reply.
        pub fn with_usage(self, prompt: u64, completion: u64, total: u64) -> Self {
            let mut rules = self.rules.lock().expect("no test panicked mid-script");
            if let Some(rule) = rules.back_mut() {
                if let Answer::Now(Ok(reply)) | Answer::Held(_, Ok(reply)) = &mut rule.answer {
                    reply.usage = Some(Usage {
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        total_tokens: total,
                    });
                }
            }
            drop(rules);
            self
        }

        /// Every request made so far, in order.
        pub fn asked(&self) -> Vec<Asked> {
            self.asked
                .lock()
                .expect("no test panicked mid-script")
                .clone()
        }

        fn script(&mut self, reply: Result<ChatResponse, ModelError>) {
            let answer = match self.hold.take() {
                Some(gate) => Answer::Held(gate, reply),
                None => Answer::Now(reply),
            };
            self.rules
                .lock()
                .expect("no test panicked mid-script")
                .push_back(Rule {
                    when: self.when.take(),
                    answer,
                });
        }
    }

    impl ModelClient for Scripted {
        fn chat(
            &self,
            request: &ChatRequest<'_>,
            cancel: &AtomicBool,
        ) -> Result<ChatResponse, ModelError> {
            let asked = Asked {
                model: request.model.to_string(),
                messages: request.messages.to_vec(),
                tools: request.tools.len(),
                tool_schemas: request.tools.to_vec(),
                tool_choice: request.tool_choice.to_string(),
                thinking: request.thinking.clone(),
                reasoning_effort: request.reasoning_effort.clone(),
                max_tokens: request.max_tokens,
                max_completion_tokens: request.max_completion_tokens,
            };
            // The first reply still scripted whose matcher accepts this
            // request, and it is spent: that reply was written for this call.
            let answer = {
                let mut rules = self.rules.lock().expect("no test panicked mid-script");
                let found = rules.iter().position(|rule| match &rule.when {
                    Some(matches) => matches(&asked),
                    None => true,
                });
                found.map(|index| {
                    rules
                        .remove(index)
                        .expect("the reply just found is still there")
                        .answer
                })
            };
            // Recorded before the reply (held or not) comes back, so a test
            // woken by a gate can read what that call was asked.
            self.asked
                .lock()
                .expect("no test panicked mid-script")
                .push(asked);
            let reply = match answer {
                Some(Answer::Now(reply)) => reply,
                Some(Answer::Held(gate, reply)) => {
                    gate.hold();
                    reply
                }
                // Never panic: a request nothing was scripted for is a test
                // bug, and saying so in the run's own error beats hanging a
                // thread that waits for a reply which is not coming.
                None => {
                    return Err(ModelError::Unreachable(
                        "the scripted model had no reply left for this request".to_string(),
                    ))
                }
            };
            match reply {
                Ok(reply) => Ok(reply),
                Err(error) => {
                    if error == ModelError::Cancelled {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    Err(error)
                }
            }
        }
    }

    /// One reply from the endpoint: this message, and this finish reason.
    pub fn reply(message: Message, finish_reason: &str) -> ChatResponse {
        ChatResponse {
            choices: vec![Choice {
                message,
                finish_reason: Some(finish_reason.to_string()),
            }],
            error: None,
            usage: None,
        }
    }

    /// A tool call as a model writes one: an id, a name, and its arguments.
    pub fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use mush_core::message::{ChatRequest, ChatResponse, Message};

    use super::fake::Scripted;
    use super::{
        retrying, transport, HttpModel, ModelClient, ModelError, BACKOFF_SLICE, RETRY_ATTEMPTS,
        RETRY_BACKOFF,
    };
    use crate::agent::AgentEvent;
    use crate::app::{AgentId, ConfigHandle};
    use crate::clock::fake::Advanceable;
    use crate::clock::Clock;
    use crate::events::fake::Recorder;
    use crate::events::Events;

    /// One model call, made the way an agent makes it: the retry line goes to
    /// the recording sink the way `AgentEvent::Notice` reaches the UI, and the
    /// backoff waits on a clock that only moves when it is told to — so a test
    /// asserts *that* the pause happened, rather than paying for it.
    fn called(
        model: &Arc<Scripted>,
        clock: &dyn Clock,
        cancel: &AtomicBool,
        log: &Arc<Recorder>,
    ) -> Result<ChatResponse, ModelError> {
        let messages = vec![Message::user("task")];
        let request = ChatRequest {
            model: "test",
            messages: &messages,
            tools: &[],
            tool_choice: "auto",
            stream: false,
            temperature: 0.0,
            max_tokens: 0,
            max_completion_tokens: None,
            thinking: None,
            reasoning_effort: None,
        };
        retrying(
            clock,
            cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            || model.chat(&request, cancel),
        )
    }

    /// The retry lines the human was shown, in order.
    fn retry_lines(log: &Recorder) -> Vec<String> {
        log.events_for(AgentId(7))
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(line) => Some(line),
                _ => None,
            })
            .collect()
    }

    /// The classification itself, on the `io::Error`s `http.rs` really returns:
    /// what the wire does is retried, and what the transport *refused* — a body
    /// past the cap, a cancellation, a name that does not resolve — is not,
    /// however alike the two look once they are strings.
    #[test]
    fn only_a_failure_of_the_wire_is_a_transport_failure() {
        let hiccups = [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::TimedOut,
            io::ErrorKind::BrokenPipe,
        ];
        for kind in hiccups {
            assert!(
                transport(&io::Error::new(kind, "the wire")),
                "{kind:?} is the wire failing, not an answer"
            );
        }
        let answers = [
            // What `http.rs` refuses: a body past its cap, a malformed status.
            io::ErrorKind::InvalidData,
            // How a cancellation is reported; a cancellation is never retried.
            io::ErrorKind::Interrupted,
            // A name that does not resolve, and a URL mush cannot parse:
            // asking again asks the same broken question.
            io::ErrorKind::NotFound,
            io::ErrorKind::InvalidInput,
        ];
        for kind in answers {
            assert!(
                !transport(&io::Error::new(kind, "an answer")),
                "{kind:?} is not a hiccup"
            );
        }
    }

    /// Two hiccups and then an answer: the call succeeds, the endpoint really
    /// was asked three times, every retry was announced, and the pause came
    /// through the clock seam instead of costing the suite the wait it proves
    /// (finding B23).
    #[test]
    fn a_transport_hiccup_is_retried_until_the_model_answers() {
        let hiccup = "Connection reset by peer (os error 104)";
        let model = Arc::new(
            Scripted::new()
                .fails_transport(hiccup)
                .fails_transport(hiccup)
                .says("done"),
        );
        let clock = Advanceable::new();
        let log = Recorder::new();

        let reply = called(&model, &clock, &AtomicBool::new(false), &log).unwrap();

        assert_eq!(reply.choices[0].message.text(), "done");
        assert_eq!(
            model.asked().len(),
            RETRY_ATTEMPTS,
            "the request was really made three times"
        );
        assert_eq!(
            retry_lines(&log),
            vec![
                format!("{hiccup} — retrying (2/3)"),
                format!("{hiccup} — retrying (3/3)"),
            ],
            "and each retry was visible, in order"
        );
        assert_eq!(
            clock.elapsed(),
            RETRY_BACKOFF + RETRY_BACKOFF * 2,
            "the backoff waits on the clock it was handed"
        );
    }

    /// Every attempt fails: the caller gets the endpoint's own words, and the
    /// attempts are named — not a bare fourth message that hides what the wire
    /// actually said.
    #[test]
    fn transport_failures_on_every_attempt_name_the_attempts() {
        let hiccup = "Connection reset by peer (os error 104)";
        let model = Arc::new(
            Scripted::new()
                .fails_transport(hiccup)
                .fails_transport(hiccup)
                .fails_transport(hiccup),
        );
        let clock = Advanceable::new();
        let log = Recorder::new();

        let error = called(&model, &clock, &AtomicBool::new(false), &log).unwrap_err();

        assert_eq!(
            model.asked().len(),
            RETRY_ATTEMPTS,
            "three attempts, no more"
        );
        match error {
            ModelError::Transport(reason) => {
                assert!(
                    reason.starts_with(hiccup),
                    "the original error reaches the caller: {reason}"
                );
                assert!(
                    reason.contains("3 attempts"),
                    "and the attempts are named: {reason}"
                );
            }
            other => panic!("a transport failure, not {other:?}"),
        }
        assert_eq!(
            retry_lines(&log).len(),
            RETRY_ATTEMPTS - 1,
            "both retries were announced before giving up"
        );
    }

    /// A clock whose `sleep` is the human pressing Ctrl-C: the flag is set at
    /// the instant the pause begins, so the retry must not be made at all.
    struct CancelledDuringBackoff {
        flag: Arc<AtomicBool>,
        clock: Advanceable,
    }

    impl Clock for CancelledDuringBackoff {
        fn now(&self) -> Instant {
            self.clock.now()
        }

        fn sleep(&self, d: Duration) {
            self.flag.store(true, Ordering::SeqCst);
            self.clock.sleep(d);
        }
    }

    /// Ctrl-C during the backoff abandons the request at once: no second
    /// attempt is made, and the answer is a cancellation — not a retry, and not
    /// a transport failure, which would end a run as one (docs/mush.md §5.5).
    #[test]
    fn a_cancellation_during_the_backoff_abandons_the_retry() {
        let model = Arc::new(
            Scripted::new()
                .fails_transport("Connection reset by peer (os error 104)")
                .says("too late"),
        );
        let flag = Arc::new(AtomicBool::new(false));
        let clock = CancelledDuringBackoff {
            flag: flag.clone(),
            clock: Advanceable::new(),
        };
        let log = Recorder::new();

        let error = called(&model, &clock, &flag, &log).unwrap_err();

        assert_eq!(error, ModelError::Cancelled);
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(
            model.asked().len(),
            1,
            "the retry the human cancelled was never made"
        );
        assert_eq!(retry_lines(&log).len(), 1, "they were told it was coming");
        assert_eq!(
            clock.clock.elapsed(),
            BACKOFF_SLICE,
            "and it stopped at the first slice of the backoff, not at its end"
        );
    }

    /// The whole live road, once, through a real (loopback) endpoint: a chunked
    /// reply cut off inside its body — the shape that produced
    /// `the endpoint's reply was refused: malformed chunk size: ""` in a real
    /// run — is classified as the wire by `HttpModel`, so `retrying` asks again
    /// on the fresh connection the broken one leaves behind, and the run gets
    /// its reply. Before finding B27's fix this was `Refused`: one attempt, and
    /// the run ended blaming the endpoint.
    #[test]
    fn a_chunked_body_cut_off_on_a_real_wire_is_retried_on_a_fresh_connection() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let answer = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
        let server = std::thread::spawn(move || {
            let mut accepted = 0;
            for round in 0..2 {
                let (mut connection, _) = listener.accept().unwrap();
                accepted += 1;
                let mut scratch = [0u8; 8192];
                let _ = connection.read(&mut scratch);
                if round == 0 {
                    // One whole chunk, then the stream ends: the connection is
                    // dropped at the end of this iteration, which is what a
                    // server dying mid-reply looks like to the client.
                    let _ = connection.write_all(
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
                    );
                } else {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        answer.len()
                    );
                    let _ = connection.write_all(head.as_bytes());
                    let _ = connection.write_all(answer.as_bytes());
                    // Long enough that the reply is read whole before the close.
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = connection.flush();
            }
            accepted
        });

        let cfg = ConfigHandle::own(mush_core::Config::new(
            format!("http://127.0.0.1:{port}"),
            "test",
            None,
        ));
        let model = HttpModel::new(cfg);
        let cancel = AtomicBool::new(false);
        let log = Recorder::new();
        let messages = vec![Message::user("task")];
        let request = ChatRequest {
            model: "test",
            messages: &messages,
            tools: &[],
            tool_choice: "auto",
            stream: false,
            temperature: 0.0,
            max_tokens: 0,
            max_completion_tokens: None,
            thinking: None,
            reasoning_effort: None,
        };

        let reply = retrying(
            crate::clock::system(),
            &cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            || model.chat(&request, &cancel),
        )
        .unwrap();

        assert_eq!(reply.choices[0].message.text(), "done");
        assert_eq!(
            server.join().unwrap(),
            2,
            "the broken connection was dropped, so the retry opened a fresh one"
        );
        let lines = retry_lines(&log);
        assert_eq!(lines.len(), 1, "the retry was visible: {lines:?}");
        assert!(lines[0].contains("retrying (2/3)"), "{lines:?}");
    }

    /// A reply whose framing broke is not an answer: nothing of it was handed to
    /// the caller, and the connection that carried the broken frame is dropped —
    /// so the retry asks a *fresh* connection, and the human reads the same
    /// `retrying — <why> (n/3)` line a transport hiccup gets (finding B27).
    #[test]
    fn a_broken_frame_is_retried_and_announced() {
        let broken = "malformed chunk size: \"\"";
        let model = Arc::new(
            Scripted::new()
                .fails(ModelError::Framing(broken.to_string()))
                .says("done"),
        );
        let clock = Advanceable::new();
        let log = Recorder::new();

        let reply = called(&model, &clock, &AtomicBool::new(false), &log).unwrap();

        assert_eq!(reply.choices[0].message.text(), "done");
        assert_eq!(
            model.asked().len(),
            2,
            "the broken reply was asked for once more"
        );
        assert_eq!(
            retry_lines(&log),
            vec![format!("{broken} — retrying (2/3)")],
            "and the retry was visible, in B23's shape"
        );
        assert_eq!(
            clock.elapsed(),
            RETRY_BACKOFF,
            "the backoff waits on the clock it was handed"
        );
    }

    /// Every attempt breaks the same way: the error keeps its class, so the run
    /// can say the *reply* broke — never that the endpoint refused it — and it
    /// names the attempts rather than hiding what the wire said.
    #[test]
    fn framing_failures_on_every_attempt_keep_their_class() {
        let broken = "malformed chunk size: \"\"";
        let model = Arc::new(
            Scripted::new()
                .fails(ModelError::Framing(broken.to_string()))
                .fails(ModelError::Framing(broken.to_string()))
                .fails(ModelError::Framing(broken.to_string())),
        );
        let clock = Advanceable::new();
        let log = Recorder::new();

        let error = called(&model, &clock, &AtomicBool::new(false), &log).unwrap_err();

        assert_eq!(
            model.asked().len(),
            RETRY_ATTEMPTS,
            "three attempts, no more"
        );
        match error {
            ModelError::Framing(reason) => {
                assert!(
                    reason.starts_with(broken),
                    "the cause reaches the caller: {reason}"
                );
                assert!(
                    reason.contains("3 attempts"),
                    "and the attempts are named: {reason}"
                );
            }
            other => panic!("a framing failure, not {other:?}"),
        }
        assert_eq!(
            retry_lines(&log).len(),
            RETRY_ATTEMPTS - 1,
            "both retries were announced before giving up"
        );
    }

    /// Ctrl-C during the backoff abandons a framing retry at once, exactly as it
    /// abandons a transport one: a cancellation is never retried, whatever the
    /// failed attempt was.
    #[test]
    fn a_cancellation_during_the_backoff_abandons_a_framing_retry() {
        let model = Arc::new(
            Scripted::new()
                .fails(ModelError::Framing(
                    "malformed chunk size: \"\"".to_string(),
                ))
                .says("too late"),
        );
        let flag = Arc::new(AtomicBool::new(false));
        let clock = CancelledDuringBackoff {
            flag: flag.clone(),
            clock: Advanceable::new(),
        };
        let log = Recorder::new();

        let error = called(&model, &clock, &flag, &log).unwrap_err();

        assert_eq!(error, ModelError::Cancelled);
        assert_eq!(
            model.asked().len(),
            1,
            "the retry the human cancelled was never made"
        );
        assert_eq!(retry_lines(&log).len(), 1, "they were told it was coming");
        assert_eq!(
            clock.clock.elapsed(),
            BACKOFF_SLICE,
            "and it stopped at the first slice of the backoff"
        );
    }

    /// What the endpoint chose is an answer, and so is a body that arrived and
    /// did not parse: one attempt, and the same error a human sees today. The
    /// 400 is among them because the one retry a complaint gets — the window it
    /// teaches the run — is the run loop's, decided once, and never a
    /// transport policy that cannot read the body. A 503 is here too: an
    /// endpoint that says it is overloaded has spoken, and its words are worth
    /// more to the human than another silent ask.
    #[test]
    fn an_answer_from_the_endpoint_is_never_retried() {
        let answers = [
            ModelError::Status {
                status: 400,
                body: r#"{"error":{"message":"bad request"}}"#.to_string(),
            },
            ModelError::Status {
                status: 503,
                body: "the endpoint is overloaded".to_string(),
            },
            ModelError::Refused("the response body is larger than 83886080 bytes".to_string()),
            ModelError::Malformed("expected value at line 1 column 1".to_string()),
        ];
        for expected in answers {
            let model = Arc::new(Scripted::new().fails(expected.clone()).says("never asked"));
            let clock = Advanceable::new();
            let log = Recorder::new();

            let error = called(&model, &clock, &AtomicBool::new(false), &log).unwrap_err();

            assert_eq!(error, expected, "the endpoint's answer reaches the caller");
            assert_eq!(model.asked().len(), 1, "{expected:?} is not a hiccup");
            assert!(
                retry_lines(&log).is_empty(),
                "nothing was announced as a retry"
            );
            assert_eq!(clock.elapsed(), Duration::ZERO, "and nothing was waited");
        }
    }
}
