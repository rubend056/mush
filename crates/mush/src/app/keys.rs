//! What a key means, as a pure decision.
//!
//! The whole keymap is one function of three values the caller already has —
//! which pane is focused, whether a picker is up, and the key — so a binding can
//! be tested by calling it, without an `App`, an actor thread or a terminal.
//! `App::apply_intent` is the only thing that carries an [`Intent`] out.
//!
//! The invariant this module owns: **no key is read twice.** Nothing below
//! `App::on_key` looks at a `KeyCode`, so there is exactly one place that
//! decides which pane a key belongs to — and a key that reaches the wrong pane
//! is a failing line in this file rather than a bug a human finds by typing.
//! The old shape matched keys *and* did their work in the same arm, so a
//! binding could only be observed through a live `App`, which is how a key
//! edited the wrong thing while its own test kept passing (finding B2).
//!
//! The sketch in `docs/refactor.md` §3.5 puts a `mode` beside the pane. There
//! is none to pass: the only modal state the keyboard has is the picker, which
//! the caller holds as a bool, and the editor pane that had insert and normal
//! modes was dropped from the tree in Stage 0 — a `mode` argument here would be
//! a value no state can produce.
//!
//! The whole table lives in [`KEYS`] — one row per binding — and both help
//! surfaces render it through [`help_table`]: `mush --help`'s KEYS block and
//! the in-app `/help` notice. A binding therefore cannot be documented in one
//! and missing from the other, which is what the hand-written `--help` prose
//! and the six-key `/help` line allowed (the key half of finding B2). This is
//! the same one-source shape `commands::table` gives the slash commands. [`key`]
//! stays the behaviour and the tests below pin it key by key; a test pins both
//! help surfaces to [`KEYS`], so the documentation cannot drift from the rows.
//!
//! The tree's keys deliberately ignore modifiers, exactly as the old arms did:
//! `Alt-C` stops a row and `Ctrl-J` moves the cursor, and both are pinned in a
//! test so that a future change to either is a decision rather than an accident.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::Focus;

/// How many rows a page key moves, in the chat's scrollback and in a picker's
/// list. One number, so "a page" is the same distance wherever a human pages.
const PAGE: i64 = 10;

/// The context a binding belongs to, so the help can group the rows the way a
/// human reads them: what works anywhere, then the modal list, then each pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// Active with a picker up and while either pane is focused.
    Anywhere,
    /// Only while a picker holds the keyboard.
    Picker,
    Agents,
    Chat,
}

impl Context {
    /// The heading [`help_table`] prints above this context's rows.
    const fn label(self) -> &'static str {
        match self {
            Context::Anywhere => "anywhere",
            Context::Picker => "in a picker",
            Context::Agents => "agents pane",
            Context::Chat => "chat pane",
        }
    }
}

/// One row of the key table: the keys, and what they do.
///
/// [`KEYS`] is the single source both `mush --help` and the in-app `/help`
/// render — the way `COMMANDS` serves both scripts — so a binding cannot be
/// documented in one surface and missing from the other. Every binding the
/// program has appears here exactly once.
pub struct Binding {
    pub context: Context,
    /// The keys as a human reads them, e.g. `j / k, ↑ / ↓`.
    pub keys: &'static str,
    /// What they do, in one clause.
    pub help: &'static str,
}

/// The whole key table, in the order the help prints it.
pub const KEYS: &[Binding] = &[
    Binding {
        context: Context::Anywhere,
        keys: "Ctrl-Q",
        help: "quit",
    },
    Binding {
        context: Context::Anywhere,
        keys: "Ctrl-C",
        help: "stop the focused agent",
    },
    Binding {
        context: Context::Anywhere,
        keys: "Ctrl-X",
        help: "stop every running agent",
    },
    Binding {
        context: Context::Anywhere,
        keys: "Ctrl-N",
        help: "start a new chat",
    },
    Binding {
        context: Context::Anywhere,
        keys: "Ctrl-P",
        help: "model picker",
    },
    Binding {
        context: Context::Anywhere,
        keys: "Tab / Shift-Tab",
        help: "cycle panes (agents, chat)",
    },
    Binding {
        context: Context::Picker,
        keys: "Enter",
        help: "take the selected row",
    },
    Binding {
        context: Context::Picker,
        keys: "Esc",
        help: "close the picker",
    },
    Binding {
        context: Context::Picker,
        keys: "j / k, ↑ / ↓",
        help: "move down / up the list",
    },
    Binding {
        context: Context::Picker,
        keys: "g / G, Home / End",
        help: "first / last row",
    },
    Binding {
        context: Context::Picker,
        keys: "PgUp / PgDn",
        help: "page the list",
    },
    Binding {
        context: Context::Agents,
        keys: "←",
        help: "the selected agent's parent",
    },
    Binding {
        context: Context::Agents,
        keys: "→",
        help: "the selected agent's first child",
    },
    Binding {
        context: Context::Agents,
        keys: "Enter",
        help: "focus the selected agent",
    },
    Binding {
        context: Context::Agents,
        keys: "j / k, ↑ / ↓",
        help: "move down / up a row",
    },
    Binding {
        context: Context::Agents,
        keys: "g / G, Home / End",
        help: "first / last row",
    },
    Binding {
        context: Context::Agents,
        keys: "c",
        help: "cancel the selected agent",
    },
    Binding {
        context: Context::Agents,
        keys: "Esc",
        help: "back to the root agent",
    },
    Binding {
        context: Context::Chat,
        keys: "Enter",
        help: "send the message",
    },
    Binding {
        context: Context::Chat,
        keys: "Shift / Alt-Enter",
        help: "new line in the message",
    },
    Binding {
        context: Context::Chat,
        keys: "letters and symbols",
        help: "type into the message box",
    },
    Binding {
        context: Context::Chat,
        keys: "← / →, Home / End",
        help: "move the box cursor",
    },
    Binding {
        context: Context::Chat,
        keys: "Backspace / Delete",
        help: "delete in the box",
    },
    Binding {
        context: Context::Chat,
        keys: "↑ / ↓, PgUp / PgDn",
        help: "scroll the transcript",
    },
    Binding {
        context: Context::Chat,
        keys: "Esc",
        help: "clear the box",
    },
];

/// The key table as text: a heading per context, then `keys` and `what it does`
/// in one aligned column.
///
/// Both `mush --help`'s KEYS block and the in-app `/help` notice print exactly
/// this string, so the two cannot disagree about a binding.
pub fn help_table() -> String {
    let width = KEYS
        .iter()
        .map(|binding| binding.keys.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    let mut shown: Option<Context> = None;
    for binding in KEYS {
        if shown != Some(binding.context) {
            if shown.is_some() {
                out.push('\n');
            }
            out.push_str(&format!("  {}:\n", binding.context.label()));
            shown = Some(binding.context);
        }
        out.push_str(&format!(
            "    {:<width$}  {}\n",
            binding.keys,
            binding.help,
            width = width
        ));
    }
    out.trim_end().to_string()
}

/// What the message box and the transcript's scrollback do with a key.
///
/// The chat owns these keys — [`super::Chat::apply`] is the only place they run
/// — but *which* key belongs to the chat is decided here with every other
/// binding, so the box cannot have a private second table that drifts from this
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatKey {
    /// A new line inside the message instead of sending it: `Shift-Enter`
    /// where the terminal reports it, `Alt-Enter` everywhere else.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    /// A printable character, inserted at the cursor.
    Insert(char),
    /// The transcript's scrollback, in rows: positive is older.
    Scroll(i64),
    /// Empty the message box, keeping a draft nowhere.
    Clear,
}

/// What a key does. Every effect the keyboard has on the program goes through
/// this enum, and every variant is applied in exactly one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Nothing owns this key. It is not an error: a terminal sends keys mush
    /// has no opinion about all the time.
    Ignore,
    Quit,
    /// New chat: `/new`, and the old conversation is gone.
    NewChat,
    /// Stop the focused agent's run (`Ctrl-C`).
    Interrupt,
    /// Stop every running agent (`Ctrl-X`) — the emergency brake, which used to
    /// be on `Ctrl-C` and killed the wrong agents.
    InterruptAll,
    /// Open the model picker (`Ctrl-P`).
    OpenModelPicker,
    /// Cycle the pane focus: `+1` for `Tab`, `-1` for `Shift-Tab`.
    CycleFocus(i64),
    PickerClose,
    PickerPick,
    /// Move the picker's cursor `step` rows, without leaving the list.
    PickerMove(i64),
    PickerFirst,
    PickerLast,
    /// Move the tree's cursor `step` rows, without leaving the pane.
    TreeMove(i64),
    /// Walk the tree along the parent links: `-1` selects the selected agent's
    /// parent (`←`), `+1` its first child (`→`). A root has no parent and a
    /// leaf has no child, so the cursor stays put — an honest no-op rather than
    /// a jump somewhere that is not the row the human asked for (finding U10).
    TreeWalk(i64),
    TreeFirst,
    TreeLast,
    /// Focus the agent under the tree's cursor.
    TreeFocus,
    /// Stop the agent under the tree's cursor.
    TreeCancel,
    /// Put the chat pane back on the root agent.
    TreeBackToRoot,
    /// Send what is in the message box to the focused agent.
    Send,
    /// A key the chat owns — see [`ChatKey`].
    Chat(ChatKey),
}

/// The whole keymap.
///
/// The order of the checks is the precedence, and it is the old `on_key`'s: the
/// app-wide `Ctrl` keys and the pane cycle first (so they work with a picker up
/// and while either pane is focused), then the picker, which takes the keyboard
/// from both panes while it is open.
pub fn key(focus: Focus, picker_open: bool, key: KeyEvent) -> Intent {
    // A terminal reports the release half of a press too (kitty and friends).
    // Only the press is a key.
    if key.kind == KeyEventKind::Release {
        return Intent::Ignore;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl {
        match key.code {
            KeyCode::Char('q') => return Intent::Quit,
            KeyCode::Char('c') => return Intent::Interrupt,
            KeyCode::Char('x') => return Intent::InterruptAll,
            KeyCode::Char('n') => return Intent::NewChat,
            KeyCode::Char('p') => return Intent::OpenModelPicker,
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => return Intent::CycleFocus(1),
        KeyCode::BackTab => return Intent::CycleFocus(-1),
        _ => {}
    }

    if picker_open {
        return picker(key);
    }
    match focus {
        Focus::Agents => tree(key),
        Focus::Chat => chat(key),
    }
}

/// The modal keys. A picker is a list and nothing else: `Enter` takes the row,
/// `Esc` gives up, and the movement keys are the tree's, because a human who
/// has used `j`/`k` once should not have to learn a second way to move down.
fn picker(key: KeyEvent) -> Intent {
    match key.code {
        KeyCode::Esc => Intent::PickerClose,
        KeyCode::Enter => Intent::PickerPick,
        KeyCode::Char('j') | KeyCode::Down => Intent::PickerMove(1),
        KeyCode::Char('k') | KeyCode::Up => Intent::PickerMove(-1),
        KeyCode::Char('g') | KeyCode::Home => Intent::PickerFirst,
        KeyCode::Char('G') | KeyCode::End => Intent::PickerLast,
        // A fifty-line list is paged, not walked: `PgUp`/`PgDn` move a whole
        // screen the way they do in the transcript, so a human does not press
        // `j` fifty times to reach the model at the bottom (finding U10's
        // neighbour: a deep list is walked the same way a deep tree is).
        KeyCode::PageUp => Intent::PickerMove(-PAGE),
        KeyCode::PageDown => Intent::PickerMove(PAGE),
        _ => Intent::Ignore,
    }
}

/// The agent pane: the rows, and the two things to do with one.
fn tree(key: KeyEvent) -> Intent {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Intent::TreeMove(1),
        KeyCode::Char('k') | KeyCode::Up => Intent::TreeMove(-1),
        KeyCode::Char('g') | KeyCode::Home => Intent::TreeFirst,
        KeyCode::Char('G') | KeyCode::End => Intent::TreeLast,
        KeyCode::Enter => Intent::TreeFocus,
        KeyCode::Char('c') => Intent::TreeCancel,
        KeyCode::Esc => Intent::TreeBackToRoot,
        // `←`/`→` walk the parent links, `j`/`k` walk the rows. They are "up
        // and down the tree" rather than "previous and next row": with a
        // grandchild selected, `←` is its parent, not its uncle above it.
        KeyCode::Left => Intent::TreeWalk(-1),
        KeyCode::Right => Intent::TreeWalk(1),
        _ => Intent::Ignore,
    }
}

/// The chat pane: sending, the message box, and the scrollback. `<Enter>` is
/// the split — a plain one sends, a modified one is a newline — and it is the
/// only key here whose meaning depends on a modifier the terminal may not
/// report.
fn chat(key: KeyEvent) -> Intent {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) || alt => {
            Intent::Chat(ChatKey::Newline)
        }
        KeyCode::Enter => Intent::Send,
        KeyCode::Backspace => Intent::Chat(ChatKey::Backspace),
        KeyCode::Delete => Intent::Chat(ChatKey::Delete),
        KeyCode::Left => Intent::Chat(ChatKey::Left),
        KeyCode::Right => Intent::Chat(ChatKey::Right),
        KeyCode::Home => Intent::Chat(ChatKey::Home),
        KeyCode::End => Intent::Chat(ChatKey::End),
        // A `Ctrl-` or `Alt-` char that got past the app-wide keys above is a
        // shortcut mush does not have; typing it would be worse than dropping
        // it, and `Shift-` is how a capital letter arrives.
        KeyCode::Char(c) if !ctrl && !alt => Intent::Chat(ChatKey::Insert(c)),
        KeyCode::Up => Intent::Chat(ChatKey::Scroll(1)),
        KeyCode::Down => Intent::Chat(ChatKey::Scroll(-1)),
        KeyCode::PageUp => Intent::Chat(ChatKey::Scroll(PAGE)),
        KeyCode::PageDown => Intent::Chat(ChatKey::Scroll(-PAGE)),
        KeyCode::Esc => Intent::Chat(ChatKey::Clear),
        _ => Intent::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn at(focus: Focus, picker_open: bool, pressed: KeyEvent) -> Intent {
        key(focus, picker_open, pressed)
    }

    /// Every pane-wide binding, from each of the three contexts it can be
    /// pressed in. The point of the table being pure is this test: no `App`, no
    /// actor, no terminal.
    #[test]
    fn the_app_keys_work_from_every_pane_and_over_a_picker() {
        for focus in [Focus::Agents, Focus::Chat] {
            for picker_open in [false, true] {
                let cases = [
                    (ctrl('q'), Intent::Quit),
                    (ctrl('c'), Intent::Interrupt),
                    (ctrl('x'), Intent::InterruptAll),
                    (ctrl('n'), Intent::NewChat),
                    (ctrl('p'), Intent::OpenModelPicker),
                    (none(KeyCode::Tab), Intent::CycleFocus(1)),
                    (none(KeyCode::BackTab), Intent::CycleFocus(-1)),
                    // Shift-Tab is also reported as BackTab with SHIFT held.
                    (
                        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                        Intent::CycleFocus(-1),
                    ),
                ];
                for (key, want) in cases {
                    assert_eq!(at(focus, picker_open, key), want, "{key:?} {focus:?}");
                }
            }
        }
    }

    /// The agent pane owns the rows: moving, both ends, focusing, stopping, and
    /// the way back to the root.
    #[test]
    fn the_tree_owns_its_rows() {
        let cases = [
            (none(KeyCode::Char('j')), Intent::TreeMove(1)),
            (none(KeyCode::Down), Intent::TreeMove(1)),
            (none(KeyCode::Char('k')), Intent::TreeMove(-1)),
            (none(KeyCode::Up), Intent::TreeMove(-1)),
            (none(KeyCode::Char('g')), Intent::TreeFirst),
            (none(KeyCode::Home), Intent::TreeFirst),
            (none(KeyCode::Char('G')), Intent::TreeLast),
            (none(KeyCode::End), Intent::TreeLast),
            (none(KeyCode::Enter), Intent::TreeFocus),
            (none(KeyCode::Char('c')), Intent::TreeCancel),
            (none(KeyCode::Esc), Intent::TreeBackToRoot),
            (none(KeyCode::Left), Intent::TreeWalk(-1)),
            (none(KeyCode::Right), Intent::TreeWalk(1)),
        ];
        for (key, want) in cases {
            assert_eq!(at(Focus::Agents, false, key), want, "{key:?}");
        }
    }

    /// The chat pane owns the message box and the scrollback, key by key,
    /// including the ones whose meaning depends on a modifier the terminal may
    /// not report at all.
    #[test]
    fn the_chat_owns_its_box_and_its_scrollback() {
        let cases = [
            (none(KeyCode::Char('a')), Intent::Chat(ChatKey::Insert('a'))),
            // A capital arrives as a char with SHIFT: it is still typing.
            (
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                Intent::Chat(ChatKey::Insert('A')),
            ),
            (none(KeyCode::Backspace), Intent::Chat(ChatKey::Backspace)),
            (none(KeyCode::Delete), Intent::Chat(ChatKey::Delete)),
            (none(KeyCode::Left), Intent::Chat(ChatKey::Left)),
            (none(KeyCode::Right), Intent::Chat(ChatKey::Right)),
            (none(KeyCode::Home), Intent::Chat(ChatKey::Home)),
            (none(KeyCode::End), Intent::Chat(ChatKey::End)),
            (none(KeyCode::Up), Intent::Chat(ChatKey::Scroll(1))),
            (none(KeyCode::Down), Intent::Chat(ChatKey::Scroll(-1))),
            (none(KeyCode::PageUp), Intent::Chat(ChatKey::Scroll(10))),
            (none(KeyCode::PageDown), Intent::Chat(ChatKey::Scroll(-10))),
            (none(KeyCode::Esc), Intent::Chat(ChatKey::Clear)),
        ];
        for (key, want) in cases {
            assert_eq!(at(Focus::Chat, false, key), want, "{key:?}");
        }
    }

    /// `<Enter>` is the one key three panes share: send in the chat, focus a
    /// row in the tree, take a row in a picker.
    #[test]
    fn enter_means_whatever_the_pane_it_lands_in_means() {
        assert_eq!(at(Focus::Chat, false, none(KeyCode::Enter)), Intent::Send);
        assert_eq!(
            at(Focus::Agents, false, none(KeyCode::Enter)),
            Intent::TreeFocus
        );
        assert_eq!(
            at(Focus::Chat, true, none(KeyCode::Enter)),
            Intent::PickerPick
        );
    }

    /// A newline in the message is the modified Enter, in both spellings: the
    /// one the terminal reports (`Shift`) and the one it always can (`Alt`).
    #[test]
    fn a_modified_enter_is_a_newline_and_a_plain_one_sends() {
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let key = KeyEvent::new(KeyCode::Enter, modifiers);
            assert_eq!(
                at(Focus::Chat, false, key),
                Intent::Chat(ChatKey::Newline),
                "{modifiers:?}"
            );
        }
    }

    /// A picker takes the keyboard: the rows it owns are its own, and a key it
    /// has no opinion about is dropped rather than reaching the pane underneath
    /// — which is what stops a picker's `j` from also moving the tree, and its
    /// Enter from sending a message nobody can see it typing.
    #[test]
    fn a_picker_takes_the_keyboard_from_both_panes() {
        let cases = [
            (none(KeyCode::Esc), Intent::PickerClose),
            (none(KeyCode::Char('j')), Intent::PickerMove(1)),
            (none(KeyCode::Down), Intent::PickerMove(1)),
            (none(KeyCode::Char('k')), Intent::PickerMove(-1)),
            (none(KeyCode::Up), Intent::PickerMove(-1)),
            (none(KeyCode::Char('g')), Intent::PickerFirst),
            (none(KeyCode::Home), Intent::PickerFirst),
            (none(KeyCode::Char('G')), Intent::PickerLast),
            (none(KeyCode::End), Intent::PickerLast),
            (none(KeyCode::PageUp), Intent::PickerMove(-PAGE)),
            (none(KeyCode::PageDown), Intent::PickerMove(PAGE)),
        ];
        for (key, want) in cases {
            assert_eq!(at(Focus::Chat, true, key), want, "{key:?}");
            assert_eq!(at(Focus::Agents, true, key), want, "{key:?}");
        }

        // The keys the panes would have taken are not theirs while a picker is
        // up: no typing into the box behind it, no cursor in the tree.
        for focus in [Focus::Agents, Focus::Chat] {
            for key in [
                none(KeyCode::Char('a')),
                none(KeyCode::Backspace),
                none(KeyCode::Left),
                none(KeyCode::Up),
                none(KeyCode::PageDown),
                none(KeyCode::Enter),
                none(KeyCode::Esc),
            ] {
                let intent = at(focus, true, key);
                assert!(
                    !matches!(intent, Intent::Chat(_) | Intent::Send | Intent::TreeMove(_)),
                    "{key:?} in {focus:?} reached a pane: {intent:?}"
                );
            }
        }
    }

    /// A key nobody owns yields nothing — and a released key is not a key.
    #[test]
    fn a_key_that_means_nothing_yields_nothing() {
        for focus in [Focus::Agents, Focus::Chat] {
            for picker_open in [false, true] {
                for key in [
                    none(KeyCode::F(5)),
                    none(KeyCode::Insert),
                    none(KeyCode::Null),
                    none(KeyCode::CapsLock),
                    ctrl('a'),
                    ctrl('z'),
                    // Alt-char is not typing: it is a shortcut mush does not
                    // have, in either pane.
                    KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
                ] {
                    assert_eq!(
                        at(focus, picker_open, key),
                        Intent::Ignore,
                        "{key:?} {focus:?} picker={picker_open}"
                    );
                }
            }
        }
    }

    /// `Ctrl-C` reaches the app with a picker up (it is the one key a human
    /// presses at something that is not responding), and a release event never
    /// runs anything twice.
    #[test]
    fn ctrl_c_reaches_through_a_picker_and_a_release_does_nothing() {
        assert_eq!(
            at(Focus::Chat, true, ctrl('c')),
            Intent::Interrupt,
            "the emergency brake is not behind a modal"
        );

        for code in [KeyCode::Char('c'), KeyCode::Char('q'), KeyCode::Enter] {
            let released =
                KeyEvent::new_with_kind(code, KeyModifiers::CONTROL, KeyEventKind::Release);
            assert_eq!(at(Focus::Chat, false, released), Intent::Ignore);
        }
    }

    /// Where a pane never looked at the modifiers, this table does not start:
    /// `Alt-C` still stops a row, `Ctrl-J`/`Ctrl-K` still move the tree, and a
    /// modified `Enter` still focuses one. That is the old arms' behaviour down
    /// to the leak, kept on purpose — dropping a modifier test changes a key,
    /// and a new key is a decision for the human, not for a refactor.
    #[test]
    fn the_tree_still_ignores_modifiers_the_way_it_always_did() {
        let cases = [
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT),
                Intent::TreeCancel,
            ),
            (
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                Intent::TreeMove(1),
            ),
            (
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT),
                Intent::TreeMove(-1),
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                Intent::TreeFocus,
            ),
        ];
        for (key, want) in cases {
            assert_eq!(at(Focus::Agents, false, key), want, "{key:?}");
        }
    }

    /// A newline in the box is exactly `Shift-Enter` and `Alt-Enter`: a
    /// `Ctrl-Enter` carries no modifier the chat looks at, so it sends, the way
    /// it always has.
    #[test]
    fn only_shift_and_alt_make_enter_a_newline() {
        let control = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(at(Focus::Chat, false, control), Intent::Send);
    }

    /// The tree's keys are the tree's: the chat is not moved by `j`, and the
    /// tree does not scroll or type.
    #[test]
    fn neither_pane_answers_the_other_panes_keys() {
        assert_eq!(
            at(Focus::Chat, false, none(KeyCode::Char('j'))),
            Intent::Chat(ChatKey::Insert('j'))
        );
        assert_eq!(
            at(Focus::Agents, false, none(KeyCode::Backspace)),
            Intent::Ignore
        );
        assert_eq!(
            at(Focus::Agents, false, none(KeyCode::PageUp)),
            Intent::Ignore
        );
        assert_eq!(
            at(Focus::Agents, false, none(KeyCode::Char(' '))),
            Intent::Ignore
        );
    }

    /// `←`/`→` belong to whichever pane has the keyboard: the tree walks its
    /// parent links in the agents pane, and the chat keeps them for the message
    /// box cursor. Both halves are one row of this table, so "`←` moves the
    /// tree" cannot quietly become "`←` moves the box too".
    #[test]
    fn left_and_right_belong_to_whichever_pane_has_the_keyboard() {
        assert_eq!(
            at(Focus::Agents, false, none(KeyCode::Left)),
            Intent::TreeWalk(-1)
        );
        assert_eq!(
            at(Focus::Agents, false, none(KeyCode::Right)),
            Intent::TreeWalk(1)
        );
        assert_eq!(
            at(Focus::Chat, false, none(KeyCode::Left)),
            Intent::Chat(ChatKey::Left)
        );
        assert_eq!(
            at(Focus::Chat, false, none(KeyCode::Right)),
            Intent::Chat(ChatKey::Right)
        );
    }

    /// The help both surfaces print comes from [`KEYS`], so this is where a
    /// binding can be lost: every row must be in the rendered table, each
    /// context must head its rows once, and the real scroll keys — not the
    /// wheel the terminal never sends, because mouse capture is not taken
    /// (finding K3) — must be the ones named.
    #[test]
    fn the_help_table_shows_every_binding_once() {
        let table = help_table();
        for binding in KEYS {
            assert!(!binding.keys.is_empty(), "a row with no keys");
            assert!(
                table.contains(binding.keys),
                "{} is missing from the help table:\n{table}",
                binding.keys
            );
            assert!(
                table.contains(binding.help),
                "{} is missing from the help table:\n{table}",
                binding.help
            );
        }
        for context in [
            Context::Anywhere,
            Context::Picker,
            Context::Agents,
            Context::Chat,
        ] {
            assert_eq!(
                table.matches(context.label()).count(),
                1,
                "{:?} does not head its rows exactly once:\n{table}",
                context
            );
        }
        assert!(
            table.contains("↑ / ↓, PgUp / PgDn"),
            "the transcript's real scroll keys are named:\n{table}"
        );
        assert!(
            !table.contains("wheel"),
            "`--help` advertised a wheel it never scrolls:\n{table}"
        );
    }

    /// The two stop keys name their scope, so the help cannot repeat the doc's
    /// old "`Ctrl-C` cancel running agents" (plural): `Ctrl-C` stops the
    /// *focused* agent and `Ctrl-X` stops every running one (the code side of
    /// finding K5). A doc that swaps them is then contradicted by the surface a
    /// human reads.
    #[test]
    fn the_two_stop_keys_name_their_scope() {
        let help = |keys: &str| {
            KEYS.iter()
                .find(|binding| binding.keys == keys)
                .unwrap_or_else(|| panic!("no `{keys}` row"))
                .help
        };
        assert_eq!(help("Ctrl-C"), "stop the focused agent");
        assert_eq!(help("Ctrl-X"), "stop every running agent");
        assert_ne!(
            help("Ctrl-C"),
            help("Ctrl-X"),
            "the two scopes are not the same key's job"
        );
    }
}
