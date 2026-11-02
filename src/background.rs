//! Sync, IDLE watchers and outgoing delivery.

use super::*;

/// Shows a desktop notification through the system's notifier, falling back to the
/// terminal's own notification escape codes.
pub(crate) fn notify(title: &str, body: &str) {
    let clean = |value: &str| {
        value
            .chars()
            .filter(|character| !character.is_control() && !"\\\";".contains(*character))
            .collect::<String>()
    };
    let (title, body) = (clean(title), clean(body));
    let custom = settings::with(|ui| ui.notify_command.clone());
    let command = if let Some((program, arguments)) = custom.split_first() {
        let fill = |value: &String| value.replace("{title}", &title).replace("{body}", &body);
        Command::new(fill(program))
            .args(arguments.iter().map(fill))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "display notification \"{body}\" with title \"{title}\""
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    } else if cfg!(windows) {
        Err(std::io::Error::other("no notifier"))
    } else {
        Command::new("notify-send")
            .args([
                "--app-name",
                "termail",
                "--icon",
                "mail-unread",
                &title,
                &body,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    match command {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(_) => {
            use std::io::Write;
            let mut output = stdout();
            // OSC 777 (urxvt, foot, Ghostty, WezTerm) and OSC 9 (iTerm2, kitty, Windows Terminal).
            let _ = write!(
                output,
                "\x1b]777;notify;{title};{body}\x07\x1b]9;{title}: {body}\x07"
            );
            let _ = output.flush();
        }
    }
}

impl App {
    pub(crate) fn retry_outbox(&mut self) {
        let Some(mail_id) = self.active_mail_id() else {
            return;
        };
        let Some(index) = self.mails.iter().position(|mail| mail.id == mail_id) else {
            return;
        };
        let mail = self.mails[index].clone();
        if self.is_sending(&mail) {
            self.notice = Some("This message is still being sent.".into());
            return;
        }
        self.deliver_mail(mail);
    }

    fn is_sending(&self, mail: &Mail) -> bool {
        mail.source_path
            .as_ref()
            .is_some_and(|path| self.sending.contains(path))
    }

    fn deliver_mail(&mut self, mail: Mail) {
        let (source, command, account) = match self.delivery_account(&mail) {
            Ok(account) => account,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        self.start_delivery(delivery::Delivery {
            remote: None,
            source,
            mail,
            command,
            account,
        });
    }

    /// Retries Outbox messages that failed temporarily once their backoff has passed.
    pub(crate) fn retry_due_deliveries(&mut self) {
        if self.last_retry_check.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.last_retry_check = std::time::Instant::now();
        let due: Vec<Mail> = self
            .mails
            .iter()
            .filter(|mail| mail_folder(mail) == "Outbox" && !self.is_sending(mail))
            .filter(|mail| {
                let root = match (&self.source, mail.account.as_deref()) {
                    (MailboxSource::Maildir(root), _) => Some(root.clone()),
                    (MailboxSource::Unified(accounts), Some(id)) => accounts
                        .iter()
                        .find(|(account, _)| account == id)
                        .map(|(_, root)| root.clone()),
                    _ => None,
                };
                match (root, &mail.source_path) {
                    (Some(root), Some(path)) => outbox::retry_due(&root, path),
                    _ => false,
                }
            })
            .cloned()
            .collect();
        for mail in due {
            self.deliver_mail(mail);
        }
    }

    pub(crate) fn review_delivery(&mut self) {
        let Some(id) = self.active_mail_id() else {
            return;
        };
        let Some(mail) = self.mails.iter().find(|mail| mail.id == id).cloned() else {
            return;
        };
        if self.is_sending(&mail) {
            return;
        }
        match self.delivery_account(&mail) {
            Ok((source, command, account)) => {
                self.overlay = Some(Overlay::DeliveryReview {
                    delivery: Box::new(delivery::Delivery {
                        remote: None,
                        source,
                        mail,
                        command,
                        account,
                    }),
                })
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub(crate) fn delivery_account(
        &self,
        mail: &Mail,
    ) -> Result<(MailboxSource, Vec<String>, String), String> {
        if mail_folder(mail) != "Outbox" {
            return Err("Select an Outbox message to retry.".into());
        }
        match &self.source {
            MailboxSource::Maildir(_) => Ok((
                self.source.clone(),
                self.config.send_command.clone(),
                self.config.delivery_identity(),
            )),
            MailboxSource::Unified(_) => {
                let account = self
                    .config
                    .accounts
                    .iter()
                    .find(|account| Some(account.id.as_str()) == mail.account.as_deref())
                    .ok_or("Message account is no longer configured.")?;
                let root = account
                    .maildir
                    .clone()
                    .ok_or("Message account has no Maildir.")?;
                let mut config = self.config.clone();
                let index = config
                    .accounts
                    .iter()
                    .position(|candidate| candidate.id == account.id)
                    .unwrap();
                config.activate(index);
                Ok((
                    MailboxSource::Maildir(root),
                    account.send_command.clone(),
                    config.delivery_identity(),
                ))
            }
            MailboxSource::Demo => Err("Open a Maildir account before sending.".into()),
        }
    }

    pub(crate) fn sync_if_due(&mut self) {
        if self.onboarding
            || matches!(self.overlay, Some(Overlay::AccountSetup(_)))
            || !self.uses_accounts()
        {
            return;
        }
        for (id, remote, _) in self.remote_accounts() {
            if self.auth_failed.contains(&id) {
                continue;
            }
            let due = self.last_sync.get(&id).is_none_or(|time| {
                remote.sync_interval_secs > 0
                    && time.elapsed().as_secs() >= remote.sync_interval_secs
            });
            if due && remote.has_credentials() {
                self.start_account_sync(&id);
            }
        }
    }

    pub(crate) fn start_sync(&mut self) {
        if !self.uses_accounts() {
            return;
        }
        let accounts = self.remote_accounts();
        if accounts.is_empty() {
            self.notice = Some("This account has no IMAP connection.".into());
            return;
        }
        if self
            .config
            .remote
            .as_ref()
            .is_some_and(|remote| !remote.has_credentials())
            && matches!(&self.source, MailboxSource::Maildir(root) if self.config.maildir.as_ref() == Some(root))
        {
            self.prompt_login();
            return;
        }
        let active = self.config.current_account().id;
        if self.auth_failed.contains(&active) {
            self.open_account_setup(account_setup::Mode::Login);
            return;
        }
        self.manual_sync = true;
        self.notice = Some("Syncing…".into());
        for (id, remote, _) in accounts {
            if remote.has_credentials() && !self.auth_failed.contains(&id) {
                self.start_account_sync(&id);
            }
        }
    }

    pub(crate) fn start_account_sync(&mut self, id: &str) {
        let Some((_, remote, root)) = self
            .remote_accounts()
            .into_iter()
            .find(|(candidate, ..)| candidate == id)
        else {
            return;
        };
        if !self.syncing.insert(id.to_owned()) {
            return;
        }
        self.last_sync
            .insert(id.to_owned(), std::time::Instant::now());
        let sender = self.sync_tx.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || imap_client::sync(&remote, &root))
                .await
                .unwrap_or_else(|_| Err("Sync worker stopped.".into()));
            let _ = sender.send((id, result));
        });
    }

    pub(crate) fn finish_sync(
        &mut self,
        id: String,
        result: Result<imap_client::Synced, remote::RemoteError>,
    ) {
        self.syncing.remove(&id);
        let manual = self.manual_sync && self.syncing.is_empty();
        if self.syncing.is_empty() {
            self.manual_sync = false;
        }
        let Some((_, remote, root)) = self
            .remote_accounts()
            .into_iter()
            .find(|(candidate, ..)| *candidate == id)
        else {
            return;
        };
        let visible = self.shows_maildir(&root);
        match result {
            Ok(synced) => {
                if !self.watchers.contains_key(&id) && remote.sync_interval_secs > 0 {
                    self.watch_account(id.clone(), remote);
                }
                if synced.downloaded > 0 && self.config.notifications != Some(false) {
                    let label = self
                        .config
                        .accounts
                        .iter()
                        .find(|account| account.id == id)
                        .map_or_else(|| self.account_label.clone(), config::Account::label);
                    notify(
                        "termail",
                        &format!(
                            "{} new message{} in {label}",
                            synced.downloaded,
                            if synced.downloaded == 1 { "" } else { "s" }
                        ),
                    );
                }
                self.emit_event(
                    "sync",
                    &[
                        ("account", id.clone()),
                        ("summary", synced.summary.clone()),
                        ("downloaded", synced.downloaded.to_string()),
                    ],
                );
                if visible && (synced.changed || manual || synced.held > 0) {
                    self.notice = Some(synced.summary);
                    self.reload();
                } else if !visible && synced.downloaded > 0 {
                    let label = self
                        .config
                        .accounts
                        .iter()
                        .find(|account| account.id == id)
                        .map_or(id.clone(), config::Account::label);
                    self.notice = Some(format!(
                        "{} new in {label} · press p to switch",
                        synced.downloaded
                    ));
                }
            }
            Err(error) if error.kind == remote::ErrorKind::Auth => {
                // Retrying a rejected password every minute can get the account locked,
                // so the account waits for a new one.
                self.auth_failed.insert(id.clone());
                if let Some(stop) = self.watchers.remove(&id) {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                remote.forget_cached();
                let active = self.config.remote.as_ref().is_some_and(|active| {
                    active.username == remote.username && active.imap_host == remote.imap_host
                });
                if active && self.overlay.is_none() {
                    self.open_account_setup(account_setup::Mode::Login);
                    if let Some(Overlay::AccountSetup(form)) = &mut self.overlay {
                        form.error = Some(error.message);
                    }
                } else {
                    let label = self
                        .config
                        .accounts
                        .iter()
                        .find(|account| account.id == id)
                        .map_or_else(|| self.account_label.clone(), config::Account::label);
                    self.notice = Some(format!(
                        "Sign-in to {label} failed · press p, choose it, then l · {}",
                        error.message
                    ));
                }
            }
            Err(error) => {
                if visible {
                    self.notice = Some(format!("Sync failed · {error}"));
                }
            }
        }
    }

    /// Keeps an IDLE connection open so new mail starts a sync right away.
    pub(crate) fn watch_account(&mut self, id: String, remote: remote::RemoteConfig) {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.watchers.insert(id.clone(), stop.clone());
        let folders = if remote.watch.is_empty() {
            vec![remote.inbox.clone()]
        } else {
            remote.watch.clone()
        };
        for folder in folders {
            self.watch_folder(id.clone(), remote.clone(), folder, stop.clone());
        }
    }

    /// Keeps one IDLE connection on `folder`; IMAP watches one folder per connection.
    pub(crate) fn watch_folder(
        &self,
        id: String,
        remote: remote::RemoteConfig,
        folder: String,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let sender = self.watch_tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("termail-idle-{folder}"))
            .spawn(move || {
                let mut backoff = 30;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // The IMAP library panics on some unexpected IDLE replies; treat that
                    // like a dropped connection instead of losing the watcher.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        imap_client::wait_for_change(
                            &remote,
                            &folder,
                            Duration::from_secs(25 * 60),
                            &stop,
                        )
                    }))
                    .unwrap_or_else(|_| Err("The IDLE connection failed.".into()));
                    match outcome {
                        Ok(changed) => {
                            backoff = 30;
                            if changed && sender.send(id.clone()).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.kind == remote::ErrorKind::Unsupported => return,
                        Err(_) => {
                            for _ in 0..backoff {
                                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                    return;
                                }
                                std::thread::sleep(Duration::from_secs(1));
                            }
                            backoff = (backoff * 2).min(15 * 60);
                        }
                    }
                }
            });
        if spawned.is_err() {
            crate::log::write("idle", "unable to start a watcher thread");
        }
    }

    pub(crate) fn stop_watchers(&mut self) {
        for (_, stop) in self.watchers.drain() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(crate) fn start_delivery(&mut self, mut delivery: delivery::Delivery) {
        let mut config = self.config.clone();
        if config.delivery_identity() != delivery.account {
            for index in 0..config.accounts.len() {
                config.activate(index);
                if config.delivery_identity() == delivery.account {
                    break;
                }
            }
        }
        if config.delivery_identity() != delivery.account {
            self.notice = Some("Message account is no longer configured.".into());
            return;
        }
        if config.remote.is_some()
            && !matches!(&delivery.source, MailboxSource::Maildir(root) if config.maildir.as_ref() == Some(root))
        {
            self.notice = Some("Select the configured account before sending through SMTP.".into());
            return;
        }
        delivery.remote = config.remote;
        let path = delivery.mail.source_path.clone();
        if let Some(path) = &path {
            self.sending.insert(path.clone());
        }
        self.notice = Some(if self.sending.len() > 1 {
            format!("Sending {} messages…", self.sending.len())
        } else {
            "Sending message…".into()
        });
        let sender = self.delivery_tx.clone();
        tokio::spawn(async move {
            let _ = sender.send((path, delivery.run().await));
        });
    }

    pub(crate) fn finish_delivery(&mut self, path: Option<PathBuf>, notice: String) {
        if let Some(path) = path {
            self.sending.remove(&path);
        }
        self.notice = Some(notice);
        self.reload();
    }
}
