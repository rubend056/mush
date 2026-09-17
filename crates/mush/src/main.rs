//! mush — a small, fast terminal surface for coding agents.
//!
//! Usage: `mush [DIRECTORY]` opens a workspace. An agent connected to the
//! configured OpenAI-compatible endpoint reads and writes it; mush shows the
//! tree of agents and the repository's state at a glance.

mod agent;
mod app;
mod clock;
mod events;
mod http;
mod input;
mod jobs;
mod machine;
mod model;
mod session_save;
mod ui;

use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
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
    url: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    context: Option<usize>,
    temperature: Option<f32>,
    max_completion_tokens: Option<bool>,
    reasoning_effort: Option<config::ReasoningEffort>,
    thinking: Option<config::ThinkingMode>,
    /// `-y` / `--yes`: recorded in [`AUTO_APPROVE`] and nowhere else.
    yes: bool,
    /// `--print-config`: print the resolved config and exit, instead of opening
    /// the terminal.
    print_config: bool,
}

impl Args {
    /// The command line as the config layer sees it. mush has no API-key flag;
    /// a key comes from `MUSH_API_KEY` or the home config.
    fn overrides(&self) -> Overrides {
        Overrides {
            url: self.url.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            api_key: None,
            context: self.context,
            temperature: self.temperature,
            max_completion_tokens: self.max_completion_tokens,
            reasoning_effort: self.reasoning_effort,
            thinking: self.thinking,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    parse_from(std::env::args().skip(1))
}

/// [`parse_args`] over an explicit argument list, so the flags are testable
/// without a process environment.
fn parse_from<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let mut dir: Option<PathBuf> = None;
    let mut url = None;
    let mut model = None;
    let mut provider = None;
    let mut context = None;
    let mut temperature = None;
    let mut max_completion_tokens = None;
    let mut reasoning_effort = None;
    let mut thinking = None;
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
            "--max-completion-tokens" => max_completion_tokens = Some(true),
            "--url" => url = Some(args.next().ok_or("--url needs a value")?),
            "--model" => model = Some(args.next().ok_or("--model needs a value")?),
            "--provider" => provider = Some(args.next().ok_or("--provider needs a value")?),
            "--context" => {
                let value = args.next().ok_or("--context needs a value")?;
                let tokens = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--context needs a token count, got `{value}`"))?;
                context = Some(tokens);
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
                temperature = Some(stated);
            }
            "--reasoning-effort" => {
                let value = args.next().ok_or("--reasoning-effort needs a value")?;
                // Rejected by name rather than ignored, like every other value
                // mush cannot use: an unknown effort must never reach an
                // endpoint, and dropping it would send the provider's default
                // instead of the effort the human asked for.
                let stated = config::ReasoningEffort::parse(&value).map_err(|_| {
                    format!("--reasoning-effort needs low, medium, high or none, got `{value}`")
                })?;
                reasoning_effort = Some(stated);
            }
            "--thinking" => {
                let value = args.next().ok_or("--thinking needs a value")?;
                // The same rule: `--thinking of` is a typo, not an instruction
                // to leave the thinking mode on.
                let stated = config::ThinkingMode::parse(&value)
                    .map_err(|_| format!("--thinking needs on or off, got `{value}`"))?;
                thinking = Some(stated);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option `{other}` (try --help)"));
            }
            other => set_dir(&mut dir, other)?,
        }
    }

    Ok(Args {
        dir: dir.unwrap_or_else(|| PathBuf::from(".")),
        url,
        model,
        provider,
        context,
        temperature,
        max_completion_tokens,
        reasoning_effort,
        thinking,
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
         \x20                      Reasoning effort sent as `reasoning_effort`: low, medium or high,\n\
         \x20                      or none to send no such field (default: {};\n\
         \x20                      $MUSH_REASONING_EFFORT)\n\
         \x20   --thinking MODE    on asks for the provider's thinking mode, off sends no `thinking`\n\
         \x20                      field at all (default: {}; $MUSH_THINKING)\n\
         \x20   --max-completion-tokens\n\
         \x20                      Send the reply cap (a quarter of the window) as\n\
         \x20                      `max_completion_tokens` instead of `max_tokens`, as\n\
         \x20                      OpenAI's reasoning models require\n\
         \x20   -y, --yes          Pre-approve this session's work. Recorded only: mush asks\n\
         \x20                      nothing yet, so this changes no behaviour today\n\
         \x20   --print-config     Print the resolved config (endpoint, provider, model, window\n\
         \x20                      and whether it was stated, temperature, reasoning effort and\n\
         \x20                      thinking mode, reply-cap size and name, key masked) and exit 0\n\n\
         KEYS:\n{keys}\n\n\
         COMMANDS (type in the chat):\n\
         {commands}\n\
         Endpoint, API key, model, and the request knobs live in\n\
         $MUSH_CONFIG or the platform config directory. That file is hand-editable,\n\
         every field is optional, and the one mush writes documents itself.\n\
         --print-config shows what those layers resolved to. The conversation is\n\
         stored in <DIRECTORY>/.mush/session.json.\n",
        env!("CARGO_PKG_VERSION"),
        mush_core::provider::names_hint(),
        mush_core::provider::DEFAULT_PROVIDER.name(),
        mush_core::provider::effort_default_hint(),
        mush_core::provider::thinking_default_hint(),
    )
}

/// `--print-config`: the resolved config and nothing else — no workspace, no
/// `.mush/`, no request to an endpoint. This is what makes a hand-edited home
/// file debuggable, and the only way to see the precedence chain rather than
/// guess at it.
fn print_config(config: &Config) {
    for (field, value) in describe(config, auto_approve()) {
        println!("{field:<13}{value}");
    }
}

/// One `field  value` line per fact, in the order a human reads them. The
/// values are what a request will carry, not what some file wished for; the
/// window is the one fact whose *source* matters, so it is named.
fn describe(config: &Config, approved: bool) -> Vec<(String, String)> {
    let key = match config.api_key.as_deref().filter(|key| !key.is_empty()) {
        Some(key) => format!("{} (masked)", mask_key(key)),
        None => "(none)".to_string(),
    };
    let model = if config.model.is_empty() {
        "(none yet)".to_string()
    } else {
        config.model.clone()
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
    // stated is the provider's, not the human's.
    let source = if config.reasoning_effort_stated() {
        "stated"
    } else {
        "the provider's default"
    };
    let effort = format!("{} ({source})", config.reasoning_effort().unwrap_or("none"));
    let source = if config.thinking_stated() {
        "stated"
    } else {
        "the provider's default"
    };
    let thinking = format!(
        "{} ({source})",
        if config.thinking_enabled() {
            "on"
        } else {
            "off"
        }
    );
    let approve = if approved {
        "yes (-y recorded; nothing asks yet)"
    } else {
        "no"
    };
    vec![
        ("endpoint".to_string(), config.base_url.clone()),
        ("provider".to_string(), config.provider.name().to_string()),
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
    ]
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    // `-y` is a fact about this session, not a config value: it is recorded here
    // and read by the features that will ask (and by `--print-config`). Nothing
    // else changes because of it.
    AUTO_APPROVE.store(args.yes, Ordering::Relaxed);
    let overrides = args.overrides();

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

    if args.print_config {
        // The resolved config, then out: no terminal is entered, no `.mush/` is
        // created, and no request is made. The workspace's stored session is
        // still read, because it is a layer of the precedence being shown.
        let stored = Session::load(&dir);
        let config = config::resolve(&overrides, &UserConfig::load(), stored.as_ref())?;
        print_config(&config);
        return Ok(());
    }

    let workspace = Workspace::new(&dir)?;
    session::ensure_mush_dir(workspace.root())?;

    // CLI flags > environment > saved session > home config > defaults; the
    // whole precedence lives in one tested function in mush-core.
    let stored = Session::load(workspace.root());
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
    let mut app = App::new(workspace, cell, stored, root, tx.clone(), save);

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

    install_panic_hook();
    let mut guard = TerminalGuard::enter()?;
    // The terminal's width is only known here; `/notes` wraps its popup to it,
    // so record it before the first key can be read. A resize reports its own.
    if let Ok(size) = guard.terminal.size() {
        app.set_term_width(size.width);
    }
    let result = event_loop(&mut guard.terminal, &mut app, &rx);
    drop(guard);
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &Receiver<Msg>,
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
                // told the new width so a `/notes` report wraps to it too.
                Event::Resize(width, _) => {
                    app.set_term_width(width);
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
            terminal.draw(|frame| ui::draw(frame, app))?;
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

    #[test]
    fn cli_overrides_map_to_the_config_layer() {
        let args = Args {
            dir: PathBuf::from("."),
            url: Some("http://host:1".into()),
            model: None,
            provider: Some("deepseek".into()),
            context: Some(64_000),
            temperature: Some(0.2),
            max_completion_tokens: Some(true),
            reasoning_effort: Some(config::ReasoningEffort::Medium),
            thinking: Some(config::ThinkingMode::Off),
            yes: true,
            print_config: false,
        };
        let overrides = args.overrides();
        assert_eq!(overrides.url.as_deref(), Some("http://host:1"));
        assert_eq!(overrides.provider.as_deref(), Some("deepseek"));
        assert_eq!(overrides.model, None);
        assert_eq!(overrides.context, Some(64_000));
        assert_eq!(overrides.temperature, Some(0.2));
        assert_eq!(overrides.max_completion_tokens, Some(true));
        assert_eq!(
            overrides.reasoning_effort,
            Some(config::ReasoningEffort::Medium)
        );
        assert_eq!(overrides.thinking, Some(config::ThinkingMode::Off));
        // The key never comes from argv, and `-y` is not a config value: it is
        // recorded for the features that will ask, and nothing else.
        assert_eq!(overrides.api_key, None);
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
            "none",
            "--thinking",
            "off",
            "--max-completion-tokens",
            "--print-config",
            "work",
        ];
        let args = parse_from(argv.into_iter().map(str::to_string)).unwrap();
        assert_eq!(args.dir, PathBuf::from("work"));
        assert_eq!(args.temperature, Some(0.25));
        assert_eq!(args.max_completion_tokens, Some(true));
        // `none` is a statement, and it survives parsing as one: the config
        // layer has to be able to tell it from silence.
        assert_eq!(args.reasoning_effort, Some(config::ReasoningEffort::Off));
        assert_eq!(args.thinking, Some(config::ThinkingMode::Off));
        assert!(args.yes, "`-y` is remembered, not acted on");
        assert!(args.print_config);

        // Long form, and nothing else stated: every flag stays unset.
        let args = parse_from(["--yes".to_string()].into_iter()).unwrap();
        assert!(args.yes);
        assert_eq!(args.temperature, None);
        assert_eq!(args.max_completion_tokens, None);
        assert_eq!(args.reasoning_effort, None, "unstated is not `none`");
        assert_eq!(args.thinking, None);
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
        cfg.reasoning_effort = Some(config::ReasoningEffort::Medium);
        cfg.thinking = Some(config::ThinkingMode::Off);

        let lines = describe(&cfg, true);
        let field = |name: &str| {
            lines
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("no `{name}` line in {lines:?}"))
        };
        assert_eq!(field("endpoint"), "http://host:1");
        assert_eq!(field("provider"), "custom");
        assert_eq!(field("model"), "deepseek-v4-pro");
        assert_eq!(field("window"), "64000 tokens (stated)");
        assert_eq!(field("temperature"), "0.0", "0 is a value, not an absence");
        assert_eq!(field("reasoning"), "medium (stated)");
        assert_eq!(field("thinking"), "off (stated)");
        assert_eq!(
            field("reply cap"),
            "16000 tokens as max_completion_tokens",
            "a quarter of the stated 64k window"
        );
        assert_eq!(field("api key"), "sk-1…7890 (masked)");
        assert_eq!(field("auto-approve"), "yes (-y recorded; nothing asks yet)");

        // An unresolved window says so, and a default request samples at 1.0
        // under the name every endpoint documents.
        let plain = Config::new("http://host:1", "", None);
        let plain = describe(&plain, false);
        let field = |name: &str| {
            plain
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        assert_eq!(field("model"), "(none yet)");
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
            "2048 tokens as max_tokens",
            "an 8192-token window affords 2048"
        );
        assert_eq!(field("api key"), "(none)");
        assert_eq!(field("auto-approve"), "no");

        // DeepSeek with nothing stated is the preset request: the effort and
        // the thinking mode are sent, and the line says whose they are.
        let mut preset = Config::new("https://api.deepseek.com", "deepseek-flash", None);
        preset.provider = config::Provider::DeepSeek;
        let preset = describe(&preset, false);
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

        let lines = describe(&config, false);
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
        assert_eq!(field("reply cap"), "30000 tokens as max_tokens");
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
        // The tree walk the human asked for is named here too.
        for want in ["←", "→"] {
            assert!(help.contains(want), "`{want}` is missing:\n{help}");
        }

        // And the command table, so `/compact`-style absence cannot return.
        let commands = app::commands::table(&mush_core::provider::names_piped());
        assert!(
            help.contains(&commands),
            "the command table is not in --help"
        );
        assert!(help.contains("KEYS:"));
        assert!(help.contains("COMMANDS (type in the chat):"));
    }
}
