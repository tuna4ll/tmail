//! Keyboard, mouse and command handling.

use super::*;

impl App {
    pub(crate) fn handle_event(&mut self, event: Event) {
        if matches!(event, Event::Key(_)) && self.graphics_shown() {
            self.clear_screen = true;
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let editing = self.editing_kind();
                self.on_key_event(key);
                // A cursor position belongs to one field.
                if self.editing_kind() != editing
                    || matches!(
                        key.code,
                        KeyCode::Tab
                            | KeyCode::BackTab
                            | KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Enter
                    )
                {
                    self.cursor_back = 0;
                }
            }
            Event::Paste(text) => match self.overlay.take() {
                Some(Overlay::AccountSetup(mut form)) => {
                    if !form.busy {
                        form.input(&text);
                    }
                    self.overlay = Some(Overlay::AccountSetup(form));
                }
                Some(Overlay::Compose(mut state)) => {
                    if let Some(value) = state.value_mut() {
                        insert_text(value, self.cursor_back, &text);
                    }
                    self.suggest(&mut state);
                    self.overlay = Some(Overlay::Compose(state));
                }
                Some(Overlay::BodyEditor { state, mut editor }) => {
                    editor.insert(&text);
                    self.overlay = Some(Overlay::BodyEditor { state, editor });
                }
                overlay => self.overlay = overlay,
            },
            Event::Resize(..) => {
                if let Some(reader) = &mut self.reader {
                    reader.defer_layout();
                }
            }
            Event::Mouse(mouse) => {
                if let Some(Overlay::BodyEditor { editor, .. }) = &mut self.overlay {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => editor.scroll(true, 3),
                        MouseEventKind::ScrollUp => editor.scroll(false, 3),
                        MouseEventKind::Down(MouseButton::Left) => {
                            editor.click(mouse.column, mouse.row);
                        }
                        _ => {}
                    }
                    return;
                }
                self.handle_mouse(mouse);
            }
            _ => {}
        }
    }

    /// Which text field has the keyboard, so its cursor resets when another one does.
    pub(crate) fn editing_kind(&self) -> u8 {
        match &self.overlay {
            Some(Overlay::Command(_)) => 1,
            Some(Overlay::Prompt { .. }) => 2,
            Some(Overlay::Compose(state)) => 10 + state.field as u8,
            _ if self.searching => 3,
            _ => 0,
        }
    }

    pub(crate) fn on_key_event(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Char('d') {
            self.confirm_delete = None;
        }
        if key.code != KeyCode::Char('q') {
            self.confirm_quit = false;
        }
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            match self.overlay.take() {
                Some(Overlay::Compose(state) | Overlay::ComposeClose(state))
                | Some(Overlay::DraftReady { state, .. }) => {
                    self.overlay = Some(Overlay::ComposeClose(state));
                    return;
                }
                overlay => self.overlay = overlay,
            }
            self.quit();
            return;
        }
        if self.handle_overlay_key(key) {
            return;
        }
        if self.onboarding {
            match key.code {
                KeyCode::Enter => self.open_account_setup(account_setup::Mode::Add),
                KeyCode::Char('q') => self.quit(),
                _ => {}
            }
            return;
        }
        if !matches!(self.mailbox_state, MailboxState::Ready) {
            match (key.modifiers, key.code) {
                (_, KeyCode::Char('r')) => self.reload(),
                (_, KeyCode::Char('c')) => self.start_draft(draft::Start::New),
                (_, KeyCode::Char('p')) => self.open_accounts(),
                (_, KeyCode::Char('g')) => self.open_folders(),
                (_, KeyCode::Char('?')) => self.overlay = Some(Overlay::Help { scroll: 0 }),
                (_, KeyCode::Char(':')) => {
                    self.overlay = Some(Overlay::Command(String::new()));
                }
                (_, KeyCode::Char('q')) => {
                    self.quit();
                }
                _ => {}
            }
            return;
        }

        if self.searching {
            match key.code {
                KeyCode::Enter => self.searching = false,
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.searching = false;
                    self.search_server();
                }
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.refresh_inbox();
                }
                _ => {
                    if edit_text(&mut self.query, &mut self.cursor_back, key) {
                        self.refresh_inbox();
                    }
                }
            }
            return;
        }

        if key.code == KeyCode::Esc && self.notice.take().is_some() {
            return;
        }
        if self.run_script_key(key) {
            return;
        }
        // Vim-style panning for the workspace map. Taking alt+hjkl here, before
        // the keymap rewrites custom bindings, leaves shift+J and shift+K for
        // spam and contacts.
        let vim_pan = key.modifiers.contains(KeyModifiers::ALT)
            && matches!(
                key.code,
                KeyCode::Char('h' | 'H' | 'j' | 'J' | 'k' | 'K' | 'l' | 'L')
            );
        if self.focus == Pane::Workspace && self.reader.is_none() && vim_pan {
            if let Some(workspace) = &mut self.workspace {
                workspace.handle_key(key);
            }
            return;
        }
        let key = self.keymap.translate(key);

        match key.code {
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help { scroll: 0 });
                return;
            }
            KeyCode::Char(':') => {
                self.overlay = Some(Overlay::Command(String::new()));
                return;
            }
            KeyCode::Char('g') => {
                self.open_folders();
                return;
            }
            KeyCode::Char('p') => {
                self.open_accounts();
                return;
            }
            KeyCode::Char('x') if self.active_folder == "Outbox" => {
                self.retry_outbox();
                return;
            }
            KeyCode::Char('z') if self.active_folder == "Outbox" => {
                self.review_delivery();
                return;
            }
            KeyCode::Char('m') => {
                self.toggle_read();
                return;
            }
            KeyCode::Char('s') => {
                self.toggle_starred();
                return;
            }
            KeyCode::Char('u') if self.undo.is_some() => {
                self.undo_last();
                return;
            }
            KeyCode::Char('a') => {
                self.move_active_message(true);
                return;
            }
            KeyCode::Char('d') => {
                self.move_active_message(false);
                return;
            }
            KeyCode::Char('r') => {
                self.start_draft(draft::Start::Reply);
                return;
            }
            KeyCode::Char('A') => {
                self.start_draft(draft::Start::ReplyAll);
                return;
            }
            KeyCode::Char('M') => {
                self.open_move_picker();
                return;
            }
            KeyCode::Char('J') => {
                self.toggle_spam();
                return;
            }
            KeyCode::Char('K') => {
                self.add_sender_to_contacts();
                return;
            }
            KeyCode::Char('F') => {
                self.start_draft(draft::Start::Forward);
                return;
            }
            KeyCode::Char('c') if self.reader.is_some() || self.focus == Pane::Inbox => {
                self.start_draft(draft::Start::New);
                return;
            }
            KeyCode::Char('l') if self.reader.is_some() => {
                self.open_links();
                return;
            }
            KeyCode::Char('v') if self.reader.is_some() => {
                self.open_attachments();
                return;
            }
            KeyCode::Char('I') if self.reader.is_some() => {
                self.load_remote_images();
                return;
            }
            KeyCode::Char('O') if self.reader.is_some() => {
                self.open_in_browser();
                return;
            }
            KeyCode::Char('D') if self.reader.is_some() => {
                self.download_full_message();
                return;
            }
            KeyCode::Char('U') if self.reader.is_some() => {
                self.ask_unsubscribe();
                return;
            }
            _ => {}
        }

        if self.reader.is_some() {
            match key.code {
                KeyCode::Esc => self.reader = None,
                KeyCode::Down | KeyCode::Char('j')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.reader.as_mut().unwrap().scroll_down(1);
                }
                KeyCode::Up | KeyCode::Char('k')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.reader.as_mut().unwrap().scroll_up(1);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    let page = settings::with(|ui| ui.page_lines);
                    self.reader.as_mut().unwrap().scroll_down(page);
                }
                KeyCode::PageUp => {
                    let page = settings::with(|ui| ui.page_lines);
                    self.reader.as_mut().unwrap().scroll_up(page);
                }
                KeyCode::Home => self.reader.as_mut().unwrap().scroll_up(u16::MAX),
                KeyCode::End => self.reader.as_mut().unwrap().scroll_down(u16::MAX),
                KeyCode::Char('n') => self.open_adjacent(true),
                KeyCode::Char('N') => self.open_adjacent(false),
                KeyCode::Char('t') => self.toggle_reader_colors(),
                KeyCode::Char('f') => self.toggle_popup_maximized(),
                KeyCode::Char('0') => {
                    self.popup = None;
                    self.popup_maximized = false;
                    self.clear_screen = true;
                }
                KeyCode::Left | KeyCode::Char('h')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.reader.as_mut().unwrap().scroll_left(8);
                }
                KeyCode::Right
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.reader.as_mut().unwrap().scroll_right(8);
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let half = (self.reader.as_ref().unwrap().frame_area().height / 2).max(1);
                    self.reader.as_mut().unwrap().scroll_down(half);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let half = (self.reader.as_ref().unwrap().frame_area().height / 2).max(1);
                    self.reader.as_mut().unwrap().scroll_up(half);
                }
                KeyCode::Char('+' | '=') => self.resize_popup(4, 2),
                KeyCode::Char('-' | '_') => self.resize_popup(-4, -2),
                KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.resize_popup(-4, 0)
                }
                KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.resize_popup(4, 0)
                }
                KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.resize_popup(0, -2)
                }
                KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.resize_popup(0, 2)
                }
                KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.move_popup(-4, 0)
                }
                KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.move_popup(4, 0)
                }
                KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => self.move_popup(0, -2),
                KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => self.move_popup(0, 2),
                _ => {}
            }
            return;
        }

        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) if !self.query.is_empty() => {
                self.query.clear();
                self.refresh_inbox();
            }

            (_, KeyCode::Esc | KeyCode::Char('q'))
            | (KeyModifiers::CONTROL, KeyCode::Char('c') | KeyCode::Char('C')) => {
                self.quit();
            }

            (_, KeyCode::Tab) => {
                self.focus = match self.focus {
                    Pane::Inbox => Pane::Workspace,
                    Pane::Workspace => Pane::Inbox,
                };
            }

            (_, KeyCode::Char('R')) => self.reload(),

            (_, KeyCode::Char('/')) if self.focus == Pane::Inbox => self.searching = true,

            (_, KeyCode::Char('1')) if self.focus == Pane::Inbox => {
                self.filter = InboxFilter::All;
                self.refresh_inbox();
            }

            (_, KeyCode::Char('2')) if self.focus == Pane::Inbox => {
                self.filter = InboxFilter::Unread;
                self.refresh_inbox();
            }

            (_, KeyCode::Char('3')) if self.focus == Pane::Inbox => {
                self.filter = InboxFilter::Starred;
                self.refresh_inbox();
            }

            (_, KeyCode::Down | KeyCode::Char('j'))
                if self.focus == Pane::Inbox
                    && self.selected + 1 < self.visible_conversations().len() =>
            {
                self.selected += 1;
                self.sync_workspace();
            }

            (_, KeyCode::Up | KeyCode::Char('k'))
                if self.focus == Pane::Inbox && self.selected > 0 =>
            {
                self.selected -= 1;
                self.sync_workspace();
            }

            (_, KeyCode::PageDown) if self.focus == Pane::Inbox => {
                self.select_conversation(self.selected + 10);
            }
            (_, KeyCode::PageUp) if self.focus == Pane::Inbox => {
                self.select_conversation(self.selected.saturating_sub(10));
            }
            (_, KeyCode::Home) if self.focus == Pane::Inbox => self.select_conversation(0),
            (_, KeyCode::End) if self.focus == Pane::Inbox => {
                self.select_conversation(usize::MAX);
            }

            (
                _,
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char('h' | 'j' | 'k' | 'l' | '+' | '=' | '-' | '_' | '0' | 'f' | 'c'),
            ) if self.focus == Pane::Workspace => {
                if let Some(workspace) = &mut self.workspace {
                    workspace.handle_key(key);
                }
                self.sync_inbox_selection();
            }

            (_, KeyCode::Enter | KeyCode::Char(' ')) => {
                let mail_id = match self.focus {
                    Pane::Inbox => self.entry_mail_id(),
                    Pane::Workspace => self
                        .workspace
                        .as_ref()
                        .and_then(Workspace::selected_mail_id),
                };
                if let Some(mail_id) = mail_id {
                    self.open_mail(&mail_id);
                }
            }

            _ => {}
        }
    }

    pub(crate) fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        let Some(overlay) = self.overlay.take() else {
            return false;
        };
        let mut overlay = match overlay {
            Overlay::AccountSetup(form) => {
                self.handle_account_key(form, key);
                return true;
            }
            Overlay::DeliveryReview { delivery } => {
                if action_key(key, 's') || action_key(key, 'r') {
                    match delivery.resolve(action_key(key, 's')) {
                        Ok(()) => self.start_delivery(*delivery),
                        Err(error) => self.notice = Some(error),
                    }
                } else if key.code != KeyCode::Esc {
                    self.overlay = Some(Overlay::DeliveryReview { delivery });
                }
                return true;
            }
            Overlay::Compose(state) => {
                self.handle_compose_key(state, key);
                return true;
            }
            Overlay::BodyEditor { state, editor } => {
                self.handle_body_editor_key(state, editor, key);
                return true;
            }
            Overlay::DraftReady { state, summary } => {
                if action_key(key, 's') {
                    self.finish_composer(state, true);
                } else if action_key(key, 'd') {
                    self.finish_composer(state, false);
                } else if action_key(key, 'e') || key.code == KeyCode::Esc {
                    self.overlay = Some(Overlay::Compose(state));
                } else {
                    self.overlay = Some(Overlay::DraftReady { state, summary });
                }
                return true;
            }
            Overlay::ComposeClose(state) => {
                if action_key(key, 's') {
                    self.finish_composer(state, false);
                } else if action_key(key, 'd') {
                    let _ = state.composer.remove_body();
                    // A draft autosave made for this message goes too; a draft that
                    // existed before keeps its last saved version.
                    if state.autosave.created {
                        if let Some(path) = &state.composer.original_draft {
                            let _ = fs::remove_file(path);
                        }
                        let (source, _) = self.compose_context(state.account);
                        let _ = source.delete_drafts(state.composer.message_id());
                    }
                    self.notice = Some("Draft discarded".into());
                    self.reload();
                } else if key.code == KeyCode::Esc {
                    self.overlay = Some(Overlay::Compose(state));
                } else {
                    self.overlay = Some(Overlay::ComposeClose(state));
                }
                return true;
            }
            overlay => overlay,
        };
        if overlay.step_selection(key.code) {
            self.overlay = Some(overlay);
            return true;
        }
        match &mut overlay {
            Overlay::Help { scroll } => match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {}
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(1).min(HELP.len() as u16 - 1);
                    self.overlay = Some(overlay);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Command(input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let command = input.trim().to_owned();
                    self.run_command(&command);
                }
                _ => {
                    edit_text(input, &mut self.cursor_back, key);
                    self.overlay = Some(overlay);
                }
            },
            Overlay::Folders {
                selected,
                items,
                moving,
                deleting,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('n') if !*moving => {
                    self.overlay = Some(Overlay::Prompt {
                        kind: PromptKind::NewFolder,
                        input: String::new(),
                    });
                }
                KeyCode::Char('e') if !*moving => match items.get(*selected) {
                    Some(folder) if !is_standard_folder(&folder.name) => {
                        self.overlay = Some(Overlay::Prompt {
                            input: folder.name.replace('.', "/"),
                            kind: PromptKind::RenameFolder(folder.name.clone()),
                        });
                    }
                    _ => {
                        self.notice = Some("Standard folders cannot be renamed.".into());
                        self.overlay = Some(overlay);
                    }
                },
                KeyCode::Char('x') if !*moving => match items.get(*selected) {
                    Some(folder) if is_standard_folder(&folder.name) => {
                        self.notice = Some("Standard folders cannot be deleted.".into());
                        self.overlay = Some(overlay);
                    }
                    Some(folder) if *deleting => {
                        let local = format!(".{}", folder.name);
                        self.change_folder(imap_client::FolderChange::Delete(local));
                    }
                    Some(_) => {
                        *deleting = true;
                        self.overlay = Some(overlay);
                    }
                    None => self.overlay = Some(overlay),
                },
                KeyCode::Enter => {
                    if let Some(folder) = items.get(*selected) {
                        if *moving {
                            let name = folder.name.clone();
                            self.move_active_to(&name);
                        } else {
                            self.active_folder = folder.name.clone();
                            self.reader = None;
                            self.rebuild_mailbox(None);
                        }
                    }
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Menu {
                items, selected, ..
            } => match key.code {
                KeyCode::Enter => {
                    if let Some(item) = items.get(*selected) {
                        let key = item.key;
                        self.handle_event(Event::Key(key));
                    }
                }
                KeyCode::Esc => {}
                _ => {
                    // A menu key works as a shortcut while the menu is open.
                    if let Some(item) = items.iter().find(|item| item.key == key) {
                        let key = item.key;
                        self.handle_event(Event::Key(key));
                    } else {
                        self.overlay = Some(overlay);
                    }
                }
            },
            Overlay::LinkWarning { url, .. } => {
                if action_key(key, 'o') {
                    let url = url.clone();
                    self.notice = Some(match opener::open(&url) {
                        Ok(()) => "Link opened in the default browser.".into(),
                        Err(error) => format!("Unable to open link: {error}"),
                    });
                } else if action_key(key, 'y') {
                    copy_to_clipboard(url);
                    self.notice = Some("Link copied to the clipboard.".into());
                } else if key.code != KeyCode::Esc && !action_key(key, 'n') {
                    self.overlay = Some(overlay);
                }
            }
            Overlay::Unsubscribe { unsubscribe } => {
                if action_key(key, 'y') || key.code == KeyCode::Enter {
                    let unsubscribe = unsubscribe.clone();
                    self.unsubscribe(unsubscribe);
                } else if key.code != KeyCode::Esc && !action_key(key, 'n') {
                    self.overlay = Some(overlay);
                }
            }
            Overlay::Contacts {
                selected,
                filter,
                items,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let chosen = contacts_matching(items, filter)
                        .get(*selected)
                        .map(|contact| contact.address());
                    if let Some(address) = chosen {
                        self.start_draft(draft::Start::New);
                        if let Some(Overlay::Compose(state)) = &mut self.overlay {
                            state.composer.to = address;
                            state.field = 3;
                        }
                    }
                }
                KeyCode::Char('x' | 'X') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let email = contacts_matching(items, filter)
                        .get(*selected)
                        .map(|contact| contact.email.clone());
                    if let (Some(email), Some(path)) = (email, self.config.contacts_path()) {
                        self.notice = Some(match contacts::remove(&path, &email) {
                            Ok(()) => format!("Removed {email} from contacts"),
                            Err(error) => format!("Unable to change contacts: {error}"),
                        });
                        self.address_book = contacts::load(&path);
                        items.retain(|contact| contact.email != email);
                        *selected = selected.saturating_sub(1);
                    }
                    self.overlay = Some(overlay);
                }
                _ => {
                    if edit_text(filter, &mut self.cursor_back, key) {
                        *selected = 0;
                    }
                    self.overlay = Some(overlay);
                }
            },
            Overlay::Prompt { kind, input } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let name = input.trim().trim_matches('/').to_owned();
                    if name.is_empty() || name.split('/').any(|part| part.trim().is_empty()) {
                        self.notice = Some("Enter a folder name.".into());
                        self.overlay = Some(overlay);
                    } else if is_standard_folder(&name) {
                        self.notice = Some("That name is taken by a standard folder.".into());
                        self.overlay = Some(overlay);
                    } else {
                        let change = match kind {
                            PromptKind::NewFolder => imap_client::FolderChange::Create(name),
                            PromptKind::RenameFolder(folder) => imap_client::FolderChange::Rename {
                                local: format!(".{folder}"),
                                name,
                            },
                        };
                        self.change_folder(change);
                    }
                }
                _ => {
                    edit_text(input, &mut self.cursor_back, key);
                    self.overlay = Some(overlay);
                }
            },
            Overlay::Links { selected, items } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('y') => {
                    if let Some(link) = items.get(*selected) {
                        copy_to_clipboard(link);
                        self.notice = Some("Link copied to the clipboard.".into());
                    }
                }
                KeyCode::Enter => {
                    if let Some(link) = items.get(*selected).cloned() {
                        self.open_link(&link, "");
                    }
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Attachments { selected, .. } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('s') => self.save_active_attachment(*selected, false),
                KeyCode::Char('o') | KeyCode::Enter => self.save_active_attachment(*selected, true),
                _ => self.overlay = Some(overlay),
            },
            Overlay::Accounts {
                selected,
                items,
                removing,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('x') => {
                    if let Some(index) = items.get(*selected).and_then(|item| item.index) {
                        if *removing {
                            self.remove_account(index);
                        } else {
                            *removing = true;
                            self.overlay = Some(overlay);
                        }
                    } else {
                        self.overlay = Some(overlay);
                    }
                }
                KeyCode::Char('a') => self.open_account_setup(account_setup::Mode::Add),
                KeyCode::Char('e') => {
                    if let Some(index) = items.get(*selected).and_then(|item| item.index) {
                        self.select_account(Some(index));
                        self.open_account_setup(account_setup::Mode::Edit);
                    }
                }
                KeyCode::Char('l') => {
                    if let Some(index) = items.get(*selected).and_then(|item| item.index) {
                        self.select_account(Some(index));
                        self.open_account_setup(account_setup::Mode::Login);
                    }
                }
                KeyCode::Enter => {
                    if let Some(item) = items.get(*selected) {
                        self.select_account(item.index);
                    }
                }
                _ => {
                    *removing = false;
                    self.overlay = Some(overlay);
                }
            },
            Overlay::DraftReady { .. }
            | Overlay::ComposeClose(_)
            | Overlay::Compose(_)
            | Overlay::BodyEditor { .. }
            | Overlay::DeliveryReview { .. } => {
                unreachable!()
            }
            Overlay::AccountSetup(_) => unreachable!(),
        }
        true
    }

    pub(crate) fn run_command(&mut self, command: &str) {
        match command {
            "accounts" => self.open_accounts(),
            "account add" => self.open_account_setup(account_setup::Mode::Add),
            "account edit" => self.open_account_setup(account_setup::Mode::Edit),
            "login" => self.open_account_setup(account_setup::Mode::Login),
            "reload" => self.reload(),
            "reload config" => self.reload_config(),
            "sync" => self.start_sync(),
            "upload" => self.upload_held(),
            "contacts" => self.open_contacts(),
            "accept" => self.answer_invite(crate::calendar::Answer::Accepted),
            "tentative" => self.answer_invite(crate::calendar::Answer::Tentative),
            "decline" => self.answer_invite(crate::calendar::Answer::Declined),
            "calendar" => self.add_invite_to_calendar(),
            "search server" => self.search_server(),
            "folder new" => {
                self.overlay = Some(Overlay::Prompt {
                    kind: PromptKind::NewFolder,
                    input: String::new(),
                });
            }
            "mark read" => self.mark_folder_read(),
            "filter all" => {
                self.filter = InboxFilter::All;
                self.refresh_inbox();
            }
            "filter unread" => {
                self.filter = InboxFilter::Unread;
                self.refresh_inbox();
            }
            "filter starred" => {
                self.filter = InboxFilter::Starred;
                self.refresh_inbox();
            }
            "compose" => self.start_draft(draft::Start::New),
            "reply" => self.start_draft(draft::Start::Reply),
            "reply all" => self.start_draft(draft::Start::ReplyAll),
            "forward" => self.start_draft(draft::Start::Forward),
            "help" => self.overlay = Some(Overlay::Help { scroll: 0 }),
            "quit" => self.quit(),
            "patch apply" | "patch" => self.apply_patches(""),
            "patch save" => self.save_patches(),
            "patch b4" => self.b4_patches(""),
            _ if command.starts_with("patch apply ") => {
                self.apply_patches(command.trim_start_matches("patch apply "))
            }
            _ if command.starts_with("patch b4 ") => {
                self.b4_patches(command.trim_start_matches("patch b4 "))
            }
            "lore" => self.open_lore(""),
            _ if command.starts_with("lore ") => {
                self.open_lore(command.trim_start_matches("lore "))
            }
            _ if command.starts_with("open maildir ") => {
                let path = command.trim_start_matches("open maildir ").trim();
                if path.is_empty() {
                    self.notice = Some("Enter a Maildir path.".into());
                } else {
                    self.source = MailboxSource::Maildir(expand_path(path));
                    self.filter = InboxFilter::All;
                    self.query.clear();
                    self.undo = None;
                    self.mailbox_fingerprint = 0;
                    self.reload();
                }
            }
            "script reload" => {
                self.load_script();
                if let Some(engine) = self.script.as_ref().filter(|_| self.notice.is_none()) {
                    self.notice = Some(format!("Script reloaded · {}", engine.path().display()));
                } else if self.script.is_none() && self.notice.is_none() {
                    self.notice = Some("No init.lua found next to the config.".into());
                }
            }
            "plugins" => self.describe_plugins(),
            "" => {}
            _ if self.run_script_command(command) => {}
            _ => {
                let names = self.script_command_names();
                let scripted = if names.is_empty() {
                    String::new()
                } else {
                    format!(" Script commands: {}.", names.join(", "))
                };
                self.notice = Some(format!(
                    "Unknown command: {command}. Press ? to view commands.{scripted}"
                ));
            }
        }
    }
}

/// Line editing shared by every text field. `back` is how many characters the cursor
/// sits before the end, so appending stays the default. Supports arrows, home/end,
/// delete, ctrl+a/e/k/u/w and alt or ctrl with arrows to move by word. Returns whether
/// the key belonged to the field.
pub(crate) fn edit_text(value: &mut String, back: &mut usize, key: KeyEvent) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let count = value.chars().count();
    *back = (*back).min(count);
    let position = count - *back;
    let byte = |index: usize| {
        value
            .char_indices()
            .nth(index)
            .map_or(value.len(), |(byte, _)| byte)
    };
    match key.code {
        KeyCode::Left if control || alt => *back = count - word_start(value, position),
        KeyCode::Right if control || alt => *back = count - word_end(value, position),
        KeyCode::Left => *back = (*back + 1).min(count),
        KeyCode::Right => *back = back.saturating_sub(1),
        KeyCode::Home => *back = count,
        KeyCode::End => *back = 0,
        KeyCode::Char('a' | 'A') if control => *back = count,
        KeyCode::Char('e' | 'E') if control => *back = 0,
        KeyCode::Backspace if alt => {
            let start = word_start(value, position);
            value.replace_range(byte(start)..byte(position), "");
        }
        KeyCode::Char('w' | 'W') if control => {
            let start = word_start(value, position);
            value.replace_range(byte(start)..byte(position), "");
        }
        KeyCode::Backspace if position > 0 => {
            value.replace_range(byte(position - 1)..byte(position), "");
        }
        KeyCode::Backspace => {}
        KeyCode::Delete if *back > 0 => {
            value.replace_range(byte(position)..byte(position + 1), "");
            *back -= 1;
        }
        KeyCode::Delete => {}
        KeyCode::Char('u' | 'U') if control => {
            if *back == 0 {
                value.clear();
            } else {
                value.replace_range(..byte(position), "");
            }
        }
        KeyCode::Char('k' | 'K') if control => {
            value.truncate(byte(position));
            *back = 0;
        }
        KeyCode::Char(character) if !control && !alt => {
            value.insert(byte(position), character);
        }
        _ => return false,
    }
    true
}

/// Inserts pasted text at the cursor, dropping control characters.
pub(crate) fn insert_text(value: &mut String, back: usize, text: &str) {
    let count = value.chars().count();
    let position = count - back.min(count);
    let byte = value
        .char_indices()
        .nth(position)
        .map_or(value.len(), |(byte, _)| byte);
    let clean: String = text
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    value.insert_str(byte, &clean);
}

fn is_separator(character: char) -> bool {
    character.is_whitespace() || ",;/".contains(character)
}

/// The character index where the word before `position` starts.
fn word_start(value: &str, position: usize) -> usize {
    let characters: Vec<char> = value.chars().collect();
    let mut index = position.min(characters.len());
    while index > 0 && is_separator(characters[index - 1]) {
        index -= 1;
    }
    while index > 0 && !is_separator(characters[index - 1]) {
        index -= 1;
    }
    index
}

/// The character index just after the word at or after `position`.
fn word_end(value: &str, position: usize) -> usize {
    let characters: Vec<char> = value.chars().collect();
    let mut index = position.min(characters.len());
    while index < characters.len() && is_separator(characters[index]) {
        index += 1;
    }
    while index < characters.len() && !is_separator(characters[index]) {
        index += 1;
    }
    index
}

/// Copies through the terminal (OSC 52), which also works over SSH.
pub(crate) fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let mut output = stdout();
    let _ = write!(
        output,
        "\x1b]52;c;{}\x07",
        widgets::rich::base64(text.as_bytes())
    );
    let _ = output.flush();
}

/// Folders every account has; they map to server roles and cannot be changed.
pub(crate) fn is_standard_folder(name: &str) -> bool {
    [
        "INBOX", "Drafts", "Sent", "Archive", "Trash", "Outbox", "Junk",
    ]
    .iter()
    .any(|standard| standard.eq_ignore_ascii_case(name))
}

pub(crate) fn action_key(key: KeyEvent, expected: char) -> bool {
    matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&expected))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(value: &mut String, back: &mut usize, code: KeyCode, modifiers: KeyModifiers) {
        edit_text(value, back, KeyEvent::new(code, modifiers));
    }

    #[test]
    fn edits_at_the_cursor() {
        let mut value = String::from("hello wörld");
        let mut back = 0;
        press(&mut value, &mut back, KeyCode::Left, KeyModifiers::NONE);
        press(&mut value, &mut back, KeyCode::Left, KeyModifiers::NONE);
        press(
            &mut value,
            &mut back,
            KeyCode::Char('X'),
            KeyModifiers::NONE,
        );
        assert_eq!(value, "hello wörXld");
        press(&mut value, &mut back, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(value, "hello wörXd");
        press(
            &mut value,
            &mut back,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(value, "hello d");
        press(&mut value, &mut back, KeyCode::Home, KeyModifiers::NONE);
        press(
            &mut value,
            &mut back,
            KeyCode::Char('>'),
            KeyModifiers::NONE,
        );
        press(
            &mut value,
            &mut back,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        );
        assert_eq!((value.as_str(), back), (">", 0));
        insert_text(&mut value, 1, "ab\ncd");
        assert_eq!(value, "abcd>");
    }
}
