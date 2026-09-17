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

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};

use mush_core::message::{ChatRequest, ChatResponse};

use crate::app::ConfigHandle;
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
    /// The human cancelled (Ctrl-C, `agent_control stop`, `/new`) while the
    /// reply was in flight.
    Cancelled,
    /// The request could not be encoded.
    Encode(String),
    /// The endpoint could not be reached, or answered nothing at all.
    Unreachable(String),
    /// The endpoint answered, but its reply was refused before it could be
    /// read: a body past `http`'s cap, a malformed status line or chunk line.
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
            // A refusal is not a connection failure: the endpoint answered.
            Err(error) if error.kind() == ErrorKind::InvalidData => {
                return Err(ModelError::Refused(error.to_string()));
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
        /// The reply cap the request carried, under whichever of the two names
        /// the config sends it. It is the number the endpoint cuts a reply off
        /// at, so a test can assert what a window really buys.
        pub reply_cap: u32,
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
                reply_cap: request
                    .max_tokens
                    .max(request.max_completion_tokens.unwrap_or(0)),
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
