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
            let waiting: Vec<String> = self
                .config
                .accounts
                .iter()
                .filter(|account| account.id != self.config.current_account().id)
                .filter(|account| {
                    account
                        .remote
                        .as_ref()
                        .is_some_and(|remote| !remote.has_credentials())
                })
                .map(config::Account::label)
                .collect();
            if !waiting.is_empty() && self.overlay.is_none() {
                self.notice = Some(format!(
                    "Sign in to {} to sync it · press p, choose it, then l",
                    waiting.join(", ")
                ));
            }
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
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('y' | 'Y'))
                && let Some(link) = &form.oauth_link
            {
                copy_to_clipboard(link);
                form.info = Some("Sign-in link copied.".into());
            }
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        match form.handle_key(key) {
            account_setup::Action::Submit => return self.submit_account(form),
            account_setup::Action::Discover(email) => {
                let lookup = email.clone();
                self.spawn_task(move || Task::Discovered {
                    result: discover::discover(&lookup),
                    email,
                });
            }
            account_setup::Action::Trust => return self.trust_certificate(form),
            account_setup::Action::CopyLink => {
                if let Some(link) = &form.oauth_link {
                    copy_to_clipboard(link);
                }
            }
            account_setup::Action::None => {}
        }
        self.overlay = Some(Overlay::AccountSetup(form));
    }

    /// Checks the form's account against its servers in the background.
    fn submit_account(&mut self, mut form: Box<account_setup::Form>) {
        if self.checking_account {
            form.error =
                Some("The previous connection check is finishing. Try again shortly.".into());
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        match form.build() {
            Ok(account) => {
                self.setup_generation += 1;
                form.request = self.setup_generation;
                form.busy = true;
                form.error = None;
                form.untrusted = None;
                form.oauth_link = None;
                self.checking_account = true;
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                self.setup_cancel = Some(cancel.clone());
                let request = form.request;
                let mode = form.mode;
                let sender = self.setup_tx.clone();
                let links = self.task_tx.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        account_setup::verify(account, mode, &cancel, &mut |url| {
                            let _ = links.send(Task::SetupLink {
                                request,
                                url: url.to_owned(),
                            });
                        })
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err("Connection check stopped. Try again.".to_owned().into())
                    });
                    let _ = sender.send(SetupResult { request, result });
                });
            }
            Err((field, error)) => {
                form.field = field;
                form.error = Some(error);
            }
        }
        self.overlay = Some(Overlay::AccountSetup(form));
    }

    /// Saves the certificate the form offered next to the config and checks again.
    fn trust_certificate(&mut self, mut form: Box<account_setup::Form>) {
        let Some(presented) = form.untrusted.take() else {
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        };
        let saved = (|| -> Result<PathBuf, String> {
            let directory = self
                .config
                .path
                .as_ref()
                .and_then(|path| path.parent())
                .ok_or("Start with --config PATH to choose where certificates are saved.")?
                .join("certs");
            fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
            let path = directory.join(format!("{}.pem", form.account.id));
            let mut bundle = fs::read_to_string(&path).unwrap_or_default();
            if !bundle.contains(presented.pem.trim()) {
                bundle.push_str(&presented.pem);
            }
            fs::write(&path, bundle).map_err(|error| error.to_string())?;
            Ok(path)
        })();
        match saved {
            Ok(path) => {
                form.tls_certificate = Some(path);
                form.info = Some(format!("Trusting the certificate of {}.", presented.host));
                self.submit_account(form);
            }
            Err(error) => {
                form.error = Some(format!("Unable to save the certificate: {error}"));
                self.overlay = Some(Overlay::AccountSetup(form));
            }
        }
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
        form.oauth_link = None;
        let account = match result.result {
            Ok(account) => account,
            Err(failure) => {
                if failure.untrusted.is_some() {
                    form.untrusted = failure.untrusted;
                    form.error = None;
                } else {
                    form.error = Some(failure.message);
                }
                self.overlay = Some(Overlay::AccountSetup(form));
                return;
            }
        };
        // Keeps the saving code below reading `result.account`, as before.
        struct Checked {
            account: config::Account,
        }
        let id = account.id.clone();
        let result = Checked { account };
        let saved = (|| -> Result<(), String> {
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
                if remote.keyring
                    && remote.separate_smtp()
                    && let Some(password) = &remote.smtp_session_password
                {
                    remote.store_smtp_keyring_password(&password.0)?;
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
        })();
        if let Err(error) = saved {
            form.error = Some(error);
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        let Some(root) = self.config.maildir.clone() else {
            self.notice = Some("The account has no Maildir. Edit it with p, then e.".into());
            return;
        };
        self.auth_failed.remove(&id);
        self.onboarding = false;
        self.last_sync.clear();
        self.stop_watchers();
        self.active_folder = "INBOX".into();
        self.query.clear();
        self.filter = InboxFilter::All;
        self.undo = None;
        self.reader = None;
        self.source = MailboxSource::Maildir(root);
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
                    label: match &account.remote {
                        Some(remote)
                            if self.auth_failed.contains(&account.id)
                                || !remote.has_credentials() =>
                        {
                            format!("{} · sign in needed (l)", account.label())
                        }
                        _ => account.label(),
                    },
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
