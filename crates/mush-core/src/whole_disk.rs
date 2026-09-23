//! The whole-disk walk guard: a command that would walk the filesystem from
//! `/` is refused before any shell starts.
//!
//! The report that put this here was not hypothetical. A sibling agent's
//! reconnaissance ran `find /` as a detached job and thrashed this machine's
//! disk for as long as the job was allowed to live. The human asked for a gate
//! that sees `find /` **even when a `cd` or a wrapper stands before it**. A
//! walk of the whole filesystem is almost never the question being asked — the
//! answer a model wants is inside the workspace, and `search`, `list_files` or
//! `find .` are the roads to it — and the one thing it reliably produces is
//! load on a disk every agent shares.
//!
//! The decision is [`refusal`]: a pure function of the command text and the
//! directory the command would start in, so every shape is testable without
//! spawning anything. `machine::Shell::spawn` is the one door it closes —
//! every `run_command`, every `detach: true` job and every handover to the
//! registry goes through that spawn — so no tool argument and no road can
//! skip it, and a refusal is the command's own result sentence the way a spawn
//! failure already is.
//!
//! # What the reading sees
//!
//! The command line is shell text, so this is a reading of that text and not a
//! shell:
//!
//! - The line is split on the separators that end a command (`;`, `&&`, `||`,
//!   `|`, `&`, newlines, unquoted `(` and `)`), and every piece is examined.
//!   Quoting is respected while splitting, so `echo "a; b"` stays one piece; a
//!   `#` where a word would begin opens a comment, skipped to the line's end;
//!   a heredoc's body (the lines after the next newline, up to its delimiter
//!   line) is data, not commands, and is skipped as such.
//! - The **effective cwd** is tracked across the pieces: the command starts in
//!   `cwd` (the workspace root, where `run_command` documents it runs) and
//!   every `cd <dir>` piece moves it. `..` is resolved lexically —
//!   `cd /tmp/x/../..` lands on `/`, as the shell's own `cd` does — and so is
//!   a path operand: no filesystem or symlink lookup, because the decision has
//!   to be a pure function of the text.
//! - Wrappers that change nothing are skipped, options and all: `sudo`, `nice`,
//!   `ionice`, `env` (and its `NAME=value` assignments), `timeout N`,
//!   `command`, `exec`, `nohup`, `stdbuf`, plain assignments, and the shell
//!   keywords that can lead a command (`if`, `then`, `do`, `!`, `time`, …).
//!   `sudo nice find /` is the same fact as `find /`. The two wrappers that
//!   *do* move a command's directory — `env -C DIR` and `sudo -D`/`--chdir
//!   DIR` — are read as such: for that piece the command starts in `DIR`, the
//!   same way a `cd` would have put it there.
//! - A nested shell's script is read too — `sh -c '…'`, `bash -c "…"`,
//!   `busybox sh -c …`, and the other common shells — **one level**, with a
//!   copy of the effective cwd, because a child cannot move its parent's.
//!   `busybox <applet>` is read through as the applet's own command line, so
//!   `busybox find /` is `find /`.
//! - A walker's path operands are read the way the walker itself reads them:
//!   its own options first (the table is per command, because an option's
//!   meaning is the command's — `-r` is grep's recursion and ripgrep's
//!   replacement text), a leading pattern operand where the command takes one
//!   (`grep`, `rg`, `fd`), then the paths. With no path operand the walk starts
//!   at the effective cwd, exactly as `find`, `du`, `ls -R`, `grep -r`, `rg`,
//!   `tree` and `fd` do. `/usr/bin/find` is `find`: the command's basename is
//!   what is read.
//!
//! # What is refused, and the cases the reading refuses to guess
//!
//! A walk is refused when one of its roots normalizes to `/`: a path operand
//! spelled `/`, `//`, `/.`, `/./`, `/tmp/x/../..` or the like, or no path
//! operand at all while the effective cwd is `/` — so `find /`, `cd / && find`
//! and `cd / && find .` are all the same fact. `find .`, `find src`,
//! `find /tmp/thing` and `cd /tmp && find .` are not the target and stay
//! allowed, and the other whole-disk walkers (`du`, `ls -R`, `grep -r`, `rg`,
//! `tree`, `fd`) share the rule because their roots can be read the same way.
//!
//! Two cases are deliberately *not* guessed at, and they are why an unknown cwd
//! or an unknown operand allows a bare walker:
//!
//! - `cd` with no argument goes to `$HOME`; `cd ~`, `cd -`, and any `cd` whose
//!   argument carries an expansion (`$VAR`, `$(…)`, backticks) land where this
//!   reading cannot see. Such a cwd is *unknown*, and a walker with no path
//!   operand under an unknown cwd is allowed — the cwd is not provably `/`, and
//!   guessing would refuse an honest `cd && find` that never went near the
//!   root. An absolute path operand is still refused whatever the cwd is.
//! - A path operand that carries `~`, `$` or a backtick is unknown for the
//!   same reason: `find "$DIR"` may or may not be `/`.
//!
//! # What it does not catch
//!
//! This is a guard against an honest agent thrashing the machine, not a
//! sandbox, and the refusal sentence says so rather than claiming otherwise.
//! Anything that hides the command from a text reading gets through:
//! `f""ind /`, a root spelled through a variable (`find "$ROOT"`), `eval`,
//! `xargs`, a walker started from inside `find -exec` or `fd -x`, an alias or
//! shell function only the human's profile knows, a build script that itself
//! runs `find /`, and an inner shell nested one level deeper than the single
//! `-c` script scanned here. The heredoc reading errs the other way when the
//! `<<` does not begin its own word — `cat<<EOF` is one word, so its body is
//! read as commands, and a `find /` written in it is refused.

use std::path::{Component, Path, PathBuf};

/// The refusal's one sentence: what was refused, why, the road that works, and
/// what this guard actually is. One line, because it reaches the model as the
/// command's own result.
const REFUSAL: &str = "refusing a walk of the whole filesystem rooted at `/`: it thrashes the machine and is almost never the question being asked — search inside the workspace instead (`find . -name …`, or name the subtree), and `search`/`list_files` are the tools for a workspace question. This is a text guard, not a sandbox: a deliberately hidden command can still get through.";

/// The refusal this command line owes before a shell is started for it, or
/// `None` when it may run.
///
/// `command` is the whole shell line — `sh -c`'s argument, exactly as the
/// model wrote it — and `cwd` is the directory the command would start in (the
/// workspace root, where `run_command` runs). Nothing here touches the
/// filesystem or starts a process, so a test can pin every shape without a
/// shell; the caller (`machine::Shell::spawn`) is the one that turns a
/// sentence into a refusal.
pub fn refusal(command: &str, cwd: &Path) -> Option<String> {
    scan(command, Cwd::at(cwd), true)
        .0
        .map(|()| REFUSAL.to_string())
}

/// The directory a command would run in, as far as the text reading can tell.
///
/// `Unknown` is not a failure: it is the honest answer for `cd` with no
/// argument (`$HOME`), `cd ~`/`cd -`, and any `cd` whose target carries an
/// expansion. An unknown cwd does not refuse a bare walker — see the module
/// docs for why.
#[derive(Clone, Debug)]
enum Cwd {
    /// A path the reading can name, lexically normalized.
    Known(PathBuf),
    /// A directory this reading cannot see.
    Unknown,
}

impl Cwd {
    /// The cwd a command line starts from. A relative root is not a path this
    /// reading can join onto, so it is unknown — every caller in mush hands
    /// over the workspace root, which is absolute.
    fn at(root: &Path) -> Self {
        if root.is_absolute() {
            Cwd::Known(normalize(root))
        } else {
            Cwd::Unknown
        }
    }

    /// Whether a walker starting at this operand would start at `/`.
    fn walk_is_root(&self, operand: &str) -> bool {
        resolve(operand, self).is_some_and(|root| root == Path::new("/"))
    }

    /// Whether the walker's default root — the cwd itself — is `/`.
    fn is_root(&self) -> bool {
        matches!(self, Cwd::Known(dir) if dir.as_path() == Path::new("/"))
    }
}

/// The absolute, lexically normalized path an operand names — or `None` when
/// the reading refuses to guess (`~`, an expansion, or a relative operand
/// under an unknown cwd).
fn resolve(operand: &str, cwd: &Cwd) -> Option<PathBuf> {
    if operand.starts_with('~') || operand.contains('$') || operand.contains('`') {
        return None;
    }
    let path = Path::new(operand);
    if path.is_absolute() {
        Some(normalize(path))
    } else {
        match cwd {
            Cwd::Known(dir) => Some(normalize(&dir.join(path))),
            Cwd::Unknown => None,
        }
    }
}

/// The shell's own path arithmetic, lexically: `/tmp/x/../..` is `/`, `/./` is
/// `/`, `/tmp/../tmp` is `/tmp`. Symlinks are not followed — `/link/..` is the
/// parent of `link`, not of its target — because following one would need the
/// filesystem, and the decision is a reading of the text.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            // `..` at `/` is `/`: `pop` on the root leaves it standing.
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            // A platform prefix cannot appear in a unix path; ignoring it is
            // the same answer as "no component".
            Component::Prefix(_) => {}
        }
    }
    out
}

/// How a word is quoted while the line is split.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// The words of every command on one shell line, split on the separators that
/// end a command, with quoting respected and heredoc bodies skipped.
///
/// This is a lexer for the separators that matter, not a shell: no expansion,
/// no redirection, no grammar. A word is what a shell would hand a program as
/// one argument in the common cases — quote characters removed, backslash
/// escapes applied, `"…"` kept together.
fn pieces(line: &str) -> Vec<Vec<String>> {
    let mut pieces: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = Quote::None;
    // Heredocs: `<<EOF` hands the lines after this one to the command as
    // *data*, and a body that happens to spell a command is not one.
    // `awaiting` is a delimiter's word still to come, `pending` are the
    // delimiters this line declared, and `skipping` is their bodies being
    // consumed. `<<<` (a here-string) is not a heredoc: its word is an operand.
    let mut awaiting: Option<bool> = None;
    let mut pending: Vec<(String, bool)> = Vec::new();
    let mut chars = line.chars().peekable();

    fn flush(
        word: &mut String,
        started: &mut bool,
        words: &mut Vec<String>,
        awaiting: &mut Option<bool>,
        pending: &mut Vec<(String, bool)>,
    ) {
        if !*started {
            return;
        }
        let text = std::mem::take(word);
        *started = false;
        match awaiting.take() {
            Some(strip_tabs) => pending.push((text, strip_tabs)),
            None => words.push(text),
        }
    }

    fn end_piece(words: &mut Vec<String>, pieces: &mut Vec<Vec<String>>) {
        if !words.is_empty() {
            pieces.push(std::mem::take(words));
        }
    }

    while let Some(c) = chars.next() {
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(c);
                }
            }
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '\\' => {
                    // Inside double quotes a backslash is literal unless it
                    // escapes one of the few characters a shell lets it
                    // escape.
                    if chars
                        .peek()
                        .is_some_and(|next| matches!(next, '"' | '\\' | '$' | '`'))
                    {
                        word.push(chars.next().expect("peeked"));
                    } else {
                        word.push('\\');
                    }
                }
                _ => word.push(c),
            },
            Quote::None => match c {
                '\'' => {
                    quote = Quote::Single;
                    started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    started = true;
                }
                '\\' => {
                    // A backslash escape; backslash-newline is a continuation,
                    // not a word.
                    if let Some(escaped) = chars.next() {
                        if escaped != '\n' {
                            word.push(escaped);
                            started = true;
                        }
                    }
                }
                '#' if !started => {
                    // A `#` where a word would begin opens a comment. Stop
                    // *before* the newline: the newline's own work — ending
                    // the piece and handing out heredoc bodies — still stands.
                    while chars.peek().is_some_and(|next| *next != '\n') {
                        chars.next();
                    }
                }
                '<' if !started && chars.peek() == Some(&'<') => {
                    chars.next();
                    if chars.peek() == Some(&'<') {
                        // `<<<`: a here-string's word is an operand, not a
                        // heredoc's delimiter.
                        chars.next();
                        word.push_str("<<<");
                        started = true;
                    } else if chars.peek() == Some(&'-') {
                        chars.next();
                        awaiting = Some(true);
                    } else {
                        awaiting = Some(false);
                    }
                }
                ';' | '&' | '|' | '(' | ')' | '\n' => {
                    flush(
                        &mut word,
                        &mut started,
                        &mut words,
                        &mut awaiting,
                        &mut pending,
                    );
                    end_piece(&mut words, &mut pieces);
                    if c == '\n' && !pending.is_empty() {
                        let skipping = std::mem::take(&mut pending);
                        let mut at = 0;
                        while at < skipping.len() {
                            let mut body = String::new();
                            let mut ended = false;
                            for next in chars.by_ref() {
                                if next == '\n' {
                                    ended = true;
                                    break;
                                }
                                body.push(next);
                            }
                            let (delimiter, strip_tabs) = &skipping[at];
                            let candidate = if *strip_tabs {
                                body.trim_start_matches('\t')
                            } else {
                                body.as_str()
                            };
                            if candidate == delimiter {
                                at += 1;
                            }
                            if !ended {
                                break;
                            }
                        }
                    }
                }
                c if c.is_whitespace() => {
                    flush(
                        &mut word,
                        &mut started,
                        &mut words,
                        &mut awaiting,
                        &mut pending,
                    );
                }
                c => {
                    word.push(c);
                    started = true;
                }
            },
        }
    }
    flush(
        &mut word,
        &mut started,
        &mut words,
        &mut awaiting,
        &mut pending,
    );
    end_piece(&mut words, &mut pieces);
    pieces
}

/// The one thing a nested shell's `-c` argument is: a command line of its own.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "ash"];

/// Keywords that can lead a piece without being its command.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "while", "until", "do", "done", "for", "case", "esac",
    "in", "!", "time", "{", "}",
];

/// A program that only decorates the command it runs.
struct Wrapper {
    name: &'static str,
    /// Short option letters that take a value.
    short_values: &'static str,
    /// Long options that take a value.
    long_values: &'static [&'static str],
    /// Whether one further word — `timeout`'s duration — follows the options.
    takes_duration: bool,
    /// Short options that make this a lookup rather than an execution
    /// (`command -v find`): no command runs.
    lookup_flags: &'static str,
}

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "sudo",
        short_values: "ugpCTDRrtU",
        long_values: &[
            "user",
            "group",
            "prompt",
            "close-from",
            "chdir",
            "chroot",
            "command-timeout",
            "other-user",
        ],
        takes_duration: false,
        lookup_flags: "",
    },
    Wrapper {
        name: "nice",
        short_values: "n",
        long_values: &["adjustment"],
        takes_duration: false,
        lookup_flags: "",
    },
    Wrapper {
        name: "ionice",
        short_values: "cnpu",
        long_values: &["class", "classdata", "pid", "uid"],
        takes_duration: false,
        lookup_flags: "",
    },
    Wrapper {
        name: "env",
        short_values: "uSC",
        long_values: &[
            "unset",
            "split-string",
            "chdir",
            "block-signal",
            "default-signal",
            "ignore-signal",
        ],
        takes_duration: false,
        lookup_flags: "",
    },
    Wrapper {
        name: "timeout",
        short_values: "sk",
        long_values: &["signal", "kill-after"],
        takes_duration: true,
        lookup_flags: "",
    },
    Wrapper {
        name: "command",
        short_values: "",
        long_values: &[],
        takes_duration: false,
        lookup_flags: "vV",
    },
    Wrapper {
        name: "exec",
        short_values: "a",
        long_values: &[],
        takes_duration: false,
        lookup_flags: "",
    },
    Wrapper {
        name: "nohup",
        short_values: "",
        long_values: &[],
        takes_duration: false,
        lookup_flags: "",
    },
    Wrapper {
        name: "stdbuf",
        short_values: "ioe",
        long_values: &["input", "output", "error"],
        takes_duration: false,
        lookup_flags: "",
    },
];

/// Where the command word starts, once the words that only decorate it are
/// skipped, and the directory a wrapper moves the command to if one does — or
/// `None` when the piece runs no command at all (a bare keyword, or
/// `command -v find`).
fn command_at<'a>(words: &'a [String]) -> Option<(usize, Option<&'a str>)> {
    let mut at = 0;
    let mut moved: Option<&'a str> = None;
    while at < words.len() {
        let word = words[at].as_str();
        if is_assignment(word) || KEYWORDS.contains(&word) {
            at += 1;
            continue;
        }
        let Some(wrapper) = WRAPPERS.iter().find(|wrapper| wrapper.name == word) else {
            return Some((at, moved));
        };
        at += 1;
        let options = at;
        let (end, chdir) = skip_options(
            words,
            at,
            wrapper.short_values,
            wrapper.long_values,
            match wrapper.name {
                // The two wrappers that move the directory their command runs
                // in: `env -C DIR` and `sudo -D DIR`/`--chdir DIR`.
                "env" => Some(('C', "chdir")),
                "sudo" => Some(('D', "chdir")),
                _ => None,
            },
        );
        at = end;
        if chdir.is_some() {
            moved = chdir;
        }
        if !wrapper.lookup_flags.is_empty()
            && words[options..at].iter().any(|option| {
                option.starts_with('-')
                    && !option.starts_with("--")
                    && option[1..]
                        .chars()
                        .any(|c| wrapper.lookup_flags.contains(c))
            })
        {
            return None;
        }
        if wrapper.takes_duration && at < words.len() {
            at += 1;
        }
    }
    None
}

/// Whether a word is a `NAME=value` assignment rather than a command.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Skip one wrapper's option words, and the value of every option that takes
/// one, answering the directory the wrapper moves the command to when it has
/// one (`env -C`, `sudo --chdir`). A `--` ends the options and is not the
/// command word.
fn skip_options<'a>(
    words: &'a [String],
    mut at: usize,
    short_values: &str,
    long_values: &[&str],
    chdir: Option<(char, &str)>,
) -> (usize, Option<&'a str>) {
    let mut moved: Option<&'a str> = None;
    while at < words.len() {
        let word = words[at].as_str();
        if word == "--" {
            return (at + 1, moved);
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            at += 1;
            let mut value = inline;
            if value.is_none() && long_values.contains(&name) && at < words.len() {
                value = Some(words[at].as_str());
                at += 1;
            }
            if chdir.is_some_and(|(_, long)| long == name) {
                moved = value;
            }
            continue;
        }
        if word.len() > 1 && word.starts_with('-') {
            let cluster = &word[1..];
            let mut takes_next = false;
            let mut moved_is_next_word = false;
            for (i, c) in cluster.char_indices() {
                if !short_values.contains(c) {
                    continue;
                }
                // The rest of the cluster is the value when there is one;
                // otherwise the value is the next word.
                let rest_at = i + c.len_utf8();
                takes_next = rest_at == cluster.len();
                if chdir.is_some_and(|(short, _)| c == short) {
                    if takes_next {
                        moved_is_next_word = true;
                    } else {
                        moved = Some(&cluster[rest_at..]);
                    }
                }
                break;
            }
            at += 1;
            if takes_next && at < words.len() {
                if moved_is_next_word {
                    moved = Some(words[at].as_str());
                }
                at += 1;
            }
            continue;
        }
        break;
    }
    (at, moved)
}

/// A command that walks a directory tree, and the option vocabulary needed to
/// find its path operands. Per command, because an option's meaning is the
/// command's: `-r` is grep's recursion and ripgrep's replacement text.
struct Walker {
    name: &'static str,
    /// Short option letters that take a value.
    short_values: &'static str,
    /// Long options that take a value.
    long_values: &'static [&'static str],
    /// Short option letters that turn recursion on.
    recurse_short: &'static str,
    /// Long options that mean recursion.
    recurse_long: &'static [&'static str],
    /// Whether the command only walks the whole tree with a recursion flag:
    /// `ls /` is one directory, `ls -R /` is the disk.
    needs_recurse: bool,
    /// How many leading positional operands are not path operands.
    patterns: usize,
    /// Short option letters that supply the pattern instead of a positional.
    pattern_short: &'static str,
    /// Long options that supply the pattern instead of a positional.
    pattern_long: &'static [&'static str],
    /// Long options whose value is itself a path the walk starts from (`fd`'s
    /// `--search-path`).
    path_values: &'static [&'static str],
}

const WALKERS: &[Walker] = &[
    // `du` descends whatever it is pointed at, and defaults to the cwd.
    Walker {
        name: "du",
        short_values: "BdtX",
        long_values: &[
            "block-size",
            "max-depth",
            "threshold",
            "exclude",
            "exclude-from",
            "files0-from",
            "time-style",
        ],
        recurse_short: "",
        recurse_long: &[],
        needs_recurse: false,
        patterns: 0,
        pattern_short: "",
        pattern_long: &[],
        path_values: &[],
    },
    // Only `-R`/`--recursive` makes `ls` a walk; `ls -r` is reverse order.
    Walker {
        name: "ls",
        short_values: "wTI",
        long_values: &[
            "width",
            "tabsize",
            "ignore",
            "hide",
            "block-size",
            "time-style",
            "sort",
            "format",
            "quoting-style",
            "indicator-style",
        ],
        recurse_short: "R",
        recurse_long: &["recursive"],
        needs_recurse: true,
        patterns: 0,
        pattern_short: "",
        pattern_long: &[],
        path_values: &[],
    },
    // `grep pattern paths…`, and recursive only with `-r`/`-R`. `-e`/`-f`
    // supply the pattern, so the first positional is a path then. `-r` with no
    // paths searches the cwd (GNU grep).
    Walker {
        name: "grep",
        short_values: "efmABCdD",
        long_values: &[
            "regexp",
            "file",
            "max-count",
            "after-context",
            "before-context",
            "context",
            "directories",
            "devices",
            "label",
            "binary-files",
            "include",
            "exclude",
            "exclude-from",
            "exclude-dir",
            "include-dir",
            "group-separator",
            "context-separator",
        ],
        recurse_short: "rR",
        recurse_long: &["recursive", "dereference-recursive"],
        needs_recurse: true,
        patterns: 1,
        pattern_short: "ef",
        pattern_long: &["regexp", "file"],
        path_values: &[],
    },
    // Recursive by default; `-r` here replaces text, `-g`/`-t`/`-j` take values.
    Walker {
        name: "rg",
        short_values: "efEmjgdtTABCMr",
        long_values: &[
            "regexp",
            "file",
            "encoding",
            "max-count",
            "threads",
            "glob",
            "max-depth",
            "type",
            "type-not",
            "after-context",
            "before-context",
            "context",
            "max-columns",
            "replace",
            "sort",
            "engine",
            "pre",
            "pre-glob",
            "max-filesize",
            "dfa-size-limit",
            "regex-size-limit",
            "hostname-bin",
        ],
        recurse_short: "",
        recurse_long: &[],
        needs_recurse: false,
        patterns: 1,
        pattern_short: "ef",
        pattern_long: &["regexp", "file"],
        path_values: &[],
    },
    // `tree [paths…]`, recursive by default.
    Walker {
        name: "tree",
        short_values: "LPIoHT",
        long_values: &[
            "level",
            "pattern",
            "ignore",
            "output",
            "charset",
            "filelimit",
            "timefmt",
            "sort",
            "fromfile",
        ],
        recurse_short: "",
        recurse_long: &[],
        needs_recurse: false,
        patterns: 0,
        pattern_short: "",
        pattern_long: &[],
        path_values: &[],
    },
    // `fd [pattern] [paths…]`: a single positional is the pattern, and a
    // second one is where the walk starts. `--search-path` is a path too.
    Walker {
        name: "fd",
        short_values: "dEetSjcxX",
        long_values: &[
            "max-depth",
            "min-depth",
            "exclude",
            "extension",
            "type",
            "size",
            "threads",
            "exec",
            "exec-batch",
            "batch-size",
            "color",
            "max-buffer-time",
            "changed-within",
            "changed-before",
            "base-directory",
            "owner",
            "format",
        ],
        recurse_short: "",
        recurse_long: &[],
        needs_recurse: false,
        patterns: 1,
        pattern_short: "",
        pattern_long: &[],
        path_values: &["search-path"],
    },
];

/// Whether this walker's operands send it to `/`.
fn walker_refuses(walker: &Walker, args: &[String], cwd: &Cwd) -> bool {
    let mut recursive = !walker.needs_recurse;
    let mut pattern_from_option = false;
    // A path an option supplied (`fd --search-path /`) is a path whatever the
    // positional pattern count says, so the two are kept apart.
    let mut path_values: Vec<&str> = Vec::new();
    let mut operands: Vec<&str> = Vec::new();
    let mut literal = false;
    let mut at = 0;
    while at < args.len() {
        let word = args[at].as_str();
        if !literal && word == "--" {
            literal = true;
            at += 1;
            continue;
        }
        if !literal && word.len() > 2 && word.starts_with("--") {
            let (name, inline) = match word[2..].split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (&word[2..], None),
            };
            if walker.recurse_long.contains(&name) {
                recursive = true;
            }
            if walker.pattern_long.contains(&name) {
                pattern_from_option = true;
            }
            if walker.path_values.contains(&name) {
                match inline {
                    Some(value) => path_values.push(value),
                    None => {
                        if at + 1 < args.len() {
                            path_values.push(args[at + 1].as_str());
                            at += 1;
                        }
                    }
                }
            }
            if inline.is_none() && walker.long_values.contains(&name) && at + 1 < args.len() {
                at += 1;
            }
            at += 1;
            continue;
        }
        if !literal && word.len() > 1 && word.starts_with('-') {
            let mut cluster = word[1..].chars();
            let mut takes_next = false;
            while let Some(c) = cluster.next() {
                if walker.recurse_short.contains(c) {
                    recursive = true;
                }
                if walker.pattern_short.contains(c) {
                    pattern_from_option = true;
                }
                if walker.short_values.contains(c) {
                    takes_next = cluster.next().is_none();
                    break;
                }
            }
            at += 1;
            if takes_next && at < args.len() {
                at += 1;
            }
            continue;
        }
        operands.push(word);
        at += 1;
    }
    if walker.needs_recurse && !recursive {
        return false;
    }
    let skip = if pattern_from_option {
        0
    } else {
        walker.patterns
    };
    let paths: Vec<&str> = path_values
        .into_iter()
        .chain(operands.get(skip..).unwrap_or(&[]).iter().copied())
        .collect();
    if paths.is_empty() {
        return cwd.is_root();
    }
    paths.iter().any(|path| cwd.walk_is_root(path))
}

/// Whether `find`'s operands send it to `/`. `find`'s syntax is its own:
/// leading options (`-H`, `-L`, `-P`, `-D debugopts`, `-O level`), then path
/// operands until the expression begins — the first word that starts with `-`,
/// or `!`, `(`, `)`. With no path operand `find` searches the cwd, which its
/// own `--help` says.
fn find_refuses(args: &[String], cwd: &Cwd) -> bool {
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "-H" | "-L" | "-P" => at += 1,
            // `-D debugopts` and `-O level` each take a word; the level may
            // also be attached (`-O2`).
            "-D" | "-O" => at += 2,
            word if word.len() > 2 && word.starts_with("-O") => at += 1,
            _ => break,
        }
    }
    // `find --help` and `find --version` walk nothing.
    if matches!(
        args.get(at).map(String::as_str),
        Some("--help" | "--version")
    ) {
        return false;
    }
    let mut paths: Vec<&str> = Vec::new();
    while let Some(word) = args.get(at).map(String::as_str) {
        if (word.len() > 1 && word.starts_with('-')) || matches!(word, "!" | "(" | ")") {
            break;
        }
        paths.push(word);
        at += 1;
    }
    if paths.is_empty() {
        return cwd.is_root();
    }
    paths.iter().any(|path| cwd.walk_is_root(path))
}

/// Where a `cd` piece lands, as far as the text reading can tell.
fn cd_to(args: &[String], cwd: &Cwd) -> Cwd {
    let mut at = 0;
    while let Some(word) = args.get(at).map(String::as_str) {
        if word == "--" {
            at += 1;
            break;
        }
        if word.len() > 1 && word.starts_with('-') {
            at += 1;
            continue;
        }
        break;
    }
    let Some(target) = args.get(at).map(String::as_str) else {
        // `cd` with no argument goes to `$HOME`, which this reading does not
        // read.
        return Cwd::Unknown;
    };
    if target == "-" || target.starts_with('~') || target.contains('$') || target.contains('`') {
        return Cwd::Unknown;
    }
    let path = Path::new(target);
    if path.is_absolute() {
        Cwd::Known(normalize(path))
    } else {
        match cwd {
            Cwd::Known(dir) => Cwd::Known(normalize(&dir.join(path))),
            Cwd::Unknown => Cwd::Unknown,
        }
    }
}

/// The script argument of a nested shell's `-c`, if it has one. `None` for a
/// shell given a script *file* (`bash script.sh`) or no argument at all.
fn shell_script(args: &[String]) -> Option<&str> {
    for (at, arg) in args.iter().enumerate() {
        let cluster = arg.strip_prefix('-')?;
        if cluster.is_empty() {
            continue;
        }
        let Some(position) = cluster.find('c') else {
            continue;
        };
        let attached = &cluster[position + 1..];
        if attached.is_empty() {
            return args.get(at + 1).map(String::as_str);
        }
        // `-c'…'`: the script is the rest of the same word.
        let start = arg.len() - attached.len();
        return arg.get(start..);
    }
    None
}

/// A command's name as the shell would find it: the basename, so
/// `/usr/bin/find` is `find`.
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Walk one command line: the refusal it owes, if any, and the cwd its pieces
/// leave behind. `nested` allows the single `-c` level the module docs promise;
/// a nested shell gets a *copy* of the cwd, because a child cannot move its
/// parent's.
fn scan(line: &str, cwd: Cwd, nested: bool) -> (Option<()>, Cwd) {
    let mut cwd = cwd;
    for words in pieces(line) {
        let Some((mut at, chdir)) = command_at(&words) else {
            continue;
        };
        // A wrapper that moved the directory (`env -C / find`): the command it
        // runs starts there, not where the shell is. The move is the piece's,
        // not the shell's: a later piece is back at `cwd`.
        let piece_cwd = match chdir {
            Some(dir) => resolve(dir, &cwd).map_or(Cwd::Unknown, Cwd::Known),
            None => cwd.clone(),
        };
        let mut command = basename(&words[at]);
        if command == "busybox" {
            // `busybox <applet>` is the applet's own command line.
            let Some(applet) = words[at + 1..]
                .iter()
                .position(|word| !word.starts_with('-'))
            else {
                continue;
            };
            at += 1 + applet;
            command = basename(&words[at]);
        }
        let args = &words[at + 1..];
        if command == "cd" {
            cwd = cd_to(args, &cwd);
            continue;
        }
        if SHELLS.contains(&command) {
            if nested {
                if let Some(script) = shell_script(args) {
                    if scan(script, piece_cwd, false).0.is_some() {
                        return (Some(()), cwd);
                    }
                }
            }
            continue;
        }
        let refuses = if command == "find" {
            find_refuses(args, &piece_cwd)
        } else {
            WALKERS
                .iter()
                .find(|walker| walker.name == command)
                .is_some_and(|walker| walker_refuses(walker, args, &piece_cwd))
        };
        if refuses {
            return (Some(()), cwd);
        }
    }
    (None, cwd)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{refusal, REFUSAL};

    /// A workspace root that is deep enough for `cd ..` to mean something.
    const ROOT: &str = "/home/agent/project";

    fn refused(command: &str) -> bool {
        refusal(command, Path::new(ROOT)).is_some()
    }

    fn allowed(command: &str) -> bool {
        refusal(command, Path::new(ROOT)).is_none()
    }

    /// The fact the human asked for: a `find` rooted at `/` is refused, however
    /// the path is spelled, and a `cd` before it changes nothing.
    #[test]
    fn a_find_over_the_whole_filesystem_is_refused() {
        for command in [
            "find /",
            "find / -name x",
            "find / -maxdepth 1",
            "find //",
            "find /.",
            "find /./",
            "find /tmp/x/../..",
            "find /usr/..",
            "find '/'",
            "find \"/\"",
            "/usr/bin/find /",
            "find -L /",
            "find -D search /",
            "cd / && find",
            "cd / && find .",
            "cd /; find",
            "cd /tmp/x/../.. && find -name x",
            "cd / && /usr/bin/find .",
            "cd /\nfind",
            "echo hi; find /",
            "find /tmp || find /",
            "find / | head",
            "find / &",
            "(cd / && find)",
        ] {
            assert!(refused(command), "must be refused: {command}");
        }
    }

    /// The other whole-disk walkers share the rule, because their roots are
    /// read the same way.
    #[test]
    fn the_other_whole_disk_walkers_are_refused_by_the_same_rule() {
        for command in [
            "du /",
            "du -sh /",
            "du --max-depth=1 /",
            "ls -R /",
            "ls -lR /",
            "ls -R -- /",
            "ls --recursive /",
            "grep -r foo /",
            "grep -rn --include='*.rs' foo /",
            "grep -r -e foo /",
            "grep --recursive foo /",
            "rg foo /",
            "rg -i foo /",
            "rg -r x foo /",
            "rg -e foo /",
            "tree /",
            "tree -L 1 /",
            "tree -P '*.rs' /",
            "fd foo /",
            "fd -e rs foo /",
            "fd --search-path /",
            "cd / && du",
            "cd / && ls -R",
            "cd / && grep -r foo",
            "cd / && rg foo",
            "cd / && tree",
            "cd / && fd",
        ] {
            assert!(refused(command), "must be refused: {command}");
        }
    }

    /// The roads a workspace question actually takes stay open, and the
    /// commands whose shape is *like* a whole-disk walk but whose root is not
    /// `/` are untouched.
    #[test]
    fn a_walk_inside_the_workspace_stays_allowed() {
        for command in [
            "find .",
            "find src",
            "find /tmp/thing",
            "find . -name '*.rs'",
            "find . -maxdepth 1",
            "find ..",
            "find /tmp/x/..",
            "find /tmp/../tmp",
            "cd /tmp && find .",
            "cd /tmp/x && find ..",
            "cd .. && find .",
            "cd src/.. && find .",
            "ls /",
            "ls -l /",
            "ls -r /",
            "ls -R .",
            "ls -R src",
            "du -sh .",
            "du src",
            "grep foo /etc/hosts",
            "grep -r foo src",
            "grep -R foo .",
            "rg foo src",
            "rg -e foo src",
            "tree src",
            "fd foo src",
            "fd /",
            "echo find /",
            "echo 'find /'",
            "grep find /etc/fstab",
            "cat /etc/hosts",
            "cd / && ls",
            "find /etc -name hosts",
            "# find /",
            "echo hi # find /",
            "true && find /tmp",
            "find . || find /tmp",
            "find . && find /tmp || find src",
        ] {
            assert!(allowed(command), "must stay allowed: {command}");
        }
    }

    /// What comes before the walker does not hide it: wrappers, assignments
    /// and the shell's leading keywords are skipped, options and all.
    #[test]
    fn a_wrapper_before_the_walk_changes_nothing() {
        for command in [
            "sudo find /",
            "sudo nice find /",
            "sudo -u root find /",
            "sudo --user=root find /",
            "nice -n 10 find /",
            "nice -10 find /",
            "ionice -c2 -n0 find /",
            "env FOO=bar find /",
            "env -i HOME=/root find /",
            "env -- find /",
            "timeout 5 find /",
            "timeout -k 1 5 find /",
            "timeout --signal=KILL 5 find /",
            "nohup find /",
            "command find /",
            "command -p find /",
            "exec find /",
            "stdbuf -oL find /",
            "FOO=1 find /",
            "FOO=1 sudo find /",
            // The two wrappers that move the directory the command runs in.
            "env -C / find",
            "env -C/ find",
            "env --chdir / find",
            "env --chdir=/ find",
            "env -i -C / find .",
            "env -C / find .",
            "sudo -D / find",
            "sudo --chdir / find",
            "sudo --chdir=/ find",
            "env -C / busybox find",
            "sudo -D / sh -c 'find .'",
            "if cd /; then find; fi",
            "while true; do find /; done",
            "! find /",
            "time find /",
            "{ find /; }",
        ] {
            assert!(refused(command), "must be refused: {command}");
        }
        // A lookup is not a run: `command -v find /` prints a path.
        assert!(allowed("command -v find /"));
        // A wrapper that moves the directory to somewhere that is not `/`
        // leaves the walk legal, and the move is the piece's: the next piece
        // is back at the shell's own cwd.
        for command in [
            "env -C /tmp find .",
            "env -C / find /tmp",
            "env -C /tmp find",
            "sudo -D /tmp find .",
            "env -C /tmp find . && find .",
            "env -C $DIR find",
        ] {
            assert!(allowed(command), "must stay allowed: {command}");
        }
    }

    /// The `cwd` a previous piece left is the cwd the walker starts from, and
    /// the piece order is the shell's.
    #[test]
    fn the_effective_cwd_carries_across_pieces() {
        assert!(refused("cd /tmp/foo/../.. && find"));
        assert!(refused("cd .. && cd .. && cd .. && find"));
        assert!(refused("cd / && ls -R"));
        // `/home/agent/project` up three is `/home`, not `/`: the walk is real
        // but it is not the whole disk.
        assert!(allowed("cd ../.. && find"));
        assert!(refused("cd ../../.. && find"));
    }

    /// A nested shell's script is read one level, with a copy of the cwd: the
    /// child's `cd` moves the child, and the parent's scan is untouched.
    #[test]
    fn a_nested_shells_script_is_read_too() {
        for command in [
            "sh -c 'find /'",
            "bash -c \"cd / && find\"",
            "busybox sh -c 'find /'",
            "busybox find /",
            "nice bash -c 'find /'",
            "env sh -c 'find /'",
            "sh -ec 'find /'",
            "bash -c 'cd /tmp/x/../.. && find .'",
        ] {
            assert!(refused(command), "must be refused: {command}");
        }
        for command in [
            "sh -c 'find .'",
            "bash -c 'cd /tmp && find .'",
            "bash -c 'cd /' ; find .",
            "sh -c 'echo find /'",
            "sh script.sh",
        ] {
            assert!(allowed(command), "must stay allowed: {command}");
        }
    }

    /// The cases the reading refuses to guess at, said in the module docs: a
    /// `cd` it cannot follow, or an operand it cannot read, allows a walker
    /// whose root is not provably `/` — and an absolute root is still refused.
    #[test]
    fn an_unknown_cwd_or_operand_is_not_guessed_at() {
        for command in [
            "cd && find",
            "cd ~ && find",
            "cd - && find",
            "cd $HOME && find",
            "cd \"$WORK\" && find",
            "find \"$DIR\"",
            "find ~/thing",
            // `/` plus an unreadable suffix is not provably `/` either.
            "find /$DIR",
            "cd / && find $PWD",
        ] {
            assert!(allowed(command), "must stay allowed: {command}");
        }
        // An absolute root needs no cwd to read.
        assert!(refused("cd $HOME && find /"));
    }

    /// A heredoc's body is stdin data, not commands: a `find /` written inside
    /// one is not a walk, and the lines after the body are read again.
    #[test]
    fn a_heredoc_body_is_data() {
        assert!(allowed("cat <<EOF\nfind /\nEOF"));
        assert!(allowed("cat <<'EOF'\ncd /\nfind .\nEOF"));
        assert!(allowed("cat <<-EOF\n\tfind /\n\tEOF"));
        // After the body the reading resumes, so a real walk is still caught.
        assert!(refused("cat <<EOF\nfind /\nEOF\nfind /"));
        assert!(refused("find / <<EOF\nx\nEOF"));
        // The form the reading does not see as a heredoc — `<<` must begin its
        // own word — so the body is read as commands: an over-refusal, not a
        // miss, and the module docs say so.
        assert!(refused("cat<<EOF\nfind /\nEOF"));
    }

    /// The sentence the model reads names what was refused, the road that
    /// works, and what the guard is — one line, because it is a tool result.
    #[test]
    fn the_refusal_names_the_refusal_and_the_road() {
        let sentence = refusal("find /", Path::new(ROOT)).expect("refused");
        assert_eq!(sentence, REFUSAL);
        assert!(!sentence.contains('\n'), "one line: {sentence}");
        for fact in [
            "whole filesystem",
            "`/`",
            "thrashes the machine",
            "almost never the question",
            "`find . -name",
            "`search`/`list_files`",
            "text guard, not a sandbox",
        ] {
            assert!(
                sentence.contains(fact),
                "the refusal says {fact:?}: {sentence}"
            );
        }
    }
}
