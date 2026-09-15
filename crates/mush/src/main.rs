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

use mush_core::{session, Config, Provider, Session, UserConfig, Workspace};

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
}

fn parse_args() -> Result<Args, String> {
    let mut dir: Option<PathBuf> = None;
    let mut url = None;
    let mut model = None;
    let mut provider = None;
    let mut args = std::env::args().skip(1);

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
            "--url" => url = Some(args.next().ok_or("--url needs a value")?),
            "--model" => model = Some(args.next().ok_or("--model needs a value")?),
            "--provider" => provider = Some(args.next().ok_or("--provider needs a value")?),
            other => dir = Some(PathBuf::from(other)),
        }
    }

    Ok(Args {
        dir: dir.unwrap_or_else(|| PathBuf::from(".")),
        url,
        model,
        provider,
    })
}

fn print_help() {
    println!(
        "mush {}\n\
         A small, fast, agent-agnostic terminal editor.\n\n\
         USAGE:\n    mush [DIRECTORY] [--url URL] [--model NAME] [--provider NAME]\n\n\
         OPTIONS:\n\
         \x20   --url URL      OpenAI-compatible endpoint (default: $MUSH_URL or the provider default)\n\
         \x20   --model NAME   Model id (default: $MUSH_MODEL, else auto-detected)\n\
         \x20   --provider     deepseek or custom (default: $MUSH_PROVIDER or custom)\n\n\
         KEYS:\n\
         \x20   Tab / Shift-Tab   cycle panes (agents, editor, chat)\n\
         \x20   Enter             send message (chat) · focus agent (agents)\n\
         \x20   i / Esc           enter insert / leave insert (editor)\n\
         \x20   Ctrl-P            model picker\n\
         \x20   Ctrl-S            save        Ctrl-R  reload file\n\
         \x20   Ctrl-N            new chat    Ctrl-C  cancel all agents\n\
         \x20   Ctrl-Q            quit (twice if there are unsaved changes)\n\n\
         COMMANDS (type in the chat):\n\
         \x20   /provider [deepseek|custom]  switch provider\n\
         \x20   /model                       pick a model\n\
         \x20   /url http://host:port        set the endpoint\n\
         \x20   /key <secret>                set the API key (saved to the home config)\n\
         \x20   /models                      refresh the model list\n\
         \x20   /open <path>                 open a file in the editor\n\
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

    // `mush src/main.rs` opens that file; `mush dir/` is a workspace.
    let mut dir = args.dir;
    let mut open_file: Option<String> = None;
    if dir.is_file() {
        open_file = dir.file_name().map(|name| name.to_string_lossy().into_owned());
        dir = dir.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    }

    let workspace = Workspace::new(&dir)?;
    session::ensure_mush_dir(workspace.root())?;

    // Resolution order: CLI flags > environment > saved session > home config
    // > built-in defaults. The API key comes from env or the home config.
    let env_url = std::env::var("MUSH_URL").ok().filter(|s| !s.is_empty());
    let env_provider = std::env::var("MUSH_PROVIDER").ok().filter(|s| !s.is_empty());

    let mut config = Config::from_env();
    if let Some(url) = args.url.as_deref() {
        config.base_url = url.trim_end_matches('/').to_string();
    }
    if let Some(model) = args.model.as_deref() {
        config.model = model.to_string();
    }
    if let Some(provider) = args.provider.as_deref() {
        config.provider = Provider::parse(provider)
            .ok_or_else(|| format!("unknown provider `{provider}` (try deepseek or custom)"))?;
    }

    // Home config: machine-global defaults (this is where the API key lives).
    let home = UserConfig::load();
    if config.api_key.is_none() {
        config.api_key = home.api_key.clone();
    }
    if args.provider.is_none() && env_provider.is_none() {
        if let Some(provider) = Provider::parse(&home.provider) {
            config.provider = provider;
            if args.url.is_none() && env_url.is_none() && home.base_url.is_empty() {
                config.base_url = provider.default_base_url().to_string();
            }
        }
    }
    if args.url.is_none() && env_url.is_none() && !home.base_url.is_empty() {
        config.base_url = home.base_url;
    }
    if args.model.is_none() && config.model.is_empty() && !home.model.is_empty() {
        config.model = home.model;
    }

    // Saved session: the workspace's last runtime choice, beats home defaults.
    let stored = Session::load(workspace.root());
    if let Some(session) = stored.as_ref() {
        if args.url.is_none() && env_url.is_none() && !session.base_url.is_empty() {
            config.base_url = session.base_url.clone();
        }
        if args.provider.is_none() && env_provider.is_none() && !session.provider.is_empty() {
            if let Some(provider) = Provider::parse(&session.provider) {
                config.provider = provider;
            }
        }
        if args.model.is_none() && config.model.is_empty() && !session.model.is_empty() {
            config.model = session.model.clone();
        }
    }
    if config.model.is_empty() {
        match discover_model(&config) {
            Some(model) => config.model = model,
            None => eprintln!(
                "mush: no model given and none discovered at {} — pick one with /model",
                config.models_url()
            ),
        }
    }

    // Ask any reachable endpoint; the picker falls back to the provider's
    // built-in model list when that fails.
    let models = http::list_models(&config);

    let (tx, rx) = unbounded::<Msg>();
    let (root_tx, shared_cfg) = agent::spawn(
        config.clone(),
        tx.clone(),
        workspace.root().to_path_buf(),
    );
    let mut app = App::new(workspace, config, stored, shared_cfg, root_tx, models, open_file);

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
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Release {
                    app.update(Msg::Key(key));
                }
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

fn discover_model(config: &Config) -> Option<String> {
    // The endpoint's own list is authoritative; the provider's known models are
    // the fallback (e.g. deepseek picks deepseek-flash when offline or keyless).
    http::list_models(config).into_iter().next()
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