//! Drawing the main screen, overlays and the composer.

use super::*;

impl App {
    /// Where the reader window goes: maximised, where it was moved, or centred at the
    /// size the `[reader]` table asks for.
    pub(crate) fn popup_rect(&self, screen: Rect) -> Rect {
        if self.popup_maximized {
            return screen;
        }
        match self.popup {
            Some(rect) => clamp_rect(rect, screen),
            None => {
                let reader = settings::reader();
                let width = (u32::from(screen.width) * u32::from(reader.popup_width) / 100) as u16;
                let height =
                    (u32::from(screen.height) * u32::from(reader.popup_height) / 100) as u16;
                let limit = settings::with(|ui| ui.reader_max_width);
                let width = if limit > 0 { width.min(limit) } else { width };
                let width = width.clamp(MIN_POPUP.0.min(screen.width), screen.width);
                let height = height.clamp(MIN_POPUP.1.min(screen.height), screen.height);
                Rect::new(
                    screen.x + (screen.width - width) / 2,
                    screen.y + (screen.height - height) / 2,
                    width,
                    height,
                )
            }
        }
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame) {
        if self.onboarding {
            frame.render_widget(Paragraph::new("Connect an account\n\nAdd an email account to read and send mail.\n\nenter  Add account    q  Quit")
                .style(theme::text()).alignment(Alignment::Center)
                .block(Block::bordered().title(i18n::t(" termail ")).border_style(theme::border(true))), frame.area());
            if let Some(overlay) = &mut self.overlay {
                self.overlay_geometry = draw_overlay(frame, overlay, self.cursor_back);
            }
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(frame.area());

        if !matches!(self.mailbox_state, MailboxState::Ready) {
            draw_mailbox_state(
                frame,
                rows[0],
                &self.mailbox_state,
                &self.source,
                self.spinner_tick,
            );
            let hints = if let Some(hints) = self.overlay_hints() {
                hints
            } else if let Some(notice) = &self.notice {
                action_line(notice, false)
            } else {
                status_line(&[
                    ("c", "compose"),
                    ("p", "accounts"),
                    ("r", "reload"),
                    ("?", "help"),
                    ("q", "quit"),
                ])
            };
            let mut hints = hints;
            self.hint_regions = clickable_hints(&mut hints, rows[1], self.hover);
            frame.render_widget(Paragraph::new(hints), rows[1]);
            if let Some(overlay) = &mut self.overlay {
                self.overlay_geometry = draw_overlay(frame, overlay, self.cursor_back);
            }
            return;
        }

        let visible = self.visible_conversations();
        let items: Vec<ListItem> = visible
            .iter()
            .map(|index| {
                let conversation = &self.conversations[*index];
                let last_sender = conversation.last_sender(&self.mails).unwrap_or("unknown");
                let unread = conversation.is_unread(&self.mails);
                let starred = conversation.is_starred(&self.mails);
                let marker = settings::with(|ui| {
                    let blank =
                        |marker: &str| " ".repeat(unicode_width::UnicodeWidthStr::width(marker));
                    // Screen readers say "black circle" for ●; letters read better.
                    let plain = |marker: &str, symbol: &str, letter: &str| {
                        if ui.accessible && marker == symbol {
                            letter.to_owned()
                        } else {
                            marker.to_owned()
                        }
                    };
                    let unread_marker = plain(&ui.unread_marker, "●", "N");
                    let star_marker = plain(&ui.star_marker, "★", "*");
                    format!(
                        "{}{}",
                        if unread {
                            unread_marker.clone()
                        } else {
                            blank(&unread_marker)
                        },
                        if starred {
                            star_marker.clone()
                        } else {
                            blank(&star_marker)
                        },
                    )
                });
                let subject = format!(
                    "{marker} {} · {}",
                    conversation.subject,
                    conversation.mail_ids.len()
                );
                let terms = mail::search_terms(&self.query);
                let highlights = |value: &str, field: &str| {
                    let value = value.to_lowercase();
                    terms.iter().any(|term| {
                        let needle = term
                            .strip_prefix(field)
                            .and_then(|rest| rest.strip_prefix(':'))
                            .unwrap_or(term);
                        !needle.is_empty() && !needle.contains(':') && value.contains(needle)
                    })
                };
                let subject_style = if highlights(&conversation.subject, "subject") {
                    theme::title(true)
                } else {
                    theme::text()
                };
                let sender_style = if highlights(last_sender, "from") {
                    theme::title(true)
                } else {
                    theme::muted()
                };
                let account = conversation
                    .mails(&self.mails)
                    .into_iter()
                    .find_map(|mail| mail.account.as_deref());
                let date = conversation
                    .mails(&self.mails)
                    .iter()
                    .filter_map(|mail| mail.timestamp)
                    .max()
                    .map(mail::short_date)
                    .unwrap_or_default();
                let sender = match account {
                    Some(account) if matches!(self.source, MailboxSource::Unified(_)) => {
                        format!("{date} · {last_sender} · {account}")
                    }
                    _ if date.is_empty() => format!("from {last_sender}"),
                    _ => format!("{date} · {last_sender}"),
                };
                // A sender the receiving server could not verify is flagged in the list.
                let forged = conversation.mails(&self.mails).iter().any(|mail| {
                    mail.sender_check.as_ref().is_some_and(|check| check.failed)
                        || crate::links::spoofed_name(&mail.from).is_some()
                });
                let sender = if forged {
                    format!("⚠ {sender}")
                } else {
                    sender
                };
                // Results from every folder say where they live.
                let sender = match conversation.mails(&self.mails).last() {
                    Some(mail) if self.built_everywhere => {
                        format!("{sender} · {}", mail_folder(mail))
                    }
                    _ => sender,
                };
                let (subject, sender) = match &self.script {
                    Some(engine) if engine.formats_rows() => {
                        let mails = conversation.mails(&self.mails);
                        let latest = mails
                            .iter()
                            .filter_map(|mail| mail.timestamp)
                            .max()
                            .unwrap_or_default();
                        let folder = mails
                            .last()
                            .map(|mail| mail_folder(mail).to_owned())
                            .unwrap_or_default();
                        let key = format!(
                            "{}:{}:{unread}:{starred}:{latest}:{folder}",
                            conversation.root_id,
                            conversation.mail_ids.len()
                        );
                        let row = script::Row {
                            subject: conversation.subject.clone(),
                            from: last_sender.to_owned(),
                            date: date.clone(),
                            timestamp: latest,
                            count: conversation.mail_ids.len(),
                            unread,
                            starred,
                            folder,
                            account: account.unwrap_or_default().to_owned(),
                            title: subject.clone(),
                            subtitle: sender.clone(),
                        };
                        engine.format_row(key, &row).unwrap_or((subject, sender))
                    }
                    _ => (subject, sender),
                };
                ListItem::new(vec![
                    Line::styled(subject, subject_style),
                    Line::styled(sender, sender_style),
                ])
            })
            .collect();

        let inbox_focused = self.reader.is_none() && self.focus == Pane::Inbox;
        let workspace_focused = self.reader.is_none() && self.focus == Pane::Workspace;
        let unread = self
            .mails
            .iter()
            .filter(|mail| mail.unread && mail_folder(mail) == self.active_folder)
            .count();
        let folder = if unread > 0 {
            format!("{} ({unread})", self.active_folder)
        } else {
            self.active_folder.clone()
        };
        let title = if self.searching {
            let mut line = cursor_line(
                &format!(" {}: ", i18n::t("search")),
                &self.query,
                self.cursor_back,
            );
            line.spans.push(Span::raw(" "));
            line
        } else {
            let inbox = i18n::t("inbox");
            let filter = i18n::t(self.filter.label());
            Line::from(if !self.query.is_empty() {
                format!(" {inbox} • {filter} • /{} ", self.query)
            } else if self.filter != InboxFilter::All {
                format!(" {inbox} • {filter} ")
            } else if self.skipped > 0 {
                format!(
                    " {} · {folder} • {} ",
                    self.account_label,
                    i18n::t(&format!("{} skipped", self.skipped))
                )
            } else if !self.syncing.is_empty() {
                format!(
                    " {} · {folder} • {} ",
                    self.account_label,
                    i18n::t("syncing")
                )
            } else if inbox_focused {
                format!(
                    " {} · {folder} • {} ",
                    self.account_label,
                    i18n::t("active")
                )
            } else {
                format!(" {} · {folder} ", self.account_label)
            })
        };
        let list = List::new(items)
            .style(theme::text())
            .highlight_symbol("▌ ")
            .highlight_style(theme::selection())
            .highlight_spacing(HighlightSpacing::Always)
            .repeat_highlight_symbol(true)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::border(inbox_focused))
                    .title_style(theme::title(inbox_focused))
                    .title(title),
            );
        let mut list_state = ListState::default()
            .with_offset(self.list_offset)
            .with_selected(Some(self.selected));

        let mut docked_reader_area = None;
        self.screen_width = frame.area().width;
        self.screen_height = frame.area().height;
        self.workspace_area = None;
        self.inbox_area = None;
        if rows[0].width < settings::with(|ui| ui.split_min_width) {
            match self.focus {
                Pane::Inbox => {
                    frame.render_stateful_widget(list, rows[0], &mut list_state);
                    self.inbox_area = Some(rows[0]);
                }
                Pane::Workspace => {
                    if visible.is_empty() {
                        draw_no_matches(
                            frame,
                            rows[0],
                            &self.active_folder,
                            self.conversations.is_empty(),
                        );
                    } else if let Some(workspace) = &mut self.workspace {
                        workspace.draw(frame, rows[0], workspace_focused);
                    }
                    self.workspace_area = Some(rows[0]);
                }
            }
        } else {
            let constraints = if reader_is_docked(rows[0].width, self.reader.is_some()) {
                vec![
                    Constraint::Length(self.inbox_width.min(rows[0].width / 3)),
                    Constraint::Min(0),
                    Constraint::Length(self.reader_width),
                ]
            } else {
                vec![Constraint::Length(self.inbox_width), Constraint::Min(0)]
            };
            let areas = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(rows[0]);
            frame.render_stateful_widget(list, areas[0], &mut list_state);
            self.inbox_area = Some(areas[0]);
            if visible.is_empty() {
                draw_no_matches(
                    frame,
                    areas[1],
                    &self.active_folder,
                    self.conversations.is_empty(),
                );
            } else if let Some(workspace) = &mut self.workspace {
                workspace.draw(frame, areas[1], workspace_focused);
            }
            self.workspace_area = Some(areas[1]);
            if areas.len() == 3 {
                docked_reader_area = Some(areas[2]);
            }
        }

        self.list_offset = list_state.offset();
        // Screen readers and magnifiers follow the terminal cursor.
        if settings::with(|ui| ui.accessible)
            && self.overlay.is_none()
            && (self.reader.is_none() || self.reader_docked)
            && self.focus == Pane::Inbox
            && !visible.is_empty()
            && let Some(area) = self.inbox_area
        {
            let row = area.y + 1 + (self.selected.saturating_sub(self.list_offset) * 2) as u16;
            if row < area.bottom().saturating_sub(1) {
                frame.set_cursor_position((area.x + 1, row));
            }
        }

        let hints = if let Some(hints) = self.overlay_hints() {
            hints
        } else if let Some(notice) = &self.notice {
            action_line(notice, self.undo.is_some())
        } else if self.searching {
            status_line(&[
                ("type", "search"),
                ("enter", "apply"),
                ("ctrl+r", "search server"),
                ("esc", "clear"),
            ])
        } else if self.reader.is_some() && !self.reader_docked {
            let wide = self
                .reader
                .as_ref()
                .is_some_and(widgets::reader::Reader::scrolls_sideways);
            status_line(&[
                ("j/k", "scroll"),
                if wide {
                    ("←/→", "sideways")
                } else {
                    ("ctrl+d/u", "half page")
                },
                ("r/A/F", "reply/all/forward"),
                ("f", "full screen"),
                ("+/-", "size"),
                ("t", "colours"),
                ("l", "links"),
                ("v", "files"),
                ("I", "images"),
                ("a/d", "archive/trash"),
                ("esc", "close"),
            ])
        } else if self.reader.is_some() {
            status_line(&[
                ("j/k", "scroll"),
                ("r/A/F", "reply/all/forward"),
                ("l", "links"),
                ("v", "files"),
                ("I", "images"),
                ("m/s", "read/star"),
                ("a/d", "archive/trash"),
                ("esc", "close"),
            ])
        } else {
            match self.focus {
                Pane::Inbox => status_line(&[
                    ("tab", "thread map"),
                    ("g", "folders"),
                    ("p", "accounts"),
                    ("j/k", "select"),
                    ("/", "search"),
                    ("1/2/3", "filter"),
                    ("m/s", "read/star"),
                    ("a/d", "archive/trash"),
                    ("c/r", "compose/reply"),
                    ("enter", "open"),
                    ("q", "quit"),
                ]),
                Pane::Workspace => status_line(&[
                    ("tab", "inbox"),
                    ("arrows", "select"),
                    ("enter", "open"),
                    ("f", "fit"),
                    ("+/-", "zoom"),
                    ("q", "quit"),
                ]),
            }
        };
        let mut hints = if self.notice.is_none()
            && self.overlay.is_none()
            && !settings::with(|ui| ui.show_hints)
        {
            Line::default()
        } else {
            hints
        };
        if let Some(text) = self.script_status() {
            hints
                .spans
                .insert(0, Span::styled(format!(" {text} "), theme::title(true)));
        }
        self.hint_regions = clickable_hints(&mut hints, rows[1], self.hover);
        frame.render_widget(Paragraph::new(hints), rows[1]);

        let popup = self.popup_rect(rows[0]);
        if let Some(reader) = &mut self.reader
            && let Some(mail) = self.mails.iter().find(|mail| mail.id == reader.mail_id())
        {
            self.reader_docked = docked_reader_area.is_some();
            self.reader_area = docked_reader_area;
            if let Some(area) = docked_reader_area {
                reader.draw_docked(frame, area, mail, self.picker.as_ref());
            } else {
                reader.draw(frame, popup, mail, self.picker.as_ref());
            }
        }

        if let Some(overlay) = &mut self.overlay {
            self.overlay_geometry = draw_overlay(frame, overlay, self.cursor_back);
        }
    }

    /// The status line for the open overlay, if any overlay has its own keys.
    pub(crate) fn overlay_hints(&self) -> Option<Line<'static>> {
        let hints: &[(&str, &str)] = match self.overlay.as_ref()? {
            Overlay::Help { .. } => &[("j/k", "scroll"), ("esc", "close help")],
            Overlay::Command(_) => &[("type", "command"), ("enter", "run"), ("esc", "cancel")],
            Overlay::DraftReady { .. } => &[("s", "send"), ("e", "edit"), ("d", "save draft")],
            Overlay::ComposeClose(_) => {
                &[("s", "save draft"), ("d", "discard"), ("esc", "continue")]
            }
            Overlay::Folders { moving: true, .. } => {
                &[("j/k", "select"), ("enter", "move here"), ("esc", "cancel")]
            }
            Overlay::Folders { deleting: true, .. } => {
                &[("x", "delete folder and its mail"), ("esc", "cancel")]
            }
            Overlay::Folders { .. } => &[
                ("enter", "open"),
                ("n", "new"),
                ("e", "rename"),
                ("x", "delete"),
                ("esc", "close"),
            ],
            Overlay::Unsubscribe { .. } => &[("y", "unsubscribe"), ("esc", "keep")],
            Overlay::Contacts { .. } => &[
                ("type", "filter"),
                ("enter", "write to"),
                ("ctrl+x", "remove"),
                ("esc", "close"),
            ],
            Overlay::LinkWarning { .. } => {
                &[("o", "open anyway"), ("y", "copy"), ("esc", "cancel")]
            }
            Overlay::Menu { .. } => &[("j/k", "select"), ("enter", "choose"), ("esc", "close")],
            Overlay::Prompt { .. } => &[
                ("type", "name · / for subfolders"),
                ("enter", "save"),
                ("esc", "cancel"),
            ],
            Overlay::Links { .. } => &[("j/k", "select"), ("enter", "open"), ("y", "copy")],
            Overlay::Accounts { .. } => &[("j/k", "select"), ("enter", "open"), ("esc", "cancel")],
            Overlay::Attachments { .. } => &[("j/k", "select"), ("s", "save"), ("o", "open")],
            Overlay::Compose(_) => &[
                ("tab", "next field or accept ⇥"),
                ("ctrl+s", "review"),
                ("esc", "close"),
            ],
            Overlay::BodyEditor { .. } => &[
                ("ctrl+s/esc", "done"),
                ("arrows", "move"),
                ("ctrl+k", "delete line"),
            ],
            Overlay::AccountSetup(_) | Overlay::DeliveryReview { .. } => return None,
        };
        Some(status_line(hints))
    }
}

pub(crate) fn draw_mailbox_state(
    frame: &mut Frame,
    area: Rect,
    state: &MailboxState,
    source: &MailboxSource,
    spinner_tick: u64,
) {
    let block = Block::bordered()
        .border_style(theme::border(false))
        .title_style(theme::title(false))
        .title(i18n::t(" termail "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width.min(64);
    let content = Rect::new(
        inner.x + inner.width.saturating_sub(width) / 2,
        inner.y + inner.height.saturating_sub(5) / 2,
        width,
        5.min(inner.height),
    );

    match state {
        MailboxState::Loading => {
            let spinner_area = Rect::new(
                content.x + content.width.saturating_sub(6) / 2,
                content.y,
                6.min(content.width),
                1,
            );
            if !settings::with(|ui| ui.accessible) {
                frame.render_widget(
                    FluxSpinner::new(spinner_tick)
                        .width(6)
                        .color(theme::accent()),
                    spinner_area,
                );
            }
            frame.render_widget(
                Paragraph::new(i18n::t(&format!("Loading {}", source.label())))
                    .style(theme::text())
                    .alignment(Alignment::Center),
                Rect::new(content.x, content.y.saturating_add(2), content.width, 1),
            );
        }
        MailboxState::Empty => {
            frame.render_widget(
                Paragraph::new(i18n::t(
                    "No messages found\n\nPress c to compose or p to switch accounts. New mail appears automatically.",
                ))
                .style(theme::text())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
                content,
            );
        }
        MailboxState::Error(error) => {
            frame.render_widget(
                    Paragraph::new(i18n::t(&format!(
                        "Unable to load mailbox\n\n{error}\n\nCheck the path and permissions, then press r to retry."
                    )))
                    .style(theme::text())
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                    content,
                );
        }
        MailboxState::Ready => {}
    }
}

pub(crate) fn status_line(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!(" {key} "), theme::keycap()));
        spans.push(Span::styled(format!(" {}", i18n::t(label)), theme::muted()));
    }
    Line::from(spans)
}

pub(crate) fn action_line(message: &str, undo: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(" {} ", i18n::t(message)),
        theme::title(true),
    )];
    if undo {
        spans.push(Span::styled(" u ", theme::keycap()));
        spans.push(Span::styled(
            format!(" {}", i18n::t("undo")),
            theme::muted(),
        ));
    }
    spans.push(Span::styled(" esc ", theme::keycap()));
    spans.push(Span::styled(
        format!(" {}", i18n::t("dismiss")),
        theme::muted(),
    ));
    Line::from(spans)
}

pub(crate) fn draw_no_matches(frame: &mut Frame, area: Rect, folder: &str, folder_empty: bool) {
    let block = Block::bordered()
        .border_style(theme::border(false))
        .title_style(theme::title(false))
        .title(i18n::t(" workspace "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let message = if folder_empty {
        format!("No messages in {folder}\n\nPress g to choose another folder.")
    } else {
        "No matching conversations\n\nChange the filter or press esc to clear search.".into()
    };
    let message = i18n::t(&message);
    frame.render_widget(
        Paragraph::new(message)
            .style(theme::muted())
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(3) / 2,
            inner.width,
            3.min(inner.height),
        ),
    );
}

pub(crate) const HELP: &[&str] = &[
    "navigation",
    "  j/k or arrows  select     tab  switch pane",
    "  pgup/pgdn home/end  jump",
    "  reading: space  page   n/N  next/previous conversation",
    "  enter          open / resume draft     esc  close",
    "",
    "mouse",
    "  click         select · click again or double-click to open",
    "  right-click   actions for a message (inbox, map, reader)",
    "  middle-click  read/unread        wheel  scroll anywhere",
    "  drag a pane border to resize it  click a hint to press it",
    "  click the inbox title for folders, links and files to open",
    "",
    "message actions",
    "  m  read/unread     s  star/unstar",
    "  a  archive         d  move to Trash",
    "  d d  in Trash: delete permanently",
    "  M  move to another folder     J  spam / not spam",
    "  u  undo            c  compose",
    "  r  reply           A  reply all   F  forward",
    "  l  open links       v  attachments",
    "  I  load remote images     O  open in the browser",
    "",
    "reader window",
    "  f  full screen      0  default size     t  switch colours",
    "  +/-  bigger/smaller    ctrl+arrows  resize    alt+arrows  move",
    "  drag the title to move it, the left or bottom edge or ◢ to resize",
    "  scrolling: wheel · drag the page · drag or click the scroll bar",
    "    ←/→ or shift+wheel  sideways     ctrl+d/ctrl+u  half a page",
    "  double-click the title for full screen",
    "  D  download a large message that arrived as headers",
    "  U  unsubscribe from a mailing list",
    "  x  retry selected Outbox message",
    "  z  resolve unconfirmed Outbox delivery",
    "",
    "inbox",
    "  /  search      g  folders (n new · e rename · x delete)",
    "     from: to: subject: body: in: in:all is:unread is:starred",
    "     has:attachment before: after: on: (2026-03-14, 2026-03, 7d, 2w)",
    "     \"exact phrase\" -exclude      ctrl+r  search the server",
    "  1/2/3  filters",
    "  p  accounts (a add · e edit · l sign in · x remove)",
    "",
    "compose",
    "  tab  next field      shift+tab  previous field",
    "  left/right home/end  move    ctrl+w  delete word",
    "  ctrl+u  delete to start      ctrl+k  delete to end",
    "  space on Format or Security  change it",
    "  enter on Body  $EDITOR     ctrl+e on Body  built-in editor",
    "  ctrl+s  review             ctrl+d  save draft",
    "",
    "commands",
    "  :reload                 :filter unread",
    "  :sync                   sync every account now",
    "  :upload                 send local mail kept off the server",
    "  :search server          search the server for older mail",
    "  :folder new             create a folder",
    "  :mark read              mark the whole folder read",
    "  :accounts               :account add / :account edit",
    "  :login                  enter an account password",
    "  :open maildir PATH      :compose  :reply  :quit",
    "  :reply all              :forward",
    "  :patch apply [DIR]      apply the patch series with git am",
    "  :patch save / :patch b4 save it as mbox / let b4 apply it",
    "  :lore ID                open a thread from lore.kernel.org",
    "",
    "config",
    "  [keys] archive = \"e\"     [theme] preset = \"light\"",
    "  [crypto] sign = true     notifications = false",
];

pub(crate) fn draw_overlay(
    frame: &mut Frame,
    overlay: &mut Overlay,
    cursor_back: usize,
) -> mouse::OverlayGeometry {
    let (width, height) = match overlay {
        Overlay::AccountSetup(form) => (
            76,
            if form.mode == account_setup::Mode::Login {
                12
            } else {
                22
            },
        ),
        Overlay::DeliveryReview { .. } => (72, 12),
        Overlay::Help { .. } => (66, 23),
        Overlay::Command(_) | Overlay::Prompt { .. } => (64, 3),
        Overlay::Unsubscribe { .. } => (64, 9),
        Overlay::Contacts { .. } => (70, 20),
        Overlay::LinkWarning { warnings, .. } => (72, 8 + 2 * warnings.len() as u16),
        Overlay::DraftReady { summary, .. } => (68, 11 + summary.warnings.len() as u16),
        Overlay::ComposeClose(_) => (58, 7),
        Overlay::Folders { items, .. } => (50, (items.len() as u16 + 2).min(18)),
        Overlay::Links { items, .. } => (72, (items.len() as u16 + 2).min(16)),
        Overlay::Attachments { items, .. } => (64, (items.len() as u16 + 2).min(16)),
        Overlay::Accounts { items, .. } => (64, (items.len() as u16 + 2).min(16)),
        Overlay::Compose(_) => (78, 21),
        Overlay::BodyEditor { .. } => (100, 40),
        Overlay::Menu { items, .. } => (
            items
                .iter()
                .map(|item| i18n::t(&item.label).chars().count() as u16 + 8)
                .max()
                .unwrap_or(20)
                .max(22),
            items.len() as u16 + 2,
        ),
    };
    let screen = frame.area();
    let (width, height) = (width.min(screen.width), height.min(screen.height));
    let area = match overlay {
        // The menu opens at the pointer and stays on screen.
        Overlay::Menu { x, y, .. } => Rect::new(
            (*x).min(screen.right().saturating_sub(width)),
            if *y + height > screen.bottom() {
                y.saturating_sub(height)
            } else {
                *y
            },
            width,
            height,
        ),
        _ => Rect::new(
            screen.x + screen.width.saturating_sub(width) / 2,
            screen.y + screen.height.saturating_sub(height) / 2,
            width,
            height,
        ),
    };
    let mut geometry = mouse::OverlayGeometry {
        area,
        ..Default::default()
    };
    frame.render_widget(Clear, area);
    match overlay {
        Overlay::AccountSetup(form) => form.draw(frame, area),
        Overlay::DeliveryReview { delivery } => {
            let text = format!(
                "{}\n\nCheck the receiving mailbox or sender logs first.\nRetrying without checking may send a duplicate.\n\ns  I confirmed delivery · save to Sent only\nr  I confirmed no delivery · retry sending\nesc  leave unchanged",
                delivery.mail.subject
            );
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: true })
                    .block(Block::bordered().title(i18n::t(" resolve unconfirmed delivery "))),
                area,
            );
        }
        Overlay::Help { scroll } => {
            let text = HELP
                .iter()
                .map(|line| i18n::t(line))
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(text)
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(i18n::t(" help · esc to close "))
                            .bg(theme::surface()),
                    )
                    .scroll((*scroll, 0))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        Overlay::Command(input) => {
            frame.render_widget(
                Paragraph::new(cursor_line(":", input, cursor_back))
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(i18n::t(" command "))
                            .bg(theme::surface()),
                    ),
                area,
            );
        }
        Overlay::DraftReady { summary, .. } => {
            let cc = if summary.cc.is_empty() {
                String::new()
            } else {
                format!("\nCc: {}", summary.cc)
            };
            let bcc = if summary.bcc.is_empty() {
                String::new()
            } else {
                format!("\nBcc: {}", summary.bcc)
            };
            let mut lines: Vec<Line> = format!(
                "To: {}{cc}{bcc}\nSubject: {}\nFormat: {} · Attachments: {}",
                summary.to,
                summary.subject,
                summary.format.label(),
                summary.attachments.len()
            )
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect();
            lines.extend(
                summary
                    .warnings
                    .iter()
                    .map(|warning| Line::styled(format!("! {warning}"), theme::title(true))),
            );
            lines.push(Line::default());
            lines.push(Line::from(
                " s  Send     e  Edit     d  Save draft     esc  Back",
            ));
            frame.render_widget(
                Paragraph::new(lines)
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(i18n::t(" choose an action "))
                            .bg(theme::surface()),
                    )
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::ComposeClose(_) => {
            frame.render_widget(
                Paragraph::new(
                    "Close this message?\n\n s  Save draft     d  Discard message     esc  Continue editing",
                )
                .style(theme::text())
                .alignment(Alignment::Center)
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(i18n::t(" unsaved message "))
                        .bg(theme::surface()),
                )
                .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::Contacts {
            selected,
            filter,
            items,
        } => {
            let matching = contacts_matching(items, filter);
            let rows: Vec<ListItem> = matching
                .iter()
                .map(|contact| {
                    let name = if contact.name.is_empty() {
                        "-"
                    } else {
                        contact.name.as_str()
                    };
                    ListItem::new(format!("{name:<28} {}", contact.email))
                })
                .collect();
            let count = rows.len();
            let list = List::new(rows)
                .style(theme::text())
                .highlight_symbol("▌ ")
                .highlight_style(theme::selection())
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(cursor_line(" contacts · find: ", filter, cursor_back))
                        .bg(theme::surface()),
                );
            let mut state =
                ListState::default().with_selected(Some((*selected).min(count.saturating_sub(1))));
            frame.render_stateful_widget(list, area, &mut state);
            geometry.list = Some((area.y + 1, state.offset(), count));
        }
        Overlay::LinkWarning { url, warnings } => {
            let mut lines: Vec<Line> = warnings
                .iter()
                .map(|warning| {
                    Line::styled(
                        format!("⚠ {warning}"),
                        ratatui::style::Style::default().fg(theme::error_color()),
                    )
                })
                .collect();
            lines.push(Line::default());
            lines.push(Line::styled(format!("Goes to: {url}"), theme::text()));
            lines.push(Line::default());
            lines.push(Line::styled(
                " o  Open anyway     y  Copy link     esc  Cancel",
                theme::muted(),
            ));
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_style(ratatui::style::Style::default().fg(theme::error_color()))
                        .title_style(theme::title(true))
                        .title(i18n::t(" check this link "))
                        .bg(theme::surface()),
                ),
                area,
            );
        }
        Overlay::Unsubscribe { unsubscribe, .. } => {
            let method = if unsubscribe.one_click && unsubscribe.https().is_some() {
                "Termail sends a one-click request to the sender. No page opens."
            } else if unsubscribe.mailto().is_some() {
                "Termail prepares an unsubscribe email for you to review and send."
            } else {
                "The sender's unsubscribe page opens in the browser."
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "Leave this mailing list?\n\n{method}\n\n y  Unsubscribe     esc  Keep"
                ))
                .style(theme::text())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(i18n::t(" unsubscribe "))
                        .bg(theme::surface()),
                ),
                area,
            );
        }
        Overlay::Prompt { kind, input } => {
            let title = match kind {
                PromptKind::NewFolder => " new folder ".to_owned(),
                PromptKind::RenameFolder(name) => format!(" rename {name} "),
            };
            frame.render_widget(
                Paragraph::new(cursor_line("", input, cursor_back))
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(i18n::t(&title))
                            .bg(theme::surface()),
                    ),
                area,
            );
        }
        Overlay::Folders {
            selected,
            items,
            moving,
            deleting,
        } => {
            let count = items.len();
            let rows: Vec<ListItem> = items
                .iter()
                .map(|folder| {
                    ListItem::new(format!(
                        "{:<20} {:>3} unread  {:>3} total",
                        folder.name, folder.unread, folder.total
                    ))
                })
                .collect();
            let list = List::new(rows)
                .style(theme::text())
                .highlight_symbol("▌ ")
                .highlight_style(theme::selection())
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(i18n::t(if *moving {
                            " move to "
                        } else if *deleting {
                            " press x again to delete the folder and its mail "
                        } else {
                            " folders · n new · e rename · x delete "
                        }))
                        .bg(theme::surface()),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
            geometry.list = Some((area.y + 1, state.offset(), count));
        }
        Overlay::Links { selected, items } => {
            let count = items.len();
            let rows: Vec<ListItem> = items
                .iter()
                .enumerate()
                .map(|(index, link)| ListItem::new(format!("{}. {link}", index + 1)))
                .collect();
            let list = List::new(rows)
                .style(theme::text())
                .highlight_symbol("▌ ")
                .highlight_style(theme::selection())
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(i18n::t(" links · enter to open · y to copy "))
                        .bg(theme::surface()),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
            geometry.list = Some((area.y + 1, state.offset(), count));
        }
        Overlay::Attachments { selected, items } => {
            let count = items.len();
            let rows: Vec<ListItem> = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    ListItem::new(format!(
                        "{}. {} · {} · {} bytes",
                        index + 1,
                        item.name,
                        item.media_type,
                        item.size
                    ))
                })
                .collect();
            let list = List::new(rows)
                .style(theme::text())
                .highlight_symbol("▌ ")
                .highlight_style(theme::selection())
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(i18n::t(" attachments · s save · o open "))
                        .bg(theme::surface()),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
            geometry.list = Some((area.y + 1, state.offset(), count));
        }
        Overlay::Accounts {
            selected,
            items,
            removing,
        } => {
            let count = items.len();
            let rows: Vec<ListItem> = items
                .iter()
                .map(|item| ListItem::new(item.label.clone()))
                .collect();
            let list = List::new(rows)
                .style(theme::text())
                .highlight_symbol("▌ ")
                .highlight_style(theme::selection())
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .title_style(theme::title(true))
                        .title(i18n::t(if *removing {
                            " press x again to remove · mail stays on disk "
                        } else {
                            " accounts · a add · e edit · l sign in · x remove "
                        }))
                        .bg(theme::surface()),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
            geometry.list = Some((area.y + 1, state.offset(), count));
        }
        Overlay::Compose(state) => {
            let (fields, body) = draw_compose(frame, area, state, cursor_back);
            geometry.fields = fields;
            geometry.body = Some(body);
        }
        Overlay::Menu {
            items, selected, ..
        } => {
            let rows: Vec<ListItem> = items
                .iter()
                .map(|item| {
                    let key = match item.key.code {
                        KeyCode::Char(character) => character.to_string(),
                        KeyCode::Enter => "enter".into(),
                        KeyCode::Esc => "esc".into(),
                        _ => String::new(),
                    };
                    let label = i18n::t(&item.label);
                    let width = usize::from(area.width.saturating_sub(4));
                    let gap = width.saturating_sub(label.chars().count() + key.chars().count() + 1);
                    ListItem::new(Line::from(vec![
                        Span::raw(format!(" {label}")),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(key, theme::muted()),
                    ]))
                })
                .collect();
            let list = List::new(rows)
                .style(theme::text())
                .highlight_style(theme::selection())
                .block(
                    Block::bordered()
                        .border_style(theme::border(true))
                        .bg(theme::surface()),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
            geometry.list = Some((area.y + 1, state.offset(), items.len()));
        }
        Overlay::BodyEditor { state, editor } => {
            let title = format!(
                " body · {} ",
                if state.composer.subject.is_empty() {
                    "new message"
                } else {
                    state.composer.subject.as_str()
                }
            );
            editor.draw(frame, area, &title);
        }
    }
    geometry
}

/// Draws the composer. Returns each header row with its field, and the body box.
pub(crate) fn draw_compose(
    frame: &mut Frame,
    area: Rect,
    state: &ComposeState,
    cursor_back: usize,
) -> (Vec<(Rect, usize)>, Rect) {
    let block = Block::bordered()
        .border_style(theme::border(true))
        .title_style(theme::title(true))
        .title(i18n::t(" compose "))
        .bg(theme::surface());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(9),
        Constraint::Length(4),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "{:<8} {}  · {}",
            i18n::t("From"),
            state.composer.from,
            i18n::t("ctrl+o other address")
        ))
        .style(theme::muted()),
        rows[0],
    );
    let values = [
        ("To", state.composer.to.clone()),
        ("Cc", state.composer.cc.clone()),
        ("Bcc", state.composer.bcc.clone()),
        ("Subject", state.composer.subject.clone()),
        ("Format", format!("< {} >", state.composer.format.label())),
        ("Security", format!("< {} >", state.protection.label())),
        ("Files", state.composer.attachments.clone()),
    ];
    let items: Vec<ListItem> = values
        .iter()
        .enumerate()
        .map(|(index, (label, value))| {
            let label = i18n::t(label);
            let editing = index == state.field
                && index != ComposeState::FORMAT_FIELD
                && index != ComposeState::SECURITY_FIELD;
            let hint = state
                .suggestion
                .as_ref()
                .filter(|_| index == state.field)
                .map(|suggestion| {
                    let entry = suggestion
                        .trim_end_matches([',', ' '])
                        .rsplit([',', ';'])
                        .next()
                        .unwrap_or_default()
                        .trim();
                    Span::styled(format!("  ⇥ {entry}"), theme::muted())
                });
            let line = if editing {
                cursor_line(&format!("{label:<8} "), value, cursor_back)
            } else {
                Line::raw(format!("{label:<8} {value}"))
            };
            ListItem::new(Line::from_iter(line.spans.into_iter().chain(hint)))
        })
        .collect();
    let list = List::new(items)
        .style(theme::text())
        .highlight_symbol("▌ ")
        .highlight_style(theme::selection())
        .block(
            Block::bordered()
                .border_style(theme::border(state.field != ComposeState::BODY_FIELD))
                .title_style(theme::title(state.field != ComposeState::BODY_FIELD))
                .title(i18n::t(" headers ")),
        );
    let mut selected = ListState::default()
        .with_selected((state.field != ComposeState::BODY_FIELD).then_some(state.field));
    frame.render_stateful_widget(list, rows[1], &mut selected);
    let body = state.composer.body_stats().unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "$EDITOR\n{} lines · {} chars · press enter to edit",
            body.0, body.1
        ))
        .style(if state.field == ComposeState::BODY_FIELD {
            theme::selection()
        } else {
            theme::text()
        })
        .block(
            Block::bordered()
                .border_style(theme::border(state.field == ComposeState::BODY_FIELD))
                .title_style(theme::title(state.field == ComposeState::BODY_FIELD))
                .title(i18n::t(" body ")),
        ),
        rows[2],
    );
    let feedback = state
        .error
        .as_deref()
        .or(state.autosave.status.as_deref())
        .unwrap_or("Use ; between attachment paths.");
    frame.render_widget(
        Paragraph::new(feedback)
            .style(if state.error.is_some() {
                theme::title(true)
            } else {
                theme::muted()
            })
            .wrap(Wrap { trim: true }),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(
            "tab  move or accept ⇥    shift+tab  back    space  format    enter  edit body\nctrl+s  review    ctrl+d  save draft    esc  close",
        )
        .style(theme::muted()),
        rows[4],
    );
    let fields = (0..ComposeState::BODY_FIELD)
        .filter(|index| (*index as u16) + 2 < rows[1].height)
        .map(|index| {
            (
                Rect::new(
                    rows[1].x + 1,
                    rows[1].y + 1 + index as u16,
                    rows[1].width.saturating_sub(2),
                    1,
                ),
                index,
            )
        })
        .collect();
    (fields, rows[2])
}

/// A text field with a visible cursor `back` characters before its end.
pub(crate) fn cursor_line(prefix: &str, value: &str, back: usize) -> Line<'static> {
    let characters: Vec<char> = value.chars().collect();
    let position = characters.len() - back.min(characters.len());
    let before: String = characters[..position].iter().collect();
    let at = characters
        .get(position)
        .map_or(" ".to_owned(), char::to_string);
    let after: String = characters
        .get(position + 1..)
        .unwrap_or_default()
        .iter()
        .collect();
    Line::from(vec![
        Span::raw(prefix.to_owned()),
        Span::raw(before),
        Span::styled(
            at,
            theme::keycap().add_modifier(ratatui::style::Modifier::REVERSED),
        ),
        Span::raw(after),
    ])
}

pub(crate) fn reader_is_docked(width: u16, reader_open: bool) -> bool {
    reader_open
        && settings::reader().layout == "docked"
        && width >= settings::with(|ui| ui.dock_min_width)
}

/// The smallest reader window, in cells.
pub(crate) const MIN_POPUP: (u16, u16) = (30, 8);

/// Keeps a window inside the screen and at least the smallest size.
pub(crate) fn clamp_rect(rect: Rect, screen: Rect) -> Rect {
    let width = rect
        .width
        .clamp(MIN_POPUP.0.min(screen.width), screen.width);
    let height = rect
        .height
        .clamp(MIN_POPUP.1.min(screen.height), screen.height);
    let x = rect.x.clamp(screen.x, screen.right() - width);
    let y = rect.y.clamp(screen.y, screen.bottom() - height);
    Rect::new(x, y, width, height)
}
