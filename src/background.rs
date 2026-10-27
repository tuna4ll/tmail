//! Sync, IDLE watchers and outgoing delivery.

use super::*;

impl App {
    pub(crate) fn retry_outbox(&mut self) {
        if self.sending {
            self.notice = Some("A message is still being sent.".into());
            return;
        }
        let Some(mail_id) = self.active_mail_id() else {
            return;
        };
        let Some(index) = self.mails.iter().position(|mail| mail.id == mail_id) else {
            return;
        };
        let mail = self.mails[index].clone();
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

    pub(crate) fn review_delivery(&mut self) {
        if self.sending {
            return;
        }
        let Some(id) = self.active_mail_id() else {
            return;
        };
        let Some(mail) = self.mails.iter().find(|mail| mail.id == id).cloned() else {
            return;
        };
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
        self.manual_sync = true;
        self.notice = Some("Syncing…".into());
        for (id, remote, _) in accounts {
            if remote.has_credentials() {
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
                if visible && (synced.changed || manual) {
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
            Err(error) => {
                if visible {
                    self.notice = Some(format!("Sync failed · {error}"));
                }
                if self.config.remote.as_ref().is_some_and(|active| {
                    active.username == remote.username && active.imap_host == remote.imap_host
                }) && error.kind == remote::ErrorKind::Auth
                {
                    self.prompt_login();
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
        std::thread::spawn(move || {
            let mut backoff = 30;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match imap_client::wait_for_change(
                    &remote,
                    &folder,
                    Duration::from_secs(25 * 60),
                    &stop,
                ) {
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
        self.sending = true;
        self.notice = Some("Sending message…".into());
        let sender = self.delivery_tx.clone();
        tokio::spawn(async move {
            let _ = sender.send(delivery.run().await);
        });
    }

    pub(crate) fn finish_delivery(&mut self, notice: String) {
        self.sending = false;
        self.notice = Some(notice);
        self.reload();
    }
}
