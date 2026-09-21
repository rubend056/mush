//! mush-core — the agent-agnostic heart of mush.
//!
//! This crate is pure domain logic: OpenAI-compatible message types, the simple
//! system prompt, conversation persistence under `.mush/`, and safe workspace
//! file operations. It performs file I/O but never touches a terminal, a
//! socket, or a thread. That is what makes it easy to test and hard to break.

pub mod config;
pub mod git;
pub mod message;
pub mod prompt;
pub mod provider;
pub mod session;
pub mod text;
pub mod tools;
pub mod transcript;
pub mod userconfig;
pub mod workspace;

pub use config::{Config, Overrides};
pub use git::{RepoStatus, Stat, Worktree};
pub use message::{FunctionCall, Image, Message, ToolCall, Usage};
pub use provider::Provider;
pub use session::Session;
pub use userconfig::UserConfig;
pub use workspace::Workspace;

/// Ceiling for bytes of command output handed to a model in one result. The
/// command is now the only road by which a big text result reaches the model —
/// the file tools are gone — so this is the old read ceiling, and
/// `Config::cmd_cap` scales it down to a quarter of the history budget for a
/// window too small to hold it.
pub const CMD_CAP: usize = 16_000;
/// How long a shell command may run before it is killed.
pub const CMD_TIMEOUT_SECS: u64 = 120;
