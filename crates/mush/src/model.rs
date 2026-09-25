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
//! that never left mush — the endpoint could not be dialled, or the write
//! failed before the request was whole — and one that left whole but whose
//! connection died before the reply began ([`ModelError::Unanswered`], whose
//! line admits the attempt may have been billed), never one whose reply had
//! already started: those bytes are paid for. The loop is driven by the caller,
//! because the caller is the layer that holds the clock the backoff waits on,
//! the cancel flag the human's Stop sets, and the agent whose transcript the
//! retry is announced in.

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
    /// The endpoint could not be reached *and asking again would ask the same
    /// broken question*: a URL mush cannot parse, a config cell that would not
    /// be read. Nothing about it changes if mush repeats it, so it is final —
    /// and it is not [`ModelError::Unsent`]'s, which is about a failure a
    /// repeat can honestly make.
    Unreachable(String),
    /// The request never left mush: it could not be dialled (a name that does
    /// not resolve, a connect that is refused or times out, a TLS handshake
    /// that fails), or the write failed before the request was whole. No
    /// complete request ever arrived, so the endpoint has nothing to read, run
    /// or charge for — one of the two failures [`retrying`] asks again, and the
    /// only one a repeat cannot bill (finding A2).
    Unsent(String),
    /// The request went out whole, but the connection died before the reply
    /// began: not one byte of a reply arrived, so the endpoint handed mush
    /// nothing of an answer — the *other* class [`retrying`] asks again, and
    /// the reason is the measurement (zero reply bytes), not the error kind,
    /// which a reset that lands mid-reply shares. The attempt may still have
    /// been billed: the endpoint can have read, run and charged for the request
    /// before the connection died, which is what the retry line says
    /// (finding A2).
    Unanswered(String),
    /// The wire failed after the reply began, under a request the endpoint may
    /// already have received: a connection reset or dropped before the reply
    /// framed itself, an unexpected end of stream, a read that timed out, a
    /// chunked body whose stream ended inside it. Final, never retried: reply
    /// bytes arrived, so the endpoint answered something, those bytes may
    /// already have been paid for, and a second send is a question the human
    /// pays for twice (finding A2). The call ends naming the endpoint, and the
    /// human can ask again deliberately. A connection that died *before* the
    /// reply began is [`ModelError::Unanswered`] instead: no byte of an answer
    /// was used, and that one is asked again.
    Transport(String),
    /// The reply's framing broke before its body could be read: a status line
    /// or `Content-Length` that is not one, a chunk size that is not hex or
    /// that claims past `http`'s cap, a chunk terminator the framing did not
    /// promise. [`http`] raises these, and they are *not* an answer the
    /// endpoint chose — nothing of the reply was handed over. Final like
    /// [`ModelError::Transport`] and for the same reason: the request went out
    /// whole, so the endpoint may already have
    /// read it and answered it (finding A2). The connection that carried the
    /// broken frame is dropped rather than kept, so a later ask never reads the
    /// same leftover framing (finding B27).
    Framing(String),
    /// The endpoint answered, but its reply was refused before it could be
    /// read: a body whose `Content-Length` — or whose whole length, when it
    /// ends with the stream — is past `http`'s cap, or a response head past it.
    /// A malformed status line or chunk line is *not* here — those are
    /// [`ModelError::Framing`], a reply that broke on the way in rather than an
    /// answer; neither is a chunk-size line claiming past the cap, which is the
    /// framing itself and never the body (finding A10).
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
/// the caller *nothing of a reply*. [`retrying`] reads that rule and the two
/// markers when it decides what may be asked again: [`ModelError::Unsent`], a
/// request that never left mush, and [`ModelError::Unanswered`], a connection
/// that died before the reply began — neither hands the caller a reply byte, so
/// a repeat can only duplicate or bill work if the request may already have
/// been received, which is why the second's line says so. A client that ever
/// streams a partial body must report a later failure as something that is
/// never retried.
/// `HttpModel` satisfies it by construction: it makes a `Response` only from a
/// body that framed itself (finding B27), it marks a request that never went out
/// with [`http::is_unsent`], and a connection that died before the reply began
/// with [`http::is_unanswered`].
pub trait ModelClient: Send + Sync {
    /// `cancel` is the flag the human's Stop sets; the call must notice it
    /// while it waits, not only once the endpoint has answered.
    ///
    /// `timeout` is what is left of the logical call's deadline: the whole
    /// budget for this ask, not a per-attempt one. [`retrying`] owns the one
    /// deadline and gives every attempt only its remainder, so an attempt must
    /// bound everything it does by what it was handed.
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        cancel: &AtomicBool,
        timeout: Duration,
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
        timeout: Duration,
    ) -> Result<ChatResponse, ModelError> {
        // One snapshot per request, not a lock held across the call.
        let cfg = self.cfg.config().map_err(ModelError::Unreachable)?;

        let body = serde_json::to_string(request)
            .map_err(|error| ModelError::Encode(error.to_string()))?;

        let response = match http::post_json(
            &cfg.chat_url(),
            &body,
            cfg.api_key.as_deref(),
            cancel,
            timeout,
        ) {
            Ok(response) => response,
            // The reader stops the moment the human cancels; that is a
            // cancellation, not a failure to reach the endpoint. The flag is
            // checked first, so a request that failed *because* of the cancel
            // is reported as one however the socket reported it.
            Err(_) if cancel.load(Ordering::SeqCst) => return Err(ModelError::Cancelled),
            // A request that never left mush: nothing was written, so asking
            // again is the one repeat that cannot duplicate or bill anything
            // (finding A2). The marker is asked before the kind, because an
            // unsent failure keeps the kind it happened with — a refused
            // connect is still `ConnectionRefused`.
            Err(error) if http::is_unsent(&error) => {
                return Err(ModelError::Unsent(error.to_string()))
            }
            // The request went out whole and the connection died before the
            // reply began: not one byte of an answer arrived, so there is no
            // partial reply for a repeat to duplicate — the one read-side
            // failure that may be asked again. Asked by the marker, not the
            // kind: the same `ConnectionAborted`/`ConnectionReset` that arrives
            // here also arrives *mid-reply*, where the bytes are already on
            // their way to being billed and the failure is final. The retry
            // line says the attempt may still have been billed (finding A2).
            Err(error) if http::is_unanswered(&error) => {
                return Err(ModelError::Unanswered(error.to_string()))
            }
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
            // Which class a failed call is decides whether asking again is
            // honest, so the *kind* travels with the message instead of being
            // flattened into a string no caller can classify.
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
/// connection reset or aborted, an end of stream where a reply should have
/// been, a read that timed out — the failures of a request whose reply may
/// already have begun, which is why every one of them is final.
///
/// What is *not* here is as deliberate: `InvalidData` is a refusal `http.rs`
/// already classified, and `Interrupted` is how the cancel flag is reported. The
/// two classes that *are* asked again have their own markers too:
/// [`ModelError::Unsent`] (a request that never left mush) and
/// [`ModelError::Unanswered`] (a request that left whole but whose connection
/// died before one byte of the reply arrived) — the kind alone cannot tell a
/// reset while writing (nothing was handed over) from one before the reply
/// began (nothing came back) from one that landed mid-reply (an answer is
/// already on its way, and is final). A reply whose *framing* broke is its own
/// class as well ([`ModelError::Framing`]). A signal that interrupted a read or
/// a write — the human resizing the terminal — was already made again inside
/// `http.rs`, so the interrupt itself never reaches this classifier (finding
/// B25); the only `Interrupted` that arrives here is a decision, and a
/// cancellation is never retried. The remaining kinds — `InvalidInput` from a
/// URL mush cannot parse, whatever a config cell would not say — are the
/// request mush cannot even ask, and are final for the same reason a repeat
/// would ask the same broken question.
///
/// `pub(crate)` so `http.rs`'s own tests can pin that a mid-reply death is
/// classified here and a pre-reply one is not ([`ModelError::Unanswered`]):
/// the two say opposite things and share their kinds.
pub(crate) fn transport(error: &io::Error) -> bool {
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
/// The whole deadline one logical model call gets. A slow local model is normal
/// work, so the number is generous; but it belongs to the *call*, not to the
/// attempt — [`retrying`] hands every attempt only what is left of it, so one
/// ask can never spend two, whatever the wire does. It is a parameter of
/// [`retrying`] and of [`ModelClient::chat`] rather than a constant read inside
/// the transport, so a test can pass milliseconds and prove the road a human
/// would otherwise reach in ten minutes without waiting for it.
pub const CHAT_DEADLINE: Duration = Duration::from_secs(600);
/// What the first backoff waits, doubling for the retry after it. Short on
/// purpose: the hiccup this exists for clears in a moment, and a human who is
/// being told what is happening does not need mush to wait a minute to be sure.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// The longest a backoff sleeps before the cancel flag is read again. Short
/// enough that Ctrl-C during a pause lands at once, and the same shape as the
/// poll in `agent.rs`'s waits (docs/mush.md §5.5).
const BACKOFF_SLICE: Duration = Duration::from_millis(50);

/// One model call, with the bounded retry a request that never went out
/// deserves.
///
/// `timeout` is the deadline of the **logical call**: the whole budget for one
/// ask, fixed here and handed to every attempt as what is left of it, so a call
/// can never spend two. It is a parameter rather than a constant read inside the
/// transport so that a test can pass a deadline it can afford to wait for; the
/// backoff is spent from the same budget, never past it.
///
/// `attempt` is one whole try — for the real client, one `http::post_json` —
/// given what remains of the deadline, and it must obey the [`ModelClient`]
/// contract: an `Err` means the caller got nothing of the reply, which is what
/// makes a repeat safe. This decides whether a failed try is worth another,
/// waits the backoff on the run's own [`Clock`], and hands every retry to
/// `announce`, so the human reads `Connection refused (os error 111) — retrying
/// (2/3)` in the transcript instead of watching a spinner that looks stuck.
///
/// Two failures are repeated, and each is announced in its own words. A
/// [`ModelError::Unsent`] one means the request never left mush — the endpoint
/// could not be dialled, or the write failed before the request was whole — so
/// nothing complete was ever handed to the endpoint and a repeat cannot
/// duplicate or bill anything. A [`ModelError::Unanswered`] one means the
/// request went out whole and the connection died before the reply began: not
/// one byte of an answer arrived, so there is no partial reply a repeat could
/// duplicate — but the attempt *may already have been billed*, because the
/// endpoint can have read, run and charged for the request before its
/// connection died, and the line says so rather than letting a human believe a
/// retry was free. A [`ModelError::Transport`] failure that arrived *mid-reply*
/// — a connection that dropped, a read that timed out, a body cut in half —
/// stays final: those bytes were already paid for, and a second send is a
/// question the human pays for twice (finding A2). Everything else is final
/// too, and deliberately: a [`ModelError::Framing`] one, a cancellation, a
/// status the endpoint chose (4xx *and* 5xx: an answer is not a hiccup, and the
/// 400 that teaches mush a smaller window already has exactly one retry of its
/// own in the run loop), a refusal (a reply past one of `http.rs`'s caps) and a
/// body that arrived and did not parse are all returned at once, unchanged,
/// with the endpoint named by the caller.
///
/// Worst case: [`RETRY_ATTEMPTS`] attempts, all inside the one `timeout` (each
/// is given only what is left of it), plus a backoff spent from the same
/// budget — so one ask against an endpoint that refuses to connect costs, at
/// worst, three dials plus the two backoffs `RETRY_BACKOFF` and twice that
/// (500 ms then 1 s), while one against an endpoint that accepts and stalls
/// costs its deadline, not a multiple of it. Nothing in the call is unbounded,
/// the name lookup included: it is a phase of the attempt like the connect and
/// the write, and `http.rs`'s `resolve_bounded` ends its wait at the smaller of
/// its own `RESOLVE_TIMEOUT` (10 s) and what is left of the call's deadline — a
/// ceiling, not a schedule (finding A19).
pub fn retrying<T>(
    clock: &dyn Clock,
    timeout: Duration,
    cancel: &AtomicBool,
    announce: impl Fn(&str),
    mut attempt: impl FnMut(Duration) -> Result<T, ModelError>,
) -> Result<T, ModelError> {
    let deadline = clock.now() + timeout;
    let mut tries = 1;
    loop {
        // A Stop outranks everything: before the first attempt, and between
        // every pair of them.
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::Cancelled);
        }
        let left = deadline.saturating_duration_since(clock.now());
        let error = match attempt(left) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        // Two failures are handed another attempt: a request that never left
        // mush, whose repeat cannot duplicate or bill anything, and a request
        // that left whole but whose connection died before the reply began,
        // whose repeat may still be billed — which its line says. Anything
        // else — a failure after a reply byte arrived, a cancellation, the
        // endpoint's own verdict, a refusal, a body that did not parse — is
        // final (finding A2).
        let line = match &error {
            ModelError::Unsent(message) => {
                format!("{message} — retrying ({}/{RETRY_ATTEMPTS})", tries + 1)
            }
            ModelError::Unanswered(message) => format!(
                "{message} — asking again ({}/{RETRY_ATTEMPTS}); that attempt may have been billed",
                tries + 1
            ),
            _ => return Err(error),
        };
        if tries == RETRY_ATTEMPTS {
            // The endpoint's own message, plus the fact that it is not the
            // first time it was heard: a retry layer that replaced it with a
            // bare "unreachable after retries" would hide the only detail the
            // human can act on.
            return Err(error.after_attempts());
        }
        // The one deadline is one: a retry the call cannot pay for is not
        // made, and the backoff below spends from the same budget.
        let left = deadline.saturating_duration_since(clock.now());
        if left.is_zero() {
            return Err(error);
        }
        announce(&line);
        wait(clock, cancel, tries, left)?;
        tries += 1;
    }
}

/// The pause before the retry after `tries` attempts, in slices the cancel flag
/// is read between, capped by `budget` — what is left of the logical call's
/// deadline — so the pause can never make a call outlive the deadline it was
/// given.
///
/// One long `sleep` would make Ctrl-C wait out the whole backoff, which is the
/// one thing a cancellation may never do; and a `Cancelled` returned from here
/// is the run stopping, not the wire failing.
fn wait(
    clock: &dyn Clock,
    cancel: &AtomicBool,
    tries: usize,
    budget: Duration,
) -> Result<(), ModelError> {
    let mut left = (RETRY_BACKOFF * 2u32.pow(tries as u32 - 1)).min(budget);
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
    /// The same failure, saying how many times it was asked. Only the
    /// retryable classes carry a count; every other error is returned from
    /// [`retrying`] before this is reached.
    fn after_attempts(self) -> Self {
        let say = |message: String| format!("{message} — {RETRY_ATTEMPTS} attempts failed");
        match self {
            ModelError::Unsent(message) => ModelError::Unsent(say(message)),
            ModelError::Unanswered(message) => ModelError::Unanswered(say(message)),
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
        /// The schemas themselves. They are the head of the rendered prompt, so
        /// whether two requests share a cacheable prefix is a question about
        /// these bytes — a count of zero and a count of eight are not the
        /// comparison a test needs. A count spelled beside them was a second
        /// statement of `tool_schemas.len()`; this list carries the whole fact.
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
        /// failure class the retry policy has an opinion about — a request that
        /// never went out, a failure after it did, a refusal, a body that did
        /// not parse — with no socket to produce one.
        pub fn fails(mut self, error: ModelError) -> Self {
            self.script(Err(error));
            self
        }

        /// The next call fails before the request can go out — the endpoint
        /// could not be dialled, or the write broke — one of the two classes
        /// the retry policy asks again (finding A2).
        pub fn fails_unsent(mut self, message: &str) -> Self {
            self.script(Err(ModelError::Unsent(message.to_string())));
            self
        }

        /// The next call fails after the request went out whole but before the
        /// reply began: the other class the retry policy asks again, and the
        /// one whose line admits the attempt may still have been billed
        /// (finding A2).
        pub fn fails_unanswered(mut self, message: &str) -> Self {
            self.script(Err(ModelError::Unanswered(message.to_string())));
            self
        }

        /// The next call fails the way the wire does *after* the reply began:
        /// the endpoint may already have received the request and answered
        /// part of it, so this class is final and never asked again (finding
        /// A2). A connection that dies before the reply begins is
        /// [`ModelError::Unanswered`], the one read-side shape a repeat may
        /// make.
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
            _timeout: Duration,
        ) -> Result<ChatResponse, ModelError> {
            let asked = Asked {
                model: request.model.to_string(),
                messages: request.messages.to_vec(),
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
        // The real client builds a reply from the response body, so the fake
        // goes through the same deserializer: a scripted message reaches the
        // run loop in the shape the wire would give it — a call with no id
        // gets one there, not in the loop — rather than in a shape no
        // endpoint could send.
        let message =
            serde_json::from_value(serde_json::to_value(message).expect("a message serializes"))
                .expect("a serialized message deserializes");
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use mush_core::message::{ChatRequest, ChatResponse, Message};

    use super::fake::Scripted;
    use super::{
        retrying, transport, HttpModel, ModelClient, ModelError, BACKOFF_SLICE, CHAT_DEADLINE,
        RETRY_ATTEMPTS, RETRY_BACKOFF,
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
            CHAT_DEADLINE,
            cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            |left| model.chat(&request, cancel, left),
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

    // Wall-clock bounds in this module are runaway guards, never claims about
    // this machine — see the rule on `settle_sweep` in `app/mod.rs`'s tests:
    // scale where the subject is complexity, else size the ceiling as a
    // multiple of a measured worst case and say where the measurement came
    // from.
    /// How long a test's server keeps accepting connections after the call it
    /// serves — long enough that the box cannot fail the test by delaying the
    /// first dial, and long enough that a retry, which dials at once, is still
    /// counted rather than missed.
    ///
    /// Sized from the measurement: the served call reached `accept` 51-57 ms
    /// after the call began, on this box standing still (load 25) and with
    /// twelve extra busy loops on top. Two seconds is thirty-five times that
    /// worst reading; a busy box can only delay a dial further, never shorten
    /// the window, and a fixture that never connects still fails in the test's
    /// own time.
    const ACCEPT_WINDOW: Duration = Duration::from_secs(2);

    /// The classification itself, on the `io::Error`s `http.rs` really returns:
    /// what the wire does is the transport's, and what the transport *refused*
    /// — a body past the cap, a cancellation, a name that does not resolve — is
    /// not, however alike the two look once they are strings. Both are final;
    /// only an error `http.rs` marked `Unsent` earns a retry.
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

    /// One logical call has one deadline, not one per attempt. Nothing listens
    /// for the first attempt (the refused dial that earns a retry); the
    /// endpoint the retry reaches accepts and then dribbles a byte per slice
    /// without ever framing a reply, so the deadline — not a read timeout — is
    /// what ends it. The retry is handed only what is left of the same
    /// deadline, so the ask ends at the one deadline it was promised, never at
    /// the sum of two attempts' timeouts (finding A2).
    #[test]
    fn one_ask_spends_one_call_deadline() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Nothing listens for the first attempt...
        drop(listener);
        // ...and the endpoint the retry reaches is bound the moment the
        // refusal is announced, on this thread and before the backoff begins:
        // no sleep, and no server thread the box has not scheduled yet racing
        // the retry's dial. The thread below only has to accept what the
        // kernel has already queued — a dial completes into the listener's
        // backlog whatever the thread is doing.
        let (endpoint, opened) = std::sync::mpsc::channel::<TcpListener>();
        std::thread::spawn(move || {
            let Ok(listener) = opened.recv() else {
                return;
            };
            // The endpoint never answers: a byte per slice, never a frame.
            if let Ok((mut connection, _)) = listener.accept() {
                for _ in 0..40 {
                    if connection.write_all(b"a").is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });

        let cfg = ConfigHandle::own(mush_core::Config::new(
            format!("http://127.0.0.1:{port}"),
            "test",
            None,
        ));
        let model = HttpModel::new(cfg);
        let cancel = AtomicBool::new(false);
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
        let log = Recorder::new();
        let deadline = Duration::from_secs(1);
        let started = Instant::now();
        let once = std::cell::Cell::new(true);
        let error = retrying(
            crate::clock::system(),
            deadline,
            &cancel,
            |line| {
                log.emit(AgentId(7), AgentEvent::Notice(line.to_string()));
                // The first dial was refused: this is the moment the endpoint
                // the retry reaches must exist, so it is bound here.
                if once.replace(false) {
                    let listener = TcpListener::bind(("127.0.0.1", port))
                        .expect("the refused port is free for the retry's endpoint");
                    let _ = endpoint.send(listener);
                }
            },
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap_err();
        let elapsed = started.elapsed();

        assert!(matches!(error, ModelError::Transport(_)), "{error:?}");
        assert_eq!(
            retry_lines(&log).len(),
            1,
            "the refused dial was retried, so there really were two attempts"
        );
        assert!(
            elapsed >= deadline,
            "the call waits out its one deadline: {elapsed:?}"
        );
        // The upper side is the runaway guard, sized from the measurement:
        // the call came back 4-12 ms past its one-second deadline on this box
        // under both loads, and 300 ms is some twenty-five times that. It
        // stays well below the two seconds a second full deadline would cost
        // — the shape it exists to fail.
        assert!(
            elapsed < deadline + Duration::from_millis(300),
            "one ask spends one deadline, not one per attempt: {elapsed:?}"
        );
    }

    /// The retry that remains: a request that never went out because the
    /// endpoint could not be dialled is asked again, and the listener that was
    /// not there for the first attempt serves the second (finding A2).
    #[test]
    fn a_call_that_cannot_connect_is_retried() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Nothing listens yet: the first attempt is refused, which is the one
        // failure a repeat cannot duplicate.
        drop(listener);
        // The endpoint comes into existence the moment the refusal is
        // announced, on this thread and before the backoff begins — the same
        // shape as [`one_ask_spends_one_call_deadline`]'s and for the same
        // reason: a server thread the box has not scheduled yet cannot lose
        // the retry's dial to a sleep.
        let (endpoint, opened) = std::sync::mpsc::channel::<TcpListener>();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        std::thread::spawn(move || {
            let Ok(listener) = opened.recv() else {
                return;
            };
            if let Ok((mut connection, _)) = listener.accept() {
                counter.fetch_add(1, Ordering::SeqCst);
                read_whole_request(&mut connection);
                let answer = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    answer.len()
                );
                let _ = connection.write_all(head.as_bytes());
                let _ = connection.write_all(answer.as_bytes());
                let _ = connection.flush();
                // The reply is read whole before the close because the close
                // *waits* for the client: the FIN goes out behind the bytes
                // (a bare drop on unread request bytes would be an RST that
                // could discard them), and the drain reads whatever of the
                // request is left, so the close below cannot RST either. No
                // fixed sleep is needed, and none can be beaten by a busy box.
                fin_then_drain(&mut connection);
            }
        });

        let cfg = ConfigHandle::own(mush_core::Config::new(
            format!("http://127.0.0.1:{port}"),
            "test",
            None,
        ));
        let model = HttpModel::new(cfg);
        let cancel = AtomicBool::new(false);
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
        let log = Recorder::new();
        let once = std::cell::Cell::new(true);
        let reply = retrying(
            crate::clock::system(),
            Duration::from_secs(10),
            &cancel,
            |line| {
                log.emit(AgentId(7), AgentEvent::Notice(line.to_string()));
                if once.replace(false) {
                    let listener = TcpListener::bind(("127.0.0.1", port))
                        .expect("the refused port is free for the retry's endpoint");
                    let _ = endpoint.send(listener);
                }
            },
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap();

        assert_eq!(reply.choices[0].message.text(), "done");
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "the second attempt was the one served"
        );
        let lines = retry_lines(&log);
        assert_eq!(lines.len(), 1, "the refused dial was announced: {lines:?}");
        assert!(lines[0].contains("retrying (2/3)"), "{lines:?}");
    }

    /// Read one whole request off a test server's connection: the head, then
    /// the `Content-Length` body it promised. A server that answers a request
    /// it has not read whole leaves bytes unread, and closing on those is an
    /// RST — which can discard the very reply the client is reading.
    fn read_whole_request(connection: &mut std::net::TcpStream) {
        use std::io::Read as _;

        let mut request = Vec::new();
        let mut scratch = [0u8; 4096];
        let mut whole: Option<usize> = None;
        loop {
            if let Some(whole) = whole {
                if request.len() >= whole {
                    return;
                }
            }
            let read = connection.read(&mut scratch).unwrap_or(0);
            if read == 0 {
                return;
            }
            request.extend_from_slice(&scratch[..read]);
            if whole.is_none() {
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or(0);
                    whole = Some(end + 4 + length);
                }
            }
        }
    }

    /// End a test server's connection the way a peer going away does: the FIN
    /// first, then read until the client hangs up. The drain matters, because a
    /// bare close with request bytes unread is answered with an RST — and the
    /// wire would then say `Connection reset by peer` where the test is about a
    /// reply that never began. The timeout is the backstop for a client that
    /// never hangs up at all.
    fn fin_then_drain(connection: &mut std::net::TcpStream) {
        use std::io::Read as _;

        let _ = connection.shutdown(std::net::Shutdown::Write);
        let _ = connection.set_read_timeout(Some(Duration::from_secs(2)));
        let mut drain = [0u8; 1024];
        while let Ok(read) = connection.read(&mut drain) {
            if read == 0 {
                break;
            }
        }
    }

    /// The human's line, on the whole road: an endpoint that closes the
    /// connection it had answered on — a keep-alive timeout, the ordinary way —
    /// must not cost a retry when the next call runs. `http.rs` sees the close
    /// on the socket before it writes, so the pool's dead connection is
    /// replaced by a fresh dial and the run's transcript holds no
    /// `Broken pipe (os error 32) — retrying (2/3)`. Before the check this
    /// fixture produced exactly that line: the head went into the dead socket,
    /// the body write failed, and the retry — honest, since nothing whole had
    /// gone out — was one the human should never have had to read.
    #[test]
    fn a_kept_connection_the_endpoint_closed_costs_no_retry() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        std::thread::spawn(move || {
            for answer in ["one", "two"] {
                let Ok((mut connection, _)) = listener.accept() else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                read_whole_request(&mut connection);
                let body = format!(
                    r#"{{"choices":[{{"message":{{"role":"assistant","content":"{answer}"}},"finish_reason":"stop"}}]}}"#
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = connection.write_all(head.as_bytes());
                let _ = connection.write_all(body.as_bytes());
                let _ = connection.flush();
                // The keep-alive timeout. The whole request was read above, so
                // this close is the FIN an idle timer sends — not an RST that
                // could discard the reply the client is still reading.
                drop(connection);
            }
        });

        let cfg = ConfigHandle::own(mush_core::Config::new(
            format!("http://127.0.0.1:{port}"),
            "test",
            None,
        ));
        let model = HttpModel::new(cfg);
        let cancel = AtomicBool::new(false);
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

        // The first call: a fresh dial, answered, and its connection kept.
        let first = model.chat(&request, &cancel, CHAT_DEADLINE).unwrap();
        assert_eq!(first.choices[0].message.text(), "one");
        // The idle gap between two calls: a grace, not a bound. The server's
        // close is the statement after its flush (`drop(connection)` in the
        // loop above), so this only has to outlast the FIN's transit on
        // loopback and the server thread's scheduling between those two
        // statements; no assertion reads its duration. It cannot be replaced
        // by a wait for a fact — a client cannot observe a FIN it has not
        // read yet — and it is left as it was, because a box that delayed the
        // close past it is the only thing that could fail on it.
        std::thread::sleep(Duration::from_millis(100));

        // The second call, through the retry layer the run uses: it must be
        // served on a fresh dial, with nothing announced as a retry.
        let log = Recorder::new();
        let reply = retrying(
            crate::clock::system(),
            CHAT_DEADLINE,
            &cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap();

        assert_eq!(reply.choices[0].message.text(), "two");
        assert!(
            retry_lines(&log).is_empty(),
            "the closed connection cost no retry line: {:?}",
            retry_lines(&log)
        );
        assert_eq!(
            served.load(Ordering::SeqCst),
            2,
            "a fresh connection served the second call"
        );
    }

    /// A request the endpoint has read is never sent twice, however the call
    /// fails: the endpoint sees one POST, and the call ends once its one
    /// deadline is spent (finding A2). Before the ruling this shape was three
    /// sends and three deadlines, because both the reply that never came and
    /// the deadline itself were treated as retryable.
    #[test]
    fn a_request_the_endpoint_received_is_never_sent_twice() {
        use std::io::Read as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            // Long enough for the endpoint to have seen the request and every
            // send a regression would have made after it: the request was seen
            // 50-53 ms after the call began, measured under peak load (twelve
            // busy loops over the box's own load 25).
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        let mut scratch = [0u8; 8192];
                        if connection.read(&mut scratch).unwrap_or(0) > 0 {
                            let _ = tx.send(());
                        }
                        // Hold it open and never answer: the call has to give
                        // up on its own deadline, not on a reply.
                        held.push(connection);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let cfg = ConfigHandle::own(mush_core::Config::new(
            format!("http://127.0.0.1:{port}"),
            "test",
            None,
        ));
        let model = HttpModel::new(cfg);
        let cancel = AtomicBool::new(false);
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

        let error = retrying(
            crate::clock::system(),
            Duration::from_millis(300),
            &cancel,
            |_| {},
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap_err();

        assert!(matches!(error, ModelError::Transport(_)), "{error:?}");
        // A retry would have connected at once; wait, bounded, for the one
        // request the endpoint is owed, then give any later one the same
        // 150 ms grace the old fixed sleep did, and count what arrived. The
        // wait is sized from the measurement — 50-53 ms, under peak load —
        // and a busy box can only lengthen it, never shorten it: the endpoint
        // being slow can no longer read as a missing request.
        let mut seen: Vec<()> = Vec::new();
        if rx.recv_timeout(Duration::from_secs(5)).is_ok() {
            seen.push(());
        }
        std::thread::sleep(Duration::from_millis(150));
        seen.extend(rx.try_iter());
        assert_eq!(
            seen.len(),
            1,
            "one logical call, one request the endpoint may have received"
        );
    }

    /// Two failures that never left mush and then an answer: the call succeeds,
    /// the endpoint really was asked three times, every retry was announced,
    /// and the pause came through the clock seam instead of costing the suite
    /// the wait it proves (finding A2's one retryable class).
    #[test]
    fn an_unsent_request_is_retried_until_the_model_answers() {
        let hiccup = "Connection refused (os error 111)";
        let model = Arc::new(
            Scripted::new()
                .fails_unsent(hiccup)
                .fails_unsent(hiccup)
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

    /// A failure after the request went out is final, however retryable its
    /// kind would once have been: the endpoint may already have received the
    /// request, and a second send is one the human pays for twice (finding A2).
    #[test]
    fn a_wire_failure_after_the_request_went_out_is_final() {
        let dropped = "Connection reset by peer (os error 104)";
        let model = Arc::new(Scripted::new().fails_transport(dropped).says("too late"));
        let clock = Advanceable::new();
        let log = Recorder::new();

        let error = called(&model, &clock, &AtomicBool::new(false), &log).unwrap_err();

        assert_eq!(error, ModelError::Transport(dropped.to_string()));
        assert_eq!(
            model.asked().len(),
            1,
            "the request the endpoint may have seen is not sent again"
        );
        assert!(
            retry_lines(&log).is_empty(),
            "and nothing was announced as a retry: {:?}",
            retry_lines(&log)
        );
        assert_eq!(clock.elapsed(), Duration::ZERO, "no backoff was paid");
    }

    /// Every attempt fails before the request leaves mush: the caller gets the
    /// original failure, and the attempts are named — not a bare fourth message
    /// that hides what the wire actually said.
    #[test]
    fn unsent_failures_on_every_attempt_name_the_attempts() {
        let hiccup = "Connection refused (os error 111)";
        let model = Arc::new(
            Scripted::new()
                .fails_unsent(hiccup)
                .fails_unsent(hiccup)
                .fails_unsent(hiccup),
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
            ModelError::Unsent(reason) => {
                assert!(
                    reason.starts_with(hiccup),
                    "the original error reaches the caller: {reason}"
                );
                assert!(
                    reason.contains("3 attempts"),
                    "and the attempts are named: {reason}"
                );
            }
            other => panic!("an unsent request, not {other:?}"),
        }
        assert_eq!(
            retry_lines(&log).len(),
            RETRY_ATTEMPTS - 1,
            "both retries were announced before giving up"
        );
    }

    /// The other class the retry policy asks again, in its own words: a request
    /// that left whole but whose connection died before the reply began is
    /// asked again, and the line admits the attempt may already have been
    /// billed — the risk the measurement cannot rule out (finding A2).
    #[test]
    fn an_unanswered_connection_is_retried_until_the_model_answers() {
        let hiccup = "the endpoint dropped the connection before the reply began: \
                      Connection reset by peer (os error 104)";
        let model = Arc::new(Scripted::new().fails_unanswered(hiccup).says("done"));
        let clock = Advanceable::new();
        let log = Recorder::new();

        let reply = called(&model, &clock, &AtomicBool::new(false), &log).unwrap();

        assert_eq!(reply.choices[0].message.text(), "done");
        assert_eq!(model.asked().len(), 2, "the request was really made twice");
        assert_eq!(
            retry_lines(&log),
            vec![format!(
                "{hiccup} — asking again (2/3); that attempt may have been billed"
            )],
            "the exact line: the shape, and the money it may have cost"
        );
        assert_eq!(
            clock.elapsed(),
            RETRY_BACKOFF,
            "and the backoff came through the clock seam"
        );
    }

    /// Every attempt dies before the reply begins: the caller gets the class
    /// back with its attempts named — never a `Transport` that would claim the
    /// reply had begun when none did.
    #[test]
    fn unanswered_failures_on_every_attempt_name_the_attempts() {
        let reason = "the endpoint dropped the connection before the reply began";
        let model = Arc::new(
            Scripted::new()
                .fails_unanswered(reason)
                .fails_unanswered(reason)
                .fails_unanswered(reason),
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
            ModelError::Unanswered(message) => assert_eq!(
                message,
                format!("{reason} — {RETRY_ATTEMPTS} attempts failed"),
                "the original reason plus the count"
            ),
            other => panic!("an unanswered connection, not {other:?}"),
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
    /// the failure that earned one (docs/mush.md §5.5).
    #[test]
    fn a_cancellation_during_the_backoff_abandons_the_retry() {
        let model = Arc::new(
            Scripted::new()
                .fails_unsent("Connection refused (os error 111)")
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
    /// run — is classified as the wire by `HttpModel` (never as the endpoint's
    /// refusal), and the call is final: the request went out whole, so the
    /// endpoint is not asked again, and the endpoint sees exactly one
    /// connection (finding A2; B27 for the classification).
    #[test]
    fn a_cut_off_body_on_a_real_wire_is_final_and_asked_once() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut accepted = 0;
            // Long enough that a retry would have connected at once; see
            // [`ACCEPT_WINDOW`] for the measurement behind it.
            let until = Instant::now() + ACCEPT_WINDOW;
            while Instant::now() < until {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        accepted += 1;
                        let mut scratch = [0u8; 8192];
                        let _ = connection.read(&mut scratch);
                        // One whole chunk, then the stream ends: what a server
                        // dying mid-reply looks like to the client.
                        let _ = connection.write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
                        );
                        let _ = connection.flush();
                        // And the reply ends the way a dying server's does, but
                        // *cleanly*: `shutdown` puts a FIN behind the bytes
                        // above, so the client reads the cut-off body and then
                        // the stream's end — the shape this test is about.
                        //
                        // Not a bare `drop`. A close with any of the request
                        // still unread is answered with an RST instead of a
                        // FIN, and under load that arrived before the client
                        // had read the body, making the wire say `Connection
                        // reset by peer` where this test expects the cut-off
                        // body (observed once under the loaded suite). Send
                        // the FIN first, then keep the socket until the client
                        // has read everything and hung up: reading to the
                        // stream's end drains whatever of the request the
                        // first read missed, so the close below cannot RST,
                        // and it returns as soon as the client is done. The
                        // timeout is the backstop for a client that never
                        // hangs up at all.
                        let _ = connection.shutdown(std::net::Shutdown::Write);
                        let _ = connection.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut drain = [0u8; 1024];
                        while let Ok(read) = connection.read(&mut drain) {
                            if read == 0 {
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
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

        let error = retrying(
            crate::clock::system(),
            CHAT_DEADLINE,
            &cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap_err();

        match error {
            ModelError::Transport(reason) => assert_eq!(
                reason, "the reply ended inside its chunked body",
                "the wire failing, not the endpoint refusing"
            ),
            other => panic!("a cut-off body is the wire failing, not {other:?}"),
        }
        assert_eq!(
            server.join().unwrap(),
            1,
            "one logical call, one connection: the request went out whole"
        );
        assert!(retry_lines(&log).is_empty(), "and no retry was announced");
    }

    /// A connection that dies before the reply begins is the one read-side
    /// failure a repeat may make: not one byte of the reply arrived, so there
    /// is no partial answer a repeat could duplicate — only the chance the
    /// endpoint already ran and billed the attempt, which the line says. The
    /// first connection reads a whole request and then closes with nothing
    /// written; the second answers it. A death *mid-reply* is the test below,
    /// and stays final.
    #[test]
    fn a_connection_that_dies_before_the_reply_begins_is_asked_again() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        std::thread::spawn(move || {
            // The first attempt: the whole request is read, then the endpoint
            // closes without writing one byte. `fin_then_drain` makes that a
            // clean FIN; a bare `drop` on unread request bytes would be an RST,
            // and the wire would say `Connection reset by peer` where this test
            // is about the peer that simply went away.
            let Ok((mut connection, _)) = listener.accept() else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            read_whole_request(&mut connection);
            fin_then_drain(&mut connection);

            // The second attempt, the one the retry made: answered whole. The
            // close after it is a bare `drop` like the keep-alive fixture's:
            // the whole request was read, so it is a FIN and cannot discard the
            // reply that was written and flushed first.
            let Ok((mut connection, _)) = listener.accept() else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            read_whole_request(&mut connection);
            let answer = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                answer.len()
            );
            let _ = connection.write_all(head.as_bytes());
            let _ = connection.write_all(answer.as_bytes());
            let _ = connection.flush();
            drop(connection);
        });

        let cfg = ConfigHandle::own(mush_core::Config::new(
            format!("http://127.0.0.1:{port}"),
            "test",
            None,
        ));
        let model = HttpModel::new(cfg);
        let cancel = AtomicBool::new(false);
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
        let log = Recorder::new();

        let reply = retrying(
            crate::clock::system(),
            CHAT_DEADLINE,
            &cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap();

        assert_eq!(reply.choices[0].message.text(), "done");
        assert_eq!(
            served.load(Ordering::SeqCst),
            2,
            "the retry dialled a second connection"
        );
        assert_eq!(
            retry_lines(&log),
            vec![
                "the endpoint dropped the connection before the reply began — \
                 asking again (2/3); that attempt may have been billed"
                    .to_string()
            ],
            "the line names the shape and admits the bill"
        );
    }

    /// The retry's boundary, from the other side: a connection that dies after
    /// the reply began is final. Those bytes were already handed to mush's
    /// parser — so the endpoint may already have billed them — and a repeat
    /// would be a second question the human pays for. The peer answers a head
    /// and part of its `Content-Length` body, then goes; a retry would dial at
    /// once, and the accept loop counts every connection that ever arrived.
    #[test]
    fn a_connection_that_dies_mid_reply_is_final_and_asked_once() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut accepted = 0;
            // Long enough that a retry would have connected at once; see
            // [`ACCEPT_WINDOW`] for the measurement behind it.
            let until = Instant::now() + ACCEPT_WINDOW;
            while Instant::now() < until {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        accepted += 1;
                        read_whole_request(&mut connection);
                        // A head that promised 40 bytes, then eight of them:
                        // the reply began, and ended mid-body.
                        let _ = connection.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                              Content-Length: 40\r\n\r\n{\"choices\"",
                        );
                        let _ = connection.flush();
                        fin_then_drain(&mut connection);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
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
        let log = Recorder::new();

        let error = retrying(
            crate::clock::system(),
            CHAT_DEADLINE,
            &cancel,
            |line| log.emit(AgentId(7), AgentEvent::Notice(line.to_string())),
            |left| model.chat(&request, &cancel, left),
        )
        .unwrap_err();

        match error {
            ModelError::Transport(reason) => assert_eq!(
                reason, "the body ended before its Content-Length",
                "the wire failing after the reply began"
            ),
            other => panic!("a mid-reply death is the wire failing, not {other:?}"),
        }
        assert_eq!(
            server.join().unwrap(),
            1,
            "one logical call, one connection: the reply began, so nothing is sent twice"
        );
        assert!(retry_lines(&log).is_empty(), "and no retry was announced");
    }

    /// A reply whose framing broke is not an answer: nothing of it was handed to
    /// the caller, and the class keeps that distinction. It is final, though —
    /// the request went out whole, so the endpoint may already have read it and
    /// charged for it — and the connection that carried the broken frame is
    /// dropped, never kept (finding A2, B27).
    #[test]
    fn a_broken_frame_is_final_and_keeps_its_class() {
        let broken = "malformed chunk size: \"\"";
        let model = Arc::new(
            Scripted::new()
                .fails(ModelError::Framing(broken.to_string()))
                .says("too late"),
        );
        let clock = Advanceable::new();
        let log = Recorder::new();

        let error = called(&model, &clock, &AtomicBool::new(false), &log).unwrap_err();

        assert_eq!(error, ModelError::Framing(broken.to_string()));
        assert_eq!(
            model.asked().len(),
            1,
            "the request may already have been received, so it is not asked again"
        );
        assert!(
            retry_lines(&log).is_empty(),
            "and nothing was announced as a retry"
        );
        assert_eq!(clock.elapsed(), Duration::ZERO, "and nothing was waited");
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
