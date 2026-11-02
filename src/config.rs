use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: Option<String>,
    pub email: Option<String>,
    pub signature: Option<String>,
    pub maildir: Option<PathBuf>,
    pub download_dir: Option<PathBuf>,
    pub editor: Vec<String>,
    pub send_command: Vec<String>,
    pub remote: Option<crate::remote::RemoteConfig>,
    pub default_account: Option<String>,
    pub accounts: Vec<Account>,
    /// Desktop notifications for new mail; on unless set to false.
    pub notifications: Option<bool>,
    pub theme: crate::theme::ThemeConfig,
    /// Custom keys by action name, such as `archive = "e"`.
    pub keys: std::collections::BTreeMap<String, String>,
    pub crypto: crate::crypto::CryptoConfig,
    pub ui: crate::settings::UiConfig,
    pub compose: crate::settings::ComposeConfig,
    pub reader: crate::settings::ReaderConfig,
    #[serde(skip)]
    pub(crate) active_account: Option<String>,
    #[serde(skip)]
    pub(crate) path: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) snapshot: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Account {
    pub id: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub signature: Option<String>,
    pub maildir: Option<PathBuf>,
    pub download_dir: Option<PathBuf>,
    pub editor: Vec<String>,
    pub send_command: Vec<String>,
    pub remote: Option<crate::remote::RemoteConfig>,
}

impl Config {
    pub fn load(path: Option<&Path>) -> color_eyre::Result<Self> {
        let explicit = path
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("TERMAIL_CONFIG").map(PathBuf::from));
        let path = explicit.clone().or_else(default_path);
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let path = if path.is_relative() {
            std::env::current_dir()?.join(path)
        } else {
            path
        };
        if !path.exists() {
            return Ok(Self {
                path: Some(path),
                ..Self::default()
            });
        }
        let contents = fs::read_to_string(&path)?;
        let mut config: Self = toml::from_str(&contents)?;
        config.path = Some(path.clone());
        config.snapshot = Some(contents);
        let base = path.parent().unwrap_or(Path::new("."));
        resolve_path(&mut config.maildir, base);
        resolve_path(&mut config.download_dir, base);
        if let Some(remote) = &mut config.remote {
            resolve_path(&mut remote.tls_certificate, base);
        }
        if let Some(remote) = &config.remote {
            remote.validate().map_err(color_eyre::eyre::Error::msg)?;
            if config.maildir.is_none() || !config.send_command.is_empty() {
                return Err(color_eyre::eyre::eyre!(
                    "Remote accounts require maildir and must not set send_command."
                ));
            }
        }
        for account in &mut config.accounts {
            resolve_path(&mut account.maildir, base);
            resolve_path(&mut account.download_dir, base);
            if let Some(remote) = &mut account.remote {
                resolve_path(&mut remote.tls_certificate, base);
            }
        }
        let mut ids = std::collections::HashSet::new();
        let mut roots = std::collections::HashSet::new();
        for account in &config.accounts {
            if let Some(remote) = &account.remote {
                remote.validate().map_err(color_eyre::eyre::Error::msg)?;
                if account.maildir.is_none() || !account.send_command.is_empty() {
                    return Err(color_eyre::eyre::eyre!(
                        "Remote accounts require maildir and must not set send_command."
                    ));
                }
            }
            if account.id.trim().is_empty() || !ids.insert(&account.id) {
                return Err(color_eyre::eyre::eyre!(
                    "Every account must have a unique, nonempty id."
                ));
            }
            if let Some(root) = &account.maildir {
                let root = root.canonicalize().unwrap_or_else(|_| root.clone());
                if !roots.insert(root) {
                    return Err(color_eyre::eyre::eyre!(
                        "Accounts must use separate Maildir directories."
                    ));
                }
            }
        }
        if let Some(default) = &config.default_account
            && !config.accounts.iter().any(|account| &account.id == default)
        {
            return Err(color_eyre::eyre::eyre!(
                "default_account does not match a configured account."
            ));
        }
        if !config.accounts.is_empty() {
            let selected = config
                .default_account
                .as_deref()
                .and_then(|id| config.accounts.iter().position(|account| account.id == id))
                .unwrap_or_default();
            config.activate(selected);
        }
        Ok(config)
    }

    pub fn current_account(&self) -> Account {
        Account {
            id: self
                .active_account
                .clone()
                .unwrap_or_else(|| "local".into()),
            name: self.name.clone(),
            email: self.email.clone(),
            signature: self.signature.clone(),
            maildir: self.maildir.clone(),
            download_dir: self.download_dir.clone(),
            editor: self.editor.clone(),
            send_command: self.send_command.clone(),
            remote: self.remote.clone(),
        }
    }

    pub fn save_account(
        &mut self,
        account: Account,
        editing: bool,
        make_default: bool,
    ) -> Result<(), String> {
        let path = self
            .path
            .clone()
            .ok_or("No config path is available. Start with --config PATH.")?;
        let parent = path.parent().ok_or("Invalid config path.")?;
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err("This config is a symlink. Restart with --config pointing to its target before saving.".into());
        }
        fs::create_dir_all(parent)
            .map_err(|_| "Unable to create the config directory. Check its permissions.")?;
        let _lock = ConfigLock::acquire(&path)?;
        let current = match fs::read_to_string(&path) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("Unable to read the config file. Check its permissions.".into()),
        };
        if current != self.snapshot {
            return Err(
                "Config changed outside Termail. Restart before saving account settings.".into(),
            );
        }
        let mut document = current
            .as_deref()
            .unwrap_or_default()
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "Config is invalid. Fix it before saving account settings.")?;
        if document
            .get("accounts")
            .is_none_or(|item| item.as_array().is_some_and(|accounts| accounts.is_empty()))
        {
            let mut accounts = toml_edit::ArrayOfTables::new();
            if self.maildir.is_some() {
                let mut legacy = toml_edit::Table::new();
                legacy["id"] = toml_edit::value("local");
                for key in [
                    "name",
                    "email",
                    "signature",
                    "maildir",
                    "download_dir",
                    "editor",
                    "send_command",
                    "remote",
                ] {
                    if let Some(value) = document.remove(key) {
                        legacy.insert(key, value);
                    }
                }
                accounts.push(legacy);
            }
            document["accounts"] = toml_edit::Item::ArrayOfTables(accounts);
        }
        let serialized =
            toml::to_string(&account).map_err(|_| "Unable to serialize account settings.")?;
        let updated = serialized
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "Unable to serialize account settings.")?;
        let accounts = document["accounts"]
            .as_array_of_tables_mut()
            .ok_or("Use [[accounts]] tables before editing accounts in Termail.")?;
        let existing = accounts.iter().position(|table| {
            table.get("id").and_then(toml_edit::Item::as_str) == Some(&account.id)
        });
        if editing {
            let table = accounts
                .get_mut(existing.ok_or("The account no longer exists in config.")?)
                .unwrap();
            merge_settings(table, updated.as_table());
        } else {
            if existing.is_some() {
                return Err("This account ID is already in use.".into());
            }
            accounts.push(updated.as_table().clone());
        }
        let fallback = accounts
            .iter()
            .filter_map(|table| table.get("id").and_then(toml_edit::Item::as_str))
            .find(|id| *id != account.id)
            .map(str::to_owned);
        if make_default || document.get("default_account").is_none() {
            document["default_account"] = toml_edit::value(account.id.clone());
        } else if document["default_account"].as_str() == Some(&account.id)
            && let Some(fallback) = fallback
        {
            document["default_account"] = toml_edit::value(fallback);
        }
        position_tables(document.as_table_mut(), &mut 0);
        let contents = document.to_string();
        let temporary = parent.join(crate::storage::unique_name("config"));
        let result = (|| {
            crate::storage::write_new(&temporary, contents.as_bytes())
                .map_err(|_| "Unable to write config. Check disk space and permissions.")?;
            let mut saved = Self::load(Some(&temporary)).map_err(
                |_| "Account settings are invalid. The original config was left unchanged.",
            )?;
            let legacy = self.current_account();
            for saved_account in &mut saved.accounts {
                let previous = if saved_account.id == account.id {
                    Some(&account)
                } else {
                    self.accounts
                        .iter()
                        .find(|old| old.id == saved_account.id)
                        .or_else(|| {
                            (self.accounts.is_empty() && legacy.id == saved_account.id)
                                .then_some(&legacy)
                        })
                };
                if let Some(remote) = &mut saved_account.remote {
                    remote.session_password = previous
                        .and_then(|old| old.remote.as_ref())
                        .and_then(|remote| remote.session_password.clone());
                }
            }
            let selected = saved
                .accounts
                .iter()
                .position(|item| item.id == account.id)
                .unwrap();
            saved.activate(selected);
            saved.path = Some(path.clone());
            crate::storage::rename(&temporary, &path)
                .map_err(|_| "Unable to replace config. Check directory permissions.")?;
            *self = saved;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Removes an account from the config file. Its Maildir stays on disk.
    pub fn remove_account(&mut self, id: &str) -> Result<(), String> {
        let path = self
            .path
            .clone()
            .ok_or("No config path is available. Start with --config PATH.")?;
        let parent = path.parent().ok_or("Invalid config path.")?;
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err("This config is a symlink. Restart with --config pointing to its target before saving.".into());
        }
        let _lock = ConfigLock::acquire(&path)?;
        let current = fs::read_to_string(&path)
            .map_err(|_| "Unable to read the config file. Check its permissions.")?;
        if Some(&current) != self.snapshot.as_ref() {
            return Err(
                "Config changed outside Termail. Restart before saving account settings.".into(),
            );
        }
        let mut document = current
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "Config is invalid. Fix it before saving account settings.")?;
        let accounts = document
            .get_mut("accounts")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
            .ok_or("Use [[accounts]] tables before editing accounts in Termail.")?;
        let position = accounts
            .iter()
            .position(|table| table.get("id").and_then(toml_edit::Item::as_str) == Some(id))
            .ok_or("The account no longer exists in config.")?;
        if accounts.len() == 1 {
            return Err("Add another account before removing the last one.".into());
        }
        accounts.remove(position);
        let fallback = accounts
            .iter()
            .find_map(|table| table.get("id").and_then(toml_edit::Item::as_str))
            .map(str::to_owned)
            .unwrap_or_default();
        if document
            .get("default_account")
            .and_then(toml_edit::Item::as_str)
            == Some(id)
        {
            document["default_account"] = toml_edit::value(fallback);
        }
        let contents = document.to_string();
        let temporary = parent.join(crate::storage::unique_name("config"));
        let result = (|| {
            crate::storage::write_new(&temporary, contents.as_bytes())
                .map_err(|_| "Unable to write config. Check disk space and permissions.")?;
            let mut saved = Self::load(Some(&temporary)).map_err(
                |_| "Unable to remove the account. The original config was left unchanged.",
            )?;
            for account in &mut saved.accounts {
                if let Some(remote) = &mut account.remote {
                    remote.session_password = self
                        .accounts
                        .iter()
                        .find(|old| old.id == account.id)
                        .and_then(|old| old.remote.as_ref())
                        .and_then(|old| old.session_password.clone());
                }
            }
            let selected = saved.active_account_index();
            saved.activate(selected);
            saved.path = Some(path.clone());
            crate::storage::rename(&temporary, &path)
                .map_err(|_| "Unable to replace config. Check directory permissions.")?;
            *self = saved;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub fn identity(&self) -> Option<String> {
        match (&self.name, &self.email) {
            (Some(name), Some(email)) => Some(crate::message::display_address(name, email)),
            (None, Some(email)) => Some(email.clone()),
            _ => None,
        }
    }

    /// `editor = ["builtin"]` edits bodies inside termail.
    pub fn uses_builtin_editor(&self) -> bool {
        self.editor
            .first()
            .is_some_and(|program| program == "builtin")
    }

    pub fn editor_command(&self) -> Vec<String> {
        if !self.editor.is_empty() {
            return self.editor.clone();
        }
        if let Ok(editor) = std::env::var("EDITOR") {
            let command: Vec<String> = editor.split_whitespace().map(Into::into).collect();
            if !command.is_empty() {
                return command;
            }
        }
        if cfg!(target_os = "windows") {
            vec!["notepad".into()]
        } else if cfg!(target_os = "macos") {
            vec!["open".into(), "-W".into(), "-a".into(), "TextEdit".into()]
        } else {
            vec!["vi".into()]
        }
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir
            .clone()
            .or_else(|| UserDirs::new().and_then(|dirs| dirs.download_dir().map(Path::to_path_buf)))
            .unwrap_or_else(std::env::temp_dir)
    }

    pub fn activate(&mut self, index: usize) -> bool {
        let Some(account) = self.accounts.get(index).cloned() else {
            return false;
        };
        self.active_account = Some(account.id.clone());
        self.name = account.name;
        self.email = account.email;
        self.signature = account.signature;
        self.maildir = account.maildir;
        self.download_dir = account.download_dir;
        self.editor = account.editor;
        self.send_command = account.send_command;
        self.remote = account.remote;
        true
    }

    pub fn active_account_index(&self) -> usize {
        self.active_account
            .as_deref()
            .and_then(|id| self.accounts.iter().position(|account| account.id == id))
            .unwrap_or_default()
    }

    pub fn delivery_identity(&self) -> String {
        let identity = format!(
            "{}:{}",
            self.active_account.as_deref().unwrap_or("local"),
            self.email.as_deref().unwrap_or_default()
        );
        match &self.remote {
            Some(remote) => format!(
                "{identity}:{}:{}:{}:{}:{}",
                remote.imap_host,
                remote.imap_port,
                remote.smtp_host,
                remote.smtp_port,
                remote.username
            ),
            None => identity,
        }
    }
}

impl Account {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .filter(|name| !name.is_empty())
            .or_else(|| self.email.clone())
            .unwrap_or_else(|| self.id.clone())
    }
}

struct ConfigLock(fs::File);

impl ConfigLock {
    fn acquire(path: &Path) -> Result<Self, String> {
        let lock_path = path.with_file_name(format!(
            ".{}.lock",
            path.file_name().unwrap().to_string_lossy()
        ));
        let lock = match crate::storage::create(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                    .map_err(|_| "Unable to lock the config file.")?
            }
            Err(_) => return Err("Unable to lock the config file.".into()),
        };
        lock.try_lock()
            .map_err(|_| "Another process is saving this config. Try again.")?;
        Ok(Self(lock))
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn merge_settings(target: &mut toml_edit::Table, updated: &toml_edit::Table) {
    for (key, value) in updated {
        if let Some(table) = target.get_mut(key).and_then(toml_edit::Item::as_table_mut)
            && let Some(updated) = value.as_table()
        {
            merge_settings(table, updated);
        } else if target.get(key).map(ToString::to_string) != Some(value.to_string()) {
            target.insert(key, value.clone());
        }
    }
}

fn position_tables(table: &mut toml_edit::Table, position: &mut usize) {
    table.set_position(*position);
    *position += 1;
    for (_, item) in table.iter_mut() {
        if let Some(table) = item.as_table_mut() {
            position_tables(table, position);
        } else if let Some(tables) = item.as_array_of_tables_mut() {
            for table in tables.iter_mut() {
                position_tables(table, position);
            }
        }
    }
}

fn resolve_path(path: &mut Option<PathBuf>, base: &Path) {
    if path.as_ref().is_some_and(|path| path.is_relative()) {
        *path = path.take().map(|path| base.join(path));
    }
}

pub fn default_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "tunakilic", "termail")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::Config;

    struct SetupFixture(std::path::PathBuf);

    impl SetupFixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(crate::storage::unique_name("account-config")))
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join("config.toml")
        }
        fn account(&self, id: &str) -> super::Account {
            super::Account {
                id: id.into(),
                email: Some(format!("{id}@example.test")),
                maildir: Some(self.0.join(id)),
                remote: Some(crate::remote::RemoteConfig {
                    imap_host: "localhost".into(),
                    smtp_host: "localhost".into(),
                    username: format!("{id}@example.test"),
                    password_env: "TERMAIL_SETUP_TEST_PASSWORD".into(),
                    session_password: Some(std::sync::Arc::new(crate::remote::SessionPassword(
                        zeroize::Zeroizing::new("fake-password".into()),
                    ))),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
    }

    impl Drop for SetupFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn saves_first_account_privately_without_passwords() {
        let fixture = SetupFixture::new();
        let mut config = Config::load(Some(&fixture.path())).unwrap();
        assert!(!fixture.0.exists());
        config
            .save_account(fixture.account("personal"), false, true)
            .unwrap();
        let contents = fs::read_to_string(fixture.path()).unwrap();
        assert!(!contents.contains("fake-password"));
        assert!(!contents.contains("session_password"));
        assert!(!format!("{config:?}").contains("fake-password"));
        assert_eq!(
            config.remote.as_ref().unwrap().password().unwrap(),
            "fake-password"
        );
        let reloaded = Config::load(Some(&fixture.path())).unwrap();
        assert!(reloaded.remote.as_ref().unwrap().session_password.is_none());
        assert_eq!(reloaded.default_account.as_deref(), Some("personal"));
        assert_eq!(reloaded.maildir, Some(fixture.0.join("personal")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn edits_accounts_without_losing_other_settings_or_session_passwords() {
        let fixture = SetupFixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        fs::write(
            fixture.path(),
            "custom_setting = 'keep me'\neditor = ['vi']\n",
        )
        .unwrap();
        let mut config = Config::load(Some(&fixture.path())).unwrap();
        config
            .save_account(fixture.account("personal"), false, true)
            .unwrap();
        config
            .save_account(fixture.account("work"), false, false)
            .unwrap();
        assert_eq!(config.default_account.as_deref(), Some("personal"));
        assert!(
            config.accounts[0]
                .remote
                .as_ref()
                .unwrap()
                .session_password
                .is_some()
        );
        let mut edited = config.current_account();
        edited.name = Some("Work mail".into());
        config.save_account(edited, true, true).unwrap();
        let contents = fs::read_to_string(fixture.path()).unwrap();
        assert!(contents.contains("custom_setting = 'keep me'"));
        assert!(contents.contains("editor = ['vi']"));
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.default_account.as_deref(), Some("work"));
        assert_eq!(config.name.as_deref(), Some("Work mail"));
    }

    #[test]
    fn refuses_external_changes_and_invalid_saves() {
        let fixture = SetupFixture::new();
        let mut config = Config::load(Some(&fixture.path())).unwrap();
        config
            .save_account(fixture.account("personal"), false, true)
            .unwrap();
        let before = fs::read_to_string(fixture.path()).unwrap();
        let mut invalid = fixture.account("work");
        invalid.remote.as_mut().unwrap().imap_port = 0;
        assert!(config.save_account(invalid, false, false).is_err());
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), before);
        assert_eq!(config.accounts.len(), 1);
        fs::write(
            fixture.path(),
            format!("custom_setting = 'external'\n{before}"),
        )
        .unwrap();
        assert!(
            config
                .save_account(fixture.account("work"), false, false)
                .unwrap_err()
                .contains("changed outside")
        );
        assert!(
            fs::read_to_string(fixture.path())
                .unwrap()
                .contains("external")
        );
    }

    #[test]
    fn migrates_legacy_account_without_changing_delivery_identity() {
        let fixture = SetupFixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        fs::write(
            fixture.path(),
            "email = 'legacy@example.test'\nmaildir = 'legacy'\nsend_command = ['legacy-sender']\n",
        )
        .unwrap();
        let mut config = Config::load(Some(&fixture.path())).unwrap();
        let identity = config.delivery_identity();
        config
            .save_account(fixture.account("work"), false, true)
            .unwrap();
        assert_eq!(config.accounts.len(), 2);
        config.activate(0);
        assert_eq!(config.delivery_identity(), identity);
        assert_eq!(config.maildir, Some(fixture.0.join("legacy")));
        assert_eq!(config.send_command, vec!["legacy-sender"]);
    }

    #[test]
    fn empty_account_array_can_be_configured_in_app() {
        let fixture = SetupFixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        fs::write(fixture.path(), "accounts = []\n").unwrap();
        let mut config = Config::load(Some(&fixture.path())).unwrap();
        config
            .save_account(fixture.account("personal"), false, true)
            .unwrap();
        assert_eq!(
            Config::load(Some(&fixture.path())).unwrap().accounts.len(),
            1
        );
    }

    #[test]
    fn default_account_can_be_changed_without_removing_accounts() {
        let fixture = SetupFixture::new();
        let mut config = Config::load(Some(&fixture.path())).unwrap();
        config
            .save_account(fixture.account("personal"), false, true)
            .unwrap();
        config
            .save_account(fixture.account("work"), false, true)
            .unwrap();
        assert_eq!(config.default_account.as_deref(), Some("work"));
        config
            .save_account(config.current_account(), true, false)
            .unwrap();
        assert_eq!(config.default_account.as_deref(), Some("personal"));
        assert_eq!(config.accounts.len(), 2);
    }

    #[test]
    fn distinguishes_accounts_with_the_same_email() {
        let mut config = Config {
            accounts: vec![
                super::Account {
                    id: "personal".into(),
                    email: Some("me@example.com".into()),
                    ..Default::default()
                },
                super::Account {
                    id: "work".into(),
                    email: Some("me@example.com".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        config.activate(1);
        assert_eq!(config.active_account_index(), 1);
        assert_eq!(config.delivery_identity(), "work:me@example.com");
        config.activate(0);
        assert_eq!(config.delivery_identity(), "personal:me@example.com");
    }

    #[test]
    fn resolves_relative_maildir_from_config() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termail-config-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(
            &path,
            "email = 'tuna@example.com'\nmaildir = 'mail'\ndownload_dir = 'downloads'\n",
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(config.maildir, Some(root.join("mail")));
        assert_eq!(config.download_dir, Some(root.join("downloads")));
        assert_eq!(config.identity().as_deref(), Some("tuna@example.com"));
    }

    #[test]
    fn loads_remote_account_and_rejects_plaintext_password_fields() {
        let root = std::env::temp_dir().join(crate::storage::unique_name("remote-config"));
        fs::create_dir(&root).unwrap();
        let path = root.join("config.toml");
        let contents = "[[accounts]]\nid = 'personal'\nemail = 'me@example.test'\nmaildir = 'mail'\n[accounts.remote]\nimap_host = 'localhost'\nsmtp_host = 'localhost'\nusername = 'me@example.test'\npassword_env = 'TERMAIL_PASSWORD'\n";
        fs::write(&path, contents).unwrap();
        let mut config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.maildir, Some(root.join("mail")));
        assert!(config.remote.as_ref().unwrap().sent.is_empty());
        let identity = config.delivery_identity();
        config.remote.as_mut().unwrap().smtp_host = "other.example.test".into();
        assert_ne!(config.delivery_identity(), identity);
        fs::write(&path, format!("{contents}password = 'fake-password'\n")).unwrap();
        assert!(Config::load(Some(&path)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selects_the_default_account() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termail-accounts-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(
            &path,
            "default_account = 'work'\n[[accounts]]\nid = 'personal'\nemail = 'me@example.com'\nmaildir = 'personal'\n[[accounts]]\nid = 'work'\nname = 'Work'\nemail = 'me@work.test'\nmaildir = 'work'\n",
        )
        .unwrap();

        let mut config = Config::load(Some(&path)).unwrap();

        assert_eq!(config.identity().as_deref(), Some("Work <me@work.test>"));
        assert_eq!(config.maildir, Some(root.join("work")));
        assert_eq!(config.active_account_index(), 1);
        assert!(config.activate(0));
        assert_eq!(config.email.as_deref(), Some("me@example.com"));
        fs::remove_dir_all(root).unwrap();
    }
}
