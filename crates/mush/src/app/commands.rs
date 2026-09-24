//! The slash commands: one parse, and one table everything reads.
//!
//! A typed line becomes a value — [`Command`] with its arguments already read —
//! and a line that is not a command becomes an error value rather than a string
//! comparison buried in a `match` arm. Parsing is pure, so `/url http://…`
//! can be tested without an `App`.
//!
//! The invariant this module owns: **there is one table.** The parser's arms,
//! the `/help` notice in the transcript and `mush --help` all read
//! [`COMMANDS`], so a command cannot exist in one of them and be missing from
//! the others. It used to: `/compact` was implemented, listed by `/help`, and
//! absent from `--help`, because the three lists were written by hand in three
//! places (this is the help/status drift half of finding B2). The arguments
//! themselves are only ever read here — `App` matches on the variant and uses
//! the typed value — so a usage line cannot promise a shape the executor does
//! not accept.
//!
//! The whole list, spelled as a human types it (a name in brackets is an alias
//! the parser accepts and help does not advertise):
//!
//! | command | argument | what it does |
//! |---|---|---|
//! | `/provider [PROVIDERS]` | optional | switch provider, or open the picker |
//! | `/model` | ignored | open the model picker |
//! | `/url <url>` | required | point at another endpoint |
//! | `/key [SECRET]` | optional | show the key in use, or set one |
//! | `/models` | ignored | re-read the endpoint's model list |
//! | `/context [N|auto]` | optional | say the window and its road, state one, or re-derive |
//! | `/compact` | ignored | fold the focused conversation into a summary |
//! | `/notes` | ignored | read every note about the focused agent |
//! | `/glyphs [ascii\|symbols]` | optional | show the tool marks, or switch the rung |
//! | `/help` (`/?`) | ignored | list the keys and the commands |
//! | `/quit` (`/q`) | ignored | leave mush |
//!
//! Git is not a command surface: the tree names the branch and the worktree,
//! and `git` itself is the tool for acting on them. The worktree commands mush
//! used to wrap (`/diff`, `/merge`, `/discard`, `/forget`, `/worktrees`) are
//! gone, and so is `/new` (Ctrl-N). `/context` went with them once — the meter
//! is on screen — and is back because the meter paints the number, and only a
//! command can say which road the number came by.
//!
//! Anything else is an error value: an unknown slash, or a real command whose
//! argument does not read.

use crate::app::symbols::Symbols;

/// A command, with its arguments read the way the executor will use them.
///
/// Arguments are typed rather than passed as text: `/url` with no argument is
/// refused here, where the rule is written, instead of in the arm that would
/// have had to parse it and report back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Quit,
    Help,
    /// `/model`: open the picker.
    Model,
    /// `/provider` with no argument: open the picker.
    Provider(Option<String>),
    Url(String),
    /// `/key` with no argument: report the key in use.
    ApiKey(Option<String>),
    /// `/models`: re-read the endpoint's list.
    Models,
    /// `/context`: say the window in force and the road it came by.
    Context(ContextArg),
    Compact,
    /// `/notes`: read every note about the focused agent, in full.
    ///
    /// Every one of them, not just the two the foot had room for: the picker
    /// lists the agent's whole set of notices, the ones the foot already showed
    /// included, because that is what the lines mush wrote about the agent are.
    Notes,
    /// `/glyphs`: the marks the panes paint, one row per tool, and — with an
    /// argument — the rung to paint them at.
    ///
    /// The argument is read here like every other ([`Symbols::parse`]) and the
    /// arm that carries it out moves a typed value, so the words the switch
    /// accepts and the words its refusal names cannot drift apart.
    Glyphs(Option<Symbols>),
}

/// `/context`'s argument: what the human asked of the window.
///
/// Read here like every other argument, so the arm that carries it out matches
/// a value rather than parsing text a second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextArg {
    /// No argument: say the window in force and the road it came by.
    Report,
    /// A token count, read by [`mush_core::config::parse_context`]: the one
    /// number reader `--context`, `MUSH_CONTEXT` and this command share, so
    /// one typo cannot be answered two ways by three doors.
    State(usize),
    /// `auto`: drop this workspace's statement, and derive the window from the
    /// model table again.
    Auto,
}

/// Why a typed line is not a command to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// Not a slash command: it is a message for the agent, and only the caller
    /// knows which agent that is.
    NotACommand,
    /// A slash nobody implements.
    Unknown(String),
    /// A real command whose argument is missing or does not read. The string is
    /// the whole line to show a human, spelled with the command that produced
    /// it, so the usage text lives beside the rule rather than in the arm.
    Usage(String),
}

/// One slash command: how it is spelled, what it takes, and the line of help.
pub struct Spec {
    /// The canonical spelling, with the slash. This is the one `/help` and
    /// `mush --help` print.
    pub name: &'static str,
    /// Other spellings the parser accepts. Help shows the canonical name only:
    /// a human has to know the command exists, not that it has a shorthand.
    pub aliases: &'static [&'static str],
    /// The arguments as a human writes them, for the help column: `<…>` is
    /// required, `[…]` is optional. The provider list is the one argument this
    /// table cannot spell itself — it belongs to `mush_core::provider` — so
    /// `/provider` writes `{providers}` and [`table`] fills it in. The tests
    /// read the same convention, so a command whose argument stops being
    /// required cannot leave the help saying it is.
    pub args: &'static str,
    /// One line, for the help column.
    pub help: &'static str,
}

impl Spec {
    /// Whether a name a human typed is this command: its own spelling, or one
    /// of its shorthands. The parser asks this and then matches on
    /// [`Spec::name`], so a shorthand is a row's second spelling rather than a
    /// second `match` arm that can be forgotten.
    fn matches(&self, typed: &str) -> bool {
        self.name == typed || self.aliases.contains(&typed)
    }
}

/// Every command, in the order a human should read them.
///
/// A new command is one row here, one arm in [`parse_command`], and one arm in
/// `App::apply_command`; everything a human reads comes from the row. The test
/// `the_help_lists_exactly_the_commands_the_parser_knows` walks the table in
/// both directions, so forgetting the row is a failing test rather than a
/// command nobody can find.
pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "/provider",
        aliases: &[],
        args: "[{providers}]",
        help: "switch provider, or pick one from a list",
    },
    Spec {
        name: "/model",
        aliases: &[],
        args: "",
        help: "pick a model from the endpoint's list",
    },
    Spec {
        name: "/url",
        aliases: &[],
        args: "<url>",
        help: "point at another OpenAI-compatible endpoint",
    },
    Spec {
        name: "/key",
        aliases: &[],
        args: "[SECRET]",
        help: "show the API key in use, or set one (saved to the home config)",
    },
    Spec {
        name: "/models",
        aliases: &[],
        args: "",
        help: "refresh the model list from the endpoint",
    },
    Spec {
        name: "/context",
        aliases: &[],
        args: "[N|auto]",
        help: "say the window's size and road, state one, or auto for the table",
    },
    Spec {
        name: "/compact",
        aliases: &[],
        args: "",
        help: "fold the focused agent's conversation into a summary",
    },
    Spec {
        name: "/notes",
        aliases: &[],
        args: "",
        help: "read every note about the focused agent, in full",
    },
    Spec {
        name: "/glyphs",
        aliases: &[],
        args: "[ascii|symbols]",
        help: "show the mark each tool wears, or paint them in ascii",
    },
    Spec {
        name: "/help",
        aliases: &["/?"],
        args: "",
        help: "list the keys and these commands",
    },
    Spec {
        name: "/quit",
        aliases: &["/q"],
        args: "",
        help: "leave mush",
    },
];

/// The command table as text, one aligned line per command: what `mush --help`
/// prints. `providers` is the list `--provider` accepts, already spelled the
/// way a command line wants it.
pub fn table(providers: &str) -> String {
    table_at(providers, usize::MAX)
}

/// The same table, rendered for a surface `width` columns wide: the usage stays
/// in its column and a description that does not fit hangs under its own
/// column — or, when that column would be narrower than
/// [`mush_core::text::MIN_DESCRIPTION_COLUMNS`], under its own usage row,
/// wrapped at the surface's whole width (finding D23). `usize::MAX` is the
/// unwrapped form `--help` prints.
pub fn table_at(providers: &str, width: usize) -> String {
    let rows: Vec<(String, &str)> = COMMANDS
        .iter()
        .map(|spec| {
            let usage = format!("{} {}", spec.name, spec.args)
                .trim_end()
                .replace("{providers}", providers);
            (usage, spec.help)
        })
        .collect();
    let usage_width = rows
        .iter()
        .map(|(usage, _)| usage.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (usage, help) in &rows {
        out.push_str(&mush_core::text::columns(usage, usage_width, help, width));
    }
    out.trim_end().to_string()
}

/// Read a typed line.
///
/// The slash is the whole rule: a line that does not start with one is a
/// message for the agent, and mush never guesses which of the two a human
/// meant. Everything else about the line — which command, how many characters
/// of argument, and whether that argument reads — is decided here.
pub fn parse_command(line: &str) -> Result<Command, CommandError> {
    // Leading and trailing whitespace is the terminal's, not the command's; a
    // send already trims, and a paste into the box may not have.
    let line = line.trim();
    let Some(body) = line.strip_prefix('/') else {
        return Err(CommandError::NotACommand);
    };
    // Only a space separates name from argument, exactly as the arms this
    // replaced did: `/help\tx` is a command nobody implements, not `/help`.
    let (name, argument) = match body.split_once(' ') {
        Some((name, rest)) => (name, rest.trim()),
        None => (body, ""),
    };

    // Which names exist is the table's answer, not this function's: an unknown
    // command is an error value carrying what was typed, and the arms below
    // speak only the canonical spelling.
    let typed = format!("/{name}");
    let Some(spec) = COMMANDS.iter().find(|spec| spec.matches(&typed)) else {
        return Err(CommandError::Unknown(typed));
    };

    let command = match spec.name {
        "/quit" => Command::Quit,
        "/help" => Command::Help,
        "/model" => Command::Model,
        "/models" => Command::Models,
        "/context" if argument.is_empty() => Command::Context(ContextArg::Report),
        // `auto` is a word rather than a number, read letter-blind the way
        // `--thinking on` and `/provider` read theirs, so `AUTO` is the same
        // ask; a word mush does not know falls to the number reader's refusal
        // (which names the road) instead of being guessed at.
        "/context" if argument.eq_ignore_ascii_case("auto") => Command::Context(ContextArg::Auto),
        // The number is read by the one reader `--context` and `MUSH_CONTEXT`
        // use: a value that does not read is refused with the sentence naming
        // the road that carried it, and the arm carries out a typed value
        // rather than parsing text a second time.
        "/context" => match mush_core::config::parse_context(argument, "/context") {
            Ok(tokens) => Command::Context(ContextArg::State(tokens)),
            Err(error) => return Err(CommandError::Usage(error)),
        },
        "/compact" => Command::Compact,
        "/notes" => Command::Notes,
        // No argument reads the table; a word switches the rung. The reading is
        // [`Symbols::parse`], the one place the words are spelled, and a word
        // mush does not know is refused with the two it takes — a typo answered
        // by silence is a switch the human thinks they made.
        "/glyphs" => match argument {
            "" => Command::Glyphs(None),
            word => match Symbols::parse(word) {
                Some(symbols) => Command::Glyphs(Some(symbols)),
                None => {
                    return Err(CommandError::Usage(format!(
                        "/glyphs {word} — the rungs are ascii and symbols"
                    )))
                }
            },
        },
        "/provider" => Command::Provider(optional(argument)),
        // A key is written into the request head raw, so a control character
        // in one is a header line of its own (finding C7). The door is here,
        // where the human typed it: the refusal names the command, and only
        // the character as its escape — the key's own bytes are a secret and
        // must not be printed.
        "/key" => match optional(argument) {
            Some(key) => match mush_core::config::checked_key(&key, "/key") {
                Ok(key) => Command::ApiKey(Some(key)),
                Err(error) => return Err(CommandError::Usage(error)),
            },
            None => Command::ApiKey(None),
        },
        "/url" if argument.is_empty() => {
            return Err(CommandError::Usage(
                "usage: /url http://host:port — base URL of an OpenAI-compatible endpoint"
                    .to_string(),
            ))
        }
        // An endpoint is written into the request line raw, so a control
        // character in one is a request the value writes for itself (finding
        // D19). The arm routes through the same door every other road takes,
        // so the refusal is spoken where the human typed it instead of being
        // dropped by `set_base_url` with an ack for the endpoint in force.
        "/url" => match mush_core::config::checked_url(argument, "/url") {
            Ok(url) => Command::Url(url),
            Err(error) => return Err(CommandError::Usage(error)),
        },
        // A row with no arm here: `every_command_in_the_table_parses` fails
        // before this can happen, and the honest answer to a name that somehow
        // got here is still the error the lookup would have given.
        _ => return Err(CommandError::Unknown(typed)),
    };
    Ok(command)
}

/// An optional argument, told apart from an empty one: `/provider  ` asks for
/// the picker, it does not switch to a provider named "".
fn optional(argument: &str) -> Option<String> {
    (!argument.is_empty()).then(|| argument.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders};
    use unicode_width::UnicodeWidthStr;

    use crate::agent::{CallFacts, CallOutcome, Measure, Tone};
    use crate::app::call_grid;
    use crate::app::screen::{AgentsPane, BarPane, ChatPane, InputPane, Panes, Screen};
    use crate::app::symbols::Symbols;
    use crate::app::{AgentId, AgentRow, Focus, Painted, Rank};
    use crate::ui::{dim, HINT};
    use mush_core::message::{FunctionCall, ToolCall};

    /// One typed line for a command: with an argument when the table says one
    /// is required (`<…>`, not the `[…]` of an optional one). Every command's
    /// parser rule is exercised through this, so a row with no arm — or an arm
    /// with a stricter idea of its argument than the table has — fails here.
    fn typed(spec: &Spec) -> String {
        if spec.args.starts_with('<') {
            format!("{} 1", spec.name)
        } else {
            spec.name.to_string()
        }
    }

    /// Every command in the table parses. A row is not documentation: it is the
    /// one place the command is declared, so a row the parser does not know is
    /// a command that cannot be typed.
    #[test]
    fn every_command_in_the_table_parses() {
        for spec in COMMANDS {
            let line = typed(spec);
            parse_command(&line).unwrap_or_else(|error| {
                panic!("`{line}` is in the table but does not parse: {error:?}")
            });
        }
    }

    /// The help a human reads and the commands the parser knows are the same
    /// set, in both directions: nothing advertised that cannot be typed, and
    /// nothing typed that cannot be found.
    #[test]
    fn the_help_lists_exactly_the_commands_the_parser_knows() {
        let help = table("a|b");
        for spec in COMMANDS {
            assert!(
                help.contains(spec.name),
                "{} is implemented but missing from the help:\n{help}",
                spec.name
            );
        }

        // And the other way: every `/word` the help prints is a command. The
        // parser resolves a name through this same table, so a name that is not
        // a row (or an alias of one) is one no human could type.
        for word in help.split_whitespace() {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/');
            if !word.starts_with('/') {
                continue;
            }
            assert!(
                COMMANDS.iter().any(|spec| spec.matches(word)),
                "`{word}` is advertised but is not a command"
            );
        }
    }

    /// A shorthand is the same command: the parser accepts it, and help does
    /// not have to list it twice.
    #[test]
    fn an_alias_is_its_command() {
        for spec in COMMANDS {
            for alias in spec.aliases {
                assert_eq!(
                    parse_command(alias),
                    parse_command(spec.name),
                    "{alias} is not {}\n",
                    spec.name
                );
            }
        }
        assert_eq!(parse_command("/q"), Ok(Command::Quit));
        assert_eq!(parse_command("/?"), Ok(Command::Help));
    }

    /// `/glyphs` reads its one word here, once: no argument is the table, and a
    /// word that is not a rung is refused with the two that are. Silence would
    /// be a switch the human thinks they made.
    #[test]
    fn the_glyphs_argument_is_a_rung() {
        assert_eq!(parse_command("/glyphs"), Ok(Command::Glyphs(None)));
        assert_eq!(
            parse_command("/glyphs ascii"),
            Ok(Command::Glyphs(Some(Symbols::ASCII)))
        );
        assert_eq!(
            parse_command("/glyphs symbols"),
            Ok(Command::Glyphs(Some(Symbols::SYMBOLS)))
        );
        assert_eq!(
            parse_command("/glyphs ASCII"),
            Ok(Command::Glyphs(Some(Symbols::ASCII))),
            "a word is read letter-blind, like every other argument"
        );
        let Err(CommandError::Usage(line)) = parse_command("/glyphs emoji") else {
            panic!("an unknown rung is refused");
        };
        assert!(
            line.contains("emoji") && line.contains("ascii") && line.contains("symbols"),
            "the refusal names the word and the rungs: {line:?}"
        );
    }

    /// A line that is not a slash command is a message, and mush says so
    /// instead of comparing strings further down: `/` alone, a word, an empty
    /// line, and a sentence that merely contains a slash.
    #[test]
    fn a_message_is_not_a_command() {
        assert_eq!(parse_command("hello"), Err(CommandError::NotACommand));
        assert_eq!(parse_command(""), Err(CommandError::NotACommand));
        assert_eq!(parse_command("   "), Err(CommandError::NotACommand));
        assert_eq!(
            parse_command("what does /help do?"),
            Err(CommandError::NotACommand)
        );
        // The terminal's whitespace, not the command's: a box that kept a
        // trailing newline still sends `/quit`.
        assert_eq!(parse_command("  /quit \n"), Ok(Command::Quit));
    }

    /// An unknown command is an error value carrying what was typed, so the
    /// caller prints one line and the arm that would have compared strings is
    /// gone.
    #[test]
    fn an_unknown_command_is_an_error() {
        assert_eq!(
            parse_command("/frobnicate"),
            Err(CommandError::Unknown("/frobnicate".to_string()))
        );
        assert_eq!(
            parse_command("/frobnicate now"),
            Err(CommandError::Unknown("/frobnicate".to_string())),
            "the argument is not part of the name"
        );
        assert_eq!(
            parse_command("/"),
            Err(CommandError::Unknown("/".to_string())),
            "a bare slash is a command nobody has"
        );
        assert_eq!(
            parse_command("/New"),
            Err(CommandError::Unknown("/New".to_string())),
            "commands are lower case"
        );
    }

    /// Arguments are read here, once, the way the executor uses them: a url as
    /// a url, and the text of a secret as text.
    #[test]
    fn arguments_parse_the_way_the_executor_uses_them() {
        assert_eq!(
            parse_command("/provider ollama"),
            Ok(Command::Provider(Some("ollama".to_string())))
        );
        assert_eq!(parse_command("/provider"), Ok(Command::Provider(None)));

        // A secret may contain spaces; only the edges are trimmed.
        assert_eq!(
            parse_command("/key  sk-abc def  "),
            Ok(Command::ApiKey(Some("sk-abc def".to_string())))
        );
        assert_eq!(parse_command("/key"), Ok(Command::ApiKey(None)));

        assert_eq!(
            parse_command("/url http://127.0.0.1:8078"),
            Ok(Command::Url("http://127.0.0.1:8078".to_string()))
        );
        assert!(matches!(
            parse_command("/url"),
            Err(CommandError::Usage(line)) if line.starts_with("usage: /url http")
        ));

        // The commands that take nothing ignore what follows, as they always
        // have: `/quit now` is still `/quit`.
        assert_eq!(parse_command("/quit now"), Ok(Command::Quit));
        assert_eq!(parse_command("/models all"), Ok(Command::Models));
        assert_eq!(parse_command("/compact harder"), Ok(Command::Compact));
        assert_eq!(parse_command("/notes please"), Ok(Command::Notes));
        assert_eq!(
            parse_command("/context"),
            Ok(Command::Context(ContextArg::Report))
        );
        assert_eq!(
            parse_command("/context 32768"),
            Ok(Command::Context(ContextArg::State(32_768)))
        );
        assert_eq!(
            parse_command("/context auto"),
            Ok(Command::Context(ContextArg::Auto))
        );
        assert_eq!(
            parse_command("/context AUTO"),
            Ok(Command::Context(ContextArg::Auto)),
            "a word is read letter-blind, as /provider's and --thinking's are"
        );
    }

    /// `/key`'s secret is written into the request head raw, so a control
    /// character in one is a header line of its own (finding C7). The door is
    /// here, where the human typed it: the refusal names the command, and only
    /// the character as its escape — the key's own bytes are a secret and never
    /// reach the sentence.
    #[test]
    fn a_key_with_a_control_character_is_refused_by_name() {
        assert_eq!(
            parse_command("/key sk-a\r\nb"),
            Err(CommandError::Usage(
                "/key contains a control character (\\r) — check the value".to_string()
            ))
        );
        // A key without one still parses, and the newline a pasted block trails
        // is the line's own trim.
        assert_eq!(
            parse_command("/key sk-ok\n"),
            Ok(Command::ApiKey(Some("sk-ok".to_string())))
        );
        // No argument still reports the key in use.
        assert_eq!(parse_command("/key"), Ok(Command::ApiKey(None)));
    }

    /// The TUI's `/url` door (the residual `4f7793c` left): the endpoint is
    /// written into the request line raw, so a control character in one is a
    /// request the value writes for itself (finding D19). Routing the arm
    /// through the same door every other road takes refuses it where the human
    /// typed it, instead of `set_base_url` dropping it silently behind an ack
    /// that names the endpoint actually in force.
    #[test]
    fn a_url_with_a_control_character_is_refused_by_name() {
        let error = parse_command("/url http://h:1\r\nX: y").unwrap_err();
        assert!(
            matches!(
                &error,
                CommandError::Usage(line) if line.contains("contains a control character (\\r)")
            ),
            "{error:?}"
        );

        // A good URL still parses — and a trailing slash is trimmed by the same
        // door the flag goes through.
        assert_eq!(
            parse_command("/url http://h:1"),
            Ok(Command::Url("http://h:1".to_string()))
        );
        // No argument is still the usage line.
        assert!(matches!(
            parse_command("/url"),
            Err(CommandError::Usage(line)) if line.starts_with("usage: /url http")
        ));
    }

    /// `/context`'s number is read by [`mush_core::config::parse_context`] — the
    /// one reader `--context` and `MUSH_CONTEXT` use — so the three doors refuse
    /// one typo with one sentence, naming the road that carried the value.
    /// `main.rs`'s `the_context_flag_and_the_variable_read_one_number_one_way`
    /// pins the other two doors.
    #[test]
    fn a_context_argument_that_is_not_a_number_is_refused_by_name() {
        assert_eq!(
            parse_command("/context 8k"),
            Err(CommandError::Usage(
                "/context needs a token count, got `8k`".to_string()
            ))
        );
        assert_eq!(
            parse_command("/context 0"),
            Err(CommandError::Usage(
                "/context needs a token count, got `0`".to_string()
            )),
            "a window of no tokens is not a window"
        );
        // The same number the flag reads, read the same way: the line's own
        // trim is the box's, and the reader's trim is the same one the flag
        // and the variable go through.
        assert_eq!(
            parse_command("/context  8192"),
            Ok(Command::Context(ContextArg::State(8_192)))
        );
    }

    /// The table renders with the provider list filled in, and nothing left
    /// unsubstituted: `--help` printing `{providers}` at a human would be worse
    /// than not printing the list at all.
    #[test]
    fn the_provider_list_is_filled_into_the_table() {
        let table = table("deepseek|custom");
        assert!(
            table.contains("/provider [deepseek|custom]"),
            "the provider list is the argument a human needs:\n{table}"
        );
        assert!(!table.contains('{'), "{table}");
        assert!(!table.contains('}'), "{table}");
        // One line per command, and every help text starts in the same
        // column, whatever the arguments above it are.
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), COMMANDS.len(), "one line per command");
        let columns: Vec<usize> = COMMANDS
            .iter()
            .zip(&lines)
            .map(|(spec, line)| line.find(spec.help).expect("its own help text"))
            .collect();
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "the help column is one column: {columns:?}"
        );
    }

    /// The bar's hint names a command a human can then type. It is the one
    /// piece of status-bar text that advertises a command, so it reads the
    /// table rather than a copy of it.
    #[test]
    fn the_bar_hint_names_only_real_commands() {
        for word in HINT.split_whitespace() {
            if !word.starts_with('/') {
                continue;
            }
            assert!(
                parse_command(word).is_ok(),
                "the bar advertises `{word}`, which is not a command"
            );
        }
    }

    /// The `/help` table, exactly as `mush --help` prints it, in the manual's
    /// `commands` block — and on the front page, which carries the same block.
    /// One table behind both surfaces, and a failing test while a block
    /// disagrees with it.
    #[test]
    fn the_commands_block_matches_the_code() {
        let rendered = format!("```\n{}\n```", table(&mush_core::provider::names_piped()));
        for file in ["docs/mush.md", "README.md"] {
            crate::ui::tests::doc_block(
                file,
                "commands",
                "app::commands::tests::the_commands_block_matches_the_code",
                &rendered,
            );
        }
    }

    /// The columns a sample frame's transcript rows are painted at: the
    /// 100-column frame less the 34-column tree `agents_columns` gives it and
    /// the two columns the chat pane's own border takes. [`sample_screen`] lays
    /// the frame out at the same figures, so a sample row is the row the pane
    /// paints in that frame.
    const TRANSCRIPT_WIDTH: usize = 100 - 34 - 2;

    /// One marked line at the transcript's width, wrapped the way `chat`'s
    /// `marked` wraps it: the mark leads the first row and its own width of
    /// blank leads every wrap under it. The pane's replies, its notices and a
    /// result's payload are all this shape, so a sample paints through it
    /// rather than writing indents by hand.
    fn marked_rows(mark: &str, mark_style: Style, body: Style, text: &str) -> Vec<Line<'static>> {
        let lead = UnicodeWidthStr::width(mark);
        mush_core::text::wrap_text(text, TRANSCRIPT_WIDTH.saturating_sub(lead))
            .into_iter()
            .enumerate()
            .map(|(at, line)| {
                let head = if at == 0 {
                    Span::styled(mark.to_string(), mark_style)
                } else {
                    Span::raw(" ".repeat(lead))
                };
                Line::from(vec![head, Span::styled(line, body)])
            })
            .collect()
    }

    /// A voice's rows ([`crate::app::chat`]'s `marked`): the mark in its own
    /// style, the words plain. The human's line, a reply and a notice all wear
    /// this shape.
    fn spoken(mark: &str, style: Style, text: &str) -> Vec<Line<'static>> {
        marked_rows(mark, style, Style::default(), text)
    }

    /// One block painted in one colour throughout, the shape a tool result's
    /// payload wears: mush's own gutter ([`Symbols::GUTTER_MARK`]) leads every
    /// row — the first one *is* the gutter ([`crate::app::chat`]'s `Head::block`)
    /// and each wrapped row under it wears the same pipe, so a dump cannot be
    /// read as prose and a wrapped line hangs visibly under the row it
    /// continues — and the result's words are the style.
    fn payload(mark: &str, style: Style, text: &str) -> Vec<Line<'static>> {
        let lead = UnicodeWidthStr::width(mark);
        mush_core::text::wrap_text(text, TRANSCRIPT_WIDTH.saturating_sub(lead))
            .into_iter()
            .map(|line| {
                Line::from(vec![
                    Span::styled(mark.to_string(), style),
                    Span::styled(line, style),
                ])
            })
            .collect()
    }

    /// One tool call's rows as the pane paints them: the digest header and the
    /// result's detail rows through [`call_grid`], the one grid both views use,
    /// with the tool's own mark ([`Symbols`]). The ask, the verdict, the measure
    /// and the details are the fixture's words, but the mark, the columns and
    /// the arrow's place are the pane's arithmetic — a sample cannot show a row
    /// the grid would not paint.
    fn call_rows(
        name: &str,
        ask: &str,
        outcome: &str,
        tone: Tone,
        measure: Option<(&str, Option<&str>)>,
        details: &[&str],
    ) -> Vec<Line<'static>> {
        let call = ToolCall {
            id: "call".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        };
        let facts = CallFacts {
            ask: ask.to_string(),
            outcome: Some(CallOutcome {
                text: outcome.to_string(),
                tone,
            }),
            measure: measure.map(|(count, size)| Measure {
                count: Some(count.to_string()),
                size: size.map(str::to_string),
            }),
            details: details.iter().map(|row| row.to_string()).collect(),
        };
        let mark = Symbols::SYMBOLS.mark(name);
        let mut rows = call_grid::header(&call, &facts, TRANSCRIPT_WIDTH, mark);
        rows.extend(call_grid::details(&facts, TRANSCRIPT_WIDTH, mark));
        rows
    }

    /// One tree row, with its fields already chosen the way `App::rows`
    /// derives them. A sample frame's words are a fixture's, so no clock and no
    /// endpoint can move them.
    fn row(
        id: u64,
        depth: usize,
        glyph: &'static str,
        title: &str,
        place: &str,
        activity: &str,
    ) -> AgentRow {
        AgentRow {
            id: AgentId(id),
            depth,
            parent_gone: false,
            glyph,
            focused: false,
            result_unread: false,
            unread_children: 0,
            title: title.to_string(),
            place: place.to_string(),
            activity: activity.to_string(),
        }
    }

    /// A sample frame's `Screen` at 100×28: the layout `App::screen` works out
    /// at that size — a 34-column tree, the chat beside it, a two-row bar —
    /// with a fixture's words. The trees' title is elided by `screen::elide`,
    /// the function the pane's own title goes through, and the footer's detail
    /// line through `mush_core::text::truncate`, so the fixture cannot show a
    /// title or a line the pane would have cut.
    ///
    /// It lives in this module because a message box's `InputPane` can only be
    /// named inside `app` (`app::screen` is private), and a sample showing a box
    /// no `App` ever paints would be a picture of something else; the checks
    /// that paint and compare the frames live in `ui::tests` with the painter.
    fn sample_screen(
        title_cells: &[&str],
        rows: Vec<AgentRow>,
        cursor: usize,
        footer: Vec<Line<'static>>,
        transcript: Vec<Line<'static>>,
        word: Option<(Rank, String)>,
        facts: &str,
    ) -> Screen {
        const WIDTH: u16 = 100;
        const HEIGHT: u16 = 28;
        // The arithmetic of `App::screen` at 100×28: `bar_rows(28) == 2`,
        // `agents_columns(100) == 34` (the share clamped to its floor), a
        // three-row message box, and the transcript taking the rest.
        let agents_area = Rect::new(0, 0, 34, HEIGHT - 2);
        let chat_area = Rect::new(34, 0, WIDTH - 34, HEIGHT - 2);
        let inner = Block::default().borders(Borders::ALL).inner(agents_area);
        let cells: Vec<String> = title_cells.iter().map(|cell| cell.to_string()).collect();
        let title = crate::app::screen::elide(
            &cells,
            " · ",
            " agents · ",
            " agents ",
            inner.width as usize,
        );
        let footer_rows = if footer.is_empty() {
            0
        } else {
            footer.len() as u16 + 1
        };
        let input_area = Rect {
            y: chat_area.y + chat_area.height - 3,
            height: 3,
            ..chat_area
        };
        Screen::Panes(Box::new(Panes {
            agents: AgentsPane {
                area: agents_area,
                list_area: Rect {
                    height: inner.height - footer_rows,
                    ..inner
                },
                title,
                rows,
                cursor,
                footer,
            },
            chat: ChatPane {
                transcript_area: Rect {
                    height: chat_area.height - input_area.height,
                    ..chat_area
                },
                input_area,
                transcript: Some(Painted {
                    lines: transcript,
                    title: " mush ".to_string(),
                    select: None,
                }),
                input: Some(InputPane {
                    prompt: "› ".to_string(),
                    attachments: Vec::new(),
                    attachment_count: 0,
                    lines: vec![String::new()],
                    cursor_row: 0,
                    column: 0,
                }),
            },
            bar: BarPane {
                area: Rect::new(0, HEIGHT - 2, WIDTH, 2),
                word,
                facts: Some(facts.to_string()),
            },
            picker: None,
            focus: Focus::Chat,
        }))
    }

    /// The front page's sample: the root and one child working, one child done,
    /// a conversation in the chat, the hint under it and the facts.
    ///
    /// The transcript is the pane's own rows: every message paints its rows and
    /// then the blank `chat::closing_blank` closes it with, the calls are
    /// painted by the grid the pane uses ([`call_rows`]), their results at the
    /// gutter the pane indents them by ([`payload`]) — one call's own rows, with
    /// no blank between the header and the payload it belongs to — and the blank
    /// after the last message is the foot's to trim (`chat::body`) — which is
    /// why the foot's own row sits right under the reply.
    pub(crate) fn readme_sample_screen() -> Screen {
        let mut root = row(0, 0, "◐", "root", "", "thinking 4s");
        root.focused = true;
        let mut transcript: Vec<Line<'static>> = Vec::new();
        transcript.extend(spoken(
            "you › ",
            Style::default().fg(Color::Cyan),
            "rename the lexer module",
        ));
        transcript.push(Line::from(""));
        transcript.extend(spoken(
            "mush › ",
            Style::default().fg(Color::Green),
            "Starting with the rename.",
        ));
        // The prose breathes and the calls do not: the reply's blank stands
        // *between* its words and the call the same turn made, and the call's
        // own block follows with no blank of its own. The result below is that
        // call's payload — one block, so no blank stands between the header and
        // it.
        transcript.push(Line::from(""));
        transcript.extend(call_rows(
            "edit_file",
            "src/lex.rs",
            "3 hunks",
            Tone::Ok,
            None,
            &[],
        ));
        transcript.extend(payload(
            Symbols::GUTTER_MARK,
            dim(),
            "edited src/lex.rs — 3 edits",
        ));
        transcript.push(Line::from(""));
        transcript.extend(spoken("· ", dim(), "#2 done: wrote README.md"));
        transcript.push(Line::from(""));
        transcript.extend(spoken(
            "mush › ",
            Style::default().fg(Color::Green),
            "The tests are next.",
        ));
        transcript.extend(spoken("· ", dim(), "waiting on #1"));
        sample_screen(
            &["2 working", "Σ +12 −3"],
            vec![
                root,
                row(
                    1,
                    1,
                    "◐",
                    "lexer",
                    "mush/1 +12−3",
                    "edit_file src/lex.rs 3s",
                ),
                row(2, 1, "✓", "docs", "", "wrote README.md"),
            ],
            0,
            vec![
                Line::from(vec![
                    Span::styled(" #0 ", Style::default().fg(Color::Cyan)),
                    Span::raw("rename the lexer module"),
                ]),
                Line::from(Span::styled(
                    format!(
                        " {}",
                        mush_core::text::truncate(
                            "thinking 4s · .mush/wt/1 · git diff HEAD...mush/1",
                            30
                        )
                    ),
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            transcript,
            None,
            " ⌂ ~/p/demo │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k ~500k",
        )
    }

    /// §4.5's sample: one row per phase and one per row mark, so the picture
    /// says the same thing about the marks the `marks` block names. Its chat is
    /// the pane's own rows too ([`readme_sample_screen`]): a spawn and a wait,
    /// the wait's payload at the grid's gutter ([`payload`]), and the report
    /// rows the tree's phases were built from. The spawn's own report is not
    /// painted — the call's row and its detail row already say it — so a
    /// spawned child is one row.
    ///
    /// The child is spawned and its wait times out while it still works, which
    /// is the phase the tree's `◐ #2 tests` row shows: the chat is the same
    /// moment the tree is.
    pub(crate) fn manual_sample_screen() -> Screen {
        let mut root = row(0, 0, "◐", "root", "", "thinking 4s");
        root.focused = true;
        root.unread_children = 2;
        let mut orphan = row(8, 1, "✓", "orphan", "", "wrote src/lex.rs");
        orphan.parent_gone = true;
        let mut unread = row(7, 1, "✓", "docs", "", "wrote README.md");
        unread.result_unread = true;
        let mut transcript: Vec<Line<'static>> = Vec::new();
        transcript.extend(spoken(
            "you › ",
            Style::default().fg(Color::Cyan),
            "make the tree show every state",
        ));
        transcript.push(Line::from(""));
        transcript.extend(spoken(
            "mush › ",
            Style::default().fg(Color::Green),
            "Spawning the children.",
        ));
        transcript.push(Line::from(""));
        transcript.extend(call_rows(
            "spawn_agent",
            "tests probe",
            "#2 on mush/2",
            Tone::Running,
            None,
            &["mush/2 · .mush/wt/2"],
        ));
        // The spawn's own report is *not* painted: the call's row already says
        // `#2 on mush/2` and the detail row above names the worktree, so the
        // sentence would say the same fact twice over ([`crate::app::chat`]'s
        // `spawn_report` — one spawned child is one row). The wait below is a
        // turn of its own and its call rows are a dense list, so no blank
        // stands between the two calls; the wait's result closes its own block
        // with the blank under its payload.
        transcript.extend(call_rows(
            "wait",
            "",
            "#2 still running",
            Tone::Running,
            None,
            &[],
        ));
        transcript.extend(payload(
            Symbols::GUTTER_MARK,
            dim(),
            "wait timed out — #2 still running",
        ));
        transcript.push(Line::from(""));
        transcript.extend(spoken("· ", dim(), "#3 failed: no route to host"));
        transcript.push(Line::from(""));
        transcript.extend(spoken(
            "mush › ",
            Style::default().fg(Color::Green),
            "Every mark is on a row above.",
        ));
        sample_screen(
            &["3 working", "1 waiting", "Σ +324 −40"],
            vec![
                root,
                row(1, 1, "⧗", "lexer", "", "waiting on results 3s"),
                row(
                    2,
                    2,
                    "◐",
                    "tests",
                    "mush/2 +324−40 ⚙1",
                    "edit_file tests/lex.rs 3s",
                ),
                row(3, 1, "✗", "probe", "", "no route to host"),
                row(4, 1, "⊘", "run", "", "stopped · re-send to resume"),
                row(5, 1, "⚠", "build", "", "cut off · nothing committed"),
                row(6, 1, "≡", "fold", "", "compacting 2s"),
                unread,
                orphan,
            ],
            0,
            Vec::new(),
            transcript,
            Some((
                Rank::Said,
                "spawned #8 (orphan) — its parent was reaped".to_string(),
            )),
            " ⌂ ~/p/demo │ master ±3 +324−40 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k ~500k",
        )
    }
}
