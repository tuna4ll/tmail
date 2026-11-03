//! Interface settings from the `[ui]` table. Scripts can change them while termail
//! runs, so they live behind a lock instead of being copied around.

use std::{path::PathBuf, sync::RwLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Width of the conversation list, in columns.
    pub inbox_width: u16,
    /// Width of the reader when it sits beside the map.
    pub reader_width: u16,
    /// Below this width only one pane is shown at a time.
    pub split_min_width: u16,
    /// From this width the reader docks beside the map instead of covering it.
    pub dock_min_width: u16,
    /// The widest the reader window opens, in columns; 0 for no limit.
    pub reader_max_width: u16,
    /// Date formats (strftime) for today, this year, older mail and message headers.
    pub time_format: String,
    pub day_format: String,
    pub date_format: String,
    pub long_date_format: String,
    pub mark_read_on_open: bool,
    /// Asks before quitting with q.
    pub confirm_quit: bool,
    /// Shows key hints in the bottom line.
    pub show_hints: bool,
    /// Lets termail use the mouse. Turn off to select text with the terminal.
    pub mouse: bool,
    /// `newest` or `oldest` first in the conversation list.
    pub sort: String,
    /// Groups replies without reply headers by their subject.
    pub thread_by_subject: bool,
    /// The folder and filter shown at start.
    pub start_folder: String,
    pub start_filter: String,
    /// Lines moved by the mouse wheel and by a page in the reader.
    pub scroll_lines: u16,
    pub page_lines: u16,
    pub unread_marker: String,
    pub star_marker: String,
    /// Folders left out of the folder list.
    pub hide_folders: Vec<String>,
    /// Program for new-mail notifications. `{title}` and `{body}` are replaced.
    pub notify_command: Vec<String>,
    /// The Lua script to run at start; `init.lua` next to the config when unset.
    pub script: Option<PathBuf>,
    /// The interface language, such as `en` or `tr`; `auto` follows the system.
    pub language: String,
    /// For screen readers and magnifiers: no colours unless a theme preset is set,
    /// letters instead of symbols, no animation, and the terminal cursor on the
    /// selected message.
    pub accessible: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            inbox_width: 30,
            reader_width: 44,
            split_min_width: 78,
            dock_min_width: 112,
            reader_max_width: 0,
            time_format: "%H:%M".into(),
            day_format: "%-d %b".into(),
            date_format: "%-d %b %Y".into(),
            long_date_format: "%a %-d %b %Y %H:%M".into(),
            mark_read_on_open: true,
            confirm_quit: false,
            show_hints: true,
            mouse: true,
            sort: "newest".into(),
            thread_by_subject: true,
            start_folder: "INBOX".into(),
            start_filter: "all".into(),
            scroll_lines: 3,
            page_lines: 10,
            unread_marker: "●".into(),
            star_marker: "★".into(),
            hide_folders: Vec::new(),
            notify_command: Vec::new(),
            script: None,
            language: "auto".into(),
            accessible: false,
        }
    }
}

impl UiConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(16..=200).contains(&self.inbox_width) || !(24..=200).contains(&self.reader_width) {
            return Err("ui.inbox_width must be 16 to 200 and ui.reader_width 24 to 200.".into());
        }
        if !["newest", "oldest"].contains(&self.sort.as_str()) {
            return Err("ui.sort must be \"newest\" or \"oldest\".".into());
        }
        if !["all", "unread", "starred"].contains(&self.start_filter.as_str()) {
            return Err("ui.start_filter must be all, unread or starred.".into());
        }
        if self.scroll_lines == 0 || self.page_lines == 0 {
            return Err("ui.scroll_lines and ui.page_lines must be at least 1.".into());
        }
        for format in [
            &self.time_format,
            &self.day_format,
            &self.date_format,
            &self.long_date_format,
        ] {
            let mut ok = true;
            for item in chrono::format::StrftimeItems::new(format) {
                ok &= !matches!(item, chrono::format::Item::Error);
            }
            if !ok {
                return Err(format!("Invalid date format {format:?} in [ui]."));
            }
        }
        Ok(())
    }
}

/// The `[compose]` table: how new messages, replies and forwards start.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComposeConfig {
    /// `plain` or `html`.
    pub format: String,
    pub reply_prefix: String,
    pub forward_prefix: String,
    /// The line above a quoted reply. `{from}` and `{date}` are replaced.
    pub attribution: String,
    pub quote_prefix: String,
    /// Warn before sending attachments larger than this many megabytes in total.
    pub large_attachment_mb: u64,
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            format: "plain".into(),
            reply_prefix: "Re: ".into(),
            forward_prefix: "Fwd: ".into(),
            attribution: "On {date}, {from} wrote:".into(),
            quote_prefix: "> ".into(),
            large_attachment_mb: 18,
        }
    }
}

/// The `[reader]` table.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReaderConfig {
    /// `original` draws HTML mail in its own colours, `theme` in the reader's.
    pub html_colors: String,
    /// Opens messages in a window over the list (`popup`) or beside it (`docked`).
    pub layout: String,
    /// The window's size in percent of the screen.
    pub popup_width: u16,
    pub popup_height: u16,
    /// Loads remote images without asking.
    pub remote_images: bool,
    /// Removes tracking parameters such as `utm_source` from links before opening them.
    pub clean_links: bool,
    /// Loads remote images from these senders: whole addresses or `@domain`.
    pub remote_images_from: Vec<String>,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            html_colors: "original".into(),
            layout: "popup".into(),
            popup_width: 84,
            popup_height: 90,
            remote_images: false,
            remote_images_from: Vec::new(),
            clean_links: true,
        }
    }
}

impl ReaderConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !["original", "theme"].contains(&self.html_colors.as_str()) {
            return Err("reader.html_colors must be \"original\" or \"theme\".".into());
        }
        if !["popup", "docked"].contains(&self.layout.as_str()) {
            return Err("reader.layout must be \"popup\" or \"docked\".".into());
        }
        if !(20..=100).contains(&self.popup_width) || !(20..=100).contains(&self.popup_height) {
            return Err(
                "reader.popup_width and popup_height are percentages from 20 to 100.".into(),
            );
        }
        Ok(())
    }

    pub fn trusts(&self, address: &str) -> bool {
        let address = address.to_lowercase();
        self.remote_images
            || self.remote_images_from.iter().any(|entry| {
                let entry = entry.to_lowercase();
                if entry.starts_with('@') {
                    address.ends_with(&entry)
                } else {
                    address == entry
                }
            })
    }
}

static UI: RwLock<Option<UiConfig>> = RwLock::new(None);
static COMPOSE: RwLock<Option<ComposeConfig>> = RwLock::new(None);
static READER: RwLock<Option<ReaderConfig>> = RwLock::new(None);

pub fn init_compose(config: ComposeConfig) -> Result<(), String> {
    if !["plain", "html"].contains(&config.format.as_str()) {
        return Err("compose.format must be \"plain\" or \"html\".".into());
    }
    if let Ok(mut compose) = COMPOSE.write() {
        *compose = Some(config);
    }
    Ok(())
}

pub fn init_reader(config: ReaderConfig) -> Result<(), String> {
    config.validate()?;
    if let Ok(mut reader) = READER.write() {
        *reader = Some(config);
    }
    Ok(())
}

pub fn compose() -> ComposeConfig {
    COMPOSE
        .read()
        .ok()
        .and_then(|compose| compose.clone())
        .unwrap_or_default()
}

pub fn reader() -> ReaderConfig {
    READER
        .read()
        .ok()
        .and_then(|reader| reader.clone())
        .unwrap_or_default()
}

pub fn init(config: UiConfig) -> Result<(), String> {
    config.validate()?;
    if let Ok(mut ui) = UI.write() {
        *ui = Some(config);
    }
    Ok(())
}

/// Reads the settings without copying them.
pub fn with<R>(read: impl FnOnce(&UiConfig) -> R) -> R {
    match UI.read() {
        Ok(ui) => match ui.as_ref() {
            Some(ui) => read(ui),
            None => read(&UiConfig::default()),
        },
        Err(_) => read(&UiConfig::default()),
    }
}

pub fn get() -> UiConfig {
    with(Clone::clone)
}

/// Changes one setting by name, as a script does: `inbox_width`, or with a table
/// prefix such as `compose.format` and `reader.remote_images`. The table is checked
/// again as a whole.
pub fn set(name: &str, value: serde_json::Value) -> Result<(), String> {
    fn replace<T: Serialize + serde::de::DeserializeOwned>(
        current: T,
        table: &str,
        name: &str,
        value: serde_json::Value,
    ) -> Result<T, String> {
        let mut fields = serde_json::to_value(current).map_err(|error| error.to_string())?;
        let field = fields
            .get_mut(name)
            .ok_or_else(|| format!("There is no {table} setting called {name}."))?;
        *field = value;
        serde_json::from_value(fields).map_err(|error| format!("{table}.{name}: {error}"))
    }
    match name.split_once('.') {
        Some(("compose", field)) => init_compose(replace(compose(), "compose", field, value)?),
        Some(("reader", field)) => init_reader(replace(reader(), "reader", field, value)?),
        Some(("ui", field)) => init(replace(get(), "ui", field, value)?),
        Some((table, _)) => Err(format!(
            "Scripts can set ui, compose and reader settings, not {table}."
        )),
        None => init(replace(get(), "ui", name, value)?),
    }
}

/// Reads one setting by the same names `set` takes.
pub fn value(name: &str) -> Result<serde_json::Value, String> {
    let (table, field, fields) = match name.split_once('.') {
        Some(("compose", field)) => ("compose", field, serde_json::to_value(compose())),
        Some(("reader", field)) => ("reader", field, serde_json::to_value(reader())),
        Some(("ui", field)) => ("ui", field, serde_json::to_value(get())),
        Some((table, _)) => return Err(format!("There is no {table} table to read.")),
        None => ("ui", name, serde_json::to_value(get())),
    };
    fields
        .map_err(|error| error.to_string())?
        .get(field)
        .cloned()
        .ok_or_else(|| format!("There is no {table} setting called {field}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_values_and_reader_trust() {
        let mut ui = UiConfig::default();
        assert!(ui.validate().is_ok());
        ui.time_format = "%Q".into();
        assert!(ui.validate().is_err());
        let reader = ReaderConfig {
            remote_images_from: vec!["@github.com".into(), "Friend@Example.com".into()],
            ..ReaderConfig::default()
        };
        assert!(reader.trusts("noreply@github.com"));
        assert!(reader.trusts("friend@example.com"));
        assert!(!reader.trusts("github.com@evil.test"));
        assert!(
            ReaderConfig {
                layout: "floating".into(),
                ..ReaderConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}
