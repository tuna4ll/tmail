//! Completions for the compose form.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::mail::{Mail, email_of};

/// Everyone the mailbox has exchanged mail with, most frequent first.
pub fn contacts(mails: &[Mail]) -> Vec<String> {
    let mut seen: HashMap<String, (usize, String)> = HashMap::new();
    for mail in mails {
        for value in std::iter::once(&mail.from).chain(&mail.to).chain(&mail.cc) {
            let address = email_of(value);
            if !address.contains('@') {
                continue;
            }
            let entry = seen.entry(address).or_insert((0, value.clone()));
            entry.0 += 1;
            // Prefer a form that carries a display name.
            if !entry.1.contains('<') && value.contains('<') {
                entry.1 = value.clone();
            }
        }
    }
    let mut contacts: Vec<(usize, String)> = seen.into_values().collect();
    contacts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    contacts.into_iter().map(|(_, value)| value).collect()
}

/// The field value with its last address completed from `contacts`.
pub fn address(value: &str, contacts: &[String]) -> Option<String> {
    let (head, token) = match value.rsplit_once(',') {
        Some((head, token)) => (format!("{head}, "), token.trim_start()),
        None => (String::new(), value.trim_start()),
    };
    let token = token.to_lowercase();
    if token.is_empty() {
        return None;
    }
    let present: Vec<String> = head
        .split(',')
        .map(email_of)
        .filter(|address| !address.is_empty())
        .collect();
    let candidate = contacts.iter().find(|contact| {
        let lowered = contact.to_lowercase();
        !present.contains(&email_of(contact))
            && lowered != token
            && (email_of(contact).starts_with(&token)
                || lowered
                    .split(|c: char| c.is_whitespace() || "<\"".contains(c))
                    .any(|word| word.starts_with(&token)))
    })?;
    Some(format!("{head}{candidate}, "))
}

/// The field value with its last attachment path completed from the file system.
pub fn path(value: &str, base: &Path) -> Option<String> {
    let (head, token) = match value.rsplit_once(';') {
        Some((head, token)) => (format!("{head}; "), token.trim_start()),
        None => (String::new(), value.trim_start()),
    };
    if token.is_empty() {
        return None;
    }
    let (directory, prefix) = match token.rfind('/') {
        Some(index) => (&token[..=index], &token[index + 1..]),
        None => ("", token),
    };
    let expanded = match directory.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()?.home_dir().join(rest),
        None if directory.is_empty() => base.to_path_buf(),
        None => {
            let path = PathBuf::from(directory);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        }
    };
    let mut names: Vec<(String, bool)> = std::fs::read_dir(expanded)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.')))
                .then(|| (name, entry.path().is_dir()))
        })
        .collect();
    names.sort();
    let (name, is_dir) = names.into_iter().next()?;
    let completed = format!("{directory}{name}{}", if is_dir { "/" } else { "" });
    (completed != token).then(|| format!("{head}{completed}"))
}
