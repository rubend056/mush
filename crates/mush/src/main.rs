//! mush — a small, fast terminal surface for coding agents.
//!
//! Usage: `mush [DIRECTORY]` opens a workspace. An agent connected to the
//! configured OpenAI-compatible endpoint reads and writes it; mush shows the
//! tree of agents and the repository's state at a glance.

mod agent;
mod app;
mod attach;
mod clock;
mod events;
mod http;
mod ids;
mod input;
mod jobs;
mod lock;
mod machine;
mod model;
mod session_save;
mod theme;
mod ui;

use std::error::Error;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;

use mush_core::text::mask_key;
use mush_core::{config, session, Config, Overrides, Session, UserConfig, Workspace};
use serde_json::Value;

use app::{App, ConfigCell, Msg};

/// `-y` / `--yes`: this session's human pre-approved work that a feature would
/// otherwise stop and ask about. Nothing asks yet — the design doc's
/// single-owner rule keeps a human on the other end of every write — so the
/// flag is only *recorded*: it prints nothing, sets no config, and changes no
/// behaviour. The approval prompts that will ask read it from here.
static AUTO_APPROVE: AtomicBool = AtomicBool::new(false);

/// Whether `-y` / `--yes` was given. [`print_config`] reports it, and so will
/// the features that ask.
pub(crate) fn auto_approve() -> bool {
    AUTO_APPROVE.load(Ordering::Relaxed)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mush: {error}");
        std::process::exit(1);
    }
}

struct Args {
    dir: PathBuf,
    /// The command line as the config layer sees it, in the layer's own shape:
    /// a flag sets one field here and nowhere else, so a new knob cannot be
    /// left out of a hand-copied second struct. mush has no API-key flag; a key
    /// comes from `MUSH_API_KEY` or the home config, so `api_key` stays `None`.
    overrides: Overrides,
    /// `-y` / `--yes`: recorded in [`AUTO_APPROVE`] and nowhere else.
    yes: bool,
    /// `--print-config`: print the resolved config and exit, instead of opening
    /// the terminal.
    print_config: bool,
}

fn parse_args() -> Result<Args, String> {
    parse_from(std::env::args().skip(1))
}

/// [`parse_args`] over an explicit argument list, so the flags are testable
/// without a process environment.
fn parse_from<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let mut dir: Option<PathBuf> = None;
    let mut overrides = Overrides::default();
    let mut yes = false;
    let mut print_config = false;
    let mut only_flags = false;

    while let Some(arg) = args.next() {
        // After `--` every argument is a path, however much it looks like a
        // flag: a directory named `--url` has to be openable (finding A13).
        if only_flags {
            set_dir(&mut dir, &arg)?;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("mush {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--" => only_flags = true,
            "-y" | "--yes" => yes = true,
            "--print-config" => print_config = true,
            "--max-completion-tokens" => overrides.max_completion_tokens = Some(true),
            "--url" => overrides.url = Some(args.next().ok_or("--url needs a value")?),
            "--model" => overrides.model = Some(args.next().ok_or("--model needs a value")?),
            "--provider" => {
                overrides.provider = Some(args.next().ok_or("--provider needs a value")?)
            }
            "--context" => {
                let value = args.next().ok_or("--context needs a value")?;
                let tokens = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--context needs a token count, got `{value}`"))?;
                overrides.context = Some(tokens);
            }
            "--temperature" => {
                let value = args.next().ok_or("--temperature needs a value")?;
                // A value mush cannot use is reported by name rather than
                // dropped on the floor (finding A16's class). `NaN` and the
                // infinities parse as floats but are not temperatures.
                let stated = value
                    .parse::<f32>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .ok_or_else(|| format!("--temperature needs a number, got `{value}`"))?;
                overrides.temperature = Some(stated);
            }
            "--reasoning-effort" => {
                let value = args.next().ok_or("--reasoning-effort needs a value")?;
                // Rejected by name rather than ignored, like every other value
                // mush cannot use: an unknown effort must never reach an
                // endpoint, and dropping it would send the provider's default
                // instead of the effort the human asked for.
                let stated = config::ReasoningEffort::parse(&value).map_err(|_| {
                    format!("--reasoning-effort needs low, high or max, got `{value}`")
                })?;
                overrides.reasoning_effort = Some(stated);
            }
            "--thinking" => {
                let value = args.next().ok_or("--thinking needs a value")?;
                // The same rule: `--thinking of` is a typo, not an instruction
                // to leave the thinking mode on.
                let stated = config::ThinkingMode::parse(&value)
                    .map_err(|_| format!("--thinking needs on or off, got `{value}`"))?;
                overrides.thinking = Some(stated);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option `{other}` (try --help)"));
            }
            other => set_dir(&mut dir, other)?,
        }
    }

    Ok(Args {
        dir: dir.unwrap_or_else(|| PathBuf::from(".")),
        overrides,
        yes,
        print_config,
    })
}

fn set_dir(dir: &mut Option<PathBuf>, value: &str) -> Result<(), String> {
    if dir.is_some() {
        // Silently opening one of two directories is worse than saying so
        // (finding A14).
        return Err(format!(
            "only one directory may be given (got `{}` as well)",
            PathBuf::from(value).display()
        ));
    }
    *dir = Some(PathBuf::from(value));
    Ok(())
}

/// A subcommand that drives a *running* mush instead of opening a TUI (M3).
///
/// Each speaks one request to `<dir>/.mush/mush.sock`, prints the answer, and
/// exits — the directory is the same single optional argument the TUI takes,
/// and the subcommand's own value (an id, the text) comes before it.
#[derive(Debug, PartialEq)]
enum Cli {
    Agents {
        dir: PathBuf,
    },
    Read {
        dir: PathBuf,
        agent: u64,
        since: usize,
    },
    Focus {
        dir: PathBuf,
        agent: u64,
    },
    Edit {
        dir: PathBuf,
        agent: u64,
        base: u64,
        send: bool,
        text: String,
    },
}

/// The flags each attach subcommand takes, one row per subcommand, so a set is
/// written once and every reader agrees on it: [`Cli::detect`] reads the row of
/// the chosen subcommand before it parses any flag, and refuses one the row
/// does not name instead of parsing it and dropping the value. `mush agents
/// --since 3` and `mush read /w --send` used to parse with the value then
/// dropped, and a value mush cannot use is reported by name, never ignored
/// (finding A16's class, the rule the unknown-option arm and the second
/// directory already follow).
const ATTACH_FLAGS: &[(&str, &[&str])] = &[
    ("agents", &[]),
    ("read", &["--agent", "--since"]),
    // `--agent` is `focus`'s alternative to the positional id.
    ("focus", &["--agent"]),
    ("edit", &["--agent", "--base", "--send"]),
];

/// `Ok` when [`ATTACH_FLAGS`] gives `command` this flag; otherwise the refusal,
/// naming the flag, the subcommand that does not take it and `--help`, where
/// the ones it does take are listed.
fn require_flag(command: &str, flag: &str) -> Result<(), String> {
    let owned = ATTACH_FLAGS
        .iter()
        .any(|(name, flags)| *name == command && flags.contains(&flag));
    if owned {
        Ok(())
    } else {
        Err(format!(
            "`{flag}` is not a flag for `mush {command}` (try --help)"
        ))
    }
}

impl Cli {
    /// Recognise a subcommand as the first argument, or `None` for the TUI's
    /// own parsing. A directory named like a subcommand is not opened this way
    /// — the subcommand wins, and `--` is the escape hatch a human has.
    fn detect(argv: &[String]) -> Result<Option<Cli>, String> {
        let Some(name) = argv.first().map(String::as_str) else {
            return Ok(None);
        };
        if !matches!(name, "agents" | "read" | "focus" | "edit") {
            return Ok(None);
        }
        let mut agent: Option<u64> = None;
        let mut since = 0usize;
        let mut base = 0u64;
        let mut send = false;
        let mut positional: Vec<String> = Vec::new();
        let mut args = argv[1..].iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "--" => {
                    // End of options, as the ATTACH prose promises: everything
                    // after it is positional, so a directory named like a
                    // subcommand or a stray `--` in a script cannot turn into
                    // an unknown option (finding A5).
                    positional.extend(args.map(|arg| arg.to_string()));
                    break;
                }
                // Each flag asks the table first: a value for a flag this
                // subcommand does not take is refused before the value is even
                // read, so the refusal always blames the flag.
                "--agent" => {
                    require_flag(name, "--agent")?;
                    agent = Some(number(args.next(), "--agent")?);
                }
                "--since" => {
                    require_flag(name, "--since")?;
                    since = number(args.next(), "--since")? as usize;
                }
                "--base" => {
                    require_flag(name, "--base")?;
                    base = number(args.next(), "--base")?;
                }
                "--send" => {
                    require_flag(name, "--send")?;
                    send = true;
                }
                other if other.starts_with("--") => {
                    return Err(format!(
                        "unknown option `{other}` for `mush {name}` (try --help)"
                    ));
                }
                other => positional.push(other.to_string()),
            }
        }
        let cli = match name {
            "agents" => Cli::Agents {
                dir: trailing_dir(&positional, 0)?,
            },
            "read" => Cli::Read {
                dir: trailing_dir(&positional, 0)?,
                agent: agent.unwrap_or(0),
                since,
            },
            "focus" => Cli::Focus {
                dir: trailing_dir(&positional, 1)?,
                agent: match positional.first() {
                    Some(value) => parse_id(value, "focus")?,
                    None => agent.ok_or("`mush focus` needs an agent id")?,
                },
            },
            "edit" => Cli::Edit {
                dir: trailing_dir(&positional, 1)?,
                agent: agent.unwrap_or(0),
                base,
                send,
                text: positional
                    .first()
                    .ok_or("`mush edit` needs the text to set or send")?
                    .clone(),
            },
            _ => unreachable!(),
        };
        Ok(Some(cli))
    }

    fn dir(&self) -> &PathBuf {
        match self {
            Cli::Agents { dir }
            | Cli::Read { dir, .. }
            | Cli::Focus { dir, .. }
            | Cli::Edit { dir, .. } => dir,
        }
    }

    fn request(&self) -> attach::Request {
        let op = match self {
            Cli::Agents { .. } => attach::Op::Agents,
            Cli::Read { agent, since, .. } => attach::Op::Read {
                agent: *agent,
                since: *since,
            },
            Cli::Focus { agent, .. } => attach::Op::Focus { agent: *agent },
            Cli::Edit {
                agent,
                base,
                send,
                text,
                ..
            } => attach::Op::Edit {
                agent: *agent,
                base: *base,
                send: *send,
                text: text.clone(),
            },
        };
        attach::Request {
            id: serde_json::json!(1),
            op,
        }
    }

    /// Send the one request and act on the one answer: the rows on stdout for
    /// `read`/`agents`, nothing on success for `focus`/`edit`, and the message
    /// on stderr (as an `Err`, which `main` prints) when it failed.
    fn run(self) -> Result<(), String> {
        let response = attach::ask(self.dir(), &self.request())?;
        match response.reply {
            attach::Reply::Err(error) => Err(error.describe()),
            attach::Reply::Ok(body) => match &self {
                Cli::Read { .. } => print_lines(&body),
                Cli::Agents { .. } => print_agents(&body),
                Cli::Focus { .. } | Cli::Edit { .. } => Ok(()),
            },
        }
    }
}

/// The directory a subcommand takes: the positional *after* the ones the
/// subcommand owns, if there is one. At most one, like the TUI.
fn trailing_dir(positional: &[String], own: usize) -> Result<PathBuf, String> {
    match positional.len().saturating_sub(own) {
        0 => Ok(PathBuf::from(".")),
        1 => Ok(PathBuf::from(&positional[own])),
        _ => Err("only one directory may be given".to_string()),
    }
}

/// A flag's value as a number, reported by name when it is not one.
fn number(value: Option<&String>, flag: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("{flag} needs a value"))?;
    value
        .parse::<u64>()
        .map_err(|_| format!("{flag} needs a number, got `{value}`"))
}

fn parse_id(value: &str, command: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("`mush {command}` needs an agent id, got `{value}`"))
}

/// One transcript line, escaped so it prints as one line. A message with a
/// newline in it (a pasted brief, a tool result) is still one line on the wire,
/// and printing it raw made it read as two — under one index — with no way for
/// anything downstream to tell continuation from a new line (finding A3).
fn escape_line(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// `read`: the transcript lines, one per line as `index<TAB>text`.
fn print_lines(body: &Value) -> Result<(), String> {
    for line in attach::Transcript::read(body)?.lines {
        println!("{}\t{}", line.line, escape_line(&line.text));
    }
    Ok(())
}

/// `agents`: the roster the tree paints, one row per line, tab-separated:
/// `id parent phase activity title branch worktree children-working`. An empty
/// absent field prints as an empty column, and a root's missing parent as `-`.
///
/// The body is read as [`attach::Roster`] rather than fished key by key: a key
/// the producer renamed used to leave this printer writing an empty column
/// forever, with nothing failing (finding R23).
fn print_agents(body: &Value) -> Result<(), String> {
    for node in attach::Roster::read(body)?.agents {
        println!(
            "{id}\t{parent}\t{phase}\t{activity}\t{title}\t{branch}\t{worktree}\t{children}",
            id = node.id,
            parent = node
                .parent
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            phase = node.phase,
            activity = node.activity.unwrap_or_default(),
            title = node.title,
            branch = node.branch.unwrap_or_default(),
            worktree = node.worktree,
            children = node.children_working,
        );
    }
    Ok(())
}

fn print_help() {
    print!("{}", help_text());
}

/// The `--help` text.
///
/// Built as one string rather than printed line by line so a test can read it,
/// and so both tables come from their one source: the KEYS block is
/// [`app::keys::help_table`] (the same rows the in-app `/help` renders, so a
/// binding cannot be advertised here and missing there) and the command list is
/// the table the parser is written against, so `--help` cannot advertise a
/// command that does not exist, or miss one that does. `/compact` used to be
/// implemented, listed by `/help` and absent here, because this paragraph was
/// written by hand (the help/status drift half of finding B2). The provider
/// names are still the provider module's to spell.
fn help_text() -> String {
    let commands = app::commands::table(&mush_core::provider::names_piped());
    let keys = app::keys::help_table();
    format!(
        "mush {}\n\
         A small, fast terminal surface for coding agents.\n\n\
         USAGE:\n    mush [DIRECTORY] [--url URL] [--model NAME] [--provider NAME] [--context TOKENS]\n\
         \x20        [--temperature F] [--reasoning-effort LEVEL] [--thinking MODE]\n\
         \x20        [--max-completion-tokens] [-y] [--print-config]\n\n\
         OPTIONS:\n\
         \x20   --url URL          OpenAI-compatible endpoint (default: $MUSH_URL or the provider default)\n\
         \x20   --model NAME       Model id (default: $MUSH_MODEL, else auto-detected)\n\
         \x20   --provider NAME    {} (default: $MUSH_PROVIDER or {})\n\
         \x20   --context TOKENS   Context window when nothing else knows it (default: $MUSH_CONTEXT,\n\
         \x20                      else what the endpoint advertises, else the model's known window)\n\
         \x20   --temperature F    Sampling temperature, 0.0-2.0 (default: 1.0, the model's own choice)\n\
         \x20   --reasoning-effort LEVEL\n\
         \x20                      Reasoning effort sent as `reasoning_effort`: low, high or max\n\
         \x20                      (default: {}; $MUSH_REASONING_EFFORT)\n\
         \x20   --thinking MODE    on asks for the provider's thinking mode, off sends no `thinking`\n\
         \x20                      field at all (default: {}; $MUSH_THINKING)\n\
         \x20   --max-completion-tokens\n\
         \x20                      Send the reply cap ({}) as\n\
         \x20                      `max_completion_tokens` instead of `max_tokens`, as\n\
         \x20                      OpenAI's reasoning models require\n\
         \x20   -y, --yes          Pre-approve this session's work. Recorded only: mush asks\n\
         \x20                      nothing yet, so this changes no behaviour today\n\
         \x20   --print-config     Print the resolved config (endpoint, provider, the stored\n\
         \x20                      session, model, window and whether it was stated,\n\
         \x20                      temperature, reasoning effort and thinking mode, reply-cap\n\
         \x20                      size and name, key masked, auto-approve and theme) and exit 0\n\n\
         KEYS:\n{keys}\n\n\
         COMMANDS (type in the chat):\n\
         {commands}\n\
         ATTACH (drive a running mush from another shell; newline-delimited JSON):\n\
         \x20   mush agents [DIR]      list the agents and their state\n\
         \x20   mush read [DIR] [--agent N] [--since N]\n\
         \x20                         an agent's transcript lines\n\
         \x20   mush focus ID [DIR]   focus that agent, as Enter on its row does\n\
         \x20                         (--agent N instead of the id)\n\
         \x20   mush edit [--agent N] --base R [--send] TEXT [DIR]\n\
         \x20                         set the message box's draft, or send it as the human\n\
         \x20                         (--base 0 unless given; -- ends the options, for a\n\
         \x20                         directory named like one)\n\
         Endpoint, API key, model, and the request knobs live in\n\
         $MUSH_CONFIG or the platform config directory. That file is hand-editable,\n\
         every field is optional, and the one mush writes documents itself.\n\
         --print-config shows what those layers resolved to, and the theme the window\n\
         would wear ($MUSH_THEME: a hue's name, or 256, off, auto; default: the hue\n\
         this workspace's path hashes to). The conversation is stored in\n\
         <DIRECTORY>/.mush/session.json.\n",
        env!("CARGO_PKG_VERSION"),
        mush_core::provider::names_hint(),
        mush_core::provider::DEFAULT_PROVIDER.name(),
        mush_core::provider::effort_default_hint(),
        mush_core::provider::thinking_default_hint(),
        mush_core::config::REPLY_SHARE_WORDS,
    )
}

/// What the human is told when the stored conversation is there and cannot be
/// read.
///
/// Three things, because they are the three a human needs to get the work
/// back: which file, why mush could not use it, and where the only copy went.
/// It is a *failure* rather than a note — mush was supposed to bring this
/// conversation back and did not, and the fresh empty one on screen is not what
/// the human left here.
///
/// Paths are shown relative to the workspace: the bar already names where that
/// is (`⌂ …`), and an absolute prefix would spend the line on something the
/// human knows. The elision is [`Workspace::rel`]'s, the one rule for it — a
/// second one here would disagree about a path outside the root, or about the
/// separator a Windows path arrives with (refactor R17).
fn unreadable_session_notice(
    workspace: &Workspace,
    reason: &str,
    kept: Result<PathBuf, String>,
) -> String {
    let file = workspace.rel(&session::session_path(workspace.root()));
    match kept {
        Ok(kept) => format!(
            "could not read {file} — {reason}; kept as {} · starting a new conversation",
            workspace.rel(&kept)
        ),
        // The copy could not be set aside either. That is the worse half of the
        // news and it is said second, because naming a backup that is not there
        // would be the one lie this line must not tell. What it says instead is
        // the consequence the human has to act on — the file is still at the
        // path, and the next save writes over it — because a notice that
        // stopped at the reason left the only copy looking safe (finding S3).
        Err(error) => format!(
            "could not read {file} — {reason}; {error} · starting a new conversation, \
             and the next save will replace {file}, which is still there"
        ),
    }
}

/// The layers `--print-config` describes, resolved for `dir`.
///
/// A read and nothing else: no `.mush/` is created, no lock is taken and no
/// request is made, and a directory that does not exist yet is described as the
/// fresh workspace it would be. The theme is resolved from the same directory
/// argument, tolerating one that does not canonicalize yet, because the dump
/// describes a workspace that is allowed not to exist.
///
/// The session is read as [`session::Stored`], not through `Session::load`
/// — which answers `None` for a file that is *there and unusable* exactly as it
/// does for no file at all. The two are different facts about the layer, and a
/// dump that showed nothing for the first was showing a precedence chain with a
/// link silently missing (finding B2).
fn resolved_config(
    dir: &Path,
    overrides: &Overrides,
    env: &theme::EnvText,
) -> Result<(Config, session::Stored, theme::Theme), Box<dyn Error>> {
    let stored = Session::read(dir);
    let session = match &stored {
        session::Stored::Loaded(session) => Some(session),
        session::Stored::Absent | session::Stored::Unusable(_) => None,
    };
    let config = config::resolve(overrides, &UserConfig::load(), session)?;
    let theme = theme::Theme::resolve(env, dir)?;
    Ok((config, stored, theme))
}

/// `--print-config`: the resolved config and nothing else — no workspace, no
/// `.mush/`, no request to an endpoint. This is what makes a hand-edited home
/// file debuggable, and the only way to see the precedence chain rather than
/// guess at it.
fn print_config(config: &Config, stored: &session::Stored, theme: &theme::Theme) {
    for (field, value) in describe(config, auto_approve(), stored, theme) {
        println!("{field:<13}{value}");
    }
}

/// One `field  value` line per fact, in the order a human reads them. The
/// values are what a request will carry, not what some file wished for; the
/// window is the one fact whose *source* matters, so it is named.
fn describe(
    config: &Config,
    approved: bool,
    stored: &session::Stored,
    theme: &theme::Theme,
) -> Vec<(String, String)> {
    // The session is the workspace's own layer of the chain, and the dump says
    // what it was rather than flattening "no file" and "a file mush cannot
    // read" into the same silence (finding B2). The count is the
    // conversation's, so a readable file also says how much of one it holds;
    // the reason is *why* the file was refused, carried through for the human
    // who has to fix it by hand. A reason is built from what a hand-edited
    // file contained and reaches a terminal, so it goes through the same door
    // the model id does.
    let session_layer = match stored {
        session::Stored::Absent => "none".to_string(),
        session::Stored::Loaded(session) => {
            let messages = session.messages.len();
            format!(
                "read ({messages} message{})",
                if messages == 1 { "" } else { "s" }
            )
        }
        session::Stored::Unusable(reason) => {
            format!("unreadable — {}", mush_core::text::sanitize(reason))
        }
    };
    let key = match config.api_key.as_deref().filter(|key| !key.is_empty()) {
        Some(key) => format!("{} (masked)", mask_key(key)),
        None => "(none)".to_string(),
    };
    // The model row is the id a request carries, **defanged**: an id an endpoint
    // chose (adopted from `/v1/models`, or restored from the session) can carry
    // a control sequence, and this line reaches a terminal. It is not
    // [`Config::label`]: that is the facts line's whole `model @ endpoint`, and
    // this dump already has an `endpoint` row of its own. The word for an unset
    // model is the one the label uses, and the defanging is the same door
    // (`mush_core::text::sanitize`) every terminal-bound string goes through.
    let model = if config.model.is_empty() {
        "no model".to_string()
    } else {
        mush_core::text::sanitize(&config.model)
    };
    let window = if config.context_explicit {
        "stated"
    } else {
        "assumed from the model or the provider"
    };
    let cap = if config.uses_max_completion_tokens() {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    // The name alone would not say what a reply is cut off at, which is the one
    // number a truncated run makes a human want to see. The size is derived
    // from the window, under the same name the request will carry it.
    let reply_cap = format!("{} tokens as {cap}", config.reply_cap());
    // Both of these are stated values with a provider default, so the line says
    // which one a request will carry *and* where it came from: a value nobody
    // stated is the provider's, not the human's. Indexed by `stated`, so the
    // rule is written once.
    let sources = ["the provider's default", "stated"];
    let source = |stated: bool| sources[stated as usize];
    let effort = config.reasoning_effort().unwrap_or("none");
    let effort = format!("{effort} ({})", source(config.reasoning_effort_stated()));
    let thinking = format!(
        "{} ({})",
        if config.thinking_enabled() {
            "on"
        } else {
            "off"
        },
        source(config.thinking_stated())
    );
    let approve = if approved {
        "yes (-y recorded; nothing asks yet)"
    } else {
        "no"
    };
    vec![
        ("endpoint".to_string(), config.base_url.clone()),
        ("provider".to_string(), config.provider.name().to_string()),
        // Before the values it can supply: endpoint, provider, model and
        // window may all have come from this layer.
        ("session".to_string(), session_layer),
        ("model".to_string(), model),
        (
            "window".to_string(),
            format!("{} tokens ({window})", config.context_tokens),
        ),
        (
            "temperature".to_string(),
            format!("{:?}", config.temperature()),
        ),
        ("reasoning".to_string(), effort),
        ("thinking".to_string(), thinking),
        ("reply cap".to_string(), reply_cap),
        ("api key".to_string(), key),
        ("auto-approve".to_string(), approve.to_string()),
        // What the chrome would look like, hue and source together: a human
        // comparing two windows needs the fact `--print-config` shows to be
        // the one the window would have, environment included.
        ("theme".to_string(), theme.describe()),
    ]
}

/// Open the workspace directory, naming it when it cannot be opened.
///
/// `Workspace::new` is a canonicalize, and the io error it returns alone says
/// nothing a human can act on: `mush /typo` answered `No such file or
/// directory (os error 2)`, with no path named and no advice, while the sibling
/// case in [`run`] — a *file* where a directory was wanted — is careful to name
/// both (finding B1).
fn open_workspace(dir: &Path) -> Result<Workspace, String> {
    Workspace::new(dir).map_err(|error| {
        format!(
            "cannot open the workspace directory {}: {error} — check the path, and create \
             the directory first if it is not there yet",
            dir.display()
        )
    })
}

/// Create the workspace's `.mush/` store, naming what could not be made.
///
/// An unwritable workspace answered with a bare `Permission denied (os error
/// 13)`, the same defect as [`open_workspace`]'s (finding B1). The path is
/// spelled workspace-relative, the one elision rule the startup messages share
/// ([`Workspace::rel`], refactor R17).
fn ensure_mush_dir(workspace: &Workspace) -> Result<(), String> {
    session::ensure_mush_dir(workspace.root()).map_err(|error| {
        format!(
            "cannot create {}: {error} — mush keeps this workspace's conversation there, \
             and it has to be a writable directory",
            workspace.rel(&session::mushroom_dir(workspace.root()))
        )
    })
}

fn run() -> Result<(), Box<dyn Error>> {
    // A subcommand drives a *running* mush and never opens the TUI: it is
    // recognised before any config is resolved, so it needs no endpoint, key
    // or workspace of its own (M3).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(cli) = Cli::detect(&argv)? {
        cli.run()?;
        return Ok(());
    }
    let args = parse_args()?;
    // `-y` is a fact about this session, not a config value: it is recorded here
    // and read by the features that will ask (and by `--print-config`). Nothing
    // else changes because of it.
    AUTO_APPROVE.store(args.yes, Ordering::Relaxed);
    let overrides = args.overrides;

    let dir = args.dir;
    if dir.is_file() {
        // mush works on a directory: the agent's file tools, the git facts,
        // and the agent tree are all workspace-shaped. Opening a file would
        // promise an editing surface that no longer exists.
        return Err(format!(
            "mush works on a directory, not a file: {} — run mush in {} instead",
            dir.display(),
            dir.parent()
                .map(|parent| parent.display().to_string())
                .unwrap_or_else(|| ".".to_string())
        )
        .into());
    }

    // The three variables the theme decision reads, read once, here at the
    // edge; `theme::Theme::resolve` below is a pure function of them and a
    // path, so nothing further down touches the process environment.
    let env = theme::EnvText::read();

    if args.print_config {
        // The resolved config, then out: no terminal is entered, no `.mush/` is
        // created, no lock is taken and no request is made — [`resolved_config`]
        // is where the layers are read, and where a session file that cannot be
        // read stays a fact about the layer rather than being flattened into
        // absence (finding B2).
        let (config, stored, theme) = resolved_config(&dir, &overrides, &env)?;
        print_config(&config, &stored, &theme);
        return Ok(());
    }

    let workspace = open_workspace(&dir)?;
    // One hue per workspace, from the canonical root `open_workspace` just
    // resolved: two spellings of one directory are one window in one colour,
    // and the hue is handed to every frame below rather than re-derived.
    let theme = theme::Theme::resolve(&env, workspace.root())?;
    ensure_mush_dir(&workspace)?;
    // One mush per workspace, taken before anything is read or written: a
    // second process on this directory would write the same `session.json`, and
    // that write is a whole-file replace on a minute's debounce, so the two
    // conversations would erase each other in turn. A refused start leaves the
    // store exactly as it found it (see `lock`).
    let _lock = lock::acquire(workspace.root())?;

    // CLI flags > environment > saved session > home config > defaults; the
    // whole precedence lives in one tested function in mush-core.
    //
    // A session file mush cannot read is *not* the same as no session file
    // (finding S3): the first is a conversation the human still has, and the
    // save below would be the first thing to write over it. So the two are told
    // apart here, the unreadable one is kept beside itself before anything can
    // write, and the human is told — the app opens empty either way, and an
    // empty app with no explanation is how a lost conversation reads as one
    // that was never there.
    let (stored, unreadable) = match session::Session::read(workspace.root()) {
        session::Stored::Loaded(session) => (Some(session), None),
        session::Stored::Absent => (None, None),
        session::Stored::Unusable(reason) => {
            let kept = session::keep_unreadable(workspace.root());
            (
                None,
                Some(unreadable_session_notice(&workspace, &reason, kept)),
            )
        }
    };
    let config = config::resolve(&overrides, &UserConfig::load(), stored.as_ref())?;

    // Model discovery happens *after* the first frame, on its own thread. A
    // model from the startup precedence skips it entirely; when one has to be
    // discovered, the terminal must not wait for an endpoint that may be slow,
    // silent or unreachable before it paints anything (finding A9). The list
    // arrives as `Msg::Models`, and the app picks its first entry only if
    // nothing else named a model. `/model` and `/url` refetch on demand.
    let discovery = config.model.is_empty().then(|| config.clone());

    let (tx, rx) = unbounded::<Msg>();
    // One cell for the whole tree: the UI reads it, the root actor — and every
    // agent under it — reads the same one, so a runtime `/model` cannot reach
    // the screen without reaching the actors (finding B7).
    let cell = ConfigCell::own(config);
    let root = agent::spawn(cell.handle(), tx.clone(), workspace.root().to_path_buf());
    // The session goes out through its own thread: the UI thread hands a
    // snapshot over and keeps painting (see `session_save`). `App`'s drop is the
    // exit flush.
    let save = Arc::new(session_save::Writer::new(workspace.root().to_path_buf()));
    let attach_root = workspace.root().to_path_buf();
    let mut app = App::new(workspace, cell, stored, root, tx.clone(), save);
    // The attach socket comes up before the first frame, so a client can
    // connect the moment mush is running. A bind that fails is said on stderr
    // and mush runs without it — never a reason to die (M3); the guard removes
    // the socket file on the way out.
    let _attach = match attach::serve(&attach_root, tx.clone()) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("mush: attach disabled — {error}");
            None
        }
    };

    if let Some(cfg) = discovery {
        let tx = tx.clone();
        let endpoint = cfg.base_url.clone();
        let started = std::thread::Builder::new()
            .name("mush-models".to_string())
            .spawn(move || {
                let models = http::list_models(&cfg);
                let _ = tx.send(Msg::Models { endpoint, models });
            });
        if started.is_err() {
            // No thread to discover on means no list at all; the human is told
            // the same thing an endpoint that listed nothing would tell them,
            // rather than being left with a bar that says "no model".
            let endpoint = app.cfg().base_url.clone();
            app.update(Msg::Models {
                endpoint,
                models: Vec::new(),
            });
        }
    }

    // Before the first frame, so the line is one of the first things painted:
    // a workspace whose conversation could not be read is not a workspace with
    // nothing in it.
    if let Some(notice) = unreadable {
        app.session_unreadable(notice);
    }

    install_panic_hook();
    let mut guard = TerminalGuard::enter()?;
    // The terminal's size is only known here; `/notes` wraps its popup to it
    // and the floor is decided from it, so record both before the first key can
    // be read. A resize reports its own.
    if let Ok(size) = guard.terminal.size() {
        app.set_term_size(size.width, size.height);
    }
    let result = event_loop(&mut guard.terminal, &mut app, &rx, &theme);
    drop(guard);
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &Receiver<Msg>,
    theme: &theme::Theme,
) -> Result<(), Box<dyn Error>> {
    /// The most input events consumed before a frame is painted. A pasted
    /// megabyte is thousands of key events, and painting once at the end is
    /// what makes it instant; this cap is only so a firehose cannot starve the
    /// draw forever.
    const MAX_EVENTS_PER_FRAME: usize = 4096;

    while !app.should_quit {
        drain_actors(app, rx);

        // Read one event, then every event that is already available, and paint
        // *once*. Reading one event per frame is what made a paste crawl in one
        // character at a time and a held arrow key scroll on after the human let
        // go: each keystroke cost a full repaint, so the backlog drained at
        // frame rate while the terminal's own buffer kept filling.
        let mut timeout = Duration::from_millis(30);
        let mut handled = 0usize;
        while event::poll(timeout)? {
            // Something is queued: do not wait again inside this frame.
            timeout = Duration::ZERO;
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.update(Msg::Key(key));
                }
                // The whole paste, in one event.
                Event::Paste(text) => app.update(Msg::Paste(text)),
                // The terminal changed size: schedule a redraw. ratatui's
                // `terminal.draw` re-queries the size first, so the next
                // frame already paints at the new dimensions — and the app is
                // told the new size so a `/notes` report wraps to it and the
                // floor notice is raised or lowered (finding P11).
                Event::Resize(width, height) => {
                    app.set_term_size(width, height);
                    app.dirty_screen = true;
                }
                _ => {}
            }
            handled += 1;
            // Results that arrived while the burst was being drained must not
            // wait behind it.
            drain_actors(app, rx);
            if app.should_quit || handled >= MAX_EVENTS_PER_FRAME {
                break;
            }
        }

        app.tick();
        if app.dirty_screen {
            // One value, painted: `App` derives every word of the frame (the
            // layout tiers included), and `ui::draw` only paints it, so what is
            // on screen cannot be a second derivation of the state it draws.
            terminal.draw(|frame| {
                let screen = app.screen(frame.area());
                ui::draw(frame, &screen, theme)
            })?;
            app.dirty_screen = false;
        }
    }
    Ok(())
}

/// Fold in everything the agent actors have already said.
fn drain_actors(app: &mut App, rx: &Receiver<Msg>) {
    while let Ok(msg) = rx.try_recv() {
        app.update(msg);
    }
}

/// The terminal's modes, entered and left in one place.
///
/// Each mode is a promise to the human's shell: leaving raw mode on breaks their
/// typing, and leaving bracketed paste on makes their own pastes arrive wrapped
/// in escape codes. Everything that can end the program — a clean quit, a panic,
/// an error on the way out — has to undo all of them, so they are entered here
/// and undone by [`restore_terminal_modes`].
fn enter_terminal_modes() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste is what turns Ctrl-Shift-V from a stream of individual
    // keystrokes — one event, one repaint, and a redraw per character — into a
    // single `Event::Paste` carrying the whole paste.
    //
    // Mouse capture is deliberately NOT taken. It would let mush scroll by wheel
    // notch instead of by arrow key, but it also takes away the terminal's own
    // drag-to-select, and reading text out of the transcript is worth more than
    // a wheel notch. The lag that made the wheel feel broken was the per-keystroke
    // repaint, which the event loop no longer does.
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
}

/// Undo every mode [`enter_terminal_modes`] turned on.
fn restore_terminal_modes() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    );
}

/// Restores the terminal on both clean exit (Drop) and panic.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enter_terminal_modes()?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_modes();
        let _ = self.terminal.show_cursor();
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Every mode, or a panic leaves the human's shell in raw mode, or
        // swallowing its own pastes.
        restore_terminal_modes();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_core::Message;

    #[test]
    fn cli_overrides_map_to_the_config_layer() {
        let args = parse_from(
            [
                "-y",
                "--url",
                "http://host:1",
                "--provider",
                "deepseek",
                "--context",
                "64000",
                "--max-completion-tokens",
            ]
            .iter()
            .map(|arg| arg.to_string()),
        )
        .unwrap();
        let overrides = args.overrides;
        assert_eq!(overrides.url.as_deref(), Some("http://host:1"));
        assert_eq!(overrides.provider.as_deref(), Some("deepseek"));
        assert_eq!(overrides.model, None);
        assert_eq!(overrides.context, Some(64_000));
        assert_eq!(overrides.temperature, None, "unstated is not a value");
        assert_eq!(overrides.max_completion_tokens, Some(true));
        assert_eq!(overrides.reasoning_effort, None);
        assert_eq!(overrides.thinking, None);
        // The key never comes from argv, and `-y` is not a config value: it is
        // recorded for the features that will ask, and nothing else.
        assert_eq!(overrides.api_key, None);
        assert!(args.yes, "`-y` is not copied into the config layer");
    }

    /// The flags a human types reach the config layer, the session flag is
    /// recorded, and a directory still arrives as the positional argument.
    #[test]
    fn the_flags_a_human_types_are_parsed() {
        let argv = [
            "-y",
            "--temperature",
            "0.25",
            "--reasoning-effort",
            "max",
            "--thinking",
            "off",
            "--max-completion-tokens",
            "--print-config",
            "work",
        ];
        let args = parse_from(argv.into_iter().map(str::to_string)).unwrap();
        assert_eq!(args.dir, PathBuf::from("work"));
        assert_eq!(args.overrides.temperature, Some(0.25));
        assert_eq!(args.overrides.max_completion_tokens, Some(true));
        // A stated value survives parsing as one: the config layer has to be
        // able to tell it from silence.
        assert_eq!(
            args.overrides.reasoning_effort,
            Some(config::ReasoningEffort::Max)
        );
        assert_eq!(args.overrides.thinking, Some(config::ThinkingMode::Off));
        assert!(args.yes, "`-y` is remembered, not acted on");
        assert!(args.print_config);

        // Long form, and nothing else stated: every flag stays unset.
        let args = parse_from(["--yes".to_string()].into_iter()).unwrap();
        assert!(args.yes);
        assert_eq!(args.overrides.temperature, None);
        assert_eq!(args.overrides.max_completion_tokens, None);
        assert_eq!(
            args.overrides.reasoning_effort, None,
            "unstated is not a value"
        );
        assert_eq!(args.overrides.thinking, None);
        assert!(!args.print_config);
        assert_eq!(args.dir, PathBuf::from("."));
    }

    /// A value mush cannot use is reported by name, never ignored (the A13/A16
    /// class): every flag that takes one says which flag was wrong.
    #[test]
    fn a_bad_flag_value_names_its_flag() {
        fn error_of(argv: &[&str]) -> String {
            match parse_from(argv.iter().map(|arg| arg.to_string())) {
                Err(error) => error,
                Ok(_) => panic!("{argv:?} was accepted"),
            }
        }
        for (argv, flag) in [
            (["--temperature", "warm"], "--temperature"),
            (["--context", "8k"], "--context"),
            (["--reasoning-effort", "very"], "--reasoning-effort"),
            (["--thinking", "of"], "--thinking"),
        ] {
            let error = error_of(&argv);
            assert!(error.contains(flag), "{error}");
            // The value that was wrong is named too, so a typo is fixable
            // without guessing which argument mush meant.
            assert!(error.contains(argv[1]), "{error}");
        }
        // A float that is not a number is not a temperature either: `NaN`
        // compares false against every bound, so it must not reach a request.
        for value in ["nan", "inf", "-inf"] {
            let error = error_of(&["--temperature", value]);
            assert!(error.contains(value), "{error}");
        }
    }

    /// The sentence a workspace whose session could not be read is told. It has
    /// to name the file, the reason and where the only copy went — and when the
    /// copy could not be set aside it must say *that* rather than name a backup
    /// that is not there, plus what happens now to the copy still on disk. The
    /// file is shown relative to the workspace by [`Workspace::rel`], the one
    /// elision rule (refactor R17).
    #[test]
    fn the_unreadable_session_notice_names_the_file_and_what_becomes_of_it() {
        let ws = scratch_workspace("unreadable-notice");
        let kept = Ok(ws.root().join(".mush/session.json.bak"));
        let notice = unreadable_session_notice(&ws, "expected value at line 1 column 2", kept);
        assert_eq!(
            notice,
            "could not read .mush/session.json — expected value at line 1 column 2; \
             kept as .mush/session.json.bak · starting a new conversation"
        );
        // Relative to the workspace: the bar already says where that is, and an
        // absolute `/w/.mush/…` would spend the line on a prefix the human
        // already knows.
        assert!(
            !notice.contains(&ws.root().display().to_string()),
            "{notice}"
        );

        // The copy could not be set aside either. That is the worse half of the
        // news, and the line says it instead of pointing at a file that is not
        // there — then says the consequence, which is the half the human has to
        // act on: the conversation starts empty, the file is still where it was,
        // and the next save writes over it (finding S3).
        let notice = unreadable_session_notice(
            &ws,
            "expected value at line 1 column 2",
            Err("cannot keep session.json — Permission denied".to_string()),
        );
        assert!(notice.contains("cannot keep session.json"), "{notice}");
        assert!(!notice.contains("kept as"), "{notice}");
        assert!(!notice.contains(".bak"), "no backup was written: {notice}");
        assert!(notice.contains("starting a new conversation"), "{notice}");
        assert!(
            notice.contains("the next save will replace .mush/session.json"),
            "{notice}"
        );
        assert!(
            notice.contains("which is still there"),
            "the file the move could not take is still where it was: {notice}"
        );
        assert!(
            !notice.contains(&ws.root().display().to_string()),
            "the same workspace-relative spelling as the `Ok` arm: {notice}"
        );
        let _ = std::fs::remove_dir_all(ws.root());
    }

    /// A workspace in a directory of its own, for the tests that read a path
    /// the way a human does.
    fn scratch_workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("mush-main-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(&dir).unwrap()
    }

    /// A directory mush cannot open names the directory and what to do about
    /// it, instead of printing only the operating system's errno: `mush /typo`
    /// used to answer `No such file or directory (os error 2)`, naming neither
    /// the argument nor anything a human could do about it (finding B1). The
    /// sibling case — a *file* where a directory was wanted — has named both
    /// from the start.
    #[test]
    fn an_unopenable_workspace_names_the_directory() {
        let dir = std::env::temp_dir().join(format!("mush-main-typo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let error = open_workspace(&dir).unwrap_err();
        assert!(error.contains(&dir.display().to_string()), "{error}");
        assert!(error.contains("No such file or directory"), "{error}");
        assert!(
            error.contains("create the directory first"),
            "the advice a human can act on: {error}"
        );
    }

    /// `.mush/` that cannot be created names the path and what it is for: an
    /// unwritable workspace used to answer with a second bare errno (finding
    /// B1). A file standing where the directory belongs is the one failure
    /// every machine can produce on purpose.
    #[test]
    fn a_store_that_cannot_be_created_names_the_path() {
        let ws = scratch_workspace("store-blocked");
        std::fs::write(ws.root().join(".mush"), "not a directory").unwrap();
        let error = ensure_mush_dir(&ws).unwrap_err();
        assert!(error.contains(".mush"), "{error}");
        assert!(
            error.contains("File exists"),
            "the reason is carried through: {error}"
        );
        assert!(
            error.contains("writable directory"),
            "the advice a human can act on: {error}"
        );
        let _ = std::fs::remove_dir_all(ws.root());
    }

    /// A second positional is an error rather than a silent replacement
    /// (finding A14), and `--` still ends the flags (finding A13).
    #[test]
    fn a_second_directory_is_an_error() {
        let error = match parse_from(["one", "two"].iter().map(|arg| arg.to_string())) {
            Err(error) => error,
            Ok(_) => panic!("two directories were accepted"),
        };
        assert!(error.contains("two"), "{error}");

        let args = parse_from(["--", "--yes"].iter().map(|arg| arg.to_string())).unwrap();
        assert_eq!(args.dir, PathBuf::from("--yes"));
        assert!(!args.yes, "after `--` even `--yes` is a path");
    }

    /// `--print-config` reports what a request will carry — the endpoint, the
    /// window and *where it came from*, the reply cap's size and its name, and a
    /// masked key — rather than what any one file wished for.
    ///
    /// The cap's size belongs there: a run that ends with `reply cut off at N
    /// tokens` makes that N the one number a human wants to see before the run,
    /// not after it.
    #[test]
    fn describe_reports_the_request_not_the_wishes() {
        let mut cfg = Config::new(
            "http://host:1",
            "deepseek-v4-pro",
            Some("sk-1234567890".into()),
        );
        cfg.set_context(64_000);
        cfg.temperature = 0.0;
        cfg.max_completion_tokens = true;
        cfg.reasoning_effort = Some(config::ReasoningEffort::Max);
        cfg.thinking = Some(config::ThinkingMode::Off);

        let lines = describe(
            &cfg,
            true,
            &session::Stored::Absent,
            &theme::Theme::default(),
        );
        let field = |name: &str| {
            lines
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("no `{name}` line in {lines:?}"))
        };
        assert_eq!(field("endpoint"), "http://host:1");
        assert_eq!(field("provider"), "custom");
        // The id itself, not the facts line's `model @ endpoint`: the endpoint
        // is a row of its own two lines above.
        assert_eq!(field("model"), "deepseek-v4-pro");
        assert_eq!(field("window"), "64000 tokens (stated)");
        assert_eq!(field("temperature"), "0.0", "0 is a value, not an absence");
        assert_eq!(field("reasoning"), "max (stated)");
        assert_eq!(field("thinking"), "off (stated)");
        assert_eq!(
            field("reply cap"),
            format!("{} tokens as max_completion_tokens", cfg.reply_cap()),
            "the cap the config derives, under the name it chose"
        );
        assert_eq!(field("api key"), "sk-1…7890 (masked)");
        assert_eq!(field("auto-approve"), "yes (-y recorded; nothing asks yet)");

        // An unresolved window says so, and a default request samples at 1.0
        // under the name every endpoint documents.
        let plain = Config::new("http://host:1", "", None);
        let cap = plain.reply_cap();
        let plain = describe(
            &plain,
            false,
            &session::Stored::Absent,
            &theme::Theme::default(),
        );
        let field = |name: &str| {
            plain
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        assert_eq!(field("model"), "no model");
        assert_eq!(
            field("window"),
            "8192 tokens (assumed from the model or the provider)"
        );
        assert_eq!(field("temperature"), "1.0");
        // Nothing stated: the two knobs report the provider default they will
        // send, and name it as such rather than claiming the human asked.
        assert_eq!(field("reasoning"), "none (the provider's default)");
        assert_eq!(field("thinking"), "off (the provider's default)");
        assert_eq!(
            field("reply cap"),
            format!("{} tokens as max_tokens", cap),
            "an 8192-token window affords the floor, and the row says which number that is"
        );
        assert_eq!(field("api key"), "(none)");
        assert_eq!(field("auto-approve"), "no");

        // DeepSeek with nothing stated is the preset request: the effort and
        // the thinking mode are sent, and the line says whose they are.
        let mut preset = Config::new("https://api.deepseek.com", "deepseek-flash", None);
        preset.provider = config::Provider::DeepSeek;
        let preset = describe(
            &preset,
            false,
            &session::Stored::Absent,
            &theme::Theme::default(),
        );
        let field = |name: &str| {
            preset
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        assert_eq!(field("reasoning"), "high (the provider's default)");
        assert_eq!(field("thinking"), "on (the provider's default)");
    }

    /// The theme row says what a window would look like: the hue, the form the
    /// terminal would paint it in, and whether the workspace path or
    /// `MUSH_THEME` chose it. The fixed palette says it is fixed, rather than
    /// naming a hue nobody chose.
    #[test]
    fn describe_reports_the_theme_a_window_would_wear() {
        let cfg = Config::new("http://host:1", "m", None);
        let value = |theme: &theme::Theme| {
            describe(&cfg, false, &session::Stored::Absent, theme)
                .into_iter()
                .find(|(field, _)| field == "theme")
                .map(|(_, value)| value)
                .expect("no `theme` line")
        };
        assert_eq!(value(&theme::Theme::default()), "none (fixed colours)");

        let env = theme::EnvText {
            theme: None,
            colorterm: Some("truecolor".to_string()),
            term: None,
        };
        let themed = theme::Theme::resolve(&env, std::path::Path::new("/work")).unwrap();
        let name = themed.hue().unwrap().name;
        assert_eq!(
            value(&themed),
            format!("{name} (truecolor, from the workspace path)")
        );

        // A named hue names `MUSH_THEME` as its source, and an indexed form is
        // said as indexed — the row cannot claim a truecolor paint a 256-colour
        // terminal would not make.
        let env = theme::EnvText {
            theme: Some("teal".to_string()),
            colorterm: None,
            term: None,
        };
        let named = theme::Theme::resolve(&env, std::path::Path::new("/work")).unwrap();
        assert_eq!(value(&named), "teal (indexed, MUSH_THEME)");
    }

    /// An id an endpoint chose — adopted from `/v1/models`, or restored from
    /// the session — must not rename the terminal through `--print-config`.
    /// The row carries the id itself, defanged by the same door the facts line
    /// and every other terminal-bound string goes through (finding §4 of the
    /// contract audit).
    #[test]
    fn describe_defangs_the_model_an_endpoint_named() {
        let hostile = "boom\rREST \x1b]0;PWNED\x07\x1b[2J\x1b[Hmock";
        let cfg = Config::new("http://x:1", hostile, None);
        let lines = describe(
            &cfg,
            false,
            &session::Stored::Absent,
            &theme::Theme::default(),
        );
        let model = lines
            .iter()
            .find(|(field, _)| field == "model")
            .map(|(_, value)| value.clone())
            .unwrap();
        assert!(!model.contains('\x1b'), "{model:?}");
        assert!(!model.contains('\r'), "{model:?}");
        assert_eq!(
            model, "boom␍REST mock",
            "the id, minus what a terminal obeys"
        );
    }

    /// The session the shipped DeepSeek defaults describe, as `--print-config`
    /// spells it: the window the human asked for, and a reply cap a quarter of
    /// it under the name every endpoint documents — not the 20_480 a real run
    /// was cut off at. The cap's *size* is on the line precisely so this can be
    /// read before a run instead of after one.
    #[test]
    fn the_shipped_deepseek_session_reports_the_cap_it_sends() {
        let mut config = Config::new("https://api.deepseek.com", "", None);
        config.provider = config::Provider::DeepSeek;
        config.rederive_context();
        assert!(!config.context_explicit, "a default, not a statement");

        let lines = describe(
            &config,
            false,
            &session::Stored::Absent,
            &theme::Theme::default(),
        );
        let field = |name: &str| {
            lines
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("no `{name}` line in {lines:?}"))
        };
        assert_eq!(
            field("window"),
            "120000 tokens (assumed from the model or the provider)"
        );
        assert_eq!(
            field("reply cap"),
            format!("{} tokens as max_tokens", config.reply_cap())
        );
    }

    /// The session is a layer of the precedence chain, and `--print-config`
    /// says what that layer was: nothing there, a conversation read back (and
    /// how much of one), or a file that is there and cannot be read. A dump
    /// that showed nothing for the last case was showing a chain with a link
    /// silently missing (finding B2).
    #[test]
    fn describe_reports_the_session_layer_it_read() {
        let cfg = Config::new("http://host:1", "m", None);
        let row = |stored: &session::Stored| {
            describe(&cfg, false, stored, &theme::Theme::default())
                .into_iter()
                .find(|(field, _)| field == "session")
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("no `session` row"))
        };
        assert_eq!(row(&session::Stored::Absent), "none");

        let mut one = Session::default();
        one.messages.push(Message::user("hello"));
        assert_eq!(row(&session::Stored::Loaded(one)), "read (1 message)");

        let mut two = Session::default();
        two.messages.push(Message::user("hello"));
        two.messages.push(Message::user("again"));
        assert_eq!(row(&session::Stored::Loaded(two)), "read (2 messages)");

        // The reason is the file's own — and it is made of what a hand-edited
        // file contained, on its way to a terminal, so it goes through the
        // same door the model id does.
        let hostile = "expected value at line 1 column 2\r\x1b[2Jmock";
        let row = row(&session::Stored::Unusable(hostile.to_string()));
        assert!(
            row.starts_with("unreadable — expected value at line 1 column 2"),
            "{row:?}"
        );
        assert!(!row.contains('\x1b') && !row.contains('\r'), "{row:?}");
    }

    /// `--print-config` is a diagnostic: it reads the layers and writes
    /// nothing. A workspace nothing has opened yet is described as the fresh
    /// one it would be, with no `.mush/` appearing, and a session file it
    /// cannot read is reported unreadable and left exactly where it is —
    /// setting it aside is the *startup* path's job, not the dump's (finding
    /// B2).
    #[test]
    fn describing_a_config_writes_nothing() {
        let env = theme::EnvText {
            theme: None,
            colorterm: None,
            term: None,
        };

        let missing =
            std::env::temp_dir().join(format!("mush-main-dump-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let (_, stored, _) = resolved_config(&missing, &Overrides::default(), &env).unwrap();
        assert!(matches!(stored, session::Stored::Absent), "{stored:?}");
        assert!(
            !missing.exists(),
            "the dump created the workspace it described"
        );

        let ws = scratch_workspace("dump-unreadable");
        let file = session::session_path(ws.root());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{ not json").unwrap();
        let (config, stored, theme) =
            resolved_config(ws.root(), &Overrides::default(), &env).unwrap();
        let row = describe(&config, false, &stored, &theme)
            .into_iter()
            .find(|(field, _)| field == "session")
            .map(|(_, value)| value)
            .expect("no `session` row");
        assert!(row.starts_with("unreadable — "), "{row}");
        assert!(
            row.contains("line 1"),
            "the reason says where the file broke: {row}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{ not json",
            "the dump left the unreadable session exactly where it was"
        );
        let _ = std::fs::remove_dir_all(ws.root());
    }

    /// The full startup path with an unreachable endpoint must still produce a
    /// usable config: that is the "window opens, no model" case.
    #[test]
    fn resolution_survives_an_empty_world() {
        let config = config::resolve(&Overrides::default(), &UserConfig::default(), None).unwrap();
        assert!(!config.base_url.is_empty());
        assert!(config.chat_url().ends_with("/v1/chat/completions"));
    }

    /// `--help` renders the key table and the command table from their one
    /// source, so neither can drift: a binding or a command cannot exist in the
    /// program and be missing from `--help`.
    #[test]
    fn help_renders_the_key_and_command_tables() {
        let help = help_text();
        // The whole key table, verbatim, is in `--help` — the same string the
        // in-app `/help` prints, so the two surfaces cannot disagree.
        let keys = app::keys::help_table();
        assert!(
            help.contains(&keys),
            "the key table is not in --help:\n{help}"
        );
        // The tree walk the human asked for is named here too, and the real
        // scroll keys — not the wheel mush never takes (finding K3).
        for want in ["←", "→", "↑ / ↓, PgUp / PgDn", "page up / down the rows"] {
            assert!(help.contains(want), "`{want}` is missing:\n{help}");
        }
        assert!(
            !help.contains("wheel"),
            "a wheel it does not scroll:\n{help}"
        );

        // And the command table, so `/compact`-style absence cannot return.
        let commands = app::commands::table(&mush_core::provider::names_piped());
        assert!(
            help.contains(&commands),
            "the command table is not in --help"
        );
        assert!(help.contains("KEYS:"));
        assert!(help.contains("COMMANDS (type in the chat):"));
    }

    /// The `--help` text lists the attach subcommands and the exact usage the
    /// parser takes, so a human learns they exist and can copy a line that
    /// works. The block used to put `[DIR]` first for `focus` and `edit` while
    /// the parser read the *value* first, so following the help got an error
    /// blaming the argument the human had just typed (finding A3).
    #[test]
    fn the_help_shows_each_attach_subcommand_the_way_it_parses() {
        let help = help_text();
        for usage in [
            "mush agents [DIR]",
            "mush read [DIR] [--agent N] [--since N]",
            "mush focus ID [DIR]",
            "mush edit [--agent N] --base R [--send] TEXT [DIR]",
        ] {
            assert!(help.contains(usage), "`{usage}` is not in --help:\n{help}");
        }
        // The two orders the parser refuses are gone by name, because that is
        // the state finding A3 found the block in — and the orders it does
        // take are pinned by `the_attach_subcommands_parse`.
        assert!(!help.contains("mush focus [DIR] ID"), "{help}");
        assert!(!help.contains("mush edit [DIR]"), "{help}");
    }

    /// `--` ends the options, in a subcommand as it does for the TUI, so a
    /// directory named like a subcommand (or one starting with dashes) is
    /// openable rather than an unknown option (finding A5).
    #[test]
    fn a_double_dash_ends_the_options() {
        let parse = |argv: &[&str]| {
            Cli::detect(&argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&["read", "--"]).unwrap(),
            Some(Cli::Read {
                dir: ".".into(),
                agent: 0,
                since: 0
            })
        );
        assert_eq!(
            parse(&["read", "--", "--weird"]).unwrap(),
            Some(Cli::Read {
                dir: "--weird".into(),
                agent: 0,
                since: 0
            }),
            "everything after `--` is positional"
        );
        assert_eq!(
            parse(&["--", "agents"]).unwrap(),
            None,
            "a leading `--` is the TUI's, not a subcommand's"
        );
    }

    /// One wire line prints as one line: a transcript line carrying a newline
    /// is escaped, so a client can tell continuation from a new line (A3).
    #[test]
    fn a_read_line_is_escaped_onto_one_line() {
        assert_eq!(escape_line("one\ntwo"), "one\\ntwo");
        assert_eq!(escape_line("a\tb"), "a\\tb");
        assert_eq!(
            escape_line("c\\nd"),
            "c\\\\nd",
            "a real backslash stays visible"
        );
        assert!(!escape_line("x\ny").contains('\n'));
    }

    /// The subcommands parse before anything else: a directory, the flag forms
    /// `read`/`edit` take, and the id `focus` takes before its directory.
    #[test]
    fn the_attach_subcommands_parse() {
        let parse = |argv: &[&str]| {
            Cli::detect(&argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&["agents"]).unwrap(),
            Some(Cli::Agents { dir: ".".into() })
        );
        assert_eq!(
            parse(&["agents", "/w"]).unwrap(),
            Some(Cli::Agents { dir: "/w".into() })
        );
        assert_eq!(
            parse(&["read", "--agent", "3", "--since", "12", "/w"]).unwrap(),
            Some(Cli::Read {
                dir: "/w".into(),
                agent: 3,
                since: 12
            })
        );
        // `read` owns no positional of its own, so the directory may stand
        // before its flags as `--help` prints it and as a human would type it:
        // the one usage line the audit's fix did not have to move (finding A3),
        // and the reason it can stay as it is.
        assert_eq!(
            parse(&["read", "/w", "--agent", "3", "--since", "12"]).unwrap(),
            Some(Cli::Read {
                dir: "/w".into(),
                agent: 3,
                since: 12
            })
        );
        assert_eq!(
            parse(&["read"]).unwrap(),
            Some(Cli::Read {
                dir: ".".into(),
                agent: 0,
                since: 0
            })
        );
        assert_eq!(
            parse(&["focus", "1", "/w"]).unwrap(),
            Some(Cli::Focus {
                dir: "/w".into(),
                agent: 1
            })
        );
        assert_eq!(
            parse(&["edit", "--agent", "2", "--base", "34", "--send", "hello"]).unwrap(),
            Some(Cli::Edit {
                dir: ".".into(),
                agent: 2,
                base: 34,
                send: true,
                text: "hello".to_string(),
            })
        );
        assert_eq!(
            parse(&["edit", "--base", "0", "a draft", "/w"]).unwrap(),
            Some(Cli::Edit {
                dir: "/w".into(),
                agent: 0,
                base: 0,
                send: false,
                text: "a draft".to_string(),
            })
        );

        // The directory is still the TUI's one optional argument, and a value
        // that is not one is named, like every other flag.
        assert!(parse(&["agents", "/a", "/b"]).is_err());
        assert!(parse(&["focus"]).is_err(), "focus needs an id");
        assert!(parse(&["edit"]).is_err(), "edit needs the text");
        assert!(parse(&["read", "--agent", "x"]).is_err());
        assert!(parse(&["read", "--nope"]).is_err());

        // Anything else is not a subcommand: the TUI's own parsing sees it.
        assert_eq!(parse(&["--help"]).unwrap(), None);
        assert_eq!(parse(&["/w"]).unwrap(), None);
        assert_eq!(parse(&[]).unwrap(), None);
    }

    /// A flag the chosen subcommand does not take is refused by name, in
    /// `detect` and before any socket is touched: `mush agents --since 3`,
    /// `mush focus 1 --base 9` and `mush read /w --send` used to parse, and the
    /// value was then dropped on the floor — the one thing a value mush cannot
    /// use must not be (finding A16's class, the rule the unknown-option arm
    /// and the second directory already follow). The refusal names the flag,
    /// the subcommand that does not take it and where its own flags are.
    #[test]
    fn a_flag_outside_its_subcommand_is_refused_by_name() {
        let parse = |argv: &[&str]| {
            Cli::detect(&argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        for (argv, flag, command) in [
            (&["agents", "--since", "3"][..], "--since", "agents"),
            (&["agents", "--agent", "1"][..], "--agent", "agents"),
            (&["focus", "1", "--base", "9"][..], "--base", "focus"),
            (&["focus", "1", "--since", "3"][..], "--since", "focus"),
            (&["focus", "1", "--send"][..], "--send", "focus"),
            (&["read", "/w", "--send"][..], "--send", "read"),
            (&["read", "/w", "--base", "9"][..], "--base", "read"),
            (&["edit", "--since", "3", "a draft"][..], "--since", "edit"),
            // The flag is refused even where the subcommand needs a value, so
            // `focus --base 9` blames `--base`, not the missing id.
            (&["focus", "--base", "9"][..], "--base", "focus"),
        ] {
            let error = parse(argv).expect_err(&format!(
                "`mush {command}` does not take `{flag}`, so it must be refused, not parsed"
            ));
            for want in [flag, command, "--help"] {
                assert!(
                    error.contains(want),
                    "the refusal must name `{want}`: {error}"
                );
            }
        }
    }

    /// The flags each subcommand takes, checked as a matrix: every one of the
    /// four flags either lands for a subcommand or is refused by name, and only
    /// the ones that subcommand owns land. The expectation is spelled out here
    /// rather than read from [`ATTACH_FLAGS`], so the test states the contract
    /// instead of mirroring the table it checks.
    #[test]
    fn each_attach_subcommand_takes_exactly_the_flags_it_owns() {
        let parse = |argv: &[&str]| {
            Cli::detect(&argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        let owned: &[(&str, &[&str])] = &[
            ("agents", &[]),
            ("read", &["--agent", "--since"]),
            ("focus", &["--agent"]),
            ("edit", &["--agent", "--base", "--send"]),
        ];
        for &(command, takes) in owned {
            for flag in ["--agent", "--since", "--base", "--send"] {
                // Everything the subcommand needs besides the flag under test,
                // so acceptance turns on the flag alone: `focus` gets an id
                // unless `--agent` is the id it is testing, and `--send` is the
                // one flag that takes no value.
                let mut argv = vec![command];
                if command == "focus" && flag != "--agent" {
                    argv.push("1");
                }
                argv.push(flag);
                if flag != "--send" {
                    argv.push("1");
                }
                if command == "edit" {
                    argv.push("a draft");
                }
                let parsed = parse(&argv);
                if takes.contains(&flag) {
                    assert!(
                        parsed.is_ok(),
                        "`mush {command}` takes `{flag}`: {parsed:?}"
                    );
                } else {
                    assert!(
                        parsed.is_err(),
                        "`mush {command}` does not take `{flag}`: {parsed:?}"
                    );
                }
            }
        }
    }

    /// The CLI builds the one request line its subcommand means, and echoes it
    /// to the running mush.
    #[test]
    fn the_attach_subcommands_build_their_request() {
        let request = Cli::Focus {
            dir: ".".into(),
            agent: 2,
        }
        .request();
        assert_eq!(request.op, attach::Op::Focus { agent: 2 });
        assert_eq!(request.encode(), r#"{"agent":2,"id":1,"op":"focus"}"#);

        let request = Cli::Edit {
            dir: ".".into(),
            agent: 0,
            base: 34,
            send: false,
            text: "hi".to_string(),
        }
        .request();
        assert_eq!(
            request.op,
            attach::Op::Edit {
                agent: 0,
                base: 34,
                text: "hi".to_string(),
                send: false,
            }
        );
    }
}
