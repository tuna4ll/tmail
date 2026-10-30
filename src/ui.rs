//! Drawing the main screen, overlays and the composer.

use super::*;

impl App {
    pub(crate) fn draw(&mut self, frame: &mut Frame) {
        if self.onboarding {
            frame.render_widget(Paragraph::new("Connect an account\n\nAdd an email account to read and send mail.\n\nenter  Add account    q  Quit")
                .style(theme::text()).alignment(Alignment::Center)
                .block(Block::bordered().title(" termail ").border_style(theme::border(true))), frame.area());
            if let Some(overlay) = &self.overlay {
                draw_overlay(frame, overlay);
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
            frame.render_widget(Paragraph::new(hints), rows[1]);
            if let Some(overlay) = &self.overlay {
                draw_overlay(frame, overlay);
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
                let marker = match (unread, starred) {
                    (true, true) => "●★",
                    (true, false) => "● ",
                    (false, true) => " ★",
                    (false, false) => "  ",
                };
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
                // Results from every folder say where they live.
                let sender = match conversation.mails(&self.mails).last() {
                    Some(mail) if self.built_everywhere => {
                        format!("{sender} · {}", mail_folder(mail))
                    }
                    _ => sender,
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
            format!(" search: {}_ ", self.query)
        } else if !self.query.is_empty() {
            format!(" inbox • {} • /{} ", self.filter.label(), self.query)
        } else if self.filter != InboxFilter::All {
            format!(" inbox • {} ", self.filter.label())
        } else if self.skipped > 0 {
            format!(
                " {} · {folder} • {} skipped ",
                self.account_label, self.skipped
            )
        } else if !self.syncing.is_empty() {
            format!(" {} · {folder} • syncing ", self.account_label)
        } else if inbox_focused {
            format!(" {} · {folder} • active ", self.account_label)
        } else {
            format!(" {} · {folder} ", self.account_label)
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
        if rows[0].width < MIN_SPLIT_WIDTH {
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
                }
            }
        } else {
            let constraints = if reader_is_docked(rows[0].width, self.reader.is_some()) {
                vec![
                    Constraint::Length(INBOX_WIDTH),
                    Constraint::Min(0),
                    Constraint::Length(READER_WIDTH),
                ]
            } else {
                vec![Constraint::Length(30), Constraint::Min(0)]
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
            if areas.len() == 3 {
                docked_reader_area = Some(areas[2]);
            }
        }

        self.list_offset = list_state.offset();

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
        frame.render_widget(Paragraph::new(hints), rows[1]);

        if let Some(reader) = &mut self.reader
            && let Some(mail) = self.mails.iter().find(|mail| mail.id == reader.mail_id())
        {
            if let Some(area) = docked_reader_area {
                reader.draw_docked(frame, area, mail, self.picker.as_ref());
            } else {
                reader.draw(frame, frame.area(), mail, self.picker.as_ref());
            }
        }

        if let Some(overlay) = &self.overlay {
            draw_overlay(frame, overlay);
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
        .title(" termail ");
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
            frame.render_widget(
                FluxSpinner::new(spinner_tick).width(6).color(theme::ACCENT),
                spinner_area,
            );
            frame.render_widget(
                Paragraph::new(format!("Loading {}", source.label()))
                    .style(theme::text())
                    .alignment(Alignment::Center),
                Rect::new(content.x, content.y.saturating_add(2), content.width, 1),
            );
        }
        MailboxState::Empty => {
            frame.render_widget(
                Paragraph::new(
                    "No messages found\n\nPress c to compose or p to switch accounts. New mail appears automatically.",
                )
                .style(theme::text())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
                content,
            );
        }
        MailboxState::Error(error) => {
            frame.render_widget(
                    Paragraph::new(format!(
                        "Unable to load mailbox\n\n{error}\n\nCheck the path and permissions, then press r to retry."
                    ))
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
        spans.push(Span::styled(format!(" {label}"), theme::muted()));
    }
    Line::from(spans)
}

pub(crate) fn action_line(message: &str, undo: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(format!(" {message} "), theme::title(true))];
    if undo {
        spans.push(Span::styled(" u ", theme::keycap()));
        spans.push(Span::styled(" undo", theme::muted()));
    }
    spans.push(Span::styled(" esc ", theme::keycap()));
    spans.push(Span::styled(" dismiss", theme::muted()));
    Line::from(spans)
}

pub(crate) fn draw_no_matches(frame: &mut Frame, area: Rect, folder: &str, folder_empty: bool) {
    let block = Block::bordered()
        .border_style(theme::border(false))
        .title_style(theme::title(false))
        .title(" workspace ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let message = if folder_empty {
        format!("No messages in {folder}\n\nPress g to choose another folder.")
    } else {
        "No matching conversations\n\nChange the filter or press esc to clear search.".into()
    };
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
    "  pgup/pgdn home/end  jump   mouse  wheel and click",
    "  reading: space  page   n/N  next/previous conversation",
    "  enter          open / resume draft     esc  close",
    "",
    "message actions",
    "  m  read/unread     s  star/unstar",
    "  a  archive         d  move to Trash",
    "  d d  in Trash: delete permanently",
    "  M  move to another folder",
    "  u  undo            c  compose",
    "  r  reply           A  reply all   F  forward",
    "  l  open links       v  attachments",
    "  I  load remote images     O  open in the browser",
    "  x  retry selected Outbox message",
    "  z  resolve unconfirmed Outbox delivery",
    "",
    "inbox",
    "  /  search      g  folders      1/2/3  filters",
    "     from: to: subject: body: in: in:all is:unread is:starred",
    "     has:attachment \"exact phrase\" -exclude",
    "  p  accounts (a add · e edit · l sign in · x remove)",
    "",
    "compose",
    "  tab  next field      shift+tab  previous field",
    "  ctrl+w  delete word   ctrl+u  clear field",
    "  arrows/space  format      enter on Body  $EDITOR",
    "  ctrl+s  review             ctrl+d  save draft",
    "",
    "commands",
    "  :reload                 :filter unread",
    "  :sync                   sync every account now",
    "  :mark read              mark the whole folder read",
    "  :accounts               :account add / :account edit",
    "  :login                  enter an account password",
    "  :open maildir PATH      :compose  :reply  :quit",
    "  :reply all              :forward",
];

pub(crate) fn draw_overlay(frame: &mut Frame, overlay: &Overlay) {
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
        Overlay::DraftReady { summary, .. } => (68, 11 + summary.warnings.len() as u16),
        Overlay::ComposeClose(_) => (58, 7),
        Overlay::Folders { items, .. } => (50, (items.len() as u16 + 2).min(18)),
        Overlay::Links { items, .. } => (72, (items.len() as u16 + 2).min(16)),
        Overlay::Attachments { items, .. } => (64, (items.len() as u16 + 2).min(16)),
        Overlay::Accounts { items, .. } => (64, (items.len() as u16 + 2).min(16)),
        Overlay::Compose(_) => (78, 20),
    };
    let screen = frame.area();
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width.min(screen.width)) / 2,
        screen.y + screen.height.saturating_sub(height.min(screen.height)) / 2,
        width.min(screen.width),
        height.min(screen.height),
    );
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
                    .block(Block::bordered().title(" resolve unconfirmed delivery ")),
                area,
            );
        }
        Overlay::Help { scroll } => {
            let text = HELP.join("\n");
            frame.render_widget(
                Paragraph::new(text)
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(" help · esc to close ")
                            .bg(theme::SURFACE),
                    )
                    .scroll((*scroll, 0))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        Overlay::Command(input) => {
            frame.render_widget(
                Paragraph::new(format!(":{input}_"))
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(" command ")
                            .bg(theme::SURFACE),
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
                            .title(" choose an action ")
                            .bg(theme::SURFACE),
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
                        .title(" unsaved message ")
                        .bg(theme::SURFACE),
                )
                .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::Prompt { kind, input } => {
            let title = match kind {
                PromptKind::NewFolder => " new folder ".to_owned(),
                PromptKind::RenameFolder(name) => format!(" rename {name} "),
            };
            frame.render_widget(
                Paragraph::new(format!("{input}_"))
                    .style(theme::text())
                    .block(
                        Block::bordered()
                            .border_style(theme::border(true))
                            .title_style(theme::title(true))
                            .title(title)
                            .bg(theme::SURFACE),
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
                        .title(if *moving {
                            " move to "
                        } else if *deleting {
                            " press x again to delete the folder and its mail "
                        } else {
                            " folders · n new · e rename · x delete "
                        })
                        .bg(theme::SURFACE),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::Links { selected, items } => {
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
                        .title(" links · enter to open · y to copy ")
                        .bg(theme::SURFACE),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::Attachments { selected, items } => {
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
                        .title(" attachments · s save · o open ")
                        .bg(theme::SURFACE),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::Accounts {
            selected,
            items,
            removing,
        } => {
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
                        .title(if *removing {
                            " press x again to remove · mail stays on disk "
                        } else {
                            " accounts · a add · e edit · l sign in · x remove "
                        })
                        .bg(theme::SURFACE),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::Compose(state) => draw_compose(frame, area, state),
    }
}

pub(crate) fn draw_compose(frame: &mut Frame, area: Rect, state: &ComposeState) {
    let block = Block::bordered()
        .border_style(theme::border(true))
        .title_style(theme::title(true))
        .title(" compose ")
        .bg(theme::SURFACE);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!("From     {}", state.composer.from)).style(theme::muted()),
        rows[0],
    );
    let values = [
        ("To", state.composer.to.clone()),
        ("Cc", state.composer.cc.clone()),
        ("Bcc", state.composer.bcc.clone()),
        ("Subject", state.composer.subject.clone()),
        ("Format", format!("< {} >", state.composer.format.label())),
        ("Files", state.composer.attachments.clone()),
    ];
    let items: Vec<ListItem> = values
        .iter()
        .enumerate()
        .map(|(index, (label, value))| {
            let cursor = if index == state.field && index != ComposeState::FORMAT_FIELD {
                "_"
            } else {
                ""
            };
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
            ListItem::new(Line::from_iter(
                [Span::raw(format!("{label:<8} {value}{cursor}"))]
                    .into_iter()
                    .chain(hint),
            ))
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
                .title(" headers "),
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
                .title(" body "),
        ),
        rows[2],
    );
    let feedback = state
        .error
        .as_deref()
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
}

pub(crate) fn reader_is_docked(width: u16, reader_open: bool) -> bool {
    reader_open && width >= MIN_DOCK_WIDTH
}
