//! Applies an edited config.toml while tmail runs. Look, keys, scripts and message
//! settings change at once; account connections keep running as they are until a
//! restart, so signed-in sessions survive.

use super::*;

impl App {
    /// Reloads when the config file on disk no longer matches what was loaded.
    pub(crate) fn reload_config_if_changed(&mut self) {
        let Some(path) = self.config.path.clone() else {
            return;
        };
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        if modified == self.config_modified {
            return;
        }
        self.config_modified = modified;
        let Ok(contents) = fs::read_to_string(&path) else {
            return;
        };
        if Some(&contents) != self.config.snapshot.as_ref() {
            self.reload_config();
        }
    }

    pub(crate) fn reload_config(&mut self) {
        let path = self.config.path.clone();
        let mut fresh = match Config::load(path.as_deref()) {
            Ok(config) => config,
            Err(error) => {
                // TOML errors span several lines with a source excerpt; keep the gist.
                let text = error.to_string();
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .collect();
                let short = match (lines.first(), lines.last()) {
                    (Some(first), Some(last)) if lines.len() > 1 => format!("{first}: {last}"),
                    _ => text.clone(),
                };
                self.notice = Some(format!("Config not reloaded: {short}"));
                return;
            }
        };
        // Check every table before changing anything, so a typo changes nothing.
        let theme = theme::effective(&fresh.theme, fresh.ui.accessible);
        let checked = theme::Colors::from_config(&theme)
            .and_then(|_| fresh.ui.validate())
            .and_then(|_| fresh.reader.validate())
            .and_then(|_| {
                ["plain", "html", "markdown"]
                    .contains(&fresh.compose.format.as_str())
                    .then_some(())
                    .ok_or_else(|| {
                        "compose.format must be \"plain\", \"html\" or \"markdown\".".to_owned()
                    })
            })
            .and_then(|_| keymap::Keymap::from_config(&fresh.keys))
            .and_then(|keymap| {
                i18n::init(
                    &fresh.ui.language,
                    fresh.path.as_deref().and_then(Path::parent),
                )
                .map(|_| keymap)
            });
        let keymap = match checked {
            Ok(keymap) => keymap,
            Err(error) => {
                self.notice = Some(format!("Config not reloaded: {error}"));
                return;
            }
        };
        let _ = theme::init(&theme);
        let _ = settings::init(fresh.ui.clone());
        let _ = settings::init_compose(fresh.compose.clone());
        let _ = settings::init_reader(fresh.reader.clone());
        self.keymap = keymap;

        let restart = self.keep_connections(&mut fresh);
        fresh.active_account = self.config.active_account.clone();
        self.config_modified = fresh.path.as_deref().and_then(|path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        });
        self.config = fresh;
        self.inbox_width = settings::with(|ui| ui.inbox_width);
        self.reader_width = settings::with(|ui| ui.reader_width);
        self.address_book = self
            .config
            .contacts_path()
            .map(|path| contacts::load(&path))
            .unwrap_or_default();
        self.load_script();
        self.rebuild_mailbox(self.active_mail_id().as_deref());
        if let Some(reader) = &mut self.reader {
            reader.invalidate();
        }
        self.clear_screen = true;
        self.notice = Some(if restart {
            "Config reloaded. Account connection changes apply after a restart.".into()
        } else {
            "Config reloaded".into()
        });
    }

    /// Copies the running connections into `fresh` and keeps what only shows, such as
    /// names, signatures and aliases. Returns whether a connection setting changed.
    fn keep_connections(&self, fresh: &mut Config) -> bool {
        let same = |a: &Option<crate::remote::RemoteConfig>,
                    b: &Option<crate::remote::RemoteConfig>| {
            serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
        };
        let mut changed = !same(&self.config.remote, &fresh.remote)
            || self.config.maildir != fresh.maildir
            || self.config.send_command != fresh.send_command;
        fresh.remote = self.config.remote.clone();
        fresh.maildir = self.config.maildir.clone();
        fresh.send_command = self.config.send_command.clone();
        let mut accounts = self.config.accounts.clone();
        for account in &mut accounts {
            match fresh.accounts.iter().find(|new| new.id == account.id) {
                Some(new) => {
                    changed |= !same(&account.remote, &new.remote)
                        || account.maildir != new.maildir
                        || account.send_command != new.send_command;
                    account.name = new.name.clone();
                    account.email = new.email.clone();
                    account.aliases = new.aliases.clone();
                    account.signature = new.signature.clone();
                    account.editor = new.editor.clone();
                    account.download_dir = new.download_dir.clone();
                }
                None => changed = true,
            }
        }
        changed |= fresh
            .accounts
            .iter()
            .any(|new| !accounts.iter().any(|account| account.id == new.id));
        fresh.accounts = accounts;
        changed
    }
}
