//! THE keymap: every key the TUI understands, in one auditable table.
//!
//! `map_key` is a pure translation from (mode, tab, key) to an [`Action`];
//! `app.rs` applies actions. The help overlay is generated from
//! [`help_sections`], and a unit test proves every help entry actually
//! maps to an action in its context - the two can't drift apart silently.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::Tab;

/// Which mode the key arrived in - a projection of `app::Mode` without
/// its payloads, so this module stays payload-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeKind {
    Normal,
    Input,
    Confirm,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyCtx {
    pub mode: ModeKind,
    pub tab: Tab,
    pub tasks_output_focused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    CtrlC,
    Help,
    GoTab(Tab),
    NextTab,
    PrevTab,
    Move(i64),
    Page(i64),
    Home,
    End,
    Reload,
    RefreshDbs,
    Escape,
    EnterInput,
    ToggleMark,
    MarkAllVisible,
    CycleSort,
    ToggleSortDir,
    ToggleDetail,
    Install,
    Remove,
    Upgrade,
    UpgradeAll,
    ToggleManager,
    TasksFocusOutput,
    TasksUnfocusOutput,
    TasksToggleFollow,
    TasksCancel,
    TasksClearFinished,
    InputChar(char),
    InputBackspace,
    InputClear,
    InputSubmit,
    InputCancel,
    ConfirmYes,
    ConfirmNo,
    ConfirmMove,
    ConfirmActivate,
    HelpClose,
    /// Key is real but meaningless here; show where it works instead.
    Hint(&'static str),
}

pub fn map_key(ctx: KeyCtx, key: KeyEvent) -> Option<Action> {
    // Ctrl-C outranks every mode: quit request (guarded), or force-quit
    // inside the quit-guard modal.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Action::CtrlC);
    }
    match ctx.mode {
        ModeKind::Help => help_key(key),
        ModeKind::Confirm => confirm_key(key),
        ModeKind::Input => input_key(key),
        ModeKind::Normal => normal_key(ctx, key),
    }
}

/// Any key collapses the overlay - it is a cheat sheet, not a place to be.
fn help_key(_key: KeyEvent) -> Option<Action> {
    Some(Action::HelpClose)
}

fn confirm_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Action::ConfirmYes),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Action::ConfirmNo),
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            Some(Action::ConfirmMove)
        }
        KeyCode::Char('h') | KeyCode::Char('l') => Some(Action::ConfirmMove),
        KeyCode::Enter => Some(Action::ConfirmActivate),
        _ => None,
    }
}

fn input_key(key: KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('u') => Some(Action::InputClear),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Enter => Some(Action::InputSubmit),
        KeyCode::Esc => Some(Action::InputCancel),
        KeyCode::Backspace => Some(Action::InputBackspace),
        KeyCode::Char(c) => Some(Action::InputChar(c)),
        _ => None,
    }
}

fn normal_key(ctx: KeyCtx, key: KeyEvent) -> Option<Action> {
    let package_tab = matches!(ctx.tab, Tab::Search | Tab::Installed | Tab::Outdated);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('d') => Some(Action::Page(10)),
            KeyCode::Char('u') => Some(Action::Page(-10)),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::Help),
        KeyCode::Char('1') => Some(Action::GoTab(Tab::Search)),
        KeyCode::Char('2') => Some(Action::GoTab(Tab::Installed)),
        KeyCode::Char('3') => Some(Action::GoTab(Tab::Outdated)),
        KeyCode::Char('4') => Some(Action::GoTab(Tab::Managers)),
        KeyCode::Char('5') => Some(Action::GoTab(Tab::Tasks)),
        KeyCode::Tab | KeyCode::Char(']') => Some(Action::NextTab),
        KeyCode::BackTab | KeyCode::Char('[') => Some(Action::PrevTab),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::Move(1)),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::Move(-1)),
        KeyCode::Char('g') | KeyCode::Home => Some(Action::Home),
        KeyCode::Char('G') | KeyCode::End => Some(Action::End),
        KeyCode::PageDown => Some(Action::Page(10)),
        KeyCode::PageUp => Some(Action::Page(-10)),
        KeyCode::Char('r') => Some(Action::Reload),
        KeyCode::Char('R') => Some(Action::RefreshDbs),
        KeyCode::Esc => {
            if ctx.tab == Tab::Tasks && ctx.tasks_output_focused {
                Some(Action::TasksUnfocusOutput)
            } else {
                Some(Action::Escape)
            }
        }
        KeyCode::Char('/') => {
            if package_tab {
                Some(Action::EnterInput)
            } else {
                Some(Action::Hint("filtering lives on tabs 1-3"))
            }
        }
        KeyCode::Char(' ') => match ctx.tab {
            Tab::Search | Tab::Installed | Tab::Outdated => Some(Action::ToggleMark),
            Tab::Managers => Some(Action::ToggleManager),
            Tab::Tasks => None,
        },
        KeyCode::Char('v') => {
            if package_tab {
                Some(Action::MarkAllVisible)
            } else {
                Some(Action::Hint("marking lives on tabs 1-3"))
            }
        }
        KeyCode::Char('s') => {
            if package_tab {
                Some(Action::CycleSort)
            } else {
                Some(Action::Hint("sorting lives on tabs 1-3"))
            }
        }
        KeyCode::Char('S') => {
            if package_tab {
                Some(Action::ToggleSortDir)
            } else {
                Some(Action::Hint("sorting lives on tabs 1-3"))
            }
        }
        KeyCode::Enter => match ctx.tab {
            Tab::Search | Tab::Installed | Tab::Outdated => Some(Action::ToggleDetail),
            Tab::Managers => Some(Action::ToggleManager),
            Tab::Tasks => {
                if ctx.tasks_output_focused {
                    Some(Action::TasksUnfocusOutput)
                } else {
                    Some(Action::TasksFocusOutput)
                }
            }
        },
        KeyCode::Char('i') => {
            if package_tab {
                Some(Action::Install)
            } else {
                Some(Action::Hint("select a package on tabs 1-3 first"))
            }
        }
        KeyCode::Char('d') => {
            if package_tab {
                Some(Action::Remove)
            } else {
                Some(Action::Hint("select a package on tabs 1-3 first"))
            }
        }
        KeyCode::Char('u') => {
            if package_tab {
                Some(Action::Upgrade)
            } else {
                Some(Action::Hint("select a package on tabs 1-3 first"))
            }
        }
        KeyCode::Char('U') => match ctx.tab {
            Tab::Outdated => Some(Action::UpgradeAll),
            _ => Some(Action::Hint("upgrade-all runs from the Outdated tab (3)")),
        },
        KeyCode::Char('f') => {
            if ctx.tab == Tab::Tasks {
                Some(Action::TasksToggleFollow)
            } else {
                None
            }
        }
        KeyCode::Char('x') => {
            if ctx.tab == Tab::Tasks {
                Some(Action::TasksCancel)
            } else {
                Some(Action::Hint("cancel tasks from the Tasks tab (5)"))
            }
        }
        KeyCode::Char('C') => {
            if ctx.tab == Tab::Tasks {
                Some(Action::TasksClearFinished)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Keybind overlay content: `(key, action, description)` rows, most
/// commonly used first, one action per row - never a grouped mystery like
/// "i/s/u". A key sharing a row shares the exact same action (synonyms
/// only). Every row is probed by the keymap test below, so the overlay
/// can't promise a key that does nothing.
pub type HelpEntry = (&'static str, &'static str, &'static str);

pub fn help_sections() -> &'static [(&'static str, &'static [HelpEntry])] {
    &[
        (
            "Global",
            &[
                ("q", "quit", "exit snow (guarded while a mutation runs)"),
                ("j / ↓", "move down", "select the next row"),
                ("k / ↑", "move up", "select the previous row"),
                ("Tab / ]", "next tab", ""),
                ("Shift-Tab / [", "previous tab", ""),
                (
                    "1-5",
                    "jump to tab",
                    "Search, Installed, Outdated, Managers, Tasks",
                ),
                ("r", "reload", "re-run this tab's data fetch"),
                (
                    "R",
                    "refresh databases",
                    "index refresh on every enabled manager (confirmed)",
                ),
                ("g / Home", "first row", ""),
                ("G / End", "last row", ""),
                ("Ctrl-d / PgDn", "page down", ""),
                ("Ctrl-u / PgUp", "page up", ""),
                (
                    "Esc",
                    "back",
                    "cancel fetch, else clear marks, else clear filter",
                ),
                ("?", "keybinds", "this overlay; any key closes it"),
                (
                    "Ctrl-C",
                    "quit",
                    "same guard as q; force-quits from the quit dialog",
                ),
            ],
        ),
        (
            "Packages (Search / Installed / Outdated)",
            &[
                (
                    "/",
                    "search / filter",
                    "type a query (Search) or narrow rows (others)",
                ),
                ("i", "install", "install the marked-or-selected packages"),
                ("d", "remove", "remove the marked-or-selected packages"),
                ("u", "upgrade", "upgrade the marked-or-selected packages"),
                (
                    "U",
                    "upgrade all",
                    "everything at once, from the Outdated tab",
                ),
                (
                    "Space",
                    "mark",
                    "mark the row for a batch action, then advance",
                ),
                (
                    "v",
                    "mark visible",
                    "mark every visible row (again to unmark)",
                ),
                (
                    "Enter",
                    "details",
                    "expand or collapse the selected package's details",
                ),
                ("s", "sort", "cycle the sort column"),
                ("S", "sort direction", "flip ascending / descending"),
            ],
        ),
        (
            "Search input",
            &[
                (
                    "Enter",
                    "run search",
                    "fan out; `@manager terms` searches only that manager",
                ),
                ("Esc", "browse results", "leave the input; / returns to it"),
                ("Ctrl-U", "clear", "wipe the query"),
            ],
        ),
        (
            "Managers",
            &[(
                "Space / Enter",
                "toggle manager",
                "enable / disable it (persisted to config)",
            )],
        ),
        (
            "Tasks",
            &[
                (
                    "Enter",
                    "focus output",
                    "scroll the selected task's output; Esc unfocuses",
                ),
                ("f", "follow", "auto-scroll output as it streams"),
                (
                    "x",
                    "cancel task",
                    "kill the selected running task (confirmed for mutations)",
                ),
                ("C", "clear finished", "drop completed tasks from the list"),
            ],
        ),
        (
            "Dialogs",
            &[
                ("y", "confirm", ""),
                ("n / Esc", "cancel", ""),
                ("← / → / Tab", "pick button", ""),
                ("Enter", "activate", "press the highlighted button"),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn ctx(mode: ModeKind, tab: Tab) -> KeyCtx {
        KeyCtx {
            mode,
            tab,
            tasks_output_focused: false,
        }
    }

    /// Every key the help overlay names, probed in the context its section
    /// documents. If a help row stops mapping to an action, this fails.
    #[test]
    fn every_help_entry_maps_to_an_action() {
        let normal = |tab: Tab, k: KeyEvent| map_key(ctx(ModeKind::Normal, tab), k);
        // Global section.
        for k in [
            key(KeyCode::Char('q')),
            ctrl('c'),
            key(KeyCode::Char('?')),
            key(KeyCode::Char('1')),
            key(KeyCode::Char('2')),
            key(KeyCode::Char('3')),
            key(KeyCode::Char('4')),
            key(KeyCode::Char('5')),
            key(KeyCode::Tab),
            key(KeyCode::Char(']')),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('j')),
            key(KeyCode::Char('k')),
            key(KeyCode::Char('g')),
            key(KeyCode::Char('G')),
            ctrl('d'),
            ctrl('u'),
            key(KeyCode::Char('r')),
            key(KeyCode::Char('R')),
            key(KeyCode::Esc),
        ] {
            for tab in [
                Tab::Search,
                Tab::Installed,
                Tab::Outdated,
                Tab::Managers,
                Tab::Tasks,
            ] {
                assert!(normal(tab, k).is_some(), "dead global key {k:?} on {tab:?}");
            }
        }
        // Package section - must be real actions, not hints.
        for k in [
            key(KeyCode::Char('/')),
            key(KeyCode::Char(' ')),
            key(KeyCode::Char('v')),
            key(KeyCode::Char('s')),
            key(KeyCode::Char('S')),
            key(KeyCode::Enter),
            key(KeyCode::Char('i')),
            key(KeyCode::Char('d')),
            key(KeyCode::Char('u')),
        ] {
            for tab in [Tab::Search, Tab::Installed, Tab::Outdated] {
                let action = normal(tab, k);
                assert!(
                    action.is_some() && !matches!(action, Some(Action::Hint(_))),
                    "package key {k:?} not a real action on {tab:?}"
                );
            }
        }
        assert_eq!(
            normal(Tab::Outdated, key(KeyCode::Char('U'))),
            Some(Action::UpgradeAll)
        );
        // Managers / Tasks sections.
        assert_eq!(
            normal(Tab::Managers, key(KeyCode::Char(' '))),
            Some(Action::ToggleManager)
        );
        assert_eq!(
            normal(Tab::Managers, key(KeyCode::Enter)),
            Some(Action::ToggleManager)
        );
        assert_eq!(
            normal(Tab::Tasks, key(KeyCode::Enter)),
            Some(Action::TasksFocusOutput)
        );
        for (k, expected) in [
            (key(KeyCode::Char('f')), Action::TasksToggleFollow),
            (key(KeyCode::Char('x')), Action::TasksCancel),
            (key(KeyCode::Char('C')), Action::TasksClearFinished),
        ] {
            assert_eq!(normal(Tab::Tasks, k), Some(expected));
        }
        // Dialog keys.
        for k in [
            key(KeyCode::Char('y')),
            key(KeyCode::Char('n')),
            key(KeyCode::Esc),
            key(KeyCode::Left),
            key(KeyCode::Right),
            key(KeyCode::Enter),
        ] {
            assert!(map_key(ctx(ModeKind::Confirm, Tab::Search), k).is_some());
        }
        // Input mode fundamentals.
        for k in [
            key(KeyCode::Char('a')),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            ctrl('u'),
        ] {
            assert!(map_key(ctx(ModeKind::Input, Tab::Search), k).is_some());
        }
    }

    /// The overlay collapses on ANY key - there is no key it swallows.
    #[test]
    fn any_key_closes_help() {
        for k in [
            key(KeyCode::Char('?')),
            key(KeyCode::Char('q')),
            key(KeyCode::Esc),
            key(KeyCode::Char('j')),
            key(KeyCode::Enter),
            key(KeyCode::PageDown),
            key(KeyCode::Char('x')),
        ] {
            assert_eq!(
                map_key(ctx(ModeKind::Help, Tab::Search), k),
                Some(Action::HelpClose),
                "help did not close on {k:?}"
            );
        }
    }

    #[test]
    fn no_dead_keys_at_launch() {
        // Launch state is the Search tab with the query input focused:
        // typing goes straight into the query, Enter runs it, Esc drops to
        // browsing, Ctrl-C still quits.
        let launch = ctx(ModeKind::Input, Tab::Search);
        assert_eq!(
            map_key(launch, key(KeyCode::Char('v'))),
            Some(Action::InputChar('v'))
        );
        assert_eq!(
            map_key(launch, key(KeyCode::Enter)),
            Some(Action::InputSubmit)
        );
        assert_eq!(
            map_key(launch, key(KeyCode::Esc)),
            Some(Action::InputCancel)
        );
        assert_eq!(map_key(launch, ctrl('c')), Some(Action::CtrlC));
        // And once browsing, the footer's advertised keys all act.
        let browsing = ctx(ModeKind::Normal, Tab::Search);
        for k in [
            key(KeyCode::Char('q')),
            key(KeyCode::Char('/')),
            key(KeyCode::Char('?')),
            key(KeyCode::Tab),
            key(KeyCode::Char('j')),
            key(KeyCode::Char('2')),
        ] {
            assert!(map_key(browsing, k).is_some(), "dead launch key: {k:?}");
        }
    }

    #[test]
    fn esc_unfocuses_tasks_output_first() {
        let focused = KeyCtx {
            mode: ModeKind::Normal,
            tab: Tab::Tasks,
            tasks_output_focused: true,
        };
        assert_eq!(
            map_key(focused, key(KeyCode::Esc)),
            Some(Action::TasksUnfocusOutput)
        );
    }
}
