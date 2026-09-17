//! mush — a small, fast terminal surface for coding agents.
//!
//! Usage: `mush [DIRECTORY]` opens a workspace. An agent connected to the
//! configured OpenAI-compatible endpoint reads and writes it; mush shows the
//! tree of agents and the repository's state at a glance.

mod agent;
mod app;
mod http;
mod input;
mod model;
mod ui;

use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
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

use mush_core::{config, session, Overrides, Session, UserConfig, Workspace};

use app::{App, Msg};

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
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut dir: Option<PathBuf> = None;
    let mut url = None;
    let mut model = None;
    let mut provider = None;
    let mut context = None;
    let mut args = std::env::args().skip(1);
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
    println!(
        "mush {}\n\
         A small, fast terminal surface for coding agents.\n\n\
         USAGE:\n    mush [DIRECTORY] [--url URL] [--model NAME] [--provider NAME] [--context TOKENS]\n\n\
         OPTIONS:\n\
         \x20   --url URL          OpenAI-compatible endpoint (default: $MUSH_URL or the provider default)\n\
         \x20   --model NAME       Model id (default: $MUSH_MODEL, else auto-detected)\n\
         \x20   --provider NAME    deepseek or custom (default: $MUSH_PROVIDER or custom)\n\
         \x20   --context TOKENS   Context window when nothing else knows it (default: $MUSH_CONTEXT,\n\
         \x20                      else what the endpoint advertises, else the model's known window)\n\n\
         KEYS:\n\
         \x20   Tab / Shift-Tab   cycle panes (agents, chat)\n\
         \x20   Enter             send message (chat) · focus agent (agents)\n\
         \x20   Shift/Alt-Enter   new line in the message (multi-line messages)\n\
         \x20   j / k · Enter     select and focus an agent\n\
         \x20   c / Esc           cancel agent / back to the root (agents)\n\
         \x20   Ctrl-P            model picker\n\
         \x20   Ctrl-N            new chat    Ctrl-C  stop the focused agent\n\
         \x20   Ctrl-X            stop every running agent\n\
         \x20   wheel             scroll the transcript\n\
         \x20   Ctrl-Q            quit\n\n\
         COMMANDS (type in the chat):\n\
         \x20   /provider [deepseek|custom]  switch provider\n\
         \x20   /model                       pick a model\n\
         \x20   /context [TOKENS]            show or set the context window\n\
         \x20   /url http://host:port        set the endpoint\n\
         \x20   /key <secret>                set the API key (saved to the home config)\n\
         \x20   /models                      refresh the model list\n\
         \x20   /worktrees                   re-scan for leftover isolated worktrees\n\
         \x20   /diff <id>                   print the diff command for an isolated agent\n\
         \x20   /merge|/discard <id>         merge or throw away its work, and reclaim it\n\
         \x20   /forget <id>                 drop the agent from this session (its branch stays)\n\
         \x20   /new  /help  /quit\n\
         Endpoint, API key, and model defaults live in\n\
         $MUSH_CONFIG or the platform config directory. The conversation is stored\n\
         in <DIRECTORY>/.mush/session.json.",
        env!("CARGO_PKG_VERSION")
    );
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
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

    let workspace = Workspace::new(&dir)?;
    session::ensure_mush_dir(workspace.root())?;

    // CLI flags > environment > saved session > home config > defaults; the
    // whole precedence lives in one tested function in mush-core.
    let stored = Session::load(workspace.root());
    let mut config = config::resolve(&overrides, &UserConfig::load(), stored.as_ref())?;

    // One `/v1/models` request serves the picker, and picks the initial model
    // only when nothing else named one. A known model skips the fetch — a slow
    // or silent endpoint must not delay the first paint (finding A9); `/model`
    // and `/url` refetch on demand. The endpoint's advertised window can only
    // be adopted from a fetch that happened.
    let models = if config.model.is_empty() {
        let models = http::list_models(&config);
        match models.first() {
            Some(model) => config.model = model.id.clone(),
            None => eprintln!(
                "mush: no model given and none discovered at {} — pick one with /model",
                config.models_url()
            ),
        }
        models
    } else {
        Vec::new()
    };
    if let Some(advertised) = models
        .iter()
        .find(|model| model.id == config.model)
        .and_then(|model| model.context)
    {
        config.adopt_context(advertised);
    }

    let (tx, rx) = unbounded::<Msg>();
    let root = agent::spawn(config.clone(), tx.clone(), workspace.root().to_path_buf());
    let mut app = App::new(workspace, config, stored, root, tx.clone(), models);

    install_panic_hook();
    let mut guard = TerminalGuard::enter()?;
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
                // frame already paints at the new dimensions.
                Event::Resize(_, _) => app.dirty_screen = true,
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
        };
        let overrides = args.overrides();
        assert_eq!(overrides.url.as_deref(), Some("http://host:1"));
        assert_eq!(overrides.provider.as_deref(), Some("deepseek"));
        assert_eq!(overrides.model, None);
        assert_eq!(overrides.context, Some(64_000));
        // The key never comes from argv.
        assert_eq!(overrides.api_key, None);
    }

    /// The full startup path with an unreachable endpoint must still produce a
    /// usable config: that is the "window opens, no model" case.
    #[test]
    fn resolution_survives_an_empty_world() {
        let config = config::resolve(&Overrides::default(), &UserConfig::default(), None).unwrap();
        assert!(!config.base_url.is_empty());
        assert!(config.chat_url().ends_with("/v1/chat/completions"));
    }
}
