//! mush — a small, fast, agent-agnostic terminal editor.
//!
//! Usage: `mush [DIRECTORY]` and you are editing. An agent connected to the
//! configured OpenAI-compatible endpoint reads and writes the same workspace.

mod agent;
mod app;
mod http;
mod ui;

use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
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
            other if other.starts_with("--") && !only_flags => {
                return Err(format!("unknown option `{other}` (try --help)"));
            }
            other => dir = Some(PathBuf::from(other)),
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

fn print_help() {
    println!(
        "mush {}\n\
         A small, fast, agent-agnostic terminal editor.\n\n\
         USAGE:\n    mush [DIRECTORY] [--url URL] [--model NAME] [--provider NAME] [--context TOKENS]\n\n\
         OPTIONS:\n\
         \x20   --url URL          OpenAI-compatible endpoint (default: $MUSH_URL or the provider default)\n\
         \x20   --model NAME       Model id (default: $MUSH_MODEL, else auto-detected)\n\
         \x20   --provider NAME    deepseek or custom (default: $MUSH_PROVIDER or custom)\n\
         \x20   --context TOKENS   Context window when nothing else knows it (default: $MUSH_CONTEXT,\n\
         \x20                      else what the endpoint advertises, else the model's known window)\n\n\
         KEYS:\n\
         \x20   Tab / Shift-Tab   cycle panes (agents, editor, chat)\n\
         \x20   Enter             send message (chat) · focus agent (agents)\n\
         \x20   i / Esc           enter insert / leave insert (editor)\n\
         \x20   c / Esc           cancel agent / back to the root (agents)\n\
         \x20   Ctrl-P            model picker\n\
         \x20   Ctrl-S            save        Ctrl-R  reload file\n\
         \x20   Ctrl-N            new chat    Ctrl-C  cancel running agents\n\
         \x20   Ctrl-Q            quit (twice if there are unsaved changes)\n\n\
         COMMANDS (type in the chat):\n\
         \x20   /provider [deepseek|custom]  switch provider\n\
         \x20   /model                       pick a model\n\
         \x20   /context [TOKENS]            show or set the context window\n\
         \x20   /url http://host:port        set the endpoint\n\
         \x20   /key <secret>                set the API key (saved to the home config)\n\
         \x20   /models                      refresh the model list\n\
         \x20   /open [path]                 open a file (no path: pick one)\n\
         \x20   /worktrees                   re-scan for leftover isolated worktrees\n\
         \x20   /diff|/merge|/discard <id>   git commands for an isolated agent\n\
         \x20   /new  /help  /quit\n\
         Endpoint, API key, and model defaults live in\n\
         $MUSH_CONFIG or ~/.config/mush/config.json. The conversation is stored\n\
         in <DIRECTORY>/.mush/session.json.",
        env!("CARGO_PKG_VERSION")
    );
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let overrides = args.overrides();

    // `mush src/main.rs` opens that file; `mush dir/` is a workspace.
    let mut dir = args.dir;
    let mut open_file: Option<String> = None;
    if dir.is_file() {
        open_file = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        dir = dir
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
    }

    let workspace = Workspace::new(&dir)?;
    session::ensure_mush_dir(workspace.root())?;

    // CLI flags > environment > saved session > home config > defaults; the
    // whole precedence lives in one tested function in mush-core.
    let stored = Session::load(workspace.root());
    let mut config = config::resolve(&overrides, &UserConfig::load(), stored.as_ref())?;

    // One `/v1/models` request serves both the picker and, when nothing else
    // named a model, the initial choice. A failed lookup yields the provider's
    // known models (empty for custom endpoints). An endpoint that advertises a
    // context window overrides the guess here, before the first request.
    let models = http::list_models(&config);
    if config.model.is_empty() {
        match models.first() {
            Some(model) => config.model = model.id.clone(),
            None => eprintln!(
                "mush: no model given and none discovered at {} — pick one with /model",
                config.models_url()
            ),
        }
    }
    if let Some(advertised) = models
        .iter()
        .find(|model| model.id == config.model)
        .and_then(|model| model.context)
    {
        config.adopt_context(advertised);
    }

    let (tx, rx) = unbounded::<Msg>();
    let root = agent::spawn(config.clone(), tx.clone(), workspace.root().to_path_buf());
    let mut app = App::new(
        workspace,
        config,
        stored,
        root,
        tx.clone(),
        models,
        open_file,
    );

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
    while !app.should_quit {
        while let Ok(msg) = rx.try_recv() {
            app.update(msg);
        }

        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.update(Msg::Key(key));
                }
                // The terminal changed size: schedule a redraw. ratatui's
                // `terminal.draw` re-queries the size first, so the next
                // frame already paints at the new dimensions.
                Event::Resize(_, _) => app.dirty_screen = true,
                _ => {}
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

/// Restores the terminal on both clean exit (Drop) and panic.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
    /// usable config: that is the "editor opens, no model" case.
    #[test]
    fn resolution_survives_an_empty_world() {
        let config = config::resolve(&Overrides::default(), &UserConfig::default(), None).unwrap();
        assert!(!config.base_url.is_empty());
        assert!(config.chat_url().ends_with("/v1/chat/completions"));
    }
}
