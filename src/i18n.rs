//! Translations for the interface. Every text is written in English in the code and
//! looked up here when it is drawn, so a language is one TOML file that maps English
//! text to its translation:
//!
//! ```toml
//! "archive" = "arşivle"
//! "Deleted {} messages permanently" = "{} mesaj kalıcı olarak silindi"
//! ```
//!
//! `{}` stands for a part that changes, such as a number or a name. A text without a
//! translation stays English.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, RwLock},
};

const BUILT_IN: &[(&str, &str)] = &[("tr", include_str!("../locales/tr.toml"))];

#[derive(Default)]
struct Table {
    exact: HashMap<String, String>,
    /// Texts with `{}` parts: the pieces around them and the translation.
    patterns: Vec<(Vec<String>, String)>,
}

static TABLE: RwLock<Option<Table>> = RwLock::new(None);
static SEEN: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// The language `ui.language` asks for: `auto` reads LC_ALL, LC_MESSAGES and LANG.
fn resolve(language: &str) -> String {
    if language != "auto" {
        return language.to_lowercase();
    }
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .map(|value| {
            value
                .split(['_', '.', '@', '-'])
                .next()
                .unwrap_or_default()
                .to_lowercase()
        })
        .filter(|code| !code.is_empty() && code != "c" && code != "posix")
        .unwrap_or_else(|| "en".into())
}

/// Loads the language. A file at `<config folder>/locales/<code>.toml` adds to or
/// replaces the built-in translation, or supplies a language termail lacks.
pub fn init(language: &str, config_dir: Option<&Path>) -> Result<(), String> {
    let code = resolve(language);
    let table = load(&code, config_dir, language == "auto")?;
    if let Ok(mut current) = TABLE.write() {
        *current = table;
    }
    if let Ok(mut seen) = SEEN.lock() {
        *seen = None;
    }
    Ok(())
}

fn load(code: &str, config_dir: Option<&Path>, automatic: bool) -> Result<Option<Table>, String> {
    if code == "en" {
        return Ok(None);
    }
    let mut sources: Vec<(String, String)> = BUILT_IN
        .iter()
        .filter(|(name, _)| *name == code)
        .map(|(name, text)| (format!("built-in {name}"), (*text).to_owned()))
        .collect();
    if let Some(dir) = config_dir {
        let path = dir.join("locales").join(format!("{code}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            sources.push((path.display().to_string(), text));
        }
    }
    if sources.is_empty() {
        // An unknown system language quietly stays English; an explicit one is a mistake.
        return if automatic {
            Ok(None)
        } else {
            Err(format!(
                "ui.language \"{code}\" has no translation. Add locales/{code}.toml next to config.toml."
            ))
        };
    }
    let mut table = Table::default();
    for (name, text) in sources {
        let entries: HashMap<String, String> =
            toml::from_str(&text).map_err(|error| format!("{name}: {error}"))?;
        for (english, translated) in entries {
            if english.contains("{}") {
                let pieces = english.split("{}").map(str::to_owned).collect();
                table.patterns.push((pieces, translated));
            } else {
                table.exact.insert(english, translated);
            }
        }
    }
    // Longer patterns are more specific, so they get the first chance.
    table.patterns.sort_by_key(|(pieces, _)| {
        std::cmp::Reverse(pieces.iter().map(String::len).sum::<usize>())
    });
    Ok(Some(table))
}

/// The parts `{}` stands for, when `text` has the shape of `pieces`.
fn captures(pieces: &[String], text: &str) -> Option<Vec<String>> {
    let (first, rest) = pieces.split_first()?;
    let mut remaining = text.strip_prefix(first.as_str())?;
    let mut found = Vec::new();
    for (index, piece) in rest.iter().enumerate() {
        let last = index == rest.len() - 1;
        let at = if last {
            remaining.strip_suffix(piece.as_str())?.len()
        } else if piece.is_empty() {
            return None;
        } else {
            remaining.find(piece.as_str())?
        };
        found.push(remaining[..at].to_owned());
        remaining = &remaining[at + piece.len()..];
    }
    Some(found)
}

fn lookup(table: &Table, text: &str) -> Option<String> {
    if let Some(found) = table.exact.get(text) {
        return Some(found.clone());
    }
    table.patterns.iter().find_map(|(pieces, translated)| {
        let parts = captures(pieces, text)?;
        let mut output = String::new();
        let mut parts = parts.into_iter();
        let mut pieces = translated.split("{}").peekable();
        while let Some(piece) = pieces.next() {
            output.push_str(piece);
            if pieces.peek().is_some() {
                output.push_str(&parts.next().unwrap_or_default());
            }
        }
        Some(output)
    })
}

/// The text in the chosen language. Spaces around it stay as they are, so padded
/// titles such as `" help "` translate too.
pub fn t(text: &str) -> String {
    let Ok(table) = TABLE.read() else {
        return text.to_owned();
    };
    let Some(table) = table.as_ref() else {
        return text.to_owned();
    };
    if let Ok(mut seen) = SEEN.lock()
        && let Some(found) = seen.get_or_insert_with(HashMap::new).get(text)
    {
        return found.clone();
    }
    let trimmed = text.trim();
    let start = text.len() - text.trim_start().len();
    let translated = lookup(table, trimmed)
        .map(|found| {
            format!(
                "{}{found}{}",
                &text[..start],
                &text[start + trimmed.len()..]
            )
        })
        .unwrap_or_else(|| text.to_owned());
    if let Ok(mut seen) = SEEN.lock() {
        let seen = seen.get_or_insert_with(HashMap::new);
        // Notices carry changing numbers; keep the memo from growing without end.
        if seen.len() > 4096 {
            seen.clear();
        }
        seen.insert(text.to_owned(), translated.clone());
    }
    translated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_whole_texts_and_patterns() {
        let mut table = Table::default();
        table.exact.insert("archive".into(), "arşivle".into());
        table.patterns.push((
            vec!["Deleted ".into(), " messages from ".into(), "".into()],
            "{} mesaj silindi: {}".into(),
        ));
        assert_eq!(lookup(&table, "archive").as_deref(), Some("arşivle"));
        assert_eq!(
            lookup(&table, "Deleted 3 messages from Inbox").as_deref(),
            Some("3 mesaj silindi: Inbox")
        );
        assert_eq!(lookup(&table, "Deleted"), None);
        for (_, text) in BUILT_IN {
            let entries: HashMap<String, String> = toml::from_str(text).unwrap();
            for (english, translated) in entries {
                assert_eq!(
                    english.matches("{}").count(),
                    translated.matches("{}").count(),
                    "{english}"
                );
            }
        }
    }
}
