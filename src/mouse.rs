//! Mouse support: clickable hints, a right-click menu, draggable pane borders, and
//! clicks and the wheel in every list, the reader and the composer.

use super::*;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

/// Two clicks on the same cell within this time are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// A pane border that can be dragged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Divider {
    Inbox,
    Reader,
}

/// Where the open overlay was drawn, for mouse hits.
#[derive(Clone, Debug, Default)]
pub(crate) struct OverlayGeometry {
    pub(crate) area: Rect,
    /// First item row, scroll offset and item count of a list overlay.
    pub(crate) list: Option<(u16, usize, usize)>,
    /// Composer header rows and the field each one edits.
    pub(crate) fields: Vec<(Rect, usize)>,
    pub(crate) body: Option<Rect>,
}

/// One entry of the right-click menu: a label and the key it presses.
#[derive(Clone, Debug)]
pub(crate) struct MenuItem {
    pub(crate) label: String,
    pub(crate) key: KeyEvent,
}

fn press(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
}

fn code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn inside(area: Rect, column: u16, row: u16) -> bool {
    area.contains(ratatui::layout::Position::new(column, row))
}

impl App {
    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let (column, row) = (mouse.column, mouse.row);
        if matches!(mouse.kind, MouseEventKind::Moved) {
            self.hover = Some((column, row));
            if let Some(Overlay::Menu {
                selected, items, ..
            }) = &mut self.overlay
                && let Some(index) = menu_index(&self.overlay_geometry, row, items.len())
            {
                *selected = index;
            }
            return;
        }
        let double = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && {
            let now = Instant::now();
            let double = self.last_click.is_some_and(|(time, x, y)| {
                now.duration_since(time) < DOUBLE_CLICK && x == column && y == row
            });
            self.last_click = if double {
                None
            } else {
                Some((now, column, row))
            };
            double
        };

        // Hints in the status line press their key.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some((_, key)) = self
                .hint_regions
                .iter()
                .find(|(area, _)| inside(*area, column, row))
                .cloned()
        {
            if matches!(self.overlay, Some(Overlay::Menu { .. })) {
                self.overlay = None;
            }
            self.handle_event(Event::Key(key));
            return;
        }

        if self.overlay.is_some() {
            self.handle_overlay_mouse(mouse, double);
            return;
        }
        if self.onboarding {
            return;
        }

        if self.handle_divider(mouse) {
            return;
        }

        if let Some(reader) = &mut self.reader {
            let docked = self.reader_docked;
            if reader.contains(column, row) {
                let target = reader.target_at(column, row);
                match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        reader.scroll_down(settings::with(|ui| ui.scroll_lines))
                    }
                    MouseEventKind::ScrollUp => {
                        reader.scroll_up(settings::with(|ui| ui.scroll_lines))
                    }
                    MouseEventKind::Down(MouseButton::Left) => match target {
                        Some(widgets::reader::Target::Link(link)) => {
                            self.notice = Some(match opener::open(&link) {
                                Ok(()) => "Link opened in the default browser.".into(),
                                Err(error) => format!("Unable to open link: {error}"),
                            });
                        }
                        Some(widgets::reader::Target::Attachment(index)) => {
                            self.open_attachments();
                            if let Some(Overlay::Attachments { selected, items }) =
                                &mut self.overlay
                            {
                                *selected = index.min(items.len().saturating_sub(1));
                            }
                        }
                        None => {}
                    },
                    MouseEventKind::Down(MouseButton::Right) => {
                        self.open_context_menu(column, row);
                    }
                    _ => return,
                }
                if self.graphics_shown() {
                    self.clear_screen = true;
                }
                return;
            }
            if !docked {
                // A click beside the floating reader closes it.
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.reader = None;
                    self.clear_screen = true;
                }
                return;
            }
        }

        if self.handle_inbox_mouse(mouse, double) {
            return;
        }
        let in_workspace = self
            .workspace_area
            .is_some_and(|area| inside(area, column, row));
        if !in_workspace || self.visible_conversations().is_empty() {
            return;
        }
        let Some(workspace) = &mut self.workspace else {
            return;
        };
        let before = workspace.selected_mail_id();
        let right = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right));
        // The map selects on a left press, so a right click selects the card first.
        let event = if right {
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                ..mouse
            }
        } else {
            mouse
        };
        let clicked = workspace.handle_mouse(event);
        if right {
            let _ = workspace.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..mouse
            });
        }
        if let Some(mail_id) = clicked {
            self.focus = Pane::Workspace;
            self.sync_inbox_selection();
            if right {
                self.open_context_menu(column, row);
            } else if double || before.as_deref() == Some(mail_id.as_str()) {
                self.open_mail(&mail_id);
            }
        }
    }

    /// Starts, continues or ends dragging a pane border.
    fn handle_divider(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(area) = self.inbox_area
                    && mouse.column + 1 == area.right()
                    && mouse.row >= area.y
                    && mouse.row < area.bottom()
                    && self.workspace_area.is_some()
                {
                    self.dragging = Some(Divider::Inbox);
                    return true;
                }
                if self.reader_docked
                    && let Some(area) = self.reader_area
                    && mouse.column == area.x
                {
                    self.dragging = Some(Divider::Reader);
                    return true;
                }
                false
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(divider) = self.dragging else {
                    return false;
                };
                let width = self.screen_width;
                match divider {
                    Divider::Inbox => {
                        let x = self.inbox_area.map_or(0, |area| area.x);
                        self.inbox_width = (mouse.column + 1)
                            .saturating_sub(x)
                            .clamp(20, (width / 2).max(20));
                    }
                    Divider::Reader => {
                        self.reader_width = width
                            .saturating_sub(mouse.column)
                            .clamp(30, (width / 2).max(30));
                    }
                }
                self.clear_screen = self.graphics_shown();
                true
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging.take().is_some(),
            _ => false,
        }
    }

    /// Scrolls the inbox with the wheel; a click selects a conversation, a second
    /// click or a double click opens it, a right click shows its actions, and a
    /// click on the title shows the folders.
    pub(crate) fn handle_inbox_mouse(&mut self, mouse: MouseEvent, double: bool) -> bool {
        let Some(area) = self.inbox_area else {
            return false;
        };
        if !inside(area, mouse.column, mouse.row)
            || !matches!(self.mailbox_state, MailboxState::Ready)
        {
            return false;
        }
        if mouse.row == area.y {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.open_folders();
            }
            return true;
        }
        if mouse.row + 1 >= area.bottom() {
            return true;
        }
        let count = self.visible_conversations().len();
        let index = self.list_offset + usize::from(mouse.row - area.y - 1) / 2;
        match mouse.kind {
            MouseEventKind::ScrollDown => self.select_conversation(self.selected + 1),
            MouseEventKind::ScrollUp => self.select_conversation(self.selected.saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) if index < count => {
                self.focus = Pane::Inbox;
                if index == self.selected || double {
                    self.select_conversation(index);
                    if let Some(mail_id) = self.entry_mail_id() {
                        self.open_mail(&mail_id);
                    }
                } else {
                    self.select_conversation(index);
                }
            }
            MouseEventKind::Down(MouseButton::Right) if index < count => {
                self.focus = Pane::Inbox;
                self.select_conversation(index);
                self.open_context_menu(mouse.column, mouse.row);
            }
            MouseEventKind::Down(MouseButton::Middle) if index < count => {
                // A middle click toggles read, like marking a tab in a browser.
                self.focus = Pane::Inbox;
                self.select_conversation(index);
                self.toggle_read();
            }
            _ => {}
        }
        true
    }

    fn handle_overlay_mouse(&mut self, mouse: MouseEvent, double: bool) {
        let (column, row) = (mouse.column, mouse.row);
        let geometry = self.overlay_geometry.clone();
        let outside = !inside(geometry.area, column, row);
        let left = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let wheel = match mouse.kind {
            MouseEventKind::ScrollDown => Some(KeyCode::Down),
            MouseEventKind::ScrollUp => Some(KeyCode::Up),
            _ => None,
        };
        match &mut self.overlay {
            Some(Overlay::Menu { items, .. }) => {
                if left || matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
                    let chosen = menu_index(&geometry, row, items.len())
                        .filter(|_| !outside)
                        .map(|index| items[index].key);
                    self.overlay = None;
                    if let Some(key) = chosen {
                        self.handle_event(Event::Key(key));
                    } else if !left {
                        // A right click elsewhere opens a menu there instead.
                        self.handle_mouse(mouse);
                    }
                }
            }
            Some(Overlay::Compose(state)) => {
                if let Some(code) = wheel {
                    if code == KeyCode::Down {
                        state.move_next();
                    } else {
                        state.move_previous();
                    }
                    self.cursor_back = 0;
                    return;
                }
                if !left {
                    return;
                }
                if let Some((_, field)) = geometry
                    .fields
                    .iter()
                    .find(|(area, _)| inside(*area, column, row))
                {
                    let field = *field;
                    if state.field == field
                        && matches!(
                            field,
                            ComposeState::FORMAT_FIELD | ComposeState::SECURITY_FIELD
                        )
                    {
                        self.handle_event(Event::Key(code(KeyCode::Right)));
                    } else if state.field != field {
                        state.field = field;
                        state.error = None;
                        self.cursor_back = 0;
                        let mut state = match self.overlay.take() {
                            Some(Overlay::Compose(state)) => state,
                            _ => return,
                        };
                        self.suggest(&mut state);
                        self.overlay = Some(Overlay::Compose(state));
                    }
                } else if geometry.body.is_some_and(|area| inside(area, column, row)) {
                    state.field = ComposeState::BODY_FIELD;
                    self.handle_event(Event::Key(code(KeyCode::Enter)));
                }
            }
            Some(Overlay::Help { .. }) if wheel.is_some() => {
                let key = if wheel == Some(KeyCode::Down) {
                    'j'
                } else {
                    'k'
                };
                self.handle_event(Event::Key(press(key)));
            }
            Some(
                Overlay::Folders { .. }
                | Overlay::Links { .. }
                | Overlay::Attachments { .. }
                | Overlay::Accounts { .. },
            ) => {
                if let Some(code) = wheel {
                    self.handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                    return;
                }
                if !left {
                    return;
                }
                if outside {
                    self.handle_event(Event::Key(code(KeyCode::Esc)));
                    return;
                }
                let Some((top, offset, count)) = geometry.list else {
                    return;
                };
                if row < top {
                    return;
                }
                let index = offset + usize::from(row - top);
                if index >= count {
                    return;
                }
                let current = match &mut self.overlay {
                    Some(
                        Overlay::Folders { selected, .. }
                        | Overlay::Links { selected, .. }
                        | Overlay::Attachments { selected, .. }
                        | Overlay::Accounts { selected, .. },
                    ) => std::mem::replace(selected, index),
                    _ => return,
                };
                if current == index || double {
                    self.handle_event(Event::Key(code(KeyCode::Enter)));
                }
            }
            Some(
                Overlay::Help { .. }
                | Overlay::Command(_)
                | Overlay::Prompt { .. }
                | Overlay::Unsubscribe { .. }
                | Overlay::DraftReady { .. },
            ) if left && outside => {
                self.handle_event(Event::Key(code(KeyCode::Esc)));
            }
            _ => {}
        }
    }

    /// Shows what can be done with the selected message at the pointer.
    pub(crate) fn open_context_menu(&mut self, column: u16, row: u16) {
        let Some(mail) = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
        else {
            return;
        };
        let mut items = vec![MenuItem {
            label: if self.reader.is_some() {
                "Close".into()
            } else {
                "Open".into()
            },
            key: if self.reader.is_some() {
                code(KeyCode::Esc)
            } else {
                code(KeyCode::Enter)
            },
        }];
        let mut add = |label: &str, key: char| {
            items.push(MenuItem {
                label: label.into(),
                key: press(key),
            });
        };
        let folder = mail_folder(mail);
        if folder == "Outbox" {
            add("Retry sending", 'x');
            add("Resolve delivery", 'z');
        } else {
            add("Reply", 'r');
            add("Reply all", 'A');
            add("Forward", 'F');
        }
        add(
            if mail.unread {
                "Mark read"
            } else {
                "Mark unread"
            },
            'm',
        );
        add(if mail.starred { "Unstar" } else { "Star" }, 's');
        add("Archive", 'a');
        add(
            if folder == "Trash" {
                "Delete forever"
            } else {
                "Move to Trash"
            },
            'd',
        );
        add("Move to folder…", 'M');
        if self.reader.is_some() {
            if !mail.links.is_empty() {
                add("Links…", 'l');
            }
            if !mail.attachments.is_empty() {
                add("Attachments…", 'v');
            }
            if mail.html {
                add("Open in browser", 'O');
            }
            if mail.partial {
                add("Download full message", 'D');
            }
            if mail.unsubscribe.is_some() {
                add("Unsubscribe…", 'U');
            }
        }
        self.overlay = Some(Overlay::Menu {
            x: column,
            y: row,
            items,
            selected: 0,
        });
    }
}

fn menu_index(geometry: &OverlayGeometry, row: u16, count: usize) -> Option<usize> {
    let (top, ..) = geometry.list?;
    let index = usize::from(row.checked_sub(top)?);
    (index < count).then_some(index)
}

/// The keys a hint names: `enter`, `ctrl+s`, `j/k`, `r/A/F` and so on.
fn hint_keys(text: &str) -> Vec<(usize, usize, KeyEvent)> {
    let mut keys = Vec::new();
    let mut offset = 0;
    for part in text.split('/') {
        let width = UnicodeWidthStr::width(part);
        let key = match part {
            "enter" => Some(code(KeyCode::Enter)),
            "esc" => Some(code(KeyCode::Esc)),
            "tab" => Some(code(KeyCode::Tab)),
            "space" => Some(press(' ')),
            "pgup" => Some(code(KeyCode::PageUp)),
            "pgdn" => Some(code(KeyCode::PageDown)),
            _ if part.starts_with("ctrl+") => part
                .strip_prefix("ctrl+")
                .and_then(|rest| rest.chars().next())
                .map(|character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)),
            _ => {
                let mut characters = part.chars();
                match (characters.next(), characters.next()) {
                    (Some(character), None) => Some(press(character)),
                    _ => None,
                }
            }
        };
        if let Some(key) = key {
            keys.push((offset, width, key));
        }
        offset += width + 1;
    }
    keys
}

/// Makes the keycaps of a status line clickable and highlights the one under the
/// pointer. Returns the clickable cells and the keys they press.
pub(crate) fn clickable_hints(
    line: &mut Line<'static>,
    area: Rect,
    hover: Option<(u16, u16)>,
) -> Vec<(Rect, KeyEvent)> {
    let keycap = theme::keycap();
    let mut regions = Vec::new();
    let mut x = area.x;
    let hovered_row = hover.filter(|(_, row)| *row == area.y);
    let count = line.spans.len();
    for index in 0..count {
        let width = UnicodeWidthStr::width(line.spans[index].content.as_ref()) as u16;
        if line.spans[index].style == keycap {
            let text = line.spans[index].content.trim().to_owned();
            let keys = hint_keys(&text);
            let label_width = line
                .spans
                .get(index + 1)
                .filter(|span| span.style != keycap)
                .map_or(0, |span| {
                    UnicodeWidthStr::width(span.content.as_ref()) as u16
                });
            if let Some(first) = keys.first().map(|(.., key)| *key) {
                for (offset, part_width, key) in &keys {
                    regions.push((
                        Rect::new(x + 1 + *offset as u16, area.y, *part_width as u16, 1),
                        *key,
                    ));
                }
                // The keycap's padding and its label press the first key.
                regions.push((Rect::new(x, area.y, width + label_width, 1), first));
                if hovered_row
                    .is_some_and(|(column, _)| column >= x && column < x + width + label_width)
                {
                    line.spans[index].style = theme::hover();
                }
            }
        }
        x = x.saturating_add(width);
        if x >= area.right() {
            break;
        }
    }
    regions
}
