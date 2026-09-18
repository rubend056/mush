//! The slash commands: one parse, and one table everything reads.
//!
//! A typed line becomes a value — [`Command`] with its arguments already read —
//! and a line that is not a command becomes an error value rather than a string
//! comparison buried in a `match` arm. Parsing is pure, so `/context 240000`
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
//! | `/context [TOKENS]` | optional, a positive count | show or set the window |
//! | `/url <url>` | required | point at another endpoint |
//! | `/key [SECRET]` | optional | show the key in use, or set one |
//! | `/models` | ignored | re-read the endpoint's model list |
//! | `/worktrees` | ignored | re-scan for leftover isolated worktrees; clears dead git entries |
//! | `/diff <id>` | required, an id | run the diff of its work against HEAD |
//! | `/merge <id>` | required, an id | merge it into HEAD and reclaim it |
//! | `/discard <id>` | required, an id | throw it away and reclaim it |
//! | `/forget <id>` | required, an id | drop the agent from this session |
//! | `/compact` | ignored | fold the focused conversation into a summary |
//! | `/notes` | ignored | read the notes the foot had no room for |
//! | `/new` [`/clear`] | ignored | start a new chat |
//! | `/help` [`/?`] | ignored | list the keys and the commands |
//! | `/quit` [`/q`] | ignored | leave mush |
//!
//! Anything else is an error value: an unknown slash, or a real command whose
//! argument does not read.

use std::fmt;

/// The three things `/diff`, `/merge` and `/discard` can do to one isolated
/// agent's work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Diff,
    Merge,
    Discard,
}

impl Verb {
    /// The spelling a human types. Every message about a verb is built from
    /// this, so the refusal cannot name a command the parser does not accept.
    pub const fn name(self) -> &'static str {
        match self {
            Verb::Diff => "/diff",
            Verb::Merge => "/merge",
            Verb::Discard => "/discard",
        }
    }

    /// What it does, in the help table's own words.
    const fn help(self) -> &'static str {
        match self {
            Verb::Diff => "show the diff of its work against HEAD",
            Verb::Merge => "merge its work into HEAD, and reclaim its worktree",
            Verb::Discard => "throw its work away, and reclaim its worktree",
        }
    }
}

/// A command, with its arguments read the way the executor will use them.
///
/// Arguments are typed rather than passed as text: `/context abc` is refused
/// here, where the rule is written, instead of in the arm that would have had
/// to parse it and report back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `/new`: a fresh chat, and the old conversation is gone.
    New,
    Quit,
    Help,
    /// `/model`: open the picker.
    Model,
    /// `/provider` with no argument: open the picker.
    Provider(Option<String>),
    /// `/context` with no argument: report the window in use.
    Context(Option<usize>),
    Url(String),
    /// `/key` with no argument: report the key in use.
    ApiKey(Option<String>),
    /// `/models`: re-read the endpoint's list.
    Models,
    Worktrees,
    Compact,
    /// `/notes`: read every note the foot had no room for.
    Notes,
    Worktree {
        verb: Verb,
        id: u64,
    },
    Forget(u64),
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

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::NotACommand => write!(f, "not a command"),
            CommandError::Unknown(name) => write!(f, "unknown command: {name}"),
            CommandError::Usage(line) => write!(f, "{line}"),
        }
    }
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

const fn worktree_spec(verb: Verb) -> Spec {
    Spec {
        name: verb.name(),
        aliases: &[],
        args: "<id>",
        help: verb.help(),
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
        name: "/context",
        aliases: &[],
        args: "[TOKENS]",
        help: "show or set the context window",
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
        name: "/worktrees",
        aliases: &[],
        args: "",
        help: "re-scan worktrees; clears git entries whose checkout is gone",
    },
    worktree_spec(Verb::Diff),
    worktree_spec(Verb::Merge),
    worktree_spec(Verb::Discard),
    Spec {
        name: "/forget",
        aliases: &[],
        args: "<id>",
        help: "drop the agent from this session (its branch stays)",
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
        help: "read the notes the foot had no room for",
    },
    Spec {
        name: "/new",
        aliases: &["/clear"],
        args: "",
        help: "start a new chat",
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

/// The command table as text, one aligned line per command: what the transcript
/// notice and `mush --help` both print. `providers` is the list `--provider`
/// accepts, already spelled the way a command line wants it.
pub fn table(providers: &str) -> String {
    let rows: Vec<(String, &str)> = COMMANDS
        .iter()
        .map(|spec| {
            let usage = format!("{} {}", spec.name, spec.args)
                .trim_end()
                .replace("{providers}", providers);
            (usage, spec.help)
        })
        .collect();
    let width = rows
        .iter()
        .map(|(usage, _)| usage.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(usage, help)| format!("    {usage:<width$}  {help}"))
        .collect::<Vec<_>>()
        .join("\n")
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
    // replaced did: `/new\tx` is a command nobody implements, not `/new`.
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
        "/new" => Command::New,
        "/quit" => Command::Quit,
        "/help" => Command::Help,
        "/model" => Command::Model,
        "/models" => Command::Models,
        "/worktrees" => Command::Worktrees,
        "/compact" => Command::Compact,
        "/notes" => Command::Notes,
        "/provider" => Command::Provider(optional(argument)),
        "/key" => Command::ApiKey(optional(argument)),
        "/url" if argument.is_empty() => {
            return Err(CommandError::Usage(
                "usage: /url http://host:port — base URL of an OpenAI-compatible endpoint"
                    .to_string(),
            ))
        }
        "/url" => Command::Url(argument.to_string()),
        // No argument asks for the window in use; a count that does not read,
        // or a window of zero, is the same complaint.
        "/context" if argument.is_empty() => Command::Context(None),
        "/context" => match argument.parse::<usize>() {
            Ok(tokens) if tokens > 0 => Command::Context(Some(tokens)),
            _ => return Err(CommandError::Usage("usage: /context <tokens>".to_string())),
        },
        "/diff" | "/merge" | "/discard" => {
            let verb = match spec.name {
                "/diff" => Verb::Diff,
                "/merge" => Verb::Merge,
                _ => Verb::Discard,
            };
            match argument.parse::<u64>() {
                Ok(id) => Command::Worktree { verb, id },
                Err(_) => {
                    return Err(CommandError::Usage(format!(
                        "usage: {} <agent id>",
                        verb.name()
                    )))
                }
            }
        }
        "/forget" => match argument.parse::<u64>() {
            Ok(id) => Command::Forget(id),
            Err(_) => return Err(CommandError::Usage("usage: /forget <agent id>".to_string())),
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
mod tests {
    use super::*;
    use crate::ui::HINT;

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
                panic!("`{line}` is in the table but does not parse: {error}")
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
        assert_eq!(parse_command("/clear"), Ok(Command::New));
        assert_eq!(parse_command("/q"), Ok(Command::Quit));
        assert_eq!(parse_command("/?"), Ok(Command::Help));
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
        // trailing newline still sends `/new`.
        assert_eq!(parse_command("  /new \n"), Ok(Command::New));
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

    /// Arguments are read here, once, the way the executor uses them: a count
    /// as a number, an id as an id, and the text of a secret as text.
    #[test]
    fn arguments_parse_the_way_the_executor_uses_them() {
        assert_eq!(
            parse_command("/context 240000"),
            Ok(Command::Context(Some(240_000)))
        );
        assert_eq!(parse_command("/context"), Ok(Command::Context(None)));
        assert_eq!(
            parse_command("/context seven"),
            Err(CommandError::Usage("usage: /context <tokens>".into()))
        );
        assert_eq!(
            parse_command("/context 0"),
            Err(CommandError::Usage("usage: /context <tokens>".into())),
            "a window of zero is not a window"
        );

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

        for verb in [Verb::Diff, Verb::Merge, Verb::Discard] {
            assert_eq!(
                parse_command(&format!("{} 7", verb.name())),
                Ok(Command::Worktree { verb, id: 7 })
            );
            assert_eq!(
                parse_command(verb.name()),
                Err(CommandError::Usage(format!(
                    "usage: {} <agent id>",
                    verb.name()
                )))
            );
            assert!(matches!(
                parse_command(&format!("{} seven", verb.name())),
                Err(CommandError::Usage(_))
            ));
        }

        assert_eq!(parse_command("/forget 12"), Ok(Command::Forget(12)));
        assert_eq!(
            parse_command("/forget"),
            Err(CommandError::Usage("usage: /forget <agent id>".into()))
        );

        // The commands that take nothing ignore what follows, as they always
        // have: `/new now` starts a new chat.
        assert_eq!(parse_command("/new now"), Ok(Command::New));
        assert_eq!(parse_command("/models all"), Ok(Command::Models));
        assert_eq!(parse_command("/compact harder"), Ok(Command::Compact));
        assert_eq!(parse_command("/worktrees again"), Ok(Command::Worktrees));
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
}
