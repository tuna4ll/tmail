//! Custom key bindings from the `[keys]` table, such as `archive = "e"`.
//!
//! A custom key is translated into the action's built-in key before the normal key
//! handling runs, so every screen and hint keeps working. The built-in key stays bound.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Every action that can be rebound, with its built-in key.
pub const ACTIONS: &[(&str, char, &str)] = &[
    ("compose", 'c', "write a new message"),
    ("reply", 'r', "reply"),
    ("reply_all", 'A', "reply to everyone"),
    ("forward", 'F', "forward"),
    ("archive", 'a', "archive"),
    ("trash", 'd', "move to Trash"),
    ("toggle_read", 'm', "mark read or unread"),
    ("toggle_star", 's', "star or unstar"),
    ("undo", 'u', "undo"),
    ("move", 'M', "move to a folder"),
    ("folders", 'g', "folders"),
    ("accounts", 'p', "accounts"),
    ("search", '/', "search"),
    ("help", '?', "help"),
    ("command", ':', "command line"),
    ("links", 'l', "links"),
    ("attachments", 'v', "attachments"),
    ("images", 'I', "load remote images"),
    ("browser", 'O', "open in the browser"),
    ("download", 'D', "download a large message"),
    ("unsubscribe", 'U', "leave a mailing list"),
    ("reload", 'R', "reload"),
    ("next", 'n', "next conversation"),
    ("previous", 'N', "previous conversation"),
    ("retry", 'x', "retry an Outbox message"),
    ("review", 'z', "resolve an unconfirmed delivery"),
    ("quit", 'q', "quit"),
];

#[derive(Clone, Debug, Default)]
pub struct Keymap {
    bindings: HashMap<(KeyCode, KeyModifiers), char>,
    /// The custom bindings by action.
    pub custom: Vec<(String, String)>,
}

impl Keymap {
    pub fn from_config(keys: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut keymap = Self::default();
        for (action, key) in keys {
            let Some((_, default, _)) = ACTIONS.iter().find(|(name, ..)| name == action) else {
                return Err(format!(
                    "Unknown action {action} in [keys]. Known actions: {}.",
                    ACTIONS
                        .iter()
                        .map(|(name, ..)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            let parsed =
                parse_key(key).ok_or_else(|| format!("Invalid key {key} for {action}."))?;
            if keymap.bindings.insert(parsed, *default).is_some() {
                return Err(format!("The key {key} is bound twice in [keys]."));
            }
            keymap.custom.push((action.clone(), key.clone()));
        }
        Ok(keymap)
    }

    /// The built-in key for a custom binding, or the key unchanged.
    pub fn translate(&self, key: KeyEvent) -> KeyEvent {
        let modifiers = key.modifiers - KeyModifiers::SHIFT;
        match self.bindings.get(&(key.code, modifiers)) {
            Some(default) => KeyEvent::new(KeyCode::Char(*default), KeyModifiers::NONE),
            None => key,
        }
    }
}

/// Reads `e`, `E`, `ctrl+e`, `alt+x`, `f5`, `enter`, `tab`, `delete` and similar.
pub fn parse_key(value: &str) -> Option<(KeyCode, KeyModifiers)> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = value.trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        if let Some(stripped) = lower.strip_prefix("ctrl+") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[rest.len() - stripped.len()..];
        } else if let Some(stripped) = lower.strip_prefix("alt+") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[rest.len() - stripped.len()..];
        } else {
            break;
        }
    }
    let mut characters = rest.chars();
    let code = match (characters.next(), characters.next()) {
        (Some(character), None) => KeyCode::Char(if modifiers.is_empty() {
            character
        } else {
            character.to_ascii_lowercase()
        }),
        _ => match rest.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "space" => KeyCode::Char(' '),
            name => KeyCode::F(
                name.strip_prefix('f')?
                    .parse()
                    .ok()
                    .filter(|n| (1..=12).contains(n))?,
            ),
        },
    };
    Some((code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_keys_and_translates_custom_bindings() {
        assert_eq!(
            parse_key("ctrl+E"),
            Some((KeyCode::Char('e'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key("E"),
            Some((KeyCode::Char('E'), KeyModifiers::NONE))
        );
        assert_eq!(parse_key("f5"), Some((KeyCode::F(5), KeyModifiers::NONE)));
        assert_eq!(parse_key("f13"), None);
        let keys = BTreeMap::from([("archive".to_owned(), "e".to_owned())]);
        let keymap = Keymap::from_config(&keys).unwrap();
        let pressed = keymap.translate(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(pressed.code, KeyCode::Char('a'));
        let unknown = BTreeMap::from([("fly".to_owned(), "e".to_owned())]);
        assert!(
            Keymap::from_config(&unknown)
                .unwrap_err()
                .contains("Unknown action")
        );
        let twice = BTreeMap::from([
            ("archive".to_owned(), "e".to_owned()),
            ("trash".to_owned(), "e".to_owned()),
        ]);
        assert!(Keymap::from_config(&twice).is_err());
    }
}
