//! Loading the mailbox and keeping the conversation list and reader in step.

use super::*;

impl App {
    pub(crate) fn select_conversation(&mut self, index: usize) {
        let count = self.visible_conversations().len();
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if index != self.selected {
            self.selected = index;
            self.sync_workspace();
        }
    }

    /// Opens the newest message of the next or previous conversation in the list.
    pub(crate) fn open_adjacent(&mut self, forward: bool) {
        let count = self.visible_conversations().len();
        let target = if forward {
            self.selected + 1
        } else {
            self.selected.wrapping_sub(1)
        };
        if target >= count {
            self.notice = Some(if forward {
                "This is the last conversation.".into()
            } else {
                "This is the first conversation.".into()
            });
            return;
        }
        self.select_conversation(target);
        if let Some(mail_id) = self.entry_mail_id() {
            self.open_mail(&mail_id);
        }
    }

    pub(crate) fn sync_workspace(&mut self) {
        let Some(mail_id) = self.entry_mail_id() else {
            return;
        };
        let thread = self.selected_thread();
        if let Some(workspace) = &mut self.workspace {
            workspace.show_conversation(&thread, &mail_id);
        }
    }

    pub(crate) fn sync_inbox_selection(&mut self) {
        let Some(mail_id) = self
            .workspace
            .as_ref()
            .and_then(Workspace::selected_mail_id)
        else {
            return;
        };
        if let Some(index) = self.visible_conversations().iter().position(|index| {
            self.conversations[*index]
                .mail_ids
                .iter()
                .any(|id| id == &mail_id)
        }) {
            self.selected = index;
        }
    }

    pub(crate) fn open_mail(&mut self, mail_id: &str) {
        let mail = self
            .mails
            .iter()
            .find(|mail| mail.id == mail_id && mail_folder(mail) == "Drafts");
        if mail.is_some() && !matches!(self.source, MailboxSource::Demo) {
            let path = mail.and_then(|mail| mail.source_path.clone());
            let account = mail.and_then(|mail| self.account_index(mail));
            if let Some(path) = path {
                match draft::Composer::resume(&path) {
                    Ok(composer) => {
                        self.overlay = Some(Overlay::Compose(ComposeState {
                            composer,
                            field: 0,
                            error: None,
                            account,
                            suggestion: None,
                            protection: crypto::Protection::default(),
                            autosave: Default::default(),
                        }))
                    }
                    Err(error) => self.notice = Some(format!("Unable to open draft: {error}")),
                }
                return;
            }
        }
        if let Some(index) = self.visible_conversations().iter().position(|index| {
            self.conversations[*index]
                .mail_ids
                .iter()
                .any(|id| id == mail_id)
        }) {
            self.selected = index;
            self.reader = Some(Reader::new(mail_id.into()));
            if settings::with(|ui| ui.mark_read_on_open) {
                self.mark_seen(mail_id);
            }
            self.load_trusted_images(mail_id);
            self.emit_mail_event("open", mail_id);
            self.open_protected(mail_id);
            self.import_autocrypt(mail_id);
        }
    }

    /// Loads remote images right away for senders the `[reader]` table trusts.
    fn load_trusted_images(&mut self, mail_id: &str) {
        let Some(mail) = self.mails.iter().find(|mail| mail.id == mail_id) else {
            return;
        };
        if !mail.html || !settings::reader().trusts(&mail::email_of(&mail.from)) {
            return;
        }
        let mail = mail.clone();
        let blocked = self
            .reader
            .as_mut()
            .is_some_and(|reader| reader.body(&mail).has_blocked());
        if blocked {
            self.load_remote_images();
        }
    }

    /// Saves the key a sender attached with Autocrypt, in the background.
    fn import_autocrypt(&mut self, mail_id: &str) {
        if !self.config.crypto.autocrypt {
            return;
        }
        let Some(path) = self
            .mails
            .iter()
            .find(|mail| mail.id == mail_id)
            .and_then(|mail| mail.source_path.clone())
        else {
            return;
        };
        let crypto = self.config.crypto.clone();
        self.spawn_task(move || {
            let message = fs::read(&path)
                .ok()
                .and_then(|contents| crypto::import_autocrypt(&contents, &crypto));
            match message {
                Some(message) => Task::Done {
                    message,
                    reload: false,
                },
                None => Task::Nothing,
            }
        });
    }

    /// Decrypts or verifies a protected message for the reader. Decryption may ask for
    /// a passphrase, so it borrows the terminal; checking a signature runs in the
    /// background.
    pub(crate) fn open_protected(&mut self, mail_id: &str) {
        if let Some(result) = self.opened.get(mail_id).cloned() {
            self.show_opened(mail_id.to_owned(), result);
            return;
        }
        let Some(mail) = self.mails.iter().find(|mail| mail.id == mail_id) else {
            return;
        };
        let (Some(kind), Some(path)) = (mail.security, mail.source_path.clone()) else {
            return;
        };
        if kind.needs_secret() {
            self.pending_terminal = Some(TerminalJob::Open(mail_id.to_owned()));
            return;
        }
        let crypto = self.config.crypto.clone();
        let mail_id = mail_id.to_owned();
        self.spawn_task(move || Task::Opened {
            result: fs::read(&path)
                .map_err(|error| error.to_string())
                .and_then(|contents| crypto::open(&contents, kind, &crypto))
                .map(|opened| (opened.content, opened.status)),
            mail_id,
        });
    }

    pub(crate) fn show_opened(
        &mut self,
        mail_id: String,
        result: Result<(Vec<u8>, String), String>,
    ) {
        if let Some(reader) = &mut self.reader
            && reader.mail_id() == mail_id
        {
            match &result {
                Ok((content, status)) => reader.set_opened(content.clone(), status.clone()),
                Err(error) => reader.set_security(format!("⚠ {error}")),
            }
            self.clear_screen = true;
        }
        // An encrypted message may carry its real subject inside.
        if let Ok((content, _)) = &result
            && let Some(subject) = crypto::subject_of(content)
            && let Some(mail) = self.mails.iter_mut().find(|mail| mail.id == mail_id)
            && mail.subject != subject
        {
            mail.subject = subject;
            self.rebuild_mailbox(Some(&mail_id));
        }
        self.opened.insert(mail_id, result);
    }

    /// Opening a message marks it read, like other mail clients do.
    pub(crate) fn mark_seen(&mut self, mail_id: &str) {
        let Some(index) = self
            .mails
            .iter()
            .position(|mail| mail.id == mail_id && mail.unread)
        else {
            return;
        };
        if let Ok(change) = self.source.set_seen(&self.mails[index], true) {
            if let Some(change) = &change {
                self.mails[index].source_path = Some(change.after_path().to_path_buf());
            }
            self.mails[index].unread = false;
            self.refresh_fingerprint();
        }
    }

    pub(crate) fn reload(&mut self) {
        self.mailbox_state = MailboxState::Loading;
        self.spinner_tick = 0;
        let source = self.source.clone();
        let sender = self.load_tx.clone();
        self.load_generation += 1;
        let generation = self.load_generation;
        tokio::task::spawn_blocking(move || {
            let fingerprint = source.fingerprint().ok();
            let result = source.load();
            let _ = sender.send(MailboxLoad {
                generation,
                fingerprint,
                result,
            });
        });
    }

    pub(crate) fn apply_load(&mut self, load: MailboxLoad) {
        if load.generation != self.load_generation {
            return;
        }
        if let Some(fingerprint) = load.fingerprint {
            self.mailbox_fingerprint = fingerprint;
        }
        match load.result {
            Ok(report) if report.mails.is_empty() && report.attempted > 0 => {
                self.clear_mailbox();
                self.skipped = report.skipped.len();
                self.mailbox_state = MailboxState::Error(
                    "No readable messages found. Check the message format, then retry.".into(),
                );
            }
            Ok(report) if report.mails.is_empty() => {
                self.clear_mailbox();
                self.folders = report.folders;
                self.mailbox_state = MailboxState::Empty;
            }
            Ok(report) => {
                let active_mail_id = self.active_mail_id();
                let before: std::collections::HashSet<String> =
                    self.mails.iter().map(|mail| mail.id.clone()).collect();
                self.mails = report.mails;
                self.folders = report.folders;
                for mail in &mut self.mails {
                    if let Some(Ok((content, _))) = self.opened.get(&mail.id)
                        && let Some(subject) = crypto::subject_of(content)
                    {
                        mail.subject = subject;
                    }
                }
                // The address book comes first, then everyone mail was exchanged with.
                let mut contacts: Vec<String> = self
                    .address_book
                    .iter()
                    .map(contacts::Contact::address)
                    .collect();
                let known: Vec<String> = self
                    .address_book
                    .iter()
                    .map(|contact| contact.email.to_lowercase())
                    .collect();
                contacts.extend(
                    complete::contacts(&self.mails)
                        .into_iter()
                        .filter(|value| !known.contains(&mail::email_of(value))),
                );
                self.contacts = contacts;
                self.skipped = report.skipped.len();
                let folders = self.folder_items();
                if !folders
                    .iter()
                    .any(|folder| folder.name == self.active_folder)
                {
                    self.active_folder = folders
                        .first()
                        .map(|folder| folder.name.clone())
                        .unwrap_or_else(|| "INBOX".into());
                }
                self.rebuild_mailbox(active_mail_id.as_deref());
                if self.reader.as_ref().is_some_and(|reader| {
                    !self.mails.iter().any(|mail| mail.id == reader.mail_id())
                }) {
                    self.reader = None;
                }
                self.mailbox_state = MailboxState::Ready;
                self.emit_new_mail(&before);
            }
            Err(error) => {
                self.clear_mailbox();
                self.mailbox_state = MailboxState::Error(error.to_string());
            }
        }
    }

    pub(crate) fn clear_mailbox(&mut self) {
        self.mails.clear();
        self.conversations.clear();
        self.workspace = None;
        self.reader = None;
        self.selected = 0;
        self.skipped = 0;
    }

    pub(crate) fn active_mail_id(&self) -> Option<String> {
        self.reader
            .as_ref()
            .map(|reader| reader.mail_id().to_owned())
            .or_else(|| {
                self.workspace
                    .as_ref()
                    .and_then(Workspace::selected_mail_id)
            })
            .or_else(|| self.entry_mail_id())
    }

    pub(crate) fn rebuild_mailbox(&mut self, mail_id: Option<&str>) {
        self.search_cache.get_mut().clear();
        // Threads draw on every folder except the bins and queues, so the map shows
        // replies from Sent next to the Inbox messages they answer.
        // `in:all` searches every folder, bins included.
        let everywhere = self.searches_everywhere();
        self.built_everywhere = everywhere;
        let active = self.active_folder.clone();
        let mut conversations = group_conversations_where(&self.mails, |mail| {
            let folder = mail_folder(mail);
            if folder == active {
                Some(0)
            } else if !everywhere && ["Trash", "Junk", "Outbox", "Drafts"].contains(&folder) {
                None
            } else {
                Some(1)
            }
        });
        conversations.retain(|conversation| {
            everywhere
                || conversation
                    .mails(&self.mails)
                    .iter()
                    .any(|mail| mail_folder(mail) == active)
        });
        // Newest activity first, like every other mail client, unless ui.sort says not.
        let oldest_first = settings::with(|ui| ui.sort == "oldest");
        conversations.sort_by_cached_key(|conversation| {
            let latest = conversation
                .mails(&self.mails)
                .iter()
                .filter_map(|mail| mail.timestamp)
                .max()
                .unwrap_or_default();
            if oldest_first { latest } else { -latest }
        });
        self.conversations = conversations;
        let visible = self.visible_conversations();
        self.selected = mail_id
            .and_then(|mail_id| {
                visible.iter().position(|index| {
                    self.conversations[*index]
                        .mail_ids
                        .iter()
                        .any(|id| id == mail_id)
                })
            })
            .unwrap_or_default()
            .min(visible.len().saturating_sub(1));
        let thread = self.selected_thread();
        self.workspace = self
            .entry_mail_id()
            .map(|mail_id| Workspace::new(&thread, &mail_id));
        self.update_script_mailbox();
        if self.script_folder != self.active_folder {
            self.script_folder = self.active_folder.clone();
            let folder = self.active_folder.clone();
            self.emit_event("folder", &[("name", folder)]);
        }
    }

    pub(crate) fn visible_conversations(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let mut cache = self.search_cache.borrow_mut();
        let candidates = if query.trim().is_empty() {
            None
        } else {
            cache.candidates(&self.mails, &query)
        };
        self.conversations
            .iter()
            .enumerate()
            .filter(|(_, conversation)| match self.filter {
                InboxFilter::All => true,
                InboxFilter::Unread => conversation.is_unread(&self.mails),
                InboxFilter::Starred => conversation.is_starred(&self.mails),
            })
            .filter(|(_, conversation)| {
                conversation.positions.iter().any(|&position| {
                    candidates.is_none_or(|found| found.contains(&(position as u32)))
                        && self
                            .mails
                            .get(position)
                            .is_some_and(|mail| mail.matches(&query))
                })
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn selected_conversation(&self) -> Option<&Conversation> {
        let index = *self.visible_conversations().get(self.selected)?;
        self.conversations.get(index)
    }

    pub(crate) fn searches_everywhere(&self) -> bool {
        mail::search_terms(&self.query)
            .iter()
            .any(|term| term == "in:all")
    }

    pub(crate) fn refresh_inbox(&mut self) {
        if self.searches_everywhere() != self.built_everywhere {
            self.rebuild_mailbox(self.active_mail_id().as_deref());
        }
        let count = self.visible_conversations().len();
        self.selected = self.selected.min(count.saturating_sub(1));
        if count > 0 {
            self.sync_workspace();
        }
    }

    /// The message a conversation opens on: its newest message in the open folder, so
    /// opening an Inbox thread never lands on a copy in Sent.
    pub(crate) fn entry_mail_id(&self) -> Option<String> {
        let mails = self.selected_conversation()?.mails(&self.mails);
        mails
            .iter()
            .rev()
            .find(|mail| mail_folder(mail) == self.active_folder)
            .or(mails.last())
            .map(|mail| mail.id.clone())
    }

    /// The messages of the selected conversation, from every folder it spans.
    pub(crate) fn selected_thread(&self) -> Vec<Mail> {
        self.selected_conversation()
            .map(|conversation| {
                conversation
                    .mails(&self.mails)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Records the mailbox as it is after the app changed it, so the change does not
    /// look like one made by another program.
    pub(crate) fn refresh_fingerprint(&mut self) {
        self.scan_mailbox(true);
    }

    /// Looks for changes made outside the app. The scan walks every folder, so it runs
    /// off the UI thread.
    pub(crate) fn check_mailbox_changes(&mut self) {
        if self.fingerprint_busy
            || !matches!(
                self.mailbox_state,
                MailboxState::Ready | MailboxState::Empty
            )
        {
            return;
        }
        self.scan_mailbox(false);
    }

    fn scan_mailbox(&mut self, baseline: bool) {
        self.fingerprint_generation += 1;
        self.fingerprint_busy = true;
        let generation = self.fingerprint_generation;
        let source = self.source.clone();
        self.spawn_task(move || Task::Fingerprint {
            generation,
            value: source.fingerprint().ok(),
            baseline,
        });
    }
}

pub(crate) fn mail_folder(mail: &Mail) -> &str {
    mail.mailbox.as_deref().unwrap_or("INBOX")
}
