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
//! scripted model can serve a parent, its children and their children.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mush_core::message::{ChatRequest, ChatResponse};
use mush_core::Config;

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
    /// read: a body past [`http`]'s cap, a malformed status line or chunk line.
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
    cfg: Arc<Mutex<Config>>,
}

impl HttpModel {
    pub fn new(cfg: Arc<Mutex<Config>>) -> Self {
        Self { cfg }
    }
}

impl ModelClient for HttpModel {
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<ChatResponse, ModelError> {
        let cfg =
            self.cfg.lock().map(|cfg| cfg.clone()).map_err(|_| {
                ModelError::Unreachable("shared configuration poisoned".to_string())
            })?;

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
