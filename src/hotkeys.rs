//! Custom hotkeys (decision 41): the user's key overrides on top of the
//! default keymap (`app::shortcuts`). Pure functions, NO ui code here.
//!
//! Overrides live in `state.toml`, keyed by the command's name (its
//! `commands!` name, which is also its GPUI action's name without the
//! `notesec::` namespace):
//!
//! ```toml
//! [shortcuts]
//! SplitRight = "ctrl-alt-s"   # one keystroke, in GPUI's binding syntax
//! Quit = ""                   # unbound
//! ```
//!
//! A command's binding lives in one key context (`Command::key_context`),
//! so the name alone identifies it. An override replaces every default key
//! of its command (Redo's Ctrl+Shift+Z and Ctrl+Y become the one new key).
//! Names that aren't commands (yet, or any more) and values that aren't a
//! valid key are ignored: the command keeps its default keys. They stay in
//! the file, so a newer or older build can still use them.

use crate::app::{shortcuts, KeyGroup, Shortcut};
use crate::commands::{format_keystrokes, Command};
use gpui::{Action, KeybindingKeystroke, Keystroke};
use std::collections::BTreeMap;

/// Command name -> keystroke (`""` means unbound), as stored in state.toml.
pub type Overrides = BTreeMap<String, String>;

/// What an override asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyChoice {
    /// No key at all (palette only).
    Unbound,
    /// One keystroke, in the form GPUI's binding parser reads
    /// (`Keystroke::unparse`), e.g. `ctrl-|`.
    Key(String),
}

impl KeyChoice {
    /// The value written to state.toml.
    pub fn to_value(&self) -> String {
        match self {
            KeyChoice::Unbound => String::new(),
            KeyChoice::Key(key) => key.clone(),
        }
    }
}

/// Keys GPUI reports for a modifier pressed and released on its own.
pub fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "shift"
            | "control"
            | "ctrl"
            | "alt"
            | "platform"
            | "cmd"
            | "super"
            | "win"
            | "function"
            | "fn"
    )
}

/// F1 to F24.
fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n))
}

/// Whether `keystroke` may be a command's key. Every command works while a
/// block (or the palette's query) takes typing, so a key needs Ctrl, Alt or
/// Super, unless it is a function key (F1-F24, bare or not): a plain or
/// Shift+ key would type text instead.
pub fn check_key(keystroke: &Keystroke) -> Result<(), String> {
    if keystroke.key.is_empty() || is_modifier_key(&keystroke.key) {
        return Err("Press a key, not just a modifier".into());
    }
    let m = &keystroke.modifiers;
    if m.control || m.alt || m.platform || is_function_key(&keystroke.key) {
        Ok(())
    } else {
        Err(format!(
            "{} would type text: add Ctrl, Alt or Super (F1-F24 work alone)",
            pretty(keystroke)
        ))
    }
}

/// `value` from state.toml as a choice: `""` is unbound, anything else must
/// be one valid keystroke that `check_key` accepts.
pub fn parse_choice(value: &str) -> Result<KeyChoice, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(KeyChoice::Unbound);
    }
    if value.split_whitespace().count() != 1 {
        return Err(format!("\u{201c}{value}\u{201d} is more than one key"));
    }
    let keystroke =
        Keystroke::parse(value).map_err(|_| format!("\u{201c}{value}\u{201d} is not a key"))?;
    check_key(&keystroke)?;
    Ok(KeyChoice::Key(keystroke.unparse()))
}

/// The valid override for `command`, if any.
pub fn override_for(overrides: &Overrides, command: Command) -> Option<KeyChoice> {
    overrides
        .get(command.name())
        .and_then(|value| parse_choice(value).ok())
}

fn is_command_row(shortcut: &Shortcut, action: &dyn Action) -> bool {
    shortcut.binding.action().partial_eq(action)
}

/// The keymap in effect: the default table with each valid override
/// applied. An overridden command loses all its default rows; its new key
/// (if any) keeps the group and description of its first default row (a
/// command without a default key goes under App, described by its label).
pub fn effective_shortcuts(overrides: &Overrides) -> Vec<Shortcut> {
    let mut table = shortcuts();
    for &command in Command::ALL {
        let Some(choice) = override_for(overrides, command) else {
            continue;
        };
        let action = command.action();
        let (group, description) = table
            .iter()
            .find(|s| is_command_row(s, action.as_ref()))
            .map_or((KeyGroup::App, command.label()), |s| {
                (s.group, s.description)
            });
        table.retain(|s| !is_command_row(s, action.as_ref()));
        if let KeyChoice::Key(key) = choice {
            table.push(Shortcut {
                binding: command.binding(&key),
                context: command.key_context(),
                group,
                description,
            });
        }
    }
    table
}

/// `command`'s keys in `table`, written for people ("Ctrl+Shift+Z",
/// "Ctrl+Y"), main key first.
pub fn command_keys(table: &[Shortcut], command: Command) -> Vec<String> {
    let action = command.action();
    table
        .iter()
        .filter(|s| is_command_row(s, action.as_ref()))
        .map(|s| format_keystrokes(s.binding.keystrokes()))
        .collect()
}

/// `keystroke` written for people.
pub fn pretty(keystroke: &Keystroke) -> String {
    format_keystrokes(&[KeybindingKeystroke::from_keystroke(keystroke.clone())])
}

/// A pressed key as a choice: `check_key`, and its written form must read
/// back as the same key, so the binding matches when it is pressed again.
pub fn choice_for(keystroke: &Keystroke) -> Result<KeyChoice, String> {
    check_key(keystroke)?;
    let written = keystroke.unparse();
    match Keystroke::parse(&written) {
        Ok(back) if back.key == keystroke.key && back.modifiers == keystroke.modifiers => {
            Ok(KeyChoice::Key(written))
        }
        _ => Err(format!("{} can't be used as a shortcut", pretty(keystroke))),
    }
}

/// What already uses a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOwner {
    /// Another command (it can be rebound or unbound first).
    Command(Command),
    /// A fixed key (search, the editing keys, Esc in dialogs), described
    /// as in the shortcuts list.
    Fixed(&'static str),
}

/// Who else in `table` answers `keystroke` where `command` would. Contexts
/// overlap when either binding is global (no context) or both are the
/// same; the dialog contexts never overlap "BlockEditor", since the root
/// has one context at a time.
pub fn conflict(table: &[Shortcut], command: Command, keystroke: &Keystroke) -> Option<KeyOwner> {
    let action = command.action();
    let context = command.key_context();
    table
        .iter()
        .filter(|s| !is_command_row(s, action.as_ref()))
        .filter(|s| s.context.is_none() || context.is_none() || s.context == context)
        .find(|s| match s.binding.keystrokes() {
            [only] => {
                only.inner().key == keystroke.key && only.inner().modifiers == keystroke.modifiers
            }
            _ => false,
        })
        .map(|s| {
            match Command::ALL
                .iter()
                .find(|c| is_command_row(s, c.action().as_ref()))
            {
                Some(&c) => KeyOwner::Command(c),
                None => KeyOwner::Fixed(s.description),
            }
        })
}

/// The override that gives `command` the keys `choice` asks for: `None`
/// (no override at all) when that is exactly its default, so choosing a
/// command's own default key again doesn't mark it as changed.
pub fn override_value(command: Command, choice: &KeyChoice) -> Option<String> {
    let defaults = command_keys(&shortcuts(), command);
    let same = match choice {
        KeyChoice::Unbound => defaults.is_empty(),
        KeyChoice::Key(key) => {
            let binding = command.binding(key);
            defaults == [format_keystrokes(binding.keystrokes())]
        }
    };
    (!same).then(|| choice.to_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> Overrides {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn key(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn choices_need_a_modifier_or_a_function_key() {
        assert_eq!(parse_choice(""), Ok(KeyChoice::Unbound));
        assert_eq!(parse_choice("  "), Ok(KeyChoice::Unbound));
        assert_eq!(
            parse_choice("ctrl-alt-s"),
            Ok(KeyChoice::Key("ctrl-alt-s".into()))
        );
        assert_eq!(parse_choice("alt-p"), Ok(KeyChoice::Key("alt-p".into())));
        assert_eq!(
            parse_choice("super-k"),
            Ok(KeyChoice::Key("super-k".into()))
        );
        assert_eq!(parse_choice("f5"), Ok(KeyChoice::Key("f5".into())));
        assert_eq!(
            parse_choice("shift-f12"),
            Ok(KeyChoice::Key("shift-f12".into()))
        );
        // Shift+\ as Linux reports it round-trips.
        assert_eq!(parse_choice("ctrl-|"), Ok(KeyChoice::Key("ctrl-|".into())));
        for bad in [
            "a",
            "shift-a",
            "enter",
            "f25",
            "escape",
            "ctrl-k ctrl-s",
            "ctrl-",
            "shift",
        ] {
            assert!(parse_choice(bad).is_err(), "{bad}");
        }
        assert!(check_key(&key("x"))
            .unwrap_err()
            .contains("X would type text"));
        // A pressed key round-trips through its written form.
        for k in [
            "ctrl-|",
            "ctrl-\\",
            "ctrl--",
            "ctrl-shift-w",
            "alt-f4",
            "f9",
            "super-,",
        ] {
            assert_eq!(
                choice_for(&key(k)),
                Ok(KeyChoice::Key(k.to_string())),
                "{k}"
            );
        }
        assert!(choice_for(&key("shift-x")).is_err());
    }

    #[test]
    fn overrides_replace_every_default_key_of_their_command() {
        let defaults = shortcuts();
        let table = effective_shortcuts(&overrides(&[
            ("Redo", "ctrl-r"),
            ("Quit", ""),
            ("OpenAgenda", "ctrl-alt-a"),
            ("CycleTask", "alt-t"),
        ]));
        assert_eq!(
            command_keys(&defaults, Command::Redo),
            ["Ctrl+Shift+Z", "Ctrl+Y"]
        );
        assert_eq!(command_keys(&table, Command::Redo), ["Ctrl+R"]);
        assert!(command_keys(&table, Command::Quit).is_empty());
        assert_eq!(command_keys(&table, Command::OpenAgenda), ["Ctrl+Alt+A"]);
        // The editor-only command stays in its context.
        let cycle = table
            .iter()
            .find(|s| s.binding.action().partial_eq(&crate::app::CycleTask))
            .unwrap();
        assert_eq!(cycle.context, Some("BlockEditor"));
        // Untouched commands keep their keys; one row per new key.
        assert_eq!(command_keys(&table, Command::Undo), ["Ctrl+Z"]);
        assert_eq!(table.len(), defaults.len() - 2 - 1 + 1 + 1);
    }

    #[test]
    fn unknown_names_and_invalid_keys_are_ignored() {
        let table = effective_shortcuts(&overrides(&[
            ("NoSuchCommand", "ctrl-alt-x"),
            ("Undo", "ctrl-nope-z"),
            ("Redo", "y"),
            ("Quit", "ctrl-q ctrl-q"),
        ]));
        let keys = |t: &[Shortcut]| -> Vec<String> {
            t.iter()
                .map(|s| format_keystrokes(s.binding.keystrokes()))
                .collect()
        };
        assert_eq!(keys(&table), keys(&shortcuts()));
    }

    #[test]
    fn conflicts_are_found_where_contexts_overlap() {
        let table = shortcuts();
        assert_eq!(
            conflict(&table, Command::Quit, &key("ctrl-g")),
            Some(KeyOwner::Command(Command::ToggleGraph))
        );
        // A global key against an editor-only one, both ways.
        assert_eq!(
            conflict(&table, Command::Quit, &key("ctrl-enter")),
            Some(KeyOwner::Command(Command::CycleTask))
        );
        assert_eq!(
            conflict(&table, Command::CycleTask, &key("ctrl-g")),
            Some(KeyOwner::Command(Command::ToggleGraph))
        );
        assert_eq!(
            conflict(&table, Command::Quit, &key("ctrl-k")),
            Some(KeyOwner::Fixed("Search pages, blocks and commands"))
        );
        assert_eq!(
            conflict(&table, Command::Quit, &key("ctrl-b")),
            Some(KeyOwner::Fixed("Bold"))
        );
        // Its own key, and free keys, are fine; modifiers must match.
        assert_eq!(conflict(&table, Command::Quit, &key("ctrl-q")), None);
        assert_eq!(conflict(&table, Command::Quit, &key("ctrl-alt-g")), None);
        assert_eq!(conflict(&table, Command::Quit, &key("ctrl-shift-g")), None);
    }

    #[test]
    fn choosing_the_default_again_is_no_override() {
        assert_eq!(
            override_value(Command::Undo, &KeyChoice::Key("ctrl-z".into())),
            None
        );
        assert_eq!(
            override_value(Command::Undo, &KeyChoice::Key("ctrl-u".into())),
            Some("ctrl-u".into())
        );
        assert_eq!(
            override_value(Command::OpenAgenda, &KeyChoice::Unbound),
            None
        );
        assert_eq!(
            override_value(Command::Undo, &KeyChoice::Unbound),
            Some(String::new())
        );
        // One of two default keys is still a change.
        assert_eq!(
            override_value(Command::Redo, &KeyChoice::Key("ctrl-y".into())),
            Some("ctrl-y".into())
        );
    }

    #[test]
    fn each_command_binds_only_in_its_own_context() {
        for s in shortcuts() {
            if let Some(c) = Command::ALL
                .iter()
                .find(|c| is_command_row(&s, c.action().as_ref()))
            {
                assert_eq!(s.context, c.key_context(), "{c:?}");
            }
        }
    }
}
