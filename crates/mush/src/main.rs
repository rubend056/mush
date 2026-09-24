//! mush — a small, fast terminal surface for coding agents.
//!
//! Usage: `mush [DIRECTORY]` opens a workspace. An agent connected to the
//! configured OpenAI-compatible endpoint reads and writes it; mush shows the
//! tree of agents and the repository's state at a glance.

mod agent;
mod app;
mod attach;
mod clipboard;
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
mod signals;
mod theme;
mod ui;

use std::error::Error;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
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
        eprintln!("{}", error_line(&error.to_string()));
        std::process::exit(1);
    }
}

/// The line [`main`] prints when the run fails.
///
/// Split out so a test can read the bytes a real failure would put on stderr,
/// which `eprintln!` inside `main` cannot be asked for.
///
/// The text is sanitized here, at the one place it is printed, because it is
/// not always prose this process chose. A `Cli` failure is a sentence the app
/// made and this process only *decoded off the attach socket*
/// ([`attach::decode`]), and the app's refusal vocabulary includes lines that
/// quote outside hands: `blind_model_line` names the model id, and a model id
/// is a name an endpoint chose (`/v1/models`) — [`Config::label`] defangs that
/// same name for the facts line for exactly this reason. Nothing on the way
/// out sanitizes a `ReplyError`'s message, so an `ESC ]0;PWNED BEL` in a model
/// id would retitle the window through the CLI's stderr, and a bare `\r`
/// would paint over the line before it.
fn error_line(error: &str) -> String {
    format!("mush: {}", mush_core::text::sanitize(error))
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
                // The flag and `MUSH_CONTEXT` are two doors to one number, so
                // they read it through one function ([`config::parse_context`]):
                // the same spelling — surrounding space and all — is accepted by
                // both, and a refusal names the door it came in by.
                overrides.context = Some(config::parse_context(&value, "--context")?);
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

/// Refuse a flag given twice, by name, before its value is read.
///
/// The parser used to keep the last value and drop the first without a word —
/// the same silent loss [`require_flag`] refuses for a flag outside its
/// subcommand, and the rule every value mush cannot use follows: a value given
/// and not used is reported, never ignored (finding A16's class, H26).
fn require_once(
    seen: &mut Vec<&'static str>,
    command: &str,
    flag: &'static str,
) -> Result<(), String> {
    if seen.contains(&flag) {
        return Err(format!(
            "`{flag}` was given twice for `mush {command}` (try --help)"
        ));
    }
    seen.push(flag);
    Ok(())
}

/// `focus`'s two ids, refused by name: the positional id used to win and the
/// flag lose, silently, whatever order they came in (finding H26).
fn two_agent_ids() -> String {
    "`mush focus` takes one agent id: --agent and the positional id cannot both be given \
     (try --help)"
        .to_string()
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
        // Which flags have been given, so a second one is refused by name
        // instead of silently replacing the first value.
        let mut seen: Vec<&'static str> = Vec::new();
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
                    require_once(&mut seen, name, "--agent")?;
                    // `focus` takes its id either way, never both: the
                    // positional id and `--agent` are one id, and the flag
                    // losing in silence is finding H26. Refused here, before
                    // the flag's value is read, so `focus 1 --agent` (no number
                    // at all) hears about the conflict, not the missing value.
                    if name == "focus" && !positional.is_empty() {
                        return Err(two_agent_ids());
                    }
                    agent = Some(number(args.next(), "--agent")?);
                }
                "--since" => {
                    require_flag(name, "--since")?;
                    require_once(&mut seen, name, "--since")?;
                    since = number(args.next(), "--since")? as usize;
                }
                "--base" => {
                    require_flag(name, "--base")?;
                    require_once(&mut seen, name, "--base")?;
                    base = number(args.next(), "--base")?;
                }
                "--send" => {
                    require_flag(name, "--send")?;
                    require_once(&mut seen, name, "--send")?;
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
                agent: match (positional.first(), agent) {
                    // Two ids, whichever order they arrived in: the
                    // positional used to win and the flag vanish (finding
                    // H26).
                    (Some(_), Some(_)) => return Err(two_agent_ids()),
                    (Some(value), None) => parse_id(value, "focus")?,
                    (None, Some(agent)) => agent,
                    (None, None) => return Err("`mush focus` needs an agent id".to_string()),
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
                Cli::Read { .. } => {
                    print!("{}", lines_text(&body)?);
                    Ok(())
                }
                Cli::Agents { .. } => {
                    print!("{}", agents_text(&body)?);
                    Ok(())
                }
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
///
/// It is also the terminal door for everything these two printers emit: the
/// text defangs first, through [`mush_core::text::sanitize`] — the one door
/// every terminal-bound string in this tree goes through — and then escapes
/// what is left onto one line. Without the first half, a stored session (a
/// hand-editable file) or a roster row (a title, a branch, an endpoint-chosen
/// name) could put `ESC ]0;PWNED BEL` on the human's terminal and rename its
/// window, which is the one road in the tree that did not go through
/// `sanitize` (finding C8). The tab `sanitize` keeps is still escaped, because
/// these rows are tab-separated columns.
fn escape_line(text: &str) -> String {
    mush_core::text::sanitize(text)
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// `read`: the transcript lines, one per line as `index<TAB>text`, as the text
/// a client prints.
///
/// Split from the `print!` so a test can read the bytes a real `mush read`
/// would put on a terminal — the seam the C8 pin needs to say what *reaches*
/// the terminal rather than that nothing panicked.
fn lines_text(body: &Value) -> Result<String, String> {
    let mut out = String::new();
    for line in attach::Transcript::read(body)?.lines {
        out.push_str(&format!("{}\t{}\n", line.line, escape_line(&line.text)));
    }
    Ok(out)
}

/// `agents`: the roster the tree paints, one row per line, tab-separated:
/// `id parent phase activity title branch worktree children-working`. An empty
/// absent field prints as an empty column, and a root's missing parent as `-`.
///
/// The body is read as [`attach::Roster`] rather than fished key by key: a key
/// the producer renamed used to leave this printer writing an empty column
/// forever, with nothing failing (finding R23).
///
/// Every string column is data the model, the endpoint or a path on disk chose
/// — the audit's C8 names title, activity and branch — so every one of them
/// goes through [`escape_line`], the one door this command has.
fn agents_text(body: &Value) -> Result<String, String> {
    let mut out = String::new();
    for node in attach::Roster::read(body)?.agents {
        let parent = node
            .parent
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let activity = node.activity.unwrap_or_default();
        let branch = node.branch.unwrap_or_default();
        out.push_str(&format!(
            "{id}\t{parent}\t{phase}\t{activity}\t{title}\t{branch}\t{worktree}\t{children}\n",
            id = node.id,
            phase = escape_line(&node.phase),
            activity = escape_line(&activity),
            title = escape_line(&node.title),
            branch = escape_line(&branch),
            worktree = escape_line(&node.worktree),
            children = node.children_working,
        ));
    }
    Ok(out)
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
         \x20                      session, model and whether it can see, window and whether\n\
         \x20                      it was stated, temperature, reasoning effort and thinking\n\
         \x20                      mode, reply-cap size and name, the schemas it reserves and\n\
         \x20                      the history budget they leave, key masked, auto-approve\n\
         \x20                      and theme) and exit 0\n\n\
         KEYS:\n{keys}\n\n\
         COMMANDS (type in the chat):\n\
         {commands}\n\
         ATTACH (drive a running mush from another shell; newline-delimited JSON):\n\
         \x20   mush agents [DIR]      list the agents and their state\n\
         \x20   mush read [DIR] [--agent N] [--since N]\n\
         \x20                         an agent's transcript lines\n\
         \x20   mush focus ID [DIR]   focus that agent, as Enter on its row does\n\
         \x20                         (--agent N instead of the id)\n\
         \x20   mush edit [--agent N] [--base R] [--send] TEXT [DIR]\n\
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
/// The home config is the caller's ([`run`] reads the file once for both roads),
/// because its *complaint* when it could not be read is a fact the dump prints
/// and the startup road says before the first frame (finding C3).
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
    home: &UserConfig,
) -> Result<(config::Resolved, session::Stored, theme::Theme), Box<dyn Error>> {
    let stored = Session::read(dir);
    let session = match &stored {
        session::Stored::Loaded(session) => Some(session),
        session::Stored::Absent | session::Stored::Unusable(_) => None,
    };
    let resolved = config::resolve(overrides, home, session)?;
    let theme = theme::Theme::resolve(env, dir)?;
    Ok((resolved, stored, theme))
}

/// `--print-config`: the resolved config and nothing else — no workspace, no
/// `.mush/`, no request to an endpoint. This is what makes a hand-edited home
/// file debuggable, and the only way to see the precedence chain rather than
/// guess at it.
///
/// One column, wide enough for the longest name (`history budget`): a name that
/// overflows its padding runs into its own value, and `history budget1291500
/// bytes` is not a line a human can read.
fn print_config(
    config: &Config,
    stored: &session::Stored,
    theme: &theme::Theme,
    notices: &[String],
    home: Option<&str>,
) {
    for (field, value) in describe(config, auto_approve(), stored, notices, home, theme) {
        println!("{field:<15}{value}");
    }
}

/// One `field  value` line per fact, in the order a human reads them. The
/// values are what a request will carry, not what some file wished for; the
/// window is the one fact whose *source* matters, so it is named.
fn describe(
    config: &Config,
    approved: bool,
    stored: &session::Stored,
    notices: &[String],
    home: Option<&str>,
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
    // The image gate: whether the model in force may be sent a picture is the
    // one *capability* a request can be refused for, and it lives only in the
    // provider table — a human pasting a screenshot at a custom endpoint used
    // to learn it from the refusal, having spent the gesture (finding C12).
    // The answer comes from the same function the three gate sites ask
    // (`mush_core::provider::vision_capable`), so the dump cannot advertise a
    // picture the run would refuse, and the model it names is the one the gate
    // reads, defanged the way the model row defangs it.
    let vision = if mush_core::provider::vision_capable(&config.model) {
        "yes — image parts are sent".to_string()
    } else if config.model.is_empty() {
        "no — no model yet".to_string()
    } else {
        format!(
            "no — the table does not document image parts for {}",
            mush_core::text::sanitize(&config.model)
        )
    };
    // The window is the one fact whose *source* matters, and a number can come
    // by four roads: the human, mush's own table, the endpoint's model list, the
    // endpoint's refusal. Each is named in words by [`WindowSource::words`],
    // beside the roads themselves, so this dump and `/context`'s report cannot
    // describe one window differently — the meter has one display column for
    // the same fact ([`crate::app::window_mark`]) and paints the mark.
    let window = config.context_source.words();
    let cap = if config.uses_max_completion_tokens() {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    // The name alone would not say what a reply is cut off at, which is the one
    // number a truncated run makes a human want to see. The size is derived
    // from the window, under the same name the request will carry it.
    let reply_cap = format!("{} tokens as {cap}", config.reply_cap());
    // The other two numbers of the request reserve, resolved for this window:
    // the schemas every request pays for ([`config::SCHEMA_TOKENS`], the
    // constant `request_reserve` reads), and the history they leave
    // ([`Config::history_budget`], the function the trimmer is handed). The
    // manual's words say this dump prints what the constants resolve to;
    // printing the reply cap alone left a human to re-derive the other two
    // (audit D5).
    let schemas = format!("{} tokens", config::SCHEMA_TOKENS);
    let history_budget = format!(
        "{} bytes ({} tokens)",
        config.history_budget(),
        config.history_budget() / config::BYTES_PER_TOKEN
    );
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
    let mut rows = vec![
        ("endpoint".to_string(), config.base_url.clone()),
        ("provider".to_string(), config.provider.name().to_string()),
        // Before the values it can supply: endpoint, provider, model and
        // window may all have come from this layer.
        ("session".to_string(), session_layer),
        ("model".to_string(), model),
        // Under the model it is a fact about: the gate's answer and the id it
        // was asked about cannot be read as being about two different models.
        ("vision".to_string(), vision),
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
        ("schemas".to_string(), schemas),
        ("history budget".to_string(), history_budget),
        ("api key".to_string(), key),
        ("auto-approve".to_string(), approve.to_string()),
        // What the chrome would look like, hue and source together: a human
        // comparing two windows needs the fact `--print-config` shows to be
        // the one the window would have, environment included.
        ("theme".to_string(), theme.describe()),
    ];
    // The home config is the layer below the session, and the one whose file
    // mush could not read but did not refuse to start over: the row appears only
    // when there is a complaint, because a file that read (or none at all) is
    // not news. It sits directly under the session — before the values either
    // layer can supply — and its words come from a file on their way to a
    // terminal, so they take the same door the session row's reason does
    // (finding C3).
    if let Some(complaint) = home {
        let at = rows
            .iter()
            .position(|(field, _)| field == "session")
            .map_or(rows.len(), |at| at + 1);
        rows.insert(
            at,
            (
                "home config".to_string(),
                format!("unreadable — {}", mush_core::text::sanitize(complaint)),
            ),
        );
    }
    // The lines resolution owed the human: a stored layer that was refused
    // rather than obeyed (finding C2). A row of its own, the way the session
    // layer's `unreadable` is, because these are facts about the chain that no
    // other row can carry — and both are built from what a file contained, so
    // both go through the door every terminal-bound string goes through.
    for notice in notices {
        rows.push(("notice".to_string(), mush_core::text::sanitize(notice)));
    }
    rows
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

    // The home config is read once, here: both roads need it, and reading the
    // file twice would be a second chance for the two to disagree about what it
    // says (and about whether it could be read at all). It is a layer below the
    // session, so a file mush cannot read is not fatal — the defaults stand in —
    // but it is not silent either: the complaint travels to `--print-config` as
    // a row and to the window as a failure before the first frame (finding
    // C3).
    let home = UserConfig::load();

    if args.print_config {
        // The resolved config, then out: no terminal is entered, no `.mush/` is
        // created, no lock is taken and no request is made — [`resolved_config`]
        // is where the layers are read, and where a session file that cannot be
        // read stays a fact about the layer rather than being flattened into
        // absence (finding B2).
        let (resolved, stored, theme) = resolved_config(&dir, &overrides, &env, &home.config)?;
        print_config(
            &resolved.config,
            &stored,
            &theme,
            &resolved.notices,
            home.complaint.as_deref(),
        );
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
    let lock = lock::acquire(workspace.root())?;
    // A mush that died without unwinding left its commands' scratch files in the
    // temp directory, with an orphan still writing into them and no watcher left
    // to cap it (finding E5). The names carry the pid that owned each pair, so a
    // start can reap the dead and never a live mush's — this one included.
    machine::reap_dead_scratch();
    // A mush that is signalled ends through the same road `Ctrl-Q` takes — the
    // exit flush, the actors' endings, the kill walk, the writer's thread — and
    // the socket and the terminal go back last, after it (finding R3), instead
    // of dying raw with every process group it started still running (finding
    // E1; `signals` carries the road and its reasons, including what a second
    // and a third signal mean). The guard is held here for the whole run, and
    // dropped last of all: dropping it takes the handlers away and waits for
    // the watcher thread, which reads its own EOF.
    let _signals = match signals::install() {
        Ok(signals) => Some(signals),
        // Not fatal: a mush without handlers is the mush that existed before
        // this module, and refusing to start would trade a resource leak for no
        // mush at all. It is said, because it is not the normal state.
        Err(error) => {
            eprintln!("mush: signal handling disabled — {error}");
            None
        }
    };

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
    let resolved = config::resolve(&overrides, &home.config, stored.as_ref())?;
    // The lines a stored layer owes the human travel to the frame with the
    // config; the config itself is cloned into the cell below.
    let notices = resolved.notices;
    let config = resolved.config;

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
    // exit flush. The writer is handed the lock's identity, so a save after the
    // lock's *name* was replaced — a tool cannot do it, the human's `mv` can —
    // is refused rather than written into a store a second mush now owns; and a
    // thread the OS will not give is not a reason to refuse the workspace: the
    // writer returns its spawn failure, and a writer with no worker reports
    // every flush at once, on the status line (finding E7).
    let save: Arc<dyn session_save::SessionSave> =
        match session_save::Writer::new(workspace.root().to_path_buf(), Some(lock.identity())) {
            Ok(writer) => Arc::new(writer),
            Err(error) => {
                eprintln!("mush: the session will not be saved — {error}");
                Arc::new(session_save::Writer::without_worker(
                    workspace.root().to_path_buf(),
                    Some(lock.identity()),
                    error,
                ))
            }
        };
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
    // nothing in it, a home config mush could not read is not a machine with no
    // settings, and a notice resolution owed (a session provider this build does
    // not know, finding C2) is not noise either. The failures come last because
    // they are the ones that must not be overwritten on the bar.
    for notice in notices {
        app.say(notice);
    }
    if let Some(complaint) = home.complaint {
        app.stored_unreadable(complaint);
    }
    if let Some(notice) = unreadable {
        app.stored_unreadable(notice);
    }

    let panics = PanicRoute::new();
    install_panic_hook(panics.clone());
    let mut guard = TerminalGuard::enter()?;
    // The screen is mush's from here until it is handed back: a worker's panic
    // now leaves its words in the route instead of on the alternate screen
    // (finding PM9), and the exit road prints them below.
    panics.raise();
    // The terminal's size is only known here; `/notes` wraps its popup to it
    // and the floor is decided from it, so record both before the first key can
    // be read. A resize reports its own.
    if let Ok(size) = guard.terminal.size() {
        app.set_term_size(size.width, size.height);
    }
    let result = event_loop(&mut guard.terminal, &mut app, &rx, &theme);
    // The exit road runs first — the flush, the actors' endings, the quit fence
    // and the kill walk, the writer's thread — and the terminal and the socket
    // are handed back last (finding R3). The order is the fact: a signal that
    // arrives while the screen still looks like mush's cannot find a hand-back
    // already made, and a second one cannot skip a kill the road had not
    // reached. The hurry a press asked for before this line is spent here — it
    // asked for nothing the road can shorten — and a press after it ends the
    // road's waits at their next poll ([`signals::forced`]). Every step of that
    // road is bounded ([`App::shutdown`]), and a bound that expires — or a
    // hurry that ends the wait early — comes back as a sentence, printed below
    // on a shell that has its terminal back (findings R4, R3) — and so do the
    // words of any worker that panicked while the screen was mush's (finding
    // PM9).
    let _ = signals::take_force();
    let notes = app.shutdown();
    drop(app);
    drop(_attach);
    drop(guard);
    for note in notes.into_iter().chain(panics.lower()) {
        eprintln!("mush: {note}");
    }
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

    // The keyboard-protocol query is a *bounded* wait (up to two seconds on a
    // terminal that never answers), so it is asked here, not by the entry: the
    // UI must be up first, and it must not be asked under a terminal that
    // already has keys queued either — `handled == 0` is the first idle tick.
    let mut painted = false;
    let mut asked = false;

    while !app.should_quit {
        // A signal is read here, on the thread that owns the terminal, the tree
        // and the store: the flag turns into the quit road and the loop stops
        // before painting a frame nobody will read.
        if take_signal_quit(app) {
            break;
        }
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
                // A click, a release, a wheel notch: the pointer's own road
                // into `App`, taken because `screen_modes` captures the mouse.
                // The event travels as it arrived — `App::on_mouse` reads the
                // button and the modifier, and a terminal sends what mush
                // asked for and nothing more (`take_mouse`: no motion, no
                // drag).
                Event::Mouse(event) => app.update(Msg::Mouse(event)),
                // The terminal changed size: schedule a redraw. ratatui's
                // `terminal.draw` re-queries the size first, so the next
                // frame already paints at the new dimensions — and the app is
                // told the new size so a `/notes` report wraps to it, the
                // floor notice is raised or lowered (finding P11), and the
                // select mode re-measures the pane for the coming frame
                // (finding PM5).
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

        if !asked && painted && handled == 0 {
            asked = true;
            ask_keyboard_enhancement()?;
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
            painted = true;
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

/// Whether the keyboard enhancement flags are on the terminal's stack.
///
/// The terminal is one process-wide device and there is at most one
/// [`TerminalGuard`] alive for it, so "did we push the flags?" is one
/// process-wide fact rather than a value threaded through the panic hook: the
/// hook is installed *before* the guard exists (a panic inside the entry has to
/// be able to hand the modes back), and both roads out — the hook and
/// `Drop` — read this same flag. [`restore_terminal_modes`] takes it with
/// `swap`, so a panic's restore followed by the guard's `Drop` pops exactly one
/// frame.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// Undo every mode [`TerminalGuard::enter`] turned on.
fn restore_terminal_modes() {
    let _ = disable_raw_mode();
    let _ = restore_mode_sequences(
        &mut io::stdout(),
        KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst),
    );
}

/// The escape sequences that leave the modes [`TerminalGuard::enter`] entered,
/// written through a `Write` rather than straight to `stdout`: the panic hook's
/// one effect on the terminal, so a test can read exactly what a panic puts on
/// the wire without a terminal (finding E10).
///
/// `enhanced` is whether [`TerminalGuard::enter`] pushed the keyboard
/// enhancement flags. When it did, the pop is queued beside the leave escapes
/// and the whole hand-back goes out in one `flush` — the single-write shape the
/// comment on [`TerminalGuard::enter_with`] leans on; a separate write would
/// leave a process that died between the two with the human's shell still
/// reading mush's keys as `CSI u`. When it did not, the terminal is never sent
/// a pop it never opened.
///
/// The mouse half is [`DisableMouseCapture`], which turns off the five modes
/// the library's capture takes. The entry takes *two* of them by hand
/// ([`take_mouse`]), so three of these offs name a mode mush never turned on:
/// that is the harmless direction (a mode that was never on and is turned off
/// is not a state anybody can be in), and it keeps the hand-back one command
/// that leaves no mouse mode standing rather than a second list of exactly
/// which two the entry wrote — a list that would have to be kept in step with
/// `take_mouse` by hand.
fn restore_mode_sequences(writer: &mut impl io::Write, enhanced: bool) -> io::Result<()> {
    queue!(
        writer,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    if enhanced {
        queue!(writer, PopKeyboardEnhancementFlags)?;
    }
    writer.flush()
}

/// The screen step of [`TerminalGuard::enter`]: the alternate screen, bracketed
/// paste, and the mouse.
///
/// Bracketed paste is what turns Ctrl-Shift-V from a stream of individual
/// keystrokes — one event, one repaint, and a redraw per character — into a
/// single `Event::Paste` carrying the whole paste.
///
/// The mouse is taken now. mush used to leave it to the terminal on purpose
/// (finding K3): the terminal's own drag-to-select was worth more than a wheel
/// notch, and `Ctrl-F`/`Ctrl-Y` were built to scope a selection to one pane
/// without it. The human asked for clicks — a row selects a conversation, a
/// tool call opens on its own — so the trade is made the other way round, and
/// what it costs is said rather than hidden: a *plain* drag is mush's input
/// (a press, and no motion — [`take_mouse`]), so reading text out of the
/// transcript is the terminal's bypass key (Shift+drag in most of them), and
/// `Ctrl-F` is still the road to a rectangle of one pane; and the wheel, which
/// the terminal can no longer spend itself, is spent by `App::on_mouse`. The
/// lag that made the wheel feel broken was the per-keystroke repaint, which the
/// event loop no longer does.
///
/// The mouse paints nothing of its own — no hover highlight, no pointer glyph:
/// a click is a verb, and the frames the panes already paint are the whole
/// feedback. That is what the mode is for; it is not a second cursor.
///
/// The keyboard enhancement flags are *not* pushed here, and the protocol's
/// flags query is not asked here either: `supports_keyboard_enhancement` is a
/// query and a *bounded* wait, and paying it before this step had painted
/// anything was two seconds of blank screen on a terminal that never answers.
/// [`ask_keyboard_enhancement`] is that half, asked by the event loop once the
/// first frame is up.
///
/// Entry and undo are the two halves of one shape — queue what the terminal
/// gets, then a single `flush` — so a half-written set of screen modes is not a
/// state that exists.
fn screen_modes() -> io::Result<()> {
    let mut stdout = io::stdout();
    queue!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    take_mouse(&mut stdout)?;
    stdout.flush()
}

/// What [`take_mouse`] puts on the wire, and the reason it is not the
/// library's own command.
///
/// `crossterm`'s `EnableMouseCapture` is the library's whole set — 1000, 1002,
/// 1003, 1015 and 1006 — and two of those modes are a flood with nothing on
/// this side to read them. 1003 is *any* motion: every pixel of every move
/// across the frame arrives as an event and wakes the loop for a repaint that
/// changes nothing. 1002 is the same for a drag, and mush has no drag verb: the
/// human's drag-to-select is the terminal's own and is the bypass key's, which
/// the mode set cannot take away. 1015 (RXVT) is a coordinate encoding older
/// than 1006 (SGR), which every terminal mush runs in speaks. So the set is the
/// two modes a click needs: 1000 (press and release) and 1006 (SGR
/// coordinates, which do not run out at column 223).
///
/// Written as bytes rather than through a `Command` because the library's
/// command is the wide set, and a command of our own would be a type whose only
/// method writes these same bytes; they land in the same `stdout` buffer as the
/// `queue!` beside them, so the entry is still modes-then-one-`flush`.
fn take_mouse(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(MOUSE_ON)
}

/// Normal tracking and SGR coordinates, in that order: [`take_mouse`]'s own
/// bytes, named so a test can assert both what is sent and — just as important
/// — which of the library's modes are *not*.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";

/// The keyboard-protocol half of the entry: ask whether the terminal speaks
/// the kitty protocol, and push the flags when it answers yes.
///
/// The flags are what make `Shift-Enter`, `Shift-↑` and `Shift-↓` arrive at
/// all: a terminal that encodes keys the legacy way sends them byte-identical
/// to their unmodified forms, which is why `Alt-Enter` exists. So mush asks
/// once — `supports_keyboard_enhancement` is the protocol's flags query and a
/// *bounded* wait, and a terminal that stays silent is a terminal that cannot:
/// no push, and exactly the old behaviour (`Shift-Enter` sends). The query
/// reaching an outer terminal through tmux needs tmux ≥ 3.2 with
/// `extended-keys on`; that is the manual's sentence, not code.
///
/// The wait is why this is not part of [`screen_modes`]: a terminal that never
/// answers would have held up the first frame for up to two seconds, so the
/// event loop asks this on its first idle tick, with the UI already painted —
/// the enhancement arrives a frame later, and nothing the human sees waits on
/// it. The entry's shape is kept for the push itself (queue, then one
/// `flush`), and [`KEYBOARD_ENHANCED`] is stored *before* the write: a panic
/// between the two pops a frame the terminal may never have had — a pop on an
/// empty stack is the harmless direction — rather than leaving a pushed frame
/// no road pops.
///
/// An `Err` from the query is a terminal that did not answer in time, which is
/// no support and never a reason to stop; an `Err` from the push travels like
/// the entry's own write failure did.
fn ask_keyboard_enhancement() -> io::Result<()> {
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    KEYBOARD_ENHANCED.store(enhanced, Ordering::SeqCst);
    if !enhanced {
        return Ok(());
    }
    let mut stdout = io::stdout();
    queue!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    stdout.flush()
}

/// Restores the terminal on both clean exit (Drop) and panic.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enter the terminal: raw mode, the alternate screen, bracketed paste,
    /// the mouse — and the ratatui terminal every frame is painted through.
    ///
    /// The keyboard enhancement flags are not part of this: the protocol's
    /// flags query is a bounded wait, and [`ask_keyboard_enhancement`] asks it
    /// from the event loop once the first frame is painted, so the entry never
    /// holds the UI up for a terminal that will not answer.
    ///
    /// Each mode is a promise to the human's shell: leaving raw mode on breaks
    /// their typing, leaving bracketed paste on makes their own pastes arrive
    /// wrapped in escape codes, and leaving the mouse captured sends the clicks
    /// and wheel notches meant for their shell into a program that has exited.
    /// Everything that can end the program — a clean quit, a signal
    /// ([`crate::signals`] turns one into the quit road), a panic, an error on
    /// the way out — has to undo all of them, so they are entered in one place
    /// and undone by [`restore_terminal_modes`].
    ///
    /// The entry is all or nothing (finding PM8): `main` exits 1 when it fails,
    /// and it must not do that from a raw shell. A failure in any step after
    /// raw mode has succeeded undoes every promise already made before the
    /// error travels.
    fn enter() -> io::Result<Self> {
        Self::enter_with(
            // Raw mode is the first promise and the first thing undone: it is
            // what makes the human's typing stop echoing and `Ctrl-C` stop
            // signalling.
            enable_raw_mode,
            screen_modes,
            || Terminal::new(CrosstermBackend::new(io::stdout())),
            restore_terminal_modes,
        )
    }

    /// [`Self::enter`] with the four effects as parameters.
    ///
    /// The seam exists because the failure road is latent by construction: it
    /// needs a terminal whose writes stop answering, which a test cannot make
    /// real (finding PM8). Handing each step in — entering raw mode, writing
    /// the screen modes, building the terminal, and the undo — lets a test fail
    /// a named step after raw mode and read the undo, exactly as
    /// [`restore_mode_sequences`] lets a test read what a panic writes.
    ///
    /// The order is the fact: `raw` first, `screen` second, `build` last, and
    /// any failure after `raw` succeeded runs `restore` before returning the
    /// error. `restore` undoes *every* promise, not only the failed step's,
    /// because the escapes are written in one `flush` and a half-written one
    /// cannot be told from a whole one.
    fn enter_with(
        raw: impl FnOnce() -> io::Result<()>,
        screen: impl FnOnce() -> io::Result<()>,
        build: impl FnOnce() -> io::Result<Terminal<CrosstermBackend<Stdout>>>,
        restore: impl FnOnce(),
    ) -> io::Result<Self> {
        raw()?;
        if let Err(error) = screen() {
            restore();
            return Err(error);
        }
        match build() {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                restore();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_modes();
        let _ = self.terminal.show_cursor();
    }
}

/// The signal road's one step: the flag the watcher thread set becomes the
/// quit.
///
/// The thread matters. A handler runs on whichever thread the kernel chose;
/// the quit road runs on this one, where `App` lives — so the session's flush,
/// `kill_all` and the terminal's exit are one thread's work, and no thread has
/// to reach into another's tree. Returns whether a quit was asked, so the loop
/// can leave without painting a frame nobody will read.
pub(crate) fn take_signal_quit(app: &mut App) -> bool {
    if signals::quit_requested() {
        app.signal_quit();
        return true;
    }
    false
}

/// How many panic words are held for the exit road. A screen seven panics have
/// smeared is one whose next line is the human's `reset`; the bound is there so
/// a process panicking without end cannot grow a list forever, and what it
/// drops is counted and said with the words.
const KEPT_PANIC_WORDS: usize = 8;

/// Where a panic's words go while the terminal is mush's.
///
/// A panic on a thread that is not the terminal's used to reach the default
/// hook's stderr write immediately — with the alternate screen up and raw mode
/// on. Raw mode keeps a newline from returning the cursor to column one, and
/// ratatui's per-frame diff only rewrites cells that changed between *its* two
/// buffers, so the stray words are never repaired: they persist until their
/// cells happen to change or the screen is repainted (finding PM9). The words
/// are therefore held here and printed by the exit road once the terminal is
/// back, in the same `mush: ` shape every other exit sentence gets.
///
/// Held, not dropped: the words are usually the only account of why a thread
/// died. The one thing the hold loses is the `RUST_BACKTRACE` note the default
/// hook appends around them, which this hook cannot produce.
#[derive(Clone, Default)]
struct PanicRoute {
    /// Whether the terminal is mush's: raised once it has been entered,
    /// lowered when it has been handed back.
    up: Arc<AtomicBool>,
    /// What a non-owner panic said while `up`.
    kept: Arc<Mutex<Vec<String>>>,
    /// How many panics arrived after `kept` was full.
    dropped: Arc<AtomicUsize>,
}

impl PanicRoute {
    fn new() -> Self {
        Self::default()
    }

    /// The terminal has been entered: from here a worker's words are held.
    fn raise(&self) {
        self.up.store(true, Ordering::SeqCst);
    }

    /// Whether the screen still belongs to mush. The hook's question.
    fn up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }

    /// Hold one panic's words, or count them as dropped once the list is full.
    fn keep(&self, words: String) {
        let mut kept = self
            .kept
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if kept.len() < KEPT_PANIC_WORDS {
            kept.push(words);
        } else {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The terminal has been handed back: take the words that were held.
    ///
    /// What the bound dropped is said here rather than in silence, in the same
    /// shape as the words it stands behind.
    fn lower(&self) -> Vec<String> {
        self.up.store(false, Ordering::SeqCst);
        let mut words = std::mem::take(
            &mut *self
                .kept
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let dropped = self.dropped.swap(0, Ordering::SeqCst);
        if dropped > 0 {
            words.push(format!(
                "{dropped} more panics were not kept — the road keeps the first {KEPT_PANIC_WORDS}"
            ));
        }
        words
    }
}

/// The panic hook, installed where the terminal belongs: on the thread that
/// owns it. `route` is the half of the hook that is not about the modes: the
/// words a worker's panic leaves while the screen is mush's (finding PM9).
fn install_panic_hook(route: PanicRoute) {
    // The hook runs on whichever thread panicked, so it cannot ask "am I the
    // terminal's thread?" by looking around — the id is captured here, on the
    // thread that owns the terminal, and every panic is compared against it
    // (finding E10).
    install_panic_hook_for(std::thread::current().id(), route, restore_terminal_modes);
}

/// Install the process-wide panic hook for `owner`'s terminal, restoring its
/// modes through `restore` and holding a non-owner's words while `route` is up.
///
/// Every panic used to run [`restore_terminal_modes`], whichever thread
/// panicked — and mush is a process with a thread per agent
/// (`mush-agent-{id}`), per job (`mush-job-{id}`) and one for the session
/// writer. A worker's death therefore wrote `LeaveAlternateScreen` and the mode
/// resets to the human's terminal while the UI thread kept painting frames into
/// a screen that was no longer mush's (finding E10). A worker's panic has its
/// own roads, and none of them needs the terminal: a job's thread ends its
/// process group, the writer marks itself dead.
///
/// Its *words* still have a road, and it is not stderr: printing while the
/// alternate screen is up paints glyph garbage a frame will not repair (finding
/// PM9). So a non-owner's panic is held in `route` while the terminal is
/// mush's, and the exit road prints it once the screen is the shell's again.
///
/// The restore is a parameter rather than a call to the real one so a test can
/// install the hook for a thread of its own and read what each panic writes
/// ([`restore_mode_sequences`]); `route` is the parameter that lets the same
/// test read where a worker's words went instead.
fn install_panic_hook_for(
    owner: std::thread::ThreadId,
    route: PanicRoute,
    restore: impl Fn() + Send + Sync + 'static,
) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only the thread whose terminal this is restores it: a panic on any
        // other thread must leave the human's screen alone, whether or not the
        // UI is still running (finding E10).
        if std::thread::current().id() == owner {
            restore();
            previous(info);
            return;
        }
        // The screen is mush's: the words would be painted over by a frame
        // ratatui believes is intact, so they are held for the exit road
        // (finding PM9) — in the default hook's own shape, minus the note it
        // appends around them.
        if route.up() {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>");
            route.keep(format!("thread '{name}' {info}"));
            return;
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_core::config::WindowSource;
    use mush_core::scratch::{Held, Scratch};
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

    /// `--context` and `MUSH_CONTEXT` are two doors to one number, so the same
    /// spelling — leading or trailing space included — is read the same way by
    /// both. Before they shared [`config::parse_context`], `--context " 8192"`
    /// was a refusal while `MUSH_CONTEXT=" 8192"` was accepted: one number, two
    /// answers, one of them the wrong one.
    #[test]
    fn the_context_flag_and_the_variable_read_one_number_one_way() {
        for stated in ["8192", " 8192", "8192 ", " 8192 "] {
            let args = parse_from(["--context", stated].into_iter().map(str::to_string)).unwrap();
            assert_eq!(
                args.overrides.context,
                Some(8_192),
                "{stated:?} from the flag"
            );
            assert_eq!(
                config::parse_context_env(stated),
                Ok(8_192),
                "{stated:?} from the environment"
            );
        }
        // And they agree on the refusal too: one rule, each sentence naming the
        // road that carried the value.
        let refused = match parse_from(["--context", "8k"].into_iter().map(str::to_string)) {
            Err(error) => error,
            Ok(_) => panic!("`--context 8k` was accepted"),
        };
        assert_eq!(refused, "--context needs a token count, got `8k`");
        assert_eq!(
            config::parse_context_env("8k").unwrap_err(),
            "MUSH_CONTEXT needs a token count, got `8k`"
        );
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
    /// the way a human does: the guard comes back with the workspace, so the
    /// directory and what the test wrote in it go when the test ends.
    fn scratch_workspace(name: &str) -> Held<Workspace> {
        let dir = Scratch::new(&format!("main-{name}"));
        let ws = Workspace::new(&dir).unwrap();
        dir.hold(ws)
    }

    /// A directory mush cannot open names the directory and what to do about
    /// it, instead of printing only the operating system's errno: `mush /typo`
    /// used to answer `No such file or directory (os error 2)`, naming neither
    /// the argument nor anything a human could do about it (finding B1). The
    /// sibling case — a *file* where a directory was wanted — has named both
    /// from the start.
    #[test]
    fn an_unopenable_workspace_names_the_directory() {
        let scratch = Scratch::new("main-typo");
        let dir = scratch.join("typo");
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
            &[],
            None,
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
        assert_eq!(field("window"), "64000 tokens (stated by the human)");
        assert_eq!(field("temperature"), "0.0", "0 is a value, not an absence");
        assert_eq!(field("reasoning"), "max (stated)");
        assert_eq!(field("thinking"), "off (stated)");
        assert_eq!(
            field("reply cap"),
            format!("{} tokens as max_completion_tokens", cfg.reply_cap()),
            "the cap the config derives, under the name it chose"
        );
        assert_eq!(
            field("schemas"),
            format!("{} tokens", config::SCHEMA_TOKENS),
            "the schema reserve every request pays, from the constant the run reads"
        );
        assert_eq!(
            field("history budget"),
            format!(
                "{} bytes ({} tokens)",
                cfg.history_budget(),
                cfg.history_budget() / config::BYTES_PER_TOKEN
            ),
            "the history those leave, from the function the trimmer is handed"
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
            &[],
            None,
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
            "8192 tokens (assumed from mush's model table)"
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
            &[],
            None,
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

    /// The dump answers the image gate, not only the request knobs: whether
    /// the model in force may be sent a picture is the one *capability* a
    /// request can be refused for, and it lived only in the provider table —
    /// a human pasting a screenshot learned it by spending the gesture
    /// (finding C12). The answer is read through the same function the gate
    /// asks, so the dump cannot promise an image the run would drop.
    #[test]
    fn the_dump_answers_the_image_gate() {
        let vision = |model: &str| {
            let cfg = Config::new("http://host:1", model, None);
            describe(
                &cfg,
                false,
                &session::Stored::Absent,
                &[],
                None,
                &theme::Theme::default(),
            )
            .into_iter()
            .find(|(field, _)| field == "vision")
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("no `vision` row for `{model}`"))
        };
        // The table's one row that documents vision, and a model it does not
        // name: both directions of the gate the request will meet.
        assert_eq!(
            vision("deepseek-flash"),
            "yes \u{2014} image parts are sent"
        );
        assert_eq!(
            vision("deepseek-v4-pro"),
            "no \u{2014} the table does not document image parts for deepseek-v4-pro"
        );
        assert_eq!(vision(""), "no \u{2014} no model yet");
        // The row's model is the one the gate reads, and it is defanged like
        // the model row: an endpoint-chosen id on its way to a terminal.
        assert_eq!(
            vision("vendor/deepseek-flash"),
            "yes \u{2014} image parts are sent",
            "the gate strips a prefix; the dump asks the same function"
        );
        // `--help` names the row, so a human knows the dump answers the
        // question before they paste.
        let help = help_text();
        assert!(help.contains("whether it can see"), "{help}");
    }

    /// The theme row says what a window would look like: the hue, the form the
    /// terminal would paint it in, and whether the workspace path or
    /// `MUSH_THEME` chose it. The fixed palette says it is fixed, rather than
    /// naming a hue nobody chose.
    #[test]
    fn describe_reports_the_theme_a_window_would_wear() {
        let cfg = Config::new("http://host:1", "m", None);
        let value = |theme: &theme::Theme| {
            describe(&cfg, false, &session::Stored::Absent, &[], None, theme)
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
            &[],
            None,
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

    /// The human's live report: two mush sessions on one config painted `~1M`
    /// and `~500k` in the `ctx` line, and nothing in the frame or in
    /// `--print-config` could say which road each number had taken — the `~`
    /// meant only "the human stated none". An unstated window arrives by three
    /// roads (mush's model table, the endpoint's model list, the endpoint's
    /// refusal) and a stated one is a fourth; each is named, by the meter's
    /// one-column mark ([`crate::app::window_mark`], which `context_label` and
    /// `context_meter` both paint) and by `--print-config`'s words.
    #[test]
    fn each_road_a_window_came_by_is_named_in_the_meter_and_in_print_config() {
        for (source, mark, words) in [
            (WindowSource::Stated, "", "stated by the human"),
            (WindowSource::Table, "~", "assumed from mush's model table"),
            (
                WindowSource::Advertised,
                "≈",
                "advertised by the endpoint's model list",
            ),
            (
                WindowSource::Complaint,
                "≤",
                "named by the endpoint in a refusal",
            ),
        ] {
            let mut config = Config::new("http://127.0.0.1:1", "m", None);
            config.context_source = source;
            let lines = describe(
                &config,
                false,
                &session::Stored::Absent,
                &[],
                None,
                &theme::Theme::default(),
            );
            let window = lines
                .iter()
                .find(|(field, _)| field == "window")
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("no `window` line for {source:?}"));
            assert_eq!(
                window,
                format!("{} tokens ({words})", config.context_tokens),
                "{source:?}: the dump names the road"
            );
            assert_eq!(
                crate::app::window_mark(source),
                mark,
                "{source:?}: the mark the meter paints"
            );
            assert!(
                mark.chars().count() <= 1,
                "{mark:?} takes more than the one display column the meter has"
            );
        }
    }

    /// The session the shipped DeepSeek defaults describe, as `--print-config`
    /// spells it: the window the human asked for, and a reply cap
    /// `window / REPLY_SHARE_DIVISOR` (`mush_core::config` owns the number,
    /// `Config::reply_cap` spends it) under the name every endpoint documents —
    /// not the 20_480 a real run was cut off at. The cap's *size* is on the line
    /// precisely so this can be read before a run instead of after one.
    #[test]
    fn the_shipped_deepseek_session_reports_the_cap_it_sends() {
        let mut config = Config::new("https://api.deepseek.com", "", None);
        config.provider = config::Provider::DeepSeek;
        config.rederive_context();
        assert!(!config.context_explicit(), "a default, not a statement");

        let lines = describe(
            &config,
            false,
            &session::Stored::Absent,
            &[],
            None,
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
            "120000 tokens (assumed from mush's model table)"
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
            describe(&cfg, false, stored, &[], None, &theme::Theme::default())
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

    /// A line resolution owed about a stored layer — a session provider this
    /// build does not know (finding C2) — gets a row of its own, so the dump
    /// cannot show the config a typo fell through to without showing why. Like
    /// the session row's reason, the words came out of a file on their way to
    /// a terminal, so they go through the same door.
    #[test]
    fn describe_reports_the_notices_a_stored_layer_owed() {
        let cfg = Config::new("http://host:1", "m", None);
        let notice =
            "session: unknown provider `boom\r\x1b[2Jmock` (try deepseek or custom)".to_string();
        let lines = describe(
            &cfg,
            false,
            &session::Stored::Absent,
            std::slice::from_ref(&notice),
            None,
            &theme::Theme::default(),
        );
        let row = lines
            .iter()
            .find(|(field, _)| field == "notice")
            .map(|(_, value)| value.clone())
            .expect("no `notice` row");
        assert!(row.contains("unknown provider"), "{row}");
        assert!(!row.contains('\x1b') && !row.contains('\r'), "{row:?}");
    }

    /// A home config that is *there* and cannot be used is a row of its own in
    /// the dump — `unreadable — <reason>`, under the session, the layer it
    /// ranks below — because the values it might have carried are not read and
    /// a human chasing a vanished key has to see that (finding C3). A file that
    /// read, or none at all, adds no row: silence is the normal state.
    #[test]
    fn describe_reports_an_unreadable_home_config() {
        let cfg = Config::new("http://host:1", "m", None);
        let complaint = "could not read /tmp/x/config.json — expected value at line 1 column 2; \
                         using defaults";
        let rows = describe(
            &cfg,
            false,
            &session::Stored::Absent,
            &[],
            Some(complaint),
            &theme::Theme::default(),
        );
        let names: Vec<&str> = rows.iter().map(|(field, _)| field.as_str()).collect();
        let at = names
            .iter()
            .position(|name| *name == "home config")
            .expect("no `home config` row");
        assert_eq!(names[at - 1], "session", "under the layer it ranks below");
        let row = &rows[at].1;
        assert!(row.starts_with("unreadable — "), "{row}");
        assert!(
            row.contains("/tmp/x/config.json") && row.contains("using defaults"),
            "{row}"
        );

        let rows = describe(
            &cfg,
            false,
            &session::Stored::Absent,
            &[],
            None,
            &theme::Theme::default(),
        );
        assert!(
            !rows.iter().any(|(field, _)| field == "home config"),
            "a file that read is not news"
        );
    }

    /// End to end through the load road: the file the human edited badly comes
    /// back as the complaint the dump prints and the window says, and nothing
    /// is half-read out of it (finding C3).
    #[test]
    fn an_unreadable_home_config_travels_to_the_dump() {
        let dir = Scratch::new("main-home");
        let path = dir.join("config.json");
        std::fs::write(&path, "{ \"api_key\": \"sk-secret\", ").unwrap();

        let loaded = UserConfig::load_from(&path);
        let env = theme::EnvText {
            theme: None,
            colorterm: None,
            term: None,
        };
        let (resolved, stored, theme) =
            resolved_config(&dir, &Overrides::default(), &env, &loaded.config).unwrap();
        let rows = describe(
            &resolved.config,
            false,
            &stored,
            &resolved.notices,
            loaded.complaint.as_deref(),
            &theme,
        );
        let row = rows
            .iter()
            .find(|(field, _)| field == "home config")
            .map(|(_, value)| value.clone())
            .expect("no `home config` row");
        assert!(row.contains(&path.display().to_string()), "{row}");
        assert!(
            resolved.config.api_key.is_none(),
            "the key in a file mush could not read is not half-read"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"api_key\": \"sk-secret\", "
        );
        let _ = std::fs::remove_dir_all(&dir);
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

        let scratch = Scratch::new("main-dump-missing");
        let missing = scratch.join("never-opened");
        let home = UserConfig::default();
        let (_, stored, _) = resolved_config(&missing, &Overrides::default(), &env, &home).unwrap();
        assert!(matches!(stored, session::Stored::Absent), "{stored:?}");
        assert!(
            !missing.exists(),
            "the dump created the workspace it described"
        );

        let ws = scratch_workspace("dump-unreadable");
        let file = session::session_path(ws.root());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{ not json").unwrap();
        // The dump reads the home file too; a `MUSH_CONFIG` another test set
        // is none of this test's business, so it names a config of its own.
        let home = UserConfig::load_from(Path::new("/nonexistent/mush/config.json"));
        let (resolved, stored, theme) =
            resolved_config(ws.root(), &Overrides::default(), &env, &home.config).unwrap();
        let row = describe(
            &resolved.config,
            false,
            &stored,
            &resolved.notices,
            home.complaint.as_deref(),
            &theme,
        )
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
        let config = config::resolve(&Overrides::default(), &UserConfig::default(), None)
            .unwrap()
            .config;
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
        // scroll keys — the wheel is a pointer, and the key table is the
        // keyboard's, so it has no row here (the manual's mouse paragraph names
        // it; a wheel row belongs in the table the day the README's block is
        // re-blessed with it).
        for want in ["←", "→", "↑ / ↓, PgUp / PgDn", "page up / down the rows"] {
            assert!(help.contains(want), "`{want}` is missing:\n{help}");
        }
        assert!(
            !help.contains("wheel"),
            "the key table grew a pointer row:\n{help}"
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
            "mush edit [--agent N] [--base R] [--send] TEXT [DIR]",
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
        // And it is the terminal door: a control sequence does not ride
        // through the escaping (finding C8).
        let defanged = escape_line("look: \x1b]0;PWNED\x07 done");
        assert!(
            !defanged.contains('\x1b') && !defanged.contains('\x07'),
            "{defanged:?}"
        );
        assert!(
            defanged.contains("look:") && defanged.contains("done"),
            "{defanged:?}"
        );
    }

    /// The attach printers are the terminal door for what they print: the text
    /// a `read` row and a roster column carry is chosen by a model, an endpoint
    /// or a hand-edited file, and neither printer may let a control sequence
    /// through to the human's terminal — `ESC ]0;x BEL` renames the window of
    /// the shell that ran `mush read`, which is the one road in the tree that
    /// did not go through `text::sanitize` (finding C8).
    ///
    /// Both bodies are printed into a `String` — the bytes the real `print!`
    /// would put on a terminal — so the pin is what reaches the terminal, not
    /// that nothing panicked.
    #[test]
    fn the_read_and_agents_printers_emit_no_control_sequence() {
        let osc = "\x1b]0;PWNED\x07";
        let read = serde_json::json!({
            "lines": [
                { "line": 0, "text": format!("look: {osc} done") },
                // A tab is still escaped: these rows are TSV, and sanitize
                // deliberately keeps a tab for the wrappers.
                { "line": 1, "text": "a\tb" },
            ]
        });
        let out = lines_text(&read).unwrap();
        assert!(!out.contains('\x1b') && !out.contains('\x07'), "{out:?}");
        assert!(
            out.contains("look:") && out.contains("done"),
            "the words stay: {out:?}"
        );
        assert!(out.contains("a\\tb"), "the TSV escape survives: {out:?}");

        let roster = serde_json::json!({
            "agents": [{
                "id": 2,
                "parent": 0,
                "phase": "idle",
                "activity": format!("{osc}busy"),
                "title": format!("t{osc}"),
                "branch": format!("b{osc}"),
                "worktree": format!("/w{osc}"),
                "children_working": 0,
            }]
        });
        let out = agents_text(&roster).unwrap();
        assert!(!out.contains('\x1b') && !out.contains('\x07'), "{out:?}");
        assert!(
            out.contains("idle") && out.contains("busy") && out.contains("/w"),
            "the words stay: {out:?}"
        );
        assert_eq!(
            out.matches('\t').count(),
            7,
            "still eight TSV columns: {out:?}"
        );
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

    /// A flag given twice in one line is refused by name: the parser kept the
    /// last value and dropped the first without a word, so `--agent 1 … --agent
    /// 2` ran as agent 2 and the 1 was never mentioned again — a value given and
    /// not used, which is the one thing this parser must not do (finding H26,
    /// the rule `require_flag` already follows for a flag outside its
    /// subcommand).
    #[test]
    fn a_repeated_flag_is_refused_by_name() {
        let parse = |argv: &[&str]| {
            Cli::detect(&argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        for (argv, flag) in [
            (&["read", "--agent", "1", "--agent", "2"][..], "--agent"),
            (&["read", "--since", "1", "--since", "2"][..], "--since"),
            (
                &["edit", "--base", "1", "--base", "2", "a draft"][..],
                "--base",
            ),
            (&["edit", "--send", "--send", "a draft"][..], "--send"),
        ] {
            let error = parse(argv).expect_err(&format!("{argv:?} gives `{flag}` twice"));
            for want in [flag, "--help"] {
                assert!(
                    error.contains(want),
                    "the refusal must name `{want}`: {error}"
                );
            }
        }
        // The repeat is refused before the value is read, so a second flag with
        // no value after it still hears about the repeat rather than the number
        // it never got.
        let error = parse(&["read", "--agent", "1", "--agent"]).unwrap_err();
        assert!(error.contains("twice"), "{error}");
    }

    /// `focus` takes one agent id, and either spelling is that id. Giving both
    /// used to be a silent choice — the positional id won and `--agent N`
    /// vanished, so the flag and the value it carried were gone with no word
    /// (finding H26).
    #[test]
    fn focus_refuses_two_agent_ids() {
        let parse = |argv: &[&str]| {
            Cli::detect(&argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        for argv in [
            &["focus", "1", "--agent", "2"][..],
            &["focus", "--agent", "2", "1"][..],
            // No value at all after the flag: the two ids are what is wrong with
            // this line, so the refusal names them rather than the number the
            // flag is missing.
            &["focus", "1", "--agent"][..],
        ] {
            let error = parse(argv).expect_err(&format!("{argv:?} names two agent ids"));
            for want in ["focus", "--agent", "positional", "--help"] {
                assert!(
                    error.contains(want),
                    "the refusal must name `{want}`: {error}"
                );
            }
        }
        // Either spelling alone is the one id it always was.
        assert_eq!(
            parse(&["focus", "2"]).unwrap(),
            Some(Cli::Focus {
                dir: ".".into(),
                agent: 2
            })
        );
        assert_eq!(
            parse(&["focus", "--agent", "2"]).unwrap(),
            Some(Cli::Focus {
                dir: ".".into(),
                agent: 2
            })
        );
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

    /// The entry is all or nothing (finding PM8): a failure in a step after
    /// `enable_raw_mode` has succeeded used to travel with no `TerminalGuard`
    /// alive to undo the mode, so `main` exited 1 into a raw shell. Both
    /// failing steps are here — the alternate-screen/paste write and the
    /// terminal's own construction — because both happen after raw mode is on.
    #[test]
    fn a_failed_enter_restores_the_modes() {
        fn enter(fail: &str, restored: Arc<AtomicBool>) -> io::Result<TerminalGuard> {
            TerminalGuard::enter_with(
                || Ok(()),
                move || {
                    if fail == "screen" {
                        Err(io::Error::other("the screen write failed"))
                    } else {
                        Ok(())
                    }
                },
                move || {
                    if fail == "terminal" {
                        Err(io::Error::other("the terminal would not build"))
                    } else {
                        unreachable!("nothing leaves the entry unfinished")
                    }
                },
                move || restored.store(true, Ordering::SeqCst),
            )
        }

        for failed in ["screen", "terminal"] {
            let restored = Arc::new(AtomicBool::new(false));
            let error = enter(failed, restored.clone())
                .err()
                .unwrap_or_else(|| panic!("the {failed} step must fail"));
            assert!(
                !error.to_string().is_empty(),
                "the failure still travels: {error}"
            );
            assert!(
                restored.load(Ordering::SeqCst),
                "a failure in the {failed} step left the modes entered"
            );
        }
    }

    /// The escape sequences a panic writes, captured: the `Write` seam
    /// [`restore_mode_sequences`] takes (finding E10).
    #[derive(Clone, Default)]
    struct Modes(Arc<std::sync::Mutex<Vec<u8>>>);

    impl io::Write for Modes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A worker's panic leaves the terminal alone and its words are *held* for
    /// the exit road: with the alternate screen up, printing them paints glyph
    /// garbage a frame ratatui believes is intact will never repair (finding
    /// PM9). The panic on the thread the hook was installed for restores the
    /// modes at once and still goes to the hook that was there before. The
    /// escape sequences are read through the `Write` seam, so "no sequence at
    /// all" is an assertion rather than a screen to look at (finding E10).
    /// Before the fix the hook restored for every panic: a worker's death wrote
    /// `^[[?1049l^[[?2004l^[[?1006l^[[?1015l^[[?1003l^[[?1002l^[[?1000l` and
    /// the human's UI was left painting into their shell.
    #[test]
    fn a_worker_panic_leaves_the_terminal_alone() {
        // The hook that was there before this one: it records that it ran,
        // standing in for the default hook's stderr write.
        let printed = Arc::new(AtomicBool::new(false));
        {
            let printed = printed.clone();
            std::panic::set_hook(Box::new(move |_| printed.store(true, Ordering::SeqCst)));
        }
        let modes = Modes::default();
        let sink = modes.clone();
        let route = PanicRoute::new();
        let owner = std::thread::current().id();
        install_panic_hook_for(owner, route.clone(), move || {
            let mut sink = sink.clone();
            let _ = restore_mode_sequences(&mut sink, false);
        });
        route.raise();

        // The shape of a panicking job thread, or the session writer: a named
        // worker that dies while the UI holds the screen. The terminal is not
        // its to restore, and the words are not its to print.
        let worker = std::thread::Builder::new()
            .name("mush-job-1".to_string())
            .spawn(|| panic!("a worker died"))
            .expect("the worker thread starts");
        assert!(worker.join().is_err(), "the worker panicked");
        let written = modes.0.lock().unwrap().clone();
        assert!(
            written.is_empty(),
            "a worker's panic wrote to the human's terminal: {:?}",
            String::from_utf8_lossy(&written)
        );
        assert!(
            !printed.load(Ordering::SeqCst),
            "a worker's panic was printed into the alternate screen"
        );
        let words = route.lower();
        assert_eq!(words.len(), 1, "one panic, one account: {words:?}");
        assert!(words[0].contains("mush-job-1"), "{words:?}");
        assert!(words[0].contains("a worker died"), "{words:?}");

        // Once the terminal is handed back, the next panic's words go where
        // they always did: to the hook that was there before.
        let worker = std::thread::Builder::new()
            .name("mush-job-2".to_string())
            .spawn(|| panic!("after the road"))
            .expect("the worker thread starts");
        assert!(worker.join().is_err(), "the worker panicked");
        assert!(
            printed.load(Ordering::SeqCst),
            "after the hand-back the words are printed at once"
        );

        // The thread the hook was installed for is the terminal's: its own
        // panic is the one that must leave the modes behind — at once, and
        // without being held.
        let _ = std::panic::catch_unwind(|| panic!("the terminal's thread died"));
        let written = String::from_utf8_lossy(&modes.0.lock().unwrap()).into_owned();
        assert!(
            written.contains("\u{1b}[?1049l"),
            "leaves the alternate screen: {written:?}"
        );
        assert!(
            written.contains("\u{1b}[?2004l"),
            "and turns bracketed paste off: {written:?}"
        );
        assert!(
            written.contains("\u{1b}[?1000l") && written.contains("\u{1b}[?1006l"),
            "and puts the mouse back before the human's shell gets it: {written:?}"
        );
        assert!(
            route.lower().is_empty(),
            "the owner's panic is not held: it is the terminal's own"
        );

        // The hook is the process's: put the default back, so whatever panic
        // comes next is not shaped by this test.
        let _ = std::panic::take_hook();
    }

    /// The mouse is taken by the two modes a click needs and not by the
    /// library's whole set: 1000 (press and release) and 1006 (SGR
    /// coordinates), and *not* 1002 (drag), 1003 (any motion) or 1015 (the
    /// older coordinate encoding) — the mode that reports every pixel of every
    /// move would wake the loop for repaints that change nothing, and mush has
    /// no drag verb for 1002 to feed. The bytes are pinned because a stray mode
    /// here is one the hand-back's offs must cover and a terminal feels at once.
    #[test]
    fn the_mouse_is_taken_by_the_two_modes_a_click_needs() {
        let modes = Modes::default();
        let mut sink = modes.clone();
        take_mouse(&mut sink).expect("the sink answers");
        let written = String::from_utf8_lossy(&modes.0.lock().unwrap()).into_owned();
        assert_eq!(
            written, "\u{1b}[?1000h\u{1b}[?1006h",
            "the mode set a click needs: {written:?}"
        );
        for unwanted in ["\u{1b}[?1002h", "\u{1b}[?1003h", "\u{1b}[?1015h"] {
            assert!(
                !written.contains(unwanted),
                "{unwanted:?} is a mode nothing on this side reads: {written:?}"
            );
        }
    }

    /// The keyboard enhancement flags are popped only when they were pushed: a
    /// terminal that never answered the support query is not sent a pop it
    /// never opened, and a terminal that did answer must not keep mush's flags
    /// across the hand-back — a shell that inherits them reads every key as a
    /// `CSI u` sequence. The pop rides in the same `flush` as the leave
    /// escapes, so the hand-back cannot be half-written.
    #[test]
    fn the_keyboard_flags_are_popped_only_when_they_were_pushed() {
        let modes = Modes::default();
        let mut sink = modes.clone();
        restore_mode_sequences(&mut sink, false).expect("the sink answers");
        let plain = String::from_utf8_lossy(&modes.0.lock().unwrap()).into_owned();
        assert!(
            plain.contains("\u{1b}[?1049l"),
            "leaves the alternate screen: {plain:?}"
        );
        assert!(
            !plain.contains("\u{1b}[<1u"),
            "a terminal that never pushed the flags was sent a pop: {plain:?}"
        );

        let modes = Modes::default();
        let mut sink = modes.clone();
        restore_mode_sequences(&mut sink, true).expect("the sink answers");
        let enhanced = String::from_utf8_lossy(&modes.0.lock().unwrap()).into_owned();
        assert!(
            enhanced.contains("\u{1b}[<1u"),
            "pops the flags the terminal answered for: {enhanced:?}"
        );
        assert!(
            enhanced.contains("\u{1b}[?1049l"),
            "and the pop is part of the one hand-back: {enhanced:?}"
        );
    }

    /// The hold is bounded, and the bound is said rather than silent: a process
    /// panicking without end must not grow the list forever, and the ninth
    /// panic's words are accounted for with the eight that were kept.
    #[test]
    fn the_held_panic_words_are_bounded_and_the_bound_is_said() {
        let route = PanicRoute::new();
        for n in 0..KEPT_PANIC_WORDS + 3 {
            route.keep(format!("thread 'mush-job-{n}' panicked at nowhere: boom"));
        }
        let words = route.lower();
        assert_eq!(words.len(), KEPT_PANIC_WORDS + 1, "{words:?}");
        assert!(words[0].contains("mush-job-0"), "the first is kept");
        assert!(
            words[KEPT_PANIC_WORDS].contains("3 more panics were not kept"),
            "and the dropped three are counted: {:?}",
            words[KEPT_PANIC_WORDS]
        );
        assert!(route.lower().is_empty(), "the take is once");
    }

    /// The attach CLI's refusal reaches the terminal through [`error_line`],
    /// and its text can quote a model id an *endpoint* chose: `blind_model_line`
    /// writes `Config::model` into the sentence, a model id can be the first
    /// entry of the endpoint's `/v1/models` list, and a `ReplyError`'s message
    /// crosses the socket as a plain `String` nothing defangs on the way. The
    /// pin: what `main` prints for such a refusal carries no ESC, no OSC and no
    /// carriage return, and the id's own visible words still read.
    #[test]
    fn an_endpoint_chosen_model_id_cannot_reach_the_terminal_raw() {
        // The id is hostile in the three ways a terminal acts on it: an OSC
        // that retitles the window, a CSI that clears the frame, and a bare
        // `\r` that paints over the line it was printed on. The second half of
        // each pair is what a defanged painting of the id still says.
        for (id, visible) in [
            ("\u{1b}]0;pwned\u{7}evil-model", "evil-model"),
            ("\u{1b}[2Jevil-model", "evil-model"),
            ("evil\r-model", "evil␍-model"),
        ] {
            // The sentence `blind_model_line` makes of the id
            // (app/mod.rs:269), after `ReplyError::describe` prefixed its kind.
            let error = format!(
                "bad_request: `{id}` is not a model mush knows to accept images — Ctrl-P picks \
                 one whose row documents vision"
            );
            let line = error_line(&error);
            assert!(line.starts_with("mush: bad_request: "), "{line:?}");
            assert!(
                !line.contains('\u{1b}') && !line.contains('\u{7}') && !line.contains('\r'),
                "an escape reached the terminal through {id:?}: {line:?}"
            );
            assert!(
                line.contains(visible),
                "the id's own words still read, defanged: {line:?}"
            );
        }
    }
}
