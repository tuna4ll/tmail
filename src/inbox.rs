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
            self.mark_seen(mail_id);
        }
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
                self.mails = report.mails;
                self.folders = report.folders;
                self.contacts = complete::contacts(&self.mails);
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
        // Newest activity first, like every other mail client.
        conversations.sort_by_cached_key(|conversation| {
            std::cmp::Reverse(
                conversation
                    .mails(&self.mails)
                    .iter()
                    .filter_map(|mail| mail.timestamp)
                    .max()
                    .unwrap_or_default(),
            )
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
    }

    pub(crate) fn visible_conversations(&self) -> Vec<usize> {
        self.conversations
            .iter()
            .enumerate()
            .filter(|(_, conversation)| match self.filter {
                InboxFilter::All => true,
                InboxFilter::Unread => conversation.is_unread(&self.mails),
                InboxFilter::Starred => conversation.is_starred(&self.mails),
            })
            .filter(|(_, conversation)| conversation.matches(&self.mails, &self.query))
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

    pub(crate) fn refresh_fingerprint(&mut self) {
        if let Ok(fingerprint) = self.source.fingerprint() {
            self.mailbox_fingerprint = fingerprint;
        }
    }

    pub(crate) fn check_mailbox_changes(&mut self) {
        if !matches!(
            self.mailbox_state,
            MailboxState::Ready | MailboxState::Empty
        ) {
            return;
        }
        if let Ok(fingerprint) = self.source.fingerprint()
            && fingerprint != self.mailbox_fingerprint
        {
            self.notice = Some("Mailbox changed · refreshing".into());
            self.reload();
        }
    }
}

pub(crate) fn mail_folder(mail: &Mail) -> &str {
    mail.mailbox.as_deref().unwrap_or("INBOX")
}
