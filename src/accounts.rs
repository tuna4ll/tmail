//! Signing in, adding and switching accounts.

use super::*;

impl App {
    pub(crate) fn prepare_accounts(&mut self, needs_setup: bool, explicit_source: bool) {
        if needs_setup {
            self.onboarding = true;
            self.load_generation += 1;
            self.clear_mailbox();
            self.open_account_setup(account_setup::Mode::Add);
        } else if !explicit_source {
            self.prompt_login();
        }
    }

    pub(crate) fn prompt_login(&mut self) {
        if self.overlay.is_none()
            && matches!(&self.source, MailboxSource::Maildir(root) if self.config.maildir.as_ref() == Some(root))
            && self
                .config
                .remote
                .as_ref()
                .is_some_and(|remote| !remote.has_credentials())
        {
            self.open_account_setup(account_setup::Mode::Login);
        }
    }

    pub(crate) fn open_account_setup(&mut self, mode: account_setup::Mode) {
        if !self.sending.is_empty() {
            self.notice =
                Some("Wait for sending to finish before changing account settings.".into());
            return;
        }
        if mode != account_setup::Mode::Add && self.config.remote.is_none() {
            self.notice = Some(
                "This is a local account. Use :account add to connect an IMAP account.".into(),
            );
            return;
        }
        self.overlay = Some(Overlay::AccountSetup(Box::new(account_setup::Form::new(
            &self.config,
            mode,
        ))));
    }

    pub(crate) fn handle_account_key(&mut self, mut form: Box<account_setup::Form>, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.setup_generation += 1;
            if let Some(cancel) = &self.setup_cancel {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        if form.busy {
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        let submit = key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('s')
            || key.code == KeyCode::Enter
                && (form.mode == account_setup::Mode::Login || form.field >= 10);
        if submit {
            if self.checking_account {
                form.error =
                    Some("The previous connection check is finishing. Try again shortly.".into());
            } else {
                match form.build() {
                    Ok(account) => {
                        self.setup_generation += 1;
                        form.request = self.setup_generation;
                        form.busy = true;
                        form.error = None;
                        self.checking_account = true;
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        self.setup_cancel = Some(cancel.clone());
                        let request = form.request;
                        let sender = self.setup_tx.clone();
                        tokio::spawn(async move {
                            let checked = account.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                account_setup::verify(&checked, &cancel)
                            })
                            .await
                            .unwrap_or_else(|_| Err("Connection check stopped. Try again.".into()));
                            let _ = sender.send(SetupResult {
                                request,
                                account,
                                result,
                            });
                        });
                    }
                    Err((field, error)) => {
                        form.field = field;
                        form.error = Some(error);
                    }
                }
            }
        } else {
            match key.code {
                KeyCode::Tab | KeyCode::Down | KeyCode::Enter => form.move_field(false),
                KeyCode::BackTab | KeyCode::Up => form.move_field(true),
                KeyCode::Backspace => {
                    if let Some(value) = form.values.get_mut(form.field) {
                        value.pop();
                    }
                }
                KeyCode::Char(' ') if form.field == 10 => form.make_default = !form.make_default,
                KeyCode::Char(' ') if form.field == 11 => form.remember = !form.remember,
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(value) = form.values.get_mut(form.field) {
                        use zeroize::Zeroize;
                        value.zeroize();
                    }
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.input(&character.to_string())
                }
                _ => {}
            }
        }
        self.overlay = Some(Overlay::AccountSetup(form));
    }

    pub(crate) fn finish_account_setup(&mut self, result: SetupResult) {
        self.checking_account = false;
        self.setup_cancel = None;
        if result.request != self.setup_generation {
            return;
        }
        let Some(Overlay::AccountSetup(mut form)) = self.overlay.take() else {
            return;
        };
        if form.request != result.request {
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        form.busy = false;
        let saved = result.result.and_then(|_| {
            let root = result
                .account
                .maildir
                .as_ref()
                .ok_or("Set a local cache directory before connecting.")?;
            imap_client::initialize(root)?;
            if let Some(remote) = &result.account.remote {
                match &remote.session_password {
                    Some(password) if remote.keyring => {
                        remote.store_keyring_password(&password.0)?
                    }
                    _ if !remote.keyring => remote.forget_keyring_password(),
                    _ => {}
                }
            }
            let remembered = self
                .config
                .remote
                .as_ref()
                .is_some_and(|remote| remote.keyring);
            if form.mode == account_setup::Mode::Login
                && result
                    .account
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.keyring != remembered)
                && !self.config.accounts.is_empty()
            {
                let make_default =
                    self.config.default_account.as_deref() == Some(result.account.id.as_str());
                self.config.save_account(result.account, true, make_default)
            } else if form.mode == account_setup::Mode::Login {
                self.config.remote = result.account.remote.clone();
                if let Some(account) = self
                    .config
                    .accounts
                    .iter_mut()
                    .find(|account| account.id == result.account.id)
                {
                    account.remote = result.account.remote;
                }
                Ok(())
            } else {
                self.config.save_account(
                    result.account,
                    form.mode == account_setup::Mode::Edit,
                    form.make_default,
                )
            }
        });
        if let Err(error) = saved {
            form.error = Some(error);
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        self.onboarding = false;
        self.last_sync.clear();
        self.stop_watchers();
        self.active_folder = "INBOX".into();
        self.query.clear();
        self.filter = InboxFilter::All;
        self.undo = None;
        self.reader = None;
        self.source = MailboxSource::Maildir(self.config.maildir.clone().unwrap());
        self.account_label = self.config.current_account().label();
        self.notice = Some("Account connected".into());
        self.reload();
    }

    pub(crate) fn open_accounts(&mut self) {
        if self.config.accounts.is_empty() {
            self.open_account_setup(if self.config.remote.is_some() {
                account_setup::Mode::Edit
            } else {
                account_setup::Mode::Add
            });
            return;
        }
        let mut items = if self.config.accounts.len() > 1 {
            vec![AccountItem {
                label: "All accounts".into(),
                index: None,
            }]
        } else {
            Vec::new()
        };
        items.extend(
            self.config
                .accounts
                .iter()
                .enumerate()
                .map(|(index, account)| AccountItem {
                    label: account.label(),
                    index: Some(index),
                }),
        );
        let selected = if matches!(self.source, MailboxSource::Unified(_)) {
            0
        } else {
            self.config.active_account_index() + usize::from(self.config.accounts.len() > 1)
        };
        self.overlay = Some(Overlay::Accounts {
            selected,
            items,
            removing: false,
        });
    }

    pub(crate) fn remove_account(&mut self, index: usize) {
        if !self.sending.is_empty() {
            self.notice = Some("Wait for sending to finish before removing an account.".into());
            return;
        }
        let Some(account) = self.config.accounts.get(index).cloned() else {
            return;
        };
        match self.config.remove_account(&account.id) {
            Ok(()) => {
                if let Some(remote) = &account.remote
                    && remote.keyring
                {
                    remote.forget_keyring_password();
                }
                self.stop_watchers();
                self.syncing.remove(&account.id);
                self.last_sync.remove(&account.id);
                let root = account
                    .maildir
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                let active = self.config.active_account_index();
                self.select_account(Some(active));
                self.notice = Some(format!(
                    "Removed {}. Its mail stays in {root}.",
                    account.label()
                ));
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub(crate) fn select_account(&mut self, index: Option<usize>) {
        let source = match index {
            None => {
                let maildirs: Vec<(String, PathBuf)> = self
                    .config
                    .accounts
                    .iter()
                    .filter_map(|account| {
                        account
                            .maildir
                            .clone()
                            .map(|path| (account.id.clone(), path))
                    })
                    .collect();
                if maildirs.len() < 2 {
                    self.notice = Some("Configure Maildir paths for at least two accounts.".into());
                    return;
                }
                self.account_label = "all accounts".into();
                MailboxSource::Unified(maildirs)
            }
            Some(index) => {
                let Some(account) = self.config.accounts.get(index) else {
                    return;
                };
                let Some(path) = account.maildir.clone() else {
                    self.notice = Some("Set maildir for this account in config.toml.".into());
                    return;
                };
                self.config.activate(index);
                self.account_label = self.config.accounts[index].label();
                MailboxSource::Maildir(path)
            }
        };
        self.source = source;
        self.active_folder = "INBOX".into();
        self.filter = InboxFilter::All;
        self.query.clear();
        self.undo = None;
        self.reader = None;
        self.reload();
        self.prompt_login();
    }

    /// Accounts that mirror a server, as (id, remote settings, Maildir).
    pub(crate) fn remote_accounts(&self) -> Vec<(String, remote::RemoteConfig, PathBuf)> {
        if self.config.accounts.is_empty() {
            return self
                .config
                .remote
                .clone()
                .zip(self.config.maildir.clone())
                .map(|(remote, root)| vec![(self.config.current_account().id, remote, root)])
                .unwrap_or_default();
        }
        self.config
            .accounts
            .iter()
            .filter_map(|account| {
                Some((
                    account.id.clone(),
                    account.remote.clone()?,
                    account.maildir.clone()?,
                ))
            })
            .collect()
    }

    /// Whether the open mailbox is one of the configured accounts rather than a demo or
    /// a Maildir opened by path.
    pub(crate) fn uses_accounts(&self) -> bool {
        match &self.source {
            MailboxSource::Demo => false,
            MailboxSource::Unified(_) => true,
            MailboxSource::Maildir(root) => {
                self.config.maildir.as_ref() == Some(root)
                    || self
                        .config
                        .accounts
                        .iter()
                        .any(|account| account.maildir.as_ref() == Some(root))
            }
        }
    }

    pub(crate) fn shows_maildir(&self, root: &Path) -> bool {
        match &self.source {
            MailboxSource::Demo => false,
            MailboxSource::Maildir(path) => path == root,
            MailboxSource::Unified(accounts) => accounts.iter().any(|(_, path)| path == root),
        }
    }
}
