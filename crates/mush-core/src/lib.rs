//! mush-core — the agent-agnostic heart of mush.
//!
//! This crate is pure domain logic: OpenAI-compatible message types, the simple
//! system prompt, conversation persistence under `.mush/`, and safe workspace
//! file operations. It performs file I/O but never touches a terminal, a
//! socket, or a thread. That is what makes it easy to test and hard to break.

pub mod config;
pub mod message;
pub mod prompt;
pub mod session;
pub mod tools;
pub mod userconfig;
pub mod workspace;

pub use config::{Config, Overrides, Provider};
pub use message::{FunctionCall, Message, ToolCall};
pub use session::Session;
pub use userconfig::UserConfig;
pub use workspace::Workspace;

/// Maximum bytes of file content handed to a model in one read.
pub const READ_CAP: usize = 16_000;
/// Maximum bytes of command output handed to a model in one result.
pub const CMD_CAP: usize = 6_000;
/// How long a shell command may run before it is killed.
pub const CMD_TIMEOUT_SECS: u64 = 120;
/// Maximum number of files returned by a single listing.
pub const LIST_LIMIT: usize = 4_000;
