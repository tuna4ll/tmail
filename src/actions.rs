//! Actions on messages and folders.

use super::*;

/// The address, subject and body of a `mailto:` link.
pub(crate) fn parse_mailto(link: &str) -> (String, String, String) {
    let rest = link.get(7..).unwrap_or_default();
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut subject = String::new();
    let mut body = String::new();
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match name.to_ascii_lowercase().as_str() {
            "subject" => subject = value,
            "body" => body = value,
            _ => {}
        }
    }
    (percent_decode(address), subject, body)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = value
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            output.push(byte);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

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
        if targets.iter().any(|index| {
            self.mails[*index]
                .source_path
                .as_ref()
                .is_some_and(|path| self.sending.contains(path))
        }) {
            self.notice = Some("Wait for this message to finish sending.".into());
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
        if !self.sending.is_empty() && self.active_folder == "Outbox" {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let Some(undo) = self.undo.take() else {
            return;
        };
        if let Some(mails) = undo.demo_mails {
            self.mails = mails;
            self.search_cache.get_mut().set_index(None, 0);
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
                [
                    "INBOX", "Drafts", "Sent", "Archive", "Trash", "Junk", "Outbox",
                ]
                .into_iter()
                .map(str::to_owned),
            );
        }
        names.sort();
        names.dedup();
        let hidden = settings::with(|ui| ui.hide_folders.clone());
        names.retain(|name| {
            *name == self.active_folder
                || !hidden
                    .iter()
                    .any(|folder| folder.eq_ignore_ascii_case(name))
        });
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
            deleting: false,
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
            deleting: false,
        });
    }

    /// Moves the conversation to Junk, or back to the Inbox when it is already there.
    pub(crate) fn toggle_spam(&mut self) {
        let in_junk = self.active_folder.eq_ignore_ascii_case("Junk");
        self.reader = None;
        let (folder, label) = if in_junk {
            ("INBOX", "Not spam · moved to the Inbox")
        } else {
            ("Junk", "Marked as spam")
        };
        let targets = self.action_targets(true);
        self.change_messages(&targets, label, |source, mail| {
            let change = source.move_to_named(mail, folder)?;
            mail.mailbox = Some(folder.to_owned());
            Ok(change)
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

    /// The Maildir of the account on screen and its server, for folder changes and
    /// server searches. The all-accounts view has no single account.
    pub(crate) fn current_account_store(
        &self,
    ) -> Result<(PathBuf, Option<remote::RemoteConfig>), String> {
        let MailboxSource::Maildir(root) = &self.source else {
            return Err("Switch to a single account first (press p).".into());
        };
        let remote = self
            .remote_accounts()
            .into_iter()
            .find(|(_, _, candidate)| candidate == root)
            .map(|(_, remote, _)| remote)
            .filter(remote::RemoteConfig::has_credentials);
        Ok((root.clone(), remote))
    }

    /// Creates, renames or deletes a folder, on the server too when the account syncs.
    pub(crate) fn change_folder(&mut self, change: imap_client::FolderChange) {
        let (root, remote) = match self.current_account_store() {
            Ok(store) => store,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        if let imap_client::FolderChange::Rename { local, .. }
        | imap_client::FolderChange::Delete(local) = &change
            && self.active_folder == local.trim_start_matches('.')
        {
            self.active_folder = "INBOX".into();
            self.reader = None;
        }
        self.notice = Some("Updating folders…".into());
        self.spawn_task(move || {
            let result = match &remote {
                Some(remote) => imap_client::change_folder(remote, &root, &change)
                    .map_err(|error| error.message),
                None => mailbox::change_local_folder(&root, &change),
            };
            Task::Done {
                message: result.unwrap_or_else(|error| format!("Folder unchanged · {error}")),
                reload: true,
            }
        });
    }

    /// Searches the server for older messages that were never downloaded.
    pub(crate) fn search_server(&mut self) {
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            self.notice = Some("Type a search first.".into());
            return;
        }
        let (root, remote) = match self.current_account_store() {
            Ok((root, Some(remote))) => (root, remote),
            Ok(_) => {
                self.notice = Some("This account has no server to search.".into());
                return;
            }
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        self.notice = Some("Searching the server…".into());
        self.spawn_task(move || {
            let message = match imap_client::search_server(&remote, &root, &query, 50) {
                Ok(0) => "No older matches on the server".to_owned(),
                Ok(count) => format!("Downloaded {count} older matches from the server"),
                Err(error) => format!("Server search failed · {error}"),
            };
            Task::Done {
                message,
                reload: true,
            }
        });
    }

    /// Downloads the body of a message that arrived as headers only.
    pub(crate) fn download_full_message(&mut self) {
        let Some(mail) = self
            .reader
            .as_ref()
            .and_then(|reader| self.mails.iter().find(|mail| mail.id == reader.mail_id()))
            .cloned()
        else {
            return;
        };
        if !mail.partial {
            self.notice = Some("This message is already complete.".into());
            return;
        }
        let Some(path) = mail.source_path.clone() else {
            return;
        };
        let account = self
            .remote_accounts()
            .into_iter()
            .find(|(_, _, root)| path.starts_with(root));
        let Some((_, remote, root)) = account else {
            self.notice = Some("This message has no server to download from.".into());
            return;
        };
        self.notice = Some("Downloading the full message…".into());
        self.spawn_task(move || Task::Downloaded {
            mail_id: mail.id,
            result: imap_client::download_full(&remote, &root, &path)
                .map(|_| ())
                .map_err(|error| format!("Download failed · {error}")),
        });
    }

    /// Uploads local messages that were held back at the account's first sync.
    pub(crate) fn upload_held(&mut self) {
        let accounts: Vec<_> = self
            .remote_accounts()
            .into_iter()
            .filter(|(_, remote, root)| self.shows_maildir(root) && remote.has_credentials())
            .collect();
        if accounts.is_empty() {
            self.notice = Some("This account has no server to upload to.".into());
            return;
        }
        self.notice = Some("Uploading local messages…".into());
        for (_, remote, root) in accounts {
            self.spawn_task(move || {
                let options = imap_client::SyncOptions { upload_held: true };
                let message = match imap_client::sync_with(&remote, &root, options) {
                    Ok(synced) => synced.summary,
                    Err(error) => format!("Upload failed · {error}"),
                };
                Task::Done {
                    message,
                    reload: true,
                }
            });
        }
    }

    /// Opens a link in the browser, asking first when it looks deceptive.
    pub(crate) fn open_link(&mut self, url: &str, shown: &str) {
        let assessment = crate::links::assess(url, shown, settings::reader().clean_links);
        if assessment.blocked {
            self.notice = Some(assessment.warnings.join(" "));
        } else if assessment.warnings.is_empty() {
            self.notice = Some(match opener::open(&assessment.url) {
                Ok(()) => "Link opened in the default browser.".into(),
                Err(error) => format!("Unable to open link: {error}. Copy it from the message."),
            });
        } else {
            self.overlay = Some(Overlay::LinkWarning {
                url: assessment.url,
                warnings: assessment.warnings,
            });
        }
    }

    fn invite_mail(&mut self) -> Option<Mail> {
        let mail = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
            .filter(|mail| mail.invite.is_some())
            .cloned();
        if mail.is_none() {
            self.notice = Some("Open a message with a meeting invitation first.".into());
        }
        mail
    }

    /// Prepares the answer to an invitation for the organizer, to review and send.
    pub(crate) fn answer_invite(&mut self, answer: crate::calendar::Answer) {
        let Some(mail) = self.invite_mail() else {
            return;
        };
        let invite = mail.invite.clone().unwrap_or_default();
        if invite.organizer.is_empty() {
            self.notice = Some("The invitation names no organizer to answer.".into());
            return;
        }
        let account = self.account_index(&mail);
        let (_, config) = self.compose_context(account);
        // Answer as the address the invitation was sent to.
        let identities = config.identities();
        let me = identities
            .iter()
            .find(|identity| {
                let address = mail::email_of(identity);
                mail.to
                    .iter()
                    .chain(&mail.cc)
                    .any(|to| mail::email_of(to) == address)
            })
            .cloned()
            .or_else(|| config.identity())
            .unwrap_or_default();
        let reply = crate::calendar::reply(
            &invite,
            &mail::email_of(&me),
            config.name.as_deref().unwrap_or_default(),
            answer,
        );
        let path = std::env::temp_dir().join(format!(
            "{}.ics",
            crate::storage::unique_name("termail-reply")
        ));
        if let Err(error) = crate::storage::write_new(&path, reply.as_bytes()) {
            self.notice = Some(format!("Unable to prepare the reply: {error}"));
            return;
        }
        self.start_draft(draft::Start::New);
        if let Some(Overlay::Compose(state)) = &mut self.overlay {
            state.composer.from = me;
            state.composer.to = invite.organizer.clone();
            state.composer.subject = format!("{}: {}", answer.verb(), invite.summary);
            state.composer.calendar = Some(path);
            let _ = fs::write(
                &state.composer.body_path,
                format!(
                    "{} the invitation to {} ({}).\n",
                    answer.verb(),
                    invite.summary,
                    invite.when
                ),
            );
            self.notice = Some("Review the answer, then press ctrl+s to send it.".into());
        }
    }

    /// Opens the invitation in the system's calendar app.
    pub(crate) fn add_invite_to_calendar(&mut self) {
        let Some(mail) = self.invite_mail() else {
            return;
        };
        let Some(path) = mail.source_path.as_ref() else {
            return;
        };
        let ics = fs::read(path).ok().and_then(|contents| {
            let message = mail_parser::MessageParser::default().parse(&contents)?;
            message.parts.iter().find_map(|part| {
                use mail_parser::MimeHeaders;
                let calendar = part.content_type().is_some_and(|kind| {
                    kind.c_subtype
                        .as_deref()
                        .is_some_and(|subtype| subtype.eq_ignore_ascii_case("calendar"))
                });
                let named = part
                    .attachment_name()
                    .is_some_and(|name| name.to_ascii_lowercase().ends_with(".ics"));
                (calendar || named).then(|| part.contents().to_vec())
            })
        });
        let Some(ics) = ics else {
            self.notice = Some("The invitation could not be read.".into());
            return;
        };
        let name = mail
            .invite
            .map(|invite| invite.summary)
            .unwrap_or_else(|| "invite".into());
        let file = self
            .config
            .download_dir()
            .join(format!("{}.ics", crate::mailbox::safe_file_name(&name)));
        let opened = fs::write(&file, ics)
            .map_err(|error| error.to_string())
            .and_then(|()| opener::open(&file).map_err(|error| error.to_string()));
        self.notice = Some(match opened {
            Ok(()) => format!("Opened {} in your calendar app.", file.display()),
            Err(error) => format!("Unable to open the invitation: {error}"),
        });
    }

    pub(crate) fn open_contacts(&mut self) {
        let mut items = self.address_book.clone();
        // Everyone else mail was exchanged with, after the saved contacts.
        for value in &self.contacts {
            let email = mail::email_of(value);
            if !items
                .iter()
                .any(|contact| contact.email.eq_ignore_ascii_case(&email))
            {
                let name = value
                    .split('<')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('"')
                    .to_owned();
                items.push(contacts::Contact {
                    name: if name.contains('@') {
                        String::new()
                    } else {
                        name
                    },
                    email,
                });
            }
        }
        if items.is_empty() {
            self.notice = Some("No contacts yet. Press K on a message to add its sender.".into());
            return;
        }
        self.overlay = Some(Overlay::Contacts {
            selected: 0,
            filter: String::new(),
            items,
        });
    }

    /// Saves the open or selected message's sender in the address book.
    pub(crate) fn add_sender_to_contacts(&mut self) {
        let Some(mail) = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
        else {
            return;
        };
        let email = mail::email_of(&mail.from);
        let name = mail
            .from
            .split('<')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_owned();
        let Some(path) = self.config.contacts_path() else {
            self.notice = Some("Start with --config PATH to keep an address book.".into());
            return;
        };
        let contact = contacts::Contact {
            name: if name.contains('@') {
                String::new()
            } else {
                name
            },
            email: email.clone(),
        };
        self.notice = Some(match contacts::add(&path, &contact) {
            Ok(true) => format!("Added {email} to contacts"),
            Ok(false) => format!("{email} is already in contacts"),
            Err(error) => format!("Unable to save the contact: {error}"),
        });
        self.address_book = contacts::load(&path);
    }

    /// Asks before leaving the mailing list of the open message.
    pub(crate) fn ask_unsubscribe(&mut self) {
        let Some(mail) = self
            .reader
            .as_ref()
            .and_then(|reader| self.mails.iter().find(|mail| mail.id == reader.mail_id()))
        else {
            return;
        };
        match &mail.unsubscribe {
            Some(unsubscribe) => {
                self.overlay = Some(Overlay::Unsubscribe {
                    unsubscribe: unsubscribe.clone(),
                });
            }
            None => self.notice = Some("This message is not from a mailing list.".into()),
        }
    }

    /// Leaves the list: one-click HTTPS when offered, else an email, else the web page.
    pub(crate) fn unsubscribe(&mut self, unsubscribe: mail::Unsubscribe) {
        if unsubscribe.one_click
            && let Some(url) = unsubscribe.https().map(str::to_owned)
        {
            self.notice = Some("Unsubscribing…".into());
            self.spawn_task(move || Task::Done {
                message: match widgets::rich::post_one_click(&url) {
                    Ok(()) => "Unsubscribed · the sender confirmed the request".into(),
                    Err(error) => format!("Unsubscribe failed · {error}"),
                },
                reload: false,
            });
            return;
        }
        if let Some(mailto) = unsubscribe.mailto().map(str::to_owned) {
            self.start_draft(draft::Start::New);
            if let Some(Overlay::Compose(state)) = &mut self.overlay {
                let (to, subject, body) = parse_mailto(&mailto);
                state.composer.to = to;
                state.composer.subject = if subject.is_empty() {
                    "unsubscribe".into()
                } else {
                    subject
                };
                if !body.is_empty() {
                    let _ = fs::write(&state.composer.body_path, body);
                }
                self.notice = Some("Review the unsubscribe email, then press ctrl+s.".into());
            }
            return;
        }
        if let Some(link) = unsubscribe.links.first() {
            self.notice = Some(match opener::open(link) {
                Ok(()) => "Opened the unsubscribe page in the browser.".into(),
                Err(error) => format!("Unable to open the unsubscribe page: {error}"),
            });
        }
    }

    /// Grows or shrinks the reader window around its centre.
    pub(crate) fn resize_popup(&mut self, columns: i32, rows: i32) {
        if self.reader_docked {
            return;
        }
        let Some(current) = self
            .reader
            .as_ref()
            .map(widgets::reader::Reader::frame_area)
        else {
            return;
        };
        let width = (i32::from(current.width) + columns).max(0) as u16;
        let height = (i32::from(current.height) + rows).max(0) as u16;
        let x = (i32::from(current.x) - columns / 2).max(0) as u16;
        let y = (i32::from(current.y) - rows / 2).max(0) as u16;
        self.place_popup(Rect::new(x, y, width, height));
    }

    pub(crate) fn move_popup(&mut self, columns: i32, rows: i32) {
        if self.reader_docked {
            return;
        }
        let Some(current) = self
            .reader
            .as_ref()
            .map(widgets::reader::Reader::frame_area)
        else {
            return;
        };
        let x = (i32::from(current.x) + columns).max(0) as u16;
        let y = (i32::from(current.y) + rows).max(0) as u16;
        self.place_popup(Rect::new(x, y, current.width, current.height));
    }

    pub(crate) fn place_popup(&mut self, rect: Rect) {
        let screen = Rect::new(
            0,
            0,
            self.screen_width,
            self.screen_height.saturating_sub(1),
        );
        let rect = ui::clamp_rect(rect, screen);
        let resized = self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.frame_area().width != rect.width);
        if resized && let Some(reader) = &mut self.reader {
            reader.defer_layout();
        }
        self.popup = Some(rect);
        self.popup_maximized = false;
        self.clear_screen = self.graphics_shown() || self.clear_screen;
    }

    pub(crate) fn toggle_popup_maximized(&mut self) {
        self.popup_maximized = !self.popup_maximized;
        self.clear_screen = true;
    }

    /// Switches the open HTML message between its own colours and the reader's.
    pub(crate) fn toggle_reader_colors(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        let Some(mail) = self.mails.iter().find(|mail| mail.id == reader.mail_id()) else {
            return;
        };
        if !reader.is_html(mail) {
            self.notice = Some("Plain-text message: there are no colours to switch.".into());
            return;
        }
        let original = reader.toggle_colors(mail);
        self.notice = Some(if original {
            "Showing the message's own colours · t for reader colours".into()
        } else {
            "Showing reader colours · t for the message's own".into()
        });
        self.clear_screen = true;
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
