//! Keyboard, mouse and command handling.

use super::*;

impl App {
    pub(crate) fn handle_event(&mut self, event: Event) {
        if matches!(event, Event::Key(_)) && self.graphics_shown() {
            self.clear_screen = true;
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key_event(key),
            Event::Paste(text) => match self.overlay.take() {
                Some(Overlay::AccountSetup(mut form)) => {
                    if !form.busy {
                        form.input(&text);
                    }
                    self.overlay = Some(Overlay::AccountSetup(form));
                }
                Some(Overlay::Compose(mut state)) => {
                    if let Some(value) = state.value_mut() {
                        value.extend(text.chars().filter(|character| !character.is_control()));
                    }
                    self.suggest(&mut state);
                    self.overlay = Some(Overlay::Compose(state));
                }
                overlay => self.overlay = overlay,
            },
            Event::Mouse(mouse) => {
                if self.overlay.is_some() || self.onboarding {
                    return;
                }
                if let Some(reader) = &mut self.reader {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => reader.scroll_down(3),
                        MouseEventKind::ScrollUp => reader.scroll_up(3),
                        _ => return,
                    }
                    if reader.shows_images() && !self.uses_halfblocks() {
                        self.clear_screen = true;
                    }
                    return;
                }
                if self.handle_inbox_mouse(mouse) {
                    return;
                }
                if !self.visible_conversations().is_empty()
                    && let Some(workspace) = &mut self.workspace
                    && let Some(mail_id) = workspace.handle_mouse(mouse)
                {
                    self.open_mail(&mail_id);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn on_key_event(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Char('d') {
            self.confirm_delete = None;
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
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.refresh_inbox();
                }
                _ => {
                    if edit_text(&mut self.query, key) {
                        self.refresh_inbox();
                    }
                }
            }
            return;
        }

        if key.code == KeyCode::Esc && self.notice.take().is_some() {
            return;
        }

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
            _ => {}
        }

        if self.reader.is_some() {
            match key.code {
                KeyCode::Esc => self.reader = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.reader.as_mut().unwrap().scroll_down(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.reader.as_mut().unwrap().scroll_up(1);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.reader.as_mut().unwrap().scroll_down(10);
                }
                KeyCode::PageUp => self.reader.as_mut().unwrap().scroll_up(10),
                KeyCode::Home => self.reader.as_mut().unwrap().scroll_up(u16::MAX),
                KeyCode::End => self.reader.as_mut().unwrap().scroll_down(u16::MAX),
                KeyCode::Char('n') => self.open_adjacent(true),
                KeyCode::Char('N') => self.open_adjacent(false),
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
                    workspace.handle_key(key.code);
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

    /// Scrolls the inbox with the wheel; a click selects a conversation and a click on
    /// the selected one opens it.
    pub(crate) fn handle_inbox_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(area) = self.inbox_area else {
            return false;
        };
        let inside = mouse.column >= area.x
            && mouse.column < area.x + area.width
            && mouse.row > area.y
            && mouse.row + 1 < area.y + area.height;
        if !inside || !matches!(self.mailbox_state, MailboxState::Ready) {
            return false;
        }
        let count = self.visible_conversations().len();
        match mouse.kind {
            MouseEventKind::ScrollDown => self.select_conversation(self.selected + 1),
            MouseEventKind::ScrollUp => self.select_conversation(self.selected.saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) => {
                let index = self.list_offset + usize::from(mouse.row - area.y - 1) / 2;
                if index >= count {
                    return true;
                }
                self.focus = Pane::Inbox;
                if index == self.selected {
                    if let Some(mail_id) = self.entry_mail_id() {
                        self.open_mail(&mail_id);
                    }
                } else {
                    self.select_conversation(index);
                }
            }
            _ => {}
        }
        true
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
                    self.notice = Some("Draft discarded".into());
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
                    edit_text(input, key);
                    self.overlay = Some(overlay);
                }
            },
            Overlay::Folders {
                selected,
                items,
                moving,
            } => match key.code {
                KeyCode::Esc => {}
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
            Overlay::Links { selected, items } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('y') => {
                    if let Some(link) = items.get(*selected) {
                        copy_to_clipboard(link);
                        self.notice = Some("Link copied to the clipboard.".into());
                    }
                }
                KeyCode::Enter => {
                    if let Some(link) = items.get(*selected) {
                        self.notice = match opener::open(link) {
                            Ok(()) => Some("Link opened in the default browser.".into()),
                            Err(error) => Some(format!(
                                "Unable to open link: {error}. Copy it from the message."
                            )),
                        };
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
            "sync" => self.start_sync(),
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
            "" => {}
            _ => {
                self.notice = Some(format!(
                    "Unknown command: {command}. Press ? to view commands."
                ));
            }
        }
    }
}

/// Line editing shared by every text field: typing, backspace, ctrl+w to delete a
/// word and ctrl+u to clear. Returns whether the key changed or belonged to the field.
pub(crate) fn edit_text(value: &mut String, key: KeyEvent) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => delete_word(value),
        KeyCode::Backspace => {
            value.pop();
        }
        KeyCode::Char('w' | 'W') if control => delete_word(value),
        KeyCode::Char('u' | 'U') if control => value.clear(),
        KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
            value.push(character);
        }
        _ => return false,
    }
    true
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

pub(crate) fn delete_word(value: &mut String) {
    let trimmed = value
        .trim_end_matches(|character: char| character.is_whitespace() || ",;".contains(character))
        .len();
    value.truncate(trimmed);
    let start = value
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace() || ",;".contains(*character))
        .map_or(0, |(index, character)| index + character.len_utf8());
    value.truncate(start);
}

pub(crate) fn action_key(key: KeyEvent, expected: char) -> bool {
    matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&expected))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}
