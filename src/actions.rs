//! Actions on messages and folders.

use super::*;

impl App {
    /// The messages an action applies to: the open or highlighted message, or the whole
    /// conversation when it is chosen in the list. `folder_only` keeps just the
    /// conversation's messages in the open folder.
    pub(crate) fn action_targets(&self, folder_only: bool) -> Vec<usize> {
        if self.reader.is_none()
            && self.focus == Pane::Inbox
            && let Some(conversation) = self.selected_conversation()
        {
            return conversation
                .positions
                .iter()
                .copied()
                .filter(|index| {
                    self.mails.get(*index).is_some_and(|mail| {
                        !folder_only
                            || self.built_everywhere
                            || mail_folder(mail) == self.active_folder
                    })
                })
                .collect();
        }
        self.active_mail_id()
            .and_then(|id| self.mails.iter().position(|mail| mail.id == id))
            .into_iter()
            .collect()
    }

    /// Applies `change` to each target, keeping every file move so `u` can undo them.
    pub(crate) fn change_messages(
        &mut self,
        targets: &[usize],
        label: &str,
        mut change: impl FnMut(&MailboxSource, &mut Mail) -> Result<Option<MaildirChange>, LoadError>,
    ) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        if targets.is_empty() {
            return;
        }
        let keep = self.active_mail_id();
        let snapshot = matches!(self.source, MailboxSource::Demo).then(|| self.mails.clone());
        let mut changes = Vec::new();
        let mut failure = None;
        for index in targets {
            match change(&self.source, &mut self.mails[*index]) {
                Ok(Some(moved)) => {
                    self.mails[*index].source_path = Some(moved.after_path().to_path_buf());
                    changes.push(moved);
                }
                Ok(None) => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        let label = if targets.len() > 1 {
            format!("{label} · {} messages", targets.len())
        } else {
            label.to_owned()
        };
        self.notice = Some(match failure {
            Some(error) => format!("{label} stopped: {error}"),
            None => label.clone(),
        });
        self.undo = Some(UndoAction {
            changes,
            demo_mails: snapshot,
            label,
        });
        self.refresh_fingerprint();
        self.rebuild_mailbox(keep.as_deref());
    }

    pub(crate) fn toggle_read(&mut self) {
        let targets = self.action_targets(false);
        // Anything unread makes the action "mark read"; otherwise it marks unread.
        let seen = targets.iter().any(|index| self.mails[*index].unread);
        let targets: Vec<usize> = if seen {
            targets
                .into_iter()
                .filter(|index| self.mails[*index].unread)
                .collect()
        } else {
            targets.last().copied().into_iter().collect()
        };
        let label = if seen { "Marked read" } else { "Marked unread" };
        self.change_messages(&targets, label, |source, mail| {
            let change = source.set_seen(mail, seen)?;
            mail.unread = !seen;
            Ok(change)
        });
    }

    pub(crate) fn toggle_starred(&mut self) {
        let targets = self.action_targets(false);
        // A starred conversation is unstarred; otherwise its newest message gets the star.
        let starred: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|index| self.mails[*index].starred)
            .collect();
        let star = starred.is_empty();
        let targets: Vec<usize> = if star {
            targets.last().copied().into_iter().collect()
        } else {
            starred
        };
        let label = if star { "Starred" } else { "Removed star" };
        self.change_messages(&targets, label, |source, mail| {
            let change = source.set_starred(mail, star)?;
            mail.starred = star;
            Ok(change)
        });
    }

    pub(crate) fn undo_last(&mut self) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let Some(undo) = self.undo.take() else {
            return;
        };
        if let Some(mails) = undo.demo_mails {
            self.mails = mails;
            self.rebuild_mailbox(None);
            self.notice = Some(format!("Undid {}", undo.label.to_lowercase()));
            return;
        }
        let mut failure = None;
        for change in undo.changes.into_iter().rev() {
            if let Err(error) = change.undo() {
                failure = Some(error);
            }
        }
        self.notice = Some(match failure {
            Some(error) => format!("Unable to undo everything: {error}"),
            None => format!("Undid {}", undo.label.to_lowercase()),
        });
        self.reload();
    }

    pub(crate) fn move_active_message(&mut self, archive: bool) {
        let targets = self.action_targets(true);
        if targets.is_empty() {
            return;
        }
        if !archive
            && targets
                .iter()
                .all(|index| mail_folder(&self.mails[*index]) == "Trash")
        {
            self.delete_forever(targets);
            return;
        }
        let (folder, label) = if archive {
            ("Archive", "Archived")
        } else {
            ("Trash", "Moved to Trash")
        };
        self.reader = None;
        self.change_messages(&targets, label, |source, mail| {
            let change = if archive {
                source.archive(mail)?
            } else {
                source.trash(mail)?
            };
            mail.mailbox = Some(folder.into());
            Ok(change)
        });
    }

    pub(crate) fn delete_forever(&mut self, mut targets: Vec<usize>) {
        let key = targets
            .iter()
            .map(|index| self.mails[*index].id.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if self.confirm_delete.as_deref() != Some(key.as_str()) {
            self.confirm_delete = Some(key);
            self.notice = Some(if targets.len() > 1 {
                format!(
                    "Press d again to delete these {} messages permanently.",
                    targets.len()
                )
            } else {
                "Press d again to delete this message permanently.".into()
            });
            return;
        }
        self.confirm_delete = None;
        self.reader = None;
        self.undo = None;
        targets.sort_unstable_by(|a, b| b.cmp(a));
        let count = targets.len();
        for index in targets {
            if let Err(error) = self.source.delete(&self.mails[index]) {
                self.notice = Some(format!("Unable to delete message: {error}"));
                self.reload();
                return;
            }
            self.mails.remove(index);
        }
        self.notice = Some(if count > 1 {
            format!("Deleted {count} messages permanently")
        } else {
            "Deleted permanently".into()
        });
        self.refresh_fingerprint();
        self.rebuild_mailbox(None);
    }

    pub(crate) fn folder_items(&self) -> Vec<FolderItem> {
        let mut names: Vec<String> = self
            .mails
            .iter()
            .map(|mail| mail_folder(mail).into())
            .chain(self.folders.iter().cloned())
            .collect();
        if matches!(
            self.source,
            MailboxSource::Maildir(_) | MailboxSource::Unified(_)
        ) {
            names.extend(
                ["INBOX", "Drafts", "Sent", "Archive", "Trash", "Outbox"]
                    .into_iter()
                    .map(str::to_owned),
            );
        }
        names.sort();
        names.dedup();
        if let Some(position) = names.iter().position(|name| name == "INBOX") {
            names.swap(0, position);
        }
        names
            .into_iter()
            .map(|name| {
                let messages = self.mails.iter().filter(|mail| mail_folder(mail) == name);
                let total = messages.clone().count();
                let unread = messages.filter(|mail| mail.unread).count();
                FolderItem {
                    name,
                    unread,
                    total,
                }
            })
            .collect()
    }

    pub(crate) fn open_folders(&mut self) {
        let items = self.folder_items();
        let selected = items
            .iter()
            .position(|folder| folder.name == self.active_folder)
            .unwrap_or_default();
        self.overlay = Some(Overlay::Folders {
            selected,
            items,
            moving: false,
        });
    }

    pub(crate) fn open_move_picker(&mut self) {
        let Some(current) = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
            .map(|mail| mail_folder(mail).to_owned())
        else {
            return;
        };
        let items: Vec<FolderItem> = self
            .folder_items()
            .into_iter()
            .filter(|folder| {
                folder.name != current && !["Outbox", "Drafts"].contains(&folder.name.as_str())
            })
            .collect();
        if items.is_empty() {
            self.notice = Some("There is no other folder to move to.".into());
            return;
        }
        self.overlay = Some(Overlay::Folders {
            selected: 0,
            items,
            moving: true,
        });
    }

    pub(crate) fn move_active_to(&mut self, folder: &str) {
        let targets = self.action_targets(true);
        self.reader = None;
        let label = format!("Moved to {folder}");
        self.change_messages(&targets, &label, |source, mail| {
            let change = source.move_to_named(mail, folder)?;
            mail.mailbox = Some(folder.to_owned());
            Ok(change)
        });
    }

    /// Half-block pictures are ordinary cells that redraw cleanly; graphics protocols
    /// leave pixels behind until the screen is cleared.
    pub(crate) fn uses_halfblocks(&self) -> bool {
        self.picker.as_ref().is_none_or(|picker| {
            picker.protocol_type() == ratatui_image::picker::ProtocolType::Halfblocks
        })
    }

    pub(crate) fn graphics_shown(&self) -> bool {
        !self.uses_halfblocks() && self.reader.as_ref().is_some_and(Reader::shows_images)
    }

    pub(crate) fn open_in_browser(&mut self) {
        let Some(mail) = self
            .reader
            .as_ref()
            .and_then(|reader| self.mails.iter().find(|mail| mail.id == reader.mail_id()))
        else {
            return;
        };
        self.notice = Some(
            match widgets::rich::export_html(mail)
                .and_then(|path| opener::open(&path).map_err(std::io::Error::other))
            {
                Ok(()) => "Opened the message in your browser.".into(),
                Err(error) => format!("Unable to open in the browser: {error}"),
            },
        );
    }

    pub(crate) fn mark_folder_read(&mut self) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let indices: Vec<usize> = self
            .mails
            .iter()
            .enumerate()
            .filter(|(_, mail)| mail.unread && mail_folder(mail) == self.active_folder)
            .map(|(index, _)| index)
            .collect();
        if indices.is_empty() {
            self.notice = Some(format!(
                "Everything in {} is already read.",
                self.active_folder
            ));
            return;
        }
        let mut failed = 0;
        for index in &indices {
            match self.source.set_seen(&self.mails[*index], true) {
                Ok(change) => {
                    if let Some(change) = change {
                        self.mails[*index].source_path = Some(change.after_path().to_path_buf());
                    }
                    self.mails[*index].unread = false;
                }
                Err(_) => failed += 1,
            }
        }
        self.undo = None;
        self.notice = Some(if failed == 0 {
            format!("Marked {} messages read", indices.len())
        } else {
            format!(
                "Marked {} messages read · {failed} could not be changed",
                indices.len() - failed
            )
        });
        self.refresh_fingerprint();
        self.rebuild_mailbox(self.active_mail_id().as_deref());
    }

    /// Downloads the open message's remote pictures, which stay blocked until asked
    /// for because they can reveal that the message was read.
    pub(crate) fn load_remote_images(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        let Some(mail) = self.mails.iter().find(|mail| mail.id == reader.mail_id()) else {
            return;
        };
        let requests = reader.body(mail).start_remote();
        if requests.is_empty() {
            self.notice = Some("This message has no remote images.".into());
            return;
        }
        reader.invalidate();
        self.notice = Some(format!("Loading {} remote images…", requests.len()));
        for (slot, url) in requests {
            let sender = self.image_tx.clone();
            let mail_id = reader.mail_id().to_owned();
            tokio::task::spawn_blocking(move || {
                let _ = sender.send((mail_id, slot, widgets::rich::fetch(&url)));
            });
        }
    }

    pub(crate) fn open_links(&mut self) {
        let links = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
            .map(|mail| mail.links.clone())
            .unwrap_or_default();
        if links.is_empty() {
            self.notice = Some("This message has no web links.".into());
        } else {
            self.overlay = Some(Overlay::Links {
                selected: 0,
                items: links,
            });
        }
    }

    pub(crate) fn open_attachments(&mut self) {
        let items = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
            .map(|mail| mail.attachments.clone())
            .unwrap_or_default();
        if items.is_empty() {
            self.notice = Some("This message has no attachments.".into());
        } else {
            self.overlay = Some(Overlay::Attachments { selected: 0, items });
        }
    }

    pub(crate) fn save_active_attachment(&mut self, index: usize, open: bool) {
        let Some(mail_id) = self.active_mail_id() else {
            return;
        };
        let Some(mail) = self.mails.iter().find(|mail| mail.id == mail_id) else {
            return;
        };
        match self
            .source
            .save_attachment(mail, index, &self.config.download_dir())
        {
            Ok(path) if open => {
                self.notice = match opener::open(&path) {
                    Ok(()) => Some(format!("Saved and opened {}", path.display())),
                    Err(error) => Some(format!(
                        "Saved to {}, but unable to open it: {error}",
                        path.display()
                    )),
                };
            }
            Ok(path) => self.notice = Some(format!("Saved attachment to {}", path.display())),
            Err(error) => {
                self.notice = Some(format!(
                    "Unable to save attachment: {error}. Check the download directory."
                ));
            }
        }
    }
}
