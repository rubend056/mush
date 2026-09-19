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
//! | `/compact` | ignored | fold the focused conversation into a summary |
//! | `/notes` | ignored | read every note about the focused agent |
//! | `/help` (`/?`) | ignored | list the keys and the commands |
//! | `/quit` (`/q`) | ignored | leave mush |
//!
//! Git is not a command surface: the tree names the branch and the worktree,
//! and `git` itself is the tool for acting on them. The worktree commands mush
//! used to wrap (`/diff`, `/merge`, `/discard`, `/forget`, `/worktrees`) are
//! gone, and so is `/context` (the meter is on screen) and `/new` (Ctrl-N).
//!
//! Anything else is an error value: an unknown slash, or a real command whose
//! argument does not read.

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
    Compact,
    /// `/notes`: read every note about the focused agent, in full.
    ///
    /// Every one of them, not just the two the foot had room for: the picker
    /// lists the agent's whole set of notices, the ones the foot already showed
    /// included, because that is what the lines mush wrote about the agent are.
    Notes,
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
/// column (finding U15). `usize::MAX` is the unwrapped form `--help` prints.
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
    let description_column = 4 + usage_width + 2;
    let room = width.saturating_sub(description_column).max(1);
    let mut out = String::new();
    for (usage, help) in &rows {
        let lead = format!("    {usage:<usage_width$}  ");
        let mut wrapped = mush_core::text::wrap_text(help, room).into_iter();
        if let Some(first) = wrapped.next() {
            out.push_str(&lead);
            out.push_str(&first);
            out.push('\n');
        }
        for continuation in wrapped {
            out.push_str(&format!("{:description_column$}{continuation}\n", ""));
        }
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
