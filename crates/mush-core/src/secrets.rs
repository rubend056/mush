//! The secrets mush holds, and the rule that the children it starts for itself
//! never inherit one.
//!
//! The provider credential is **mush's**. Its one road is the wire: mush reads
//! it — from `MUSH_API_KEY` (`config::EnvText`) or from the home config — to
//! authenticate its own requests, and it writes the value down only where the
//! human asked it to. Every process mush starts for its own purposes is started
//! for someone else's: the model's `sh -c`, a `git` for the pane's status, a
//! clipboard tool. None of them talks to the provider, and the model is the
//! least trustworthy reader in the system — a repository's README, a brief or a
//! pasted file can tell it to run `env`. What a command prints is a tool
//! result, a tool result is stored verbatim in `.mush/session.json`, and the
//! store is handed to any local user through the attach socket: a child that
//! inherits the key can put it somewhere durable, in one turn, without the
//! human's say-so (finding C1).
//!
//! [`SECRET_ENV`] is the one list of what that rule removes, and [`scrub`] is
//! the one way to apply it, so the next secret mush learns to read is scrubbed
//! by construction: name it in the list and every child is started without it.
//! What is *not* in the list is deliberate. `PATH`, `HOME`, `LANG`, `EDITOR`
//! and the human's own tooling are what their command needs, and the rest of
//! the `MUSH_*` block — `MUSH_URL`, `MUSH_MODEL`, `MUSH_PROVIDER`,
//! `MUSH_CONFIG`, `MUSH_CONTEXT`, `MUSH_REASONING_EFFORT`, `MUSH_THINKING` —
//! is configuration rather than a secret: an endpoint, a model name or a path
//! is not a credential, and a child that can see where mush points leaks
//! nothing by knowing it. If a child ever genuinely needs a scrubbed variable
//! back — a credential helper with its own `MUSH_*` spelling, say — its spawn
//! site names that variable explicitly, with the reason beside it, rather than
//! this list shrinking.

use std::process::Command;

/// Every environment variable whose *value* is a secret mush holds.
///
/// Today there is one: `MUSH_API_KEY`, the provider credential — read once, by
/// `config::EnvText`, and valid only for the endpoint mush itself was pointed
/// at. A name belongs here only when reading it back out of a child's
/// environment would be a credential leak; configuration mush reads stays
/// inherited (see the module doc for the list and the reason).
pub const SECRET_ENV: &[&str] = &["MUSH_API_KEY"];

/// Tell `command` to start without every variable in [`SECRET_ENV`].
///
/// One call per spawn site, before `spawn` or `output`: the model's shell
/// (`machine::Shell`), the `git` and clipboard children, the `kill` that ends a
/// command's group. It is an `env_remove`, never an `env_clear`, so everything
/// the command was written against — `PATH`, `HOME`, `LANG`, `EDITOR`, the
/// human's tooling — is inherited exactly as it was.
pub fn scrub(command: &mut Command) -> &mut Command {
    for name in SECRET_ENV {
        command.env_remove(name);
    }
    command
}
