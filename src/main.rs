mod account_setup;
mod complete;
mod config;
mod delivery;
mod doctor;
mod draft;
#[cfg(test)]
mod fake_imap;
mod imap_client;
mod log;
mod mail;
mod mailbox;
mod message;
mod outbox;
mod remote;
mod sender;
mod storage;
mod theme;
mod widgets;

use config::Config;
use mail::{Attachment, Conversation, Mail, group_conversations_where};
use mailbox::{LoadError, LoadReport, MailboxSource, MaildirChange};
use widgets::reader::Reader;
use widgets::workspace::Workspace;

use std::{
    ffi::OsString,
    fs,
    io::stdout,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
};
use directories::BaseDirs;
use futures::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Stylize,
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
    },
};
use ratatui_spinner::FluxSpinner;
use tokio::{sync::mpsc, time::interval};

const MIN_SPLIT_WIDTH: u16 = 78;
const MIN_DOCK_WIDTH: u16 = 112;
const INBOX_WIDTH: u16 = 28;
const READER_WIDTH: u16 = 44;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let Some(options) = parse_options(std::env::args_os().skip(1))? else {
        print_help();
        return Ok(());
    };
    log::init(options.log.clone())?;
    mailbox::prune_caches();
    let config = Config::load(options.config_path.as_deref())?;
    if options.doctor {
        if !doctor::run(&config) {
            std::process::exit(1);
        }
        return Ok(());
    }
    let needs_setup = options.source.is_none() && config.maildir.is_none();
    let explicit_source = options.source.is_some();
    let source = options
        .source
        .or_else(|| config.maildir.clone().map(MailboxSource::Maildir))
        .unwrap_or(MailboxSource::Demo);

    if config.remote.is_some()
        && let Some(root) = &config.maildir
    {
        imap_client::initialize(root).map_err(color_eyre::eyre::Error::msg)?;
    }
    for account in &config.accounts {
        if account.remote.is_some()
            && let Some(root) = &account.maildir
        {
            imap_client::initialize(root).map_err(color_eyre::eyre::Error::msg)?;
        }
    }

    let terminal = ratatui::init();

    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;

    let mut app = App::new(source, config);
    // Ask the terminal which graphics protocol it speaks; half blocks work everywhere.
    app.picker = Some(
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks()),
    );
    app.prepare_accounts(needs_setup, explicit_source);
    let result = app.run(terminal).await;

    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;

    ratatui::restore();

    result?;
    // Sync and IDLE workers hold blocking sockets; their state is saved atomically, so
    // there is nothing to wait for.
    std::process::exit(0)
}

pub struct App {
    running: bool,
    selected: usize,
    mails: Vec<Mail>,
    folders: Vec<String>,
    contacts: Vec<String>,
    conversations: Vec<Conversation>,
    workspace: Option<Workspace>,
    reader: Option<Reader>,
    focus: Pane,
    source: MailboxSource,
    mailbox_state: MailboxState,
    load_tx: mpsc::UnboundedSender<MailboxLoad>,
    load_rx: mpsc::UnboundedReceiver<MailboxLoad>,
    load_generation: u64,
    delivery_tx: mpsc::UnboundedSender<String>,
    delivery_rx: mpsc::UnboundedReceiver<String>,
    sending: bool,
    sync_tx: mpsc::UnboundedSender<(String, Result<imap_client::Synced, String>)>,
    sync_rx: mpsc::UnboundedReceiver<(String, Result<imap_client::Synced, String>)>,
    syncing: std::collections::HashSet<String>,
    manual_sync: bool,
    last_sync: std::collections::HashMap<String, std::time::Instant>,
    watch_tx: mpsc::UnboundedSender<String>,
    watch_rx: mpsc::UnboundedReceiver<String>,
    watchers: std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Draws pictures with the terminal's graphics protocol, when one was detected.
    picker: Option<ratatui_image::picker::Picker>,
    image_tx: mpsc::UnboundedSender<(String, usize, Option<Vec<u8>>)>,
    image_rx: mpsc::UnboundedReceiver<(String, usize, Option<Vec<u8>>)>,
    /// Terminal graphics are not erased by ordinary redraws, so moving them needs a
    /// full clear.
    clear_screen: bool,
    /// Where the inbox list was drawn and how far it is scrolled, for mouse clicks.
    inbox_area: Option<Rect>,
    list_offset: usize,
    setup_tx: mpsc::UnboundedSender<SetupResult>,
    setup_rx: mpsc::UnboundedReceiver<SetupResult>,
    setup_generation: u64,
    checking_account: bool,
    setup_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    onboarding: bool,
    spinner_tick: u64,
    skipped: usize,
    query: String,
    searching: bool,
    filter: InboxFilter,
    undo: Option<UndoAction>,
    confirm_delete: Option<String>,
    notice: Option<String>,
    mailbox_fingerprint: u64,
    pending_editor: Option<ComposeState>,
    overlay: Option<Overlay>,
    config: Config,
    active_folder: String,
    account_label: String,
}

struct MailboxLoad {
    generation: u64,
    fingerprint: Option<u64>,
    result: Result<LoadReport, LoadError>,
}

struct SetupResult {
    request: u64,
    account: config::Account,
    result: Result<(), String>,
}

struct UndoAction {
    change: Option<MaildirChange>,
    demo_mails: Option<Vec<Mail>>,
    label: String,
}

#[derive(Debug)]
enum MailboxState {
    Loading,
    Ready,
    Empty,
    Error(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InboxFilter {
    All,
    Unread,
    Starred,
}

enum Overlay {
    AccountSetup(Box<account_setup::Form>),
    DeliveryReview {
        delivery: Box<delivery::Delivery>,
    },
    Help {
        scroll: u16,
    },
    Command(String),
    DraftReady {
        state: ComposeState,
        summary: draft::Summary,
    },
    ComposeClose(ComposeState),
    Folders {
        selected: usize,
        items: Vec<FolderItem>,
        /// Picking a destination for the active message instead of a folder to view.
        moving: bool,
    },
    Links {
        selected: usize,
        items: Vec<String>,
    },
    Attachments {
        selected: usize,
        items: Vec<Attachment>,
    },
    Accounts {
        selected: usize,
        items: Vec<AccountItem>,
        removing: bool,
    },
    Compose(ComposeState),
}

struct ComposeState {
    composer: draft::Composer,
    field: usize,
    error: Option<String>,
    /// The account to send from when composing in the all-accounts view.
    account: Option<usize>,
    /// The focused field's value with its last entry completed; tab accepts it.
    suggestion: Option<String>,
}

impl ComposeState {
    const FIELD_COUNT: usize = 7;
    const FORMAT_FIELD: usize = 4;
    const BODY_FIELD: usize = 6;

    fn value_mut(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.composer.to),
            1 => Some(&mut self.composer.cc),
            2 => Some(&mut self.composer.bcc),
            3 => Some(&mut self.composer.subject),
            5 => Some(&mut self.composer.attachments),
            _ => None,
        }
    }

    fn move_next(&mut self) {
        self.field = (self.field + 1) % Self::FIELD_COUNT;
        self.error = None;
    }

    fn move_previous(&mut self) {
        self.field = self.field.checked_sub(1).unwrap_or(Self::FIELD_COUNT - 1);
        self.error = None;
    }
}

#[derive(Clone)]
struct FolderItem {
    name: String,
    unread: usize,
    total: usize,
}

#[derive(Clone)]
struct AccountItem {
    label: String,
    index: Option<usize>,
}

impl InboxFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unread => "unread",
            Self::Starred => "starred",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Pane {
    Inbox,
    Workspace,
}

impl App {
    pub fn new(source: MailboxSource, config: Config) -> Self {
        let (load_tx, load_rx) = mpsc::unbounded_channel();
        let (delivery_tx, delivery_rx) = mpsc::unbounded_channel();
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();
        let (watch_tx, watch_rx) = mpsc::unbounded_channel();
        let (image_tx, image_rx) = mpsc::unbounded_channel();
        let (setup_tx, setup_rx) = mpsc::unbounded_channel();
        let account_label = config
            .accounts
            .get(config.active_account_index())
            .map(|account| account.label())
            .or_else(|| config.identity())
            .unwrap_or_else(|| "local".into());
        let mut app = Self {
            running: false,
            selected: 0,
            mails: Vec::new(),
            folders: Vec::new(),
            contacts: Vec::new(),
            conversations: Vec::new(),
            workspace: None,
            reader: None,
            focus: Pane::Inbox,
            source,
            mailbox_state: MailboxState::Loading,
            load_tx,
            load_rx,
            load_generation: 0,
            delivery_tx,
            delivery_rx,
            sending: false,
            sync_tx,
            sync_rx,
            syncing: Default::default(),
            manual_sync: false,
            last_sync: Default::default(),
            watch_tx,
            watch_rx,
            watchers: Default::default(),
            picker: None,
            image_tx,
            image_rx,
            clear_screen: false,
            inbox_area: None,
            list_offset: 0,
            setup_tx,
            setup_rx,
            setup_generation: 0,
            checking_account: false,
            setup_cancel: None,
            onboarding: false,
            spinner_tick: 0,
            skipped: 0,
            query: String::new(),
            searching: false,
            filter: InboxFilter::All,
            undo: None,
            confirm_delete: None,
            notice: None,
            mailbox_fingerprint: 0,
            pending_editor: None,
            overlay: None,
            config,
            active_folder: "INBOX".into(),
            account_label,
        };
        app.reload();
        app
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> color_eyre::Result<()> {
        self.running = true;
        let mut animation = interval(Duration::from_millis(80));
        let mut mailbox_sync = interval(Duration::from_secs(2));
        let mut event_stream = EventStream::new();

        while self.running {
            if std::mem::take(&mut self.clear_screen) {
                terminal.clear()?;
            }
            terminal.draw(|frame| self.draw(frame))?;
            tokio::select! {
                _ = animation.tick() => {
                    if matches!(self.mailbox_state, MailboxState::Loading) {
                        self.spinner_tick = self.spinner_tick.wrapping_add(1);
                    }
                }
                _ = mailbox_sync.tick() => {
                    self.check_mailbox_changes();
                    self.sync_if_due();
                },
                result = self.sync_rx.recv() => {
                    if let Some((account, result)) = result {
                        self.finish_sync(account, result);
                    }
                }
                image = self.image_rx.recv() => {
                    if let Some((mail_id, slot, bytes)) = image
                        && let Some(reader) = &mut self.reader
                        && reader.mail_id() == mail_id
                        && let Some(mail) = self.mails.iter().find(|mail| mail.id == mail_id)
                    {
                        reader.body(mail).finish_remote(slot, bytes);
                        reader.invalidate();
                        self.clear_screen = true;
                    }
                }
                account = self.watch_rx.recv() => {
                    if let Some(account) = account {
                        self.start_account_sync(&account);
                    }
                }
                result = self.load_rx.recv() => {
                    if let Some(result) = result {
                        self.apply_load(result);
                    }
                }
                result = self.setup_rx.recv() => {
                    if let Some(result) = result { self.finish_account_setup(result); }
                }
                result = self.delivery_rx.recv() => {
                    if let Some(notice) = result {
                        self.finish_delivery(notice);
                    }
                }
                event = event_stream.next() => {
                    if let Some(Ok(event)) = event {
                        self.handle_event(event);
                    }
                }
            }
            if let Some(state) = self.pending_editor.take() {
                terminal = self.edit_composer_body(state)?;
            }
        }

        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
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
            let hints = if matches!(self.overlay, Some(Overlay::Help { .. })) {
                status_line(&[("j/k", "scroll"), ("esc", "close help")])
            } else if matches!(self.overlay, Some(Overlay::Command(_))) {
                status_line(&[("type", "command"), ("enter", "run"), ("esc", "cancel")])
            } else if matches!(self.overlay, Some(Overlay::DraftReady { .. })) {
                status_line(&[("s", "send"), ("e", "edit"), ("d", "save draft")])
            } else if matches!(self.overlay, Some(Overlay::ComposeClose(_))) {
                status_line(&[("s", "save draft"), ("d", "discard"), ("esc", "continue")])
            } else if matches!(
                self.overlay,
                Some(Overlay::Folders { .. } | Overlay::Links { .. } | Overlay::Accounts { .. })
            ) {
                status_line(&[("j/k", "select"), ("enter", "open"), ("esc", "cancel")])
            } else if matches!(self.overlay, Some(Overlay::Attachments { .. })) {
                status_line(&[("j/k", "select"), ("s", "save"), ("o", "open")])
            } else if matches!(self.overlay, Some(Overlay::Compose(_))) {
                status_line(&[
                    ("tab", "next field"),
                    ("ctrl+s", "review"),
                    ("esc", "close"),
                ])
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

        let hints = if matches!(self.overlay, Some(Overlay::Help { .. })) {
            status_line(&[("j/k", "scroll"), ("esc", "close help")])
        } else if matches!(self.overlay, Some(Overlay::Command(_))) {
            status_line(&[("type", "command"), ("enter", "run"), ("esc", "cancel")])
        } else if matches!(self.overlay, Some(Overlay::DraftReady { .. })) {
            status_line(&[("s", "send"), ("e", "edit"), ("d", "save draft")])
        } else if matches!(self.overlay, Some(Overlay::ComposeClose(_))) {
            status_line(&[("s", "save draft"), ("d", "discard"), ("esc", "continue")])
        } else if matches!(
            self.overlay,
            Some(Overlay::Folders { .. } | Overlay::Links { .. } | Overlay::Accounts { .. })
        ) {
            status_line(&[("j/k", "select"), ("enter", "open"), ("esc", "cancel")])
        } else if matches!(self.overlay, Some(Overlay::Attachments { .. })) {
            status_line(&[("j/k", "select"), ("s", "save"), ("o", "open")])
        } else if matches!(self.overlay, Some(Overlay::Compose(_))) {
            status_line(&[
                ("tab", "next field"),
                ("ctrl+s", "review"),
                ("esc", "close"),
            ])
        } else if let Some(notice) = &self.notice {
            action_line(notice, self.undo.is_some())
        } else if self.searching {
            status_line(&[("type", "search"), ("enter", "apply"), ("esc", "clear")])
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

    fn handle_event(&mut self, event: Event) {
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

    fn on_key_event(&mut self, key: KeyEvent) {
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
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refresh_inbox();
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.query.push(character);
                    self.refresh_inbox();
                }
                _ => {}
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
                    Pane::Inbox => self
                        .selected_conversation()
                        .and_then(Conversation::latest_mail_id)
                        .map(str::to_owned),
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
    fn handle_inbox_mouse(&mut self, mouse: MouseEvent) -> bool {
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
                    if let Some(mail_id) = self
                        .selected_conversation()
                        .and_then(Conversation::latest_mail_id)
                        .map(str::to_owned)
                    {
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

    fn select_conversation(&mut self, index: usize) {
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
    fn open_adjacent(&mut self, forward: bool) {
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
        if let Some(mail_id) = self
            .selected_conversation()
            .and_then(Conversation::latest_mail_id)
            .map(str::to_owned)
        {
            self.open_mail(&mail_id);
        }
    }

    fn sync_workspace(&mut self) {
        let Some(mail_id) = self
            .selected_conversation()
            .and_then(Conversation::latest_mail_id)
            .map(str::to_owned)
        else {
            return;
        };
        let thread = self.selected_thread();
        if let Some(workspace) = &mut self.workspace {
            workspace.show_conversation(&thread, &mail_id);
        }
    }

    fn sync_inbox_selection(&mut self) {
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

    fn open_mail(&mut self, mail_id: &str) {
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
    fn mark_seen(&mut self, mail_id: &str) {
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

    fn reload(&mut self) {
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

    fn apply_load(&mut self, load: MailboxLoad) {
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

    fn clear_mailbox(&mut self) {
        self.mails.clear();
        self.conversations.clear();
        self.workspace = None;
        self.reader = None;
        self.selected = 0;
        self.skipped = 0;
    }

    fn active_mail_id(&self) -> Option<String> {
        self.reader
            .as_ref()
            .map(|reader| reader.mail_id().to_owned())
            .or_else(|| {
                self.workspace
                    .as_ref()
                    .and_then(Workspace::selected_mail_id)
            })
            .or_else(|| {
                self.selected_conversation()
                    .and_then(Conversation::latest_mail_id)
                    .map(str::to_owned)
            })
    }

    fn toggle_read(&mut self) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let Some(mail_id) = self.active_mail_id() else {
            return;
        };
        let Some(index) = self.mails.iter().position(|mail| mail.id == mail_id) else {
            return;
        };
        let target_seen = self.mails[index].unread;
        let snapshot = matches!(self.source, MailboxSource::Demo).then(|| self.mails.clone());
        match self.source.set_seen(&self.mails[index], target_seen) {
            Ok(change) => {
                if let Some(change) = &change {
                    self.mails[index].source_path = Some(change.after_path().to_path_buf());
                }
                self.mails[index].unread = !target_seen;
                let label = if target_seen {
                    "Marked read"
                } else {
                    "Marked unread"
                };
                self.undo = Some(UndoAction {
                    change,
                    demo_mails: snapshot,
                    label: label.into(),
                });
                self.notice = Some(label.into());
                self.refresh_fingerprint();
                self.rebuild_mailbox(Some(&mail_id));
            }
            Err(error) => self.notice = Some(format!("Unable to update message: {error}")),
        }
    }

    fn toggle_starred(&mut self) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let Some(mail_id) = self.active_mail_id() else {
            return;
        };
        let Some(index) = self.mails.iter().position(|mail| mail.id == mail_id) else {
            return;
        };
        let target = !self.mails[index].starred;
        let snapshot = matches!(self.source, MailboxSource::Demo).then(|| self.mails.clone());
        match self.source.set_starred(&self.mails[index], target) {
            Ok(change) => {
                if let Some(change) = &change {
                    self.mails[index].source_path = Some(change.after_path().to_path_buf());
                }
                self.mails[index].starred = target;
                let label = if target { "Starred" } else { "Removed star" };
                self.undo = Some(UndoAction {
                    change,
                    demo_mails: snapshot,
                    label: label.into(),
                });
                self.notice = Some(label.into());
                self.refresh_fingerprint();
                self.rebuild_mailbox(Some(&mail_id));
            }
            Err(error) => self.notice = Some(format!("Unable to update message: {error}")),
        }
    }

    fn undo_last(&mut self) {
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
        } else if let Some(change) = undo.change {
            match change.undo() {
                Ok(()) => {
                    self.notice = Some(format!("Undid {}", undo.label.to_lowercase()));
                    self.reload();
                }
                Err(error) => self.notice = Some(format!("Unable to undo: {error}")),
            }
        }
    }

    fn move_active_message(&mut self, archive: bool) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let Some(mail_id) = self.active_mail_id() else {
            return;
        };
        let Some(index) = self.mails.iter().position(|mail| mail.id == mail_id) else {
            return;
        };
        if !archive && mail_folder(&self.mails[index]) == "Trash" {
            self.delete_forever(index);
            return;
        }
        let snapshot = matches!(self.source, MailboxSource::Demo).then(|| self.mails.clone());
        let result = if archive {
            self.source.archive(&self.mails[index])
        } else {
            self.source.trash(&self.mails[index])
        };
        match result {
            Ok(change) => {
                if let Some(change) = &change {
                    self.mails[index].source_path = Some(change.after_path().to_path_buf());
                }
                self.mails[index].mailbox = Some(if archive { "Archive" } else { "Trash" }.into());
                self.reader = None;
                let label = if archive {
                    "Archived message"
                } else {
                    "Moved message to Trash"
                };
                self.undo = Some(UndoAction {
                    change,
                    demo_mails: snapshot,
                    label: label.into(),
                });
                self.notice = Some(label.into());
                self.refresh_fingerprint();
                self.rebuild_mailbox(None);
            }
            Err(error) => self.notice = Some(format!("Unable to move message: {error}")),
        }
    }

    /// The messages of the selected conversation, from every folder it spans.
    fn selected_thread(&self) -> Vec<Mail> {
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

    fn delete_forever(&mut self, index: usize) {
        let id = self.mails[index].id.clone();
        if self.confirm_delete.as_deref() != Some(id.as_str()) {
            self.confirm_delete = Some(id);
            self.notice = Some("Press d again to delete this message permanently.".into());
            return;
        }
        self.confirm_delete = None;
        match self.source.delete(&self.mails[index]) {
            Ok(()) => {
                self.mails.remove(index);
                self.reader = None;
                self.undo = None;
                self.notice = Some("Deleted permanently".into());
                self.refresh_fingerprint();
                self.rebuild_mailbox(None);
            }
            Err(error) => self.notice = Some(format!("Unable to delete message: {error}")),
        }
    }

    fn rebuild_mailbox(&mut self, mail_id: Option<&str>) {
        // Threads draw on every folder except the bins and queues, so the map shows
        // replies from Sent next to the Inbox messages they answer.
        let active = self.active_folder.clone();
        let mut conversations = group_conversations_where(&self.mails, |mail| {
            let folder = mail_folder(mail);
            if folder == active {
                Some(0)
            } else if ["Trash", "Junk", "Outbox", "Drafts"].contains(&folder) {
                None
            } else {
                Some(1)
            }
        });
        conversations.retain(|conversation| {
            conversation
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
            .selected_conversation()
            .and_then(Conversation::latest_mail_id)
            .map(|mail_id| Workspace::new(&thread, mail_id));
    }

    fn folder_items(&self) -> Vec<FolderItem> {
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

    fn open_folders(&mut self) {
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

    fn open_move_picker(&mut self) {
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

    fn move_active_to(&mut self, folder: &str) {
        if self.sending {
            self.notice = Some("Wait for the current send before changing messages.".into());
            return;
        }
        let Some(index) = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().position(|mail| mail.id == id))
        else {
            return;
        };
        let snapshot = matches!(self.source, MailboxSource::Demo).then(|| self.mails.clone());
        match self.source.move_to_named(&self.mails[index], folder) {
            Ok(change) => {
                if let Some(change) = &change {
                    self.mails[index].source_path = Some(change.after_path().to_path_buf());
                }
                self.mails[index].mailbox = Some(folder.to_owned());
                self.reader = None;
                let label = format!("Moved message to {folder}");
                self.undo = Some(UndoAction {
                    change,
                    demo_mails: snapshot,
                    label: label.clone(),
                });
                self.notice = Some(label);
                self.refresh_fingerprint();
                self.rebuild_mailbox(None);
            }
            Err(error) => self.notice = Some(format!("Unable to move message: {error}")),
        }
    }

    /// Half-block pictures are ordinary cells that redraw cleanly; graphics protocols
    /// leave pixels behind until the screen is cleared.
    fn uses_halfblocks(&self) -> bool {
        self.picker.as_ref().is_none_or(|picker| {
            picker.protocol_type() == ratatui_image::picker::ProtocolType::Halfblocks
        })
    }

    fn graphics_shown(&self) -> bool {
        !self.uses_halfblocks() && self.reader.as_ref().is_some_and(Reader::shows_images)
    }

    fn open_in_browser(&mut self) {
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

    fn mark_folder_read(&mut self) {
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
    fn load_remote_images(&mut self) {
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

    fn open_links(&mut self) {
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

    fn open_attachments(&mut self) {
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

    fn prepare_accounts(&mut self, needs_setup: bool, explicit_source: bool) {
        if needs_setup {
            self.onboarding = true;
            self.load_generation += 1;
            self.clear_mailbox();
            self.open_account_setup(account_setup::Mode::Add);
        } else if !explicit_source {
            self.prompt_login();
        }
    }

    fn prompt_login(&mut self) {
        if self.overlay.is_none()
            && matches!(&self.source, MailboxSource::Maildir(root) if self.config.maildir.as_ref() == Some(root))
            && self
                .config
                .remote
                .as_ref()
                .is_some_and(|remote| !remote.has_credentials())
        {
            self.open_account_setup(account_setup::Mode::Login);
        }
    }

    fn open_account_setup(&mut self, mode: account_setup::Mode) {
        if self.sending {
            self.notice =
                Some("Wait for sending to finish before changing account settings.".into());
            return;
        }
        if mode != account_setup::Mode::Add && self.config.remote.is_none() {
            self.notice = Some(
                "This is a local account. Use :account add to connect an IMAP account.".into(),
            );
            return;
        }
        self.overlay = Some(Overlay::AccountSetup(Box::new(account_setup::Form::new(
            &self.config,
            mode,
        ))));
    }

    fn handle_account_key(&mut self, mut form: Box<account_setup::Form>, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.setup_generation += 1;
            if let Some(cancel) = &self.setup_cancel {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        if form.busy {
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        let submit = key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('s')
            || key.code == KeyCode::Enter
                && (form.mode == account_setup::Mode::Login || form.field >= 10);
        if submit {
            if self.checking_account {
                form.error =
                    Some("The previous connection check is finishing. Try again shortly.".into());
            } else {
                match form.build() {
                    Ok(account) => {
                        self.setup_generation += 1;
                        form.request = self.setup_generation;
                        form.busy = true;
                        form.error = None;
                        self.checking_account = true;
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        self.setup_cancel = Some(cancel.clone());
                        let request = form.request;
                        let sender = self.setup_tx.clone();
                        tokio::spawn(async move {
                            let checked = account.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                account_setup::verify(&checked, &cancel)
                            })
                            .await
                            .unwrap_or_else(|_| Err("Connection check stopped. Try again.".into()));
                            let _ = sender.send(SetupResult {
                                request,
                                account,
                                result,
                            });
                        });
                    }
                    Err((field, error)) => {
                        form.field = field;
                        form.error = Some(error);
                    }
                }
            }
        } else {
            match key.code {
                KeyCode::Tab | KeyCode::Down | KeyCode::Enter => form.move_field(false),
                KeyCode::BackTab | KeyCode::Up => form.move_field(true),
                KeyCode::Backspace => {
                    if let Some(value) = form.values.get_mut(form.field) {
                        value.pop();
                    }
                }
                KeyCode::Char(' ') if form.field == 10 => form.make_default = !form.make_default,
                KeyCode::Char(' ') if form.field == 11 => form.remember = !form.remember,
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(value) = form.values.get_mut(form.field) {
                        use zeroize::Zeroize;
                        value.zeroize();
                    }
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.input(&character.to_string())
                }
                _ => {}
            }
        }
        self.overlay = Some(Overlay::AccountSetup(form));
    }

    fn finish_account_setup(&mut self, result: SetupResult) {
        self.checking_account = false;
        self.setup_cancel = None;
        if result.request != self.setup_generation {
            return;
        }
        let Some(Overlay::AccountSetup(mut form)) = self.overlay.take() else {
            return;
        };
        if form.request != result.request {
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        form.busy = false;
        let saved = result.result.and_then(|_| {
            let root = result
                .account
                .maildir
                .as_ref()
                .ok_or("Set a local cache directory before connecting.")?;
            imap_client::initialize(root)?;
            if let Some(remote) = &result.account.remote {
                match &remote.session_password {
                    Some(password) if remote.keyring => {
                        remote.store_keyring_password(&password.0)?
                    }
                    _ if !remote.keyring => remote.forget_keyring_password(),
                    _ => {}
                }
            }
            let remembered = self
                .config
                .remote
                .as_ref()
                .is_some_and(|remote| remote.keyring);
            if form.mode == account_setup::Mode::Login
                && result
                    .account
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.keyring != remembered)
                && !self.config.accounts.is_empty()
            {
                let make_default =
                    self.config.default_account.as_deref() == Some(result.account.id.as_str());
                self.config.save_account(result.account, true, make_default)
            } else if form.mode == account_setup::Mode::Login {
                self.config.remote = result.account.remote.clone();
                if let Some(account) = self
                    .config
                    .accounts
                    .iter_mut()
                    .find(|account| account.id == result.account.id)
                {
                    account.remote = result.account.remote;
                }
                Ok(())
            } else {
                self.config.save_account(
                    result.account,
                    form.mode == account_setup::Mode::Edit,
                    form.make_default,
                )
            }
        });
        if let Err(error) = saved {
            form.error = Some(error);
            self.overlay = Some(Overlay::AccountSetup(form));
            return;
        }
        self.onboarding = false;
        self.last_sync.clear();
        self.stop_watchers();
        self.active_folder = "INBOX".into();
        self.query.clear();
        self.filter = InboxFilter::All;
        self.undo = None;
        self.reader = None;
        self.source = MailboxSource::Maildir(self.config.maildir.clone().unwrap());
        self.account_label = self.config.current_account().label();
        self.notice = Some("Account connected".into());
        self.reload();
    }

    fn open_accounts(&mut self) {
        if self.config.accounts.is_empty() {
            self.open_account_setup(if self.config.remote.is_some() {
                account_setup::Mode::Edit
            } else {
                account_setup::Mode::Add
            });
            return;
        }
        let mut items = if self.config.accounts.len() > 1 {
            vec![AccountItem {
                label: "All accounts".into(),
                index: None,
            }]
        } else {
            Vec::new()
        };
        items.extend(
            self.config
                .accounts
                .iter()
                .enumerate()
                .map(|(index, account)| AccountItem {
                    label: account.label(),
                    index: Some(index),
                }),
        );
        let selected = if matches!(self.source, MailboxSource::Unified(_)) {
            0
        } else {
            self.config.active_account_index() + usize::from(self.config.accounts.len() > 1)
        };
        self.overlay = Some(Overlay::Accounts {
            selected,
            items,
            removing: false,
        });
    }

    fn remove_account(&mut self, index: usize) {
        if self.sending {
            self.notice = Some("Wait for sending to finish before removing an account.".into());
            return;
        }
        let Some(account) = self.config.accounts.get(index).cloned() else {
            return;
        };
        match self.config.remove_account(&account.id) {
            Ok(()) => {
                if let Some(remote) = &account.remote
                    && remote.keyring
                {
                    remote.forget_keyring_password();
                }
                self.stop_watchers();
                self.syncing.remove(&account.id);
                self.last_sync.remove(&account.id);
                let root = account
                    .maildir
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                let active = self.config.active_account_index();
                self.select_account(Some(active));
                self.notice = Some(format!(
                    "Removed {}. Its mail stays in {root}.",
                    account.label()
                ));
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn select_account(&mut self, index: Option<usize>) {
        let source = match index {
            None => {
                let maildirs: Vec<(String, PathBuf)> = self
                    .config
                    .accounts
                    .iter()
                    .filter_map(|account| {
                        account
                            .maildir
                            .clone()
                            .map(|path| (account.id.clone(), path))
                    })
                    .collect();
                if maildirs.len() < 2 {
                    self.notice = Some("Configure Maildir paths for at least two accounts.".into());
                    return;
                }
                self.account_label = "all accounts".into();
                MailboxSource::Unified(maildirs)
            }
            Some(index) => {
                let Some(account) = self.config.accounts.get(index) else {
                    return;
                };
                let Some(path) = account.maildir.clone() else {
                    self.notice = Some("Set maildir for this account in config.toml.".into());
                    return;
                };
                self.config.activate(index);
                self.account_label = self.config.accounts[index].label();
                MailboxSource::Maildir(path)
            }
        };
        self.source = source;
        self.active_folder = "INBOX".into();
        self.filter = InboxFilter::All;
        self.query.clear();
        self.undo = None;
        self.reader = None;
        self.reload();
        self.prompt_login();
    }

    fn save_active_attachment(&mut self, index: usize, open: bool) {
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

    fn retry_outbox(&mut self) {
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

    fn review_delivery(&mut self) {
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

    fn delivery_account(
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

    /// Accounts that mirror a server, as (id, remote settings, Maildir).
    fn remote_accounts(&self) -> Vec<(String, remote::RemoteConfig, PathBuf)> {
        if self.config.accounts.is_empty() {
            return self
                .config
                .remote
                .clone()
                .zip(self.config.maildir.clone())
                .map(|(remote, root)| vec![(self.config.current_account().id, remote, root)])
                .unwrap_or_default();
        }
        self.config
            .accounts
            .iter()
            .filter_map(|account| {
                Some((
                    account.id.clone(),
                    account.remote.clone()?,
                    account.maildir.clone()?,
                ))
            })
            .collect()
    }

    /// Whether the open mailbox is one of the configured accounts rather than a demo or
    /// a Maildir opened by path.
    fn uses_accounts(&self) -> bool {
        match &self.source {
            MailboxSource::Demo => false,
            MailboxSource::Unified(_) => true,
            MailboxSource::Maildir(root) => {
                self.config.maildir.as_ref() == Some(root)
                    || self
                        .config
                        .accounts
                        .iter()
                        .any(|account| account.maildir.as_ref() == Some(root))
            }
        }
    }

    fn shows_maildir(&self, root: &Path) -> bool {
        match &self.source {
            MailboxSource::Demo => false,
            MailboxSource::Maildir(path) => path == root,
            MailboxSource::Unified(accounts) => accounts.iter().any(|(_, path)| path == root),
        }
    }

    fn sync_if_due(&mut self) {
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

    fn start_sync(&mut self) {
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

    fn start_account_sync(&mut self, id: &str) {
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

    fn finish_sync(&mut self, id: String, result: Result<imap_client::Synced, String>) {
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
                }) && (error.contains("Sign in") || error.contains("login failed"))
                {
                    self.prompt_login();
                }
            }
        }
    }

    /// Keeps an IDLE connection open so new mail starts a sync right away.
    fn watch_account(&mut self, id: String, remote: remote::RemoteConfig) {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.watchers.insert(id.clone(), stop.clone());
        let sender = self.watch_tx.clone();
        std::thread::spawn(move || {
            let mut backoff = 30;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match imap_client::wait_for_change(&remote, Duration::from_secs(25 * 60), &stop) {
                    Ok(changed) => {
                        backoff = 30;
                        if changed && sender.send(id.clone()).is_err() {
                            return;
                        }
                    }
                    Err(error) if error.contains("not supported") => return,
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

    fn stop_watchers(&mut self) {
        for (_, stop) in self.watchers.drain() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn start_delivery(&mut self, mut delivery: delivery::Delivery) {
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

    fn finish_delivery(&mut self, notice: String) {
        self.sending = false;
        self.notice = Some(notice);
        self.reload();
    }

    fn quit(&mut self) {
        if self.sending || self.checking_account {
            self.notice =
                Some("Wait for the current connection or send to finish before quitting.".into());
        } else {
            self.running = false;
        }
    }

    fn refresh_fingerprint(&mut self) {
        if let Ok(fingerprint) = self.source.fingerprint() {
            self.mailbox_fingerprint = fingerprint;
        }
    }

    fn check_mailbox_changes(&mut self) {
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

    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
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
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    input.push(character);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Folders {
                selected,
                items,
                moving,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                    *selected += 1;
                    self.overlay = Some(overlay);
                }
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                    self.overlay = Some(overlay);
                }
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
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                    *selected += 1;
                    self.overlay = Some(overlay);
                }
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                    self.overlay = Some(overlay);
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
            Overlay::Attachments { selected, items } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                    *selected += 1;
                    self.overlay = Some(overlay);
                }
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                    self.overlay = Some(overlay);
                }
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
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                    *selected += 1;
                    *removing = false;
                    self.overlay = Some(overlay);
                }
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                    *removing = false;
                    self.overlay = Some(overlay);
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

    fn run_command(&mut self, command: &str) {
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

    fn start_draft(&mut self, start: draft::Start) {
        let reply = start != draft::Start::New;
        if matches!(self.source, MailboxSource::Demo) {
            self.notice = Some("Open a Maildir mailbox to save drafts.".into());
            return;
        }
        let mail = reply
            .then(|| self.active_mail_id())
            .flatten()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id));
        if reply && mail.is_none() {
            self.notice = Some("Select a message first.".into());
            return;
        }
        let account = match &self.source {
            MailboxSource::Unified(_) => Some(
                mail.and_then(|mail| self.account_index(mail))
                    .unwrap_or_else(|| self.config.active_account_index()),
            ),
            _ => None,
        };
        let (_, config) = self.compose_context(account);
        // Unified views prefix message IDs with the account; replies need the real ones.
        let original = mail.map(|mail| {
            let mut mail = mail.clone();
            if let Some(prefix) = mail.account.as_ref().map(|account| format!("{account}:")) {
                let strip = |value: &str| value.strip_prefix(&prefix).unwrap_or(value).to_owned();
                mail.id = strip(&mail.id);
                mail.reply_to = mail.reply_to.as_deref().map(strip);
                mail.references = mail.references.iter().map(|id| strip(id)).collect();
            }
            mail
        });
        match draft::Composer::start(
            start,
            original.as_ref(),
            config.identity().as_deref(),
            config.signature.as_deref(),
        ) {
            Ok(composer) => {
                let forward = start == draft::Start::Forward;
                let state = ComposeState {
                    field: if reply && !forward {
                        ComposeState::BODY_FIELD
                    } else {
                        0
                    },
                    composer,
                    error: None,
                    account,
                    suggestion: None,
                };
                if reply && !forward {
                    self.pending_editor = Some(state);
                } else {
                    self.overlay = Some(Overlay::Compose(state));
                }
            }
            Err(error) => self.notice = Some(format!("Unable to create draft: {error}")),
        }
    }

    fn account_index(&self, mail: &Mail) -> Option<usize> {
        let account = mail.account.as_deref()?;
        self.config
            .accounts
            .iter()
            .position(|candidate| candidate.id == account)
    }

    /// The mailbox and settings a draft is saved and sent with.
    fn compose_context(&self, account: Option<usize>) -> (MailboxSource, Config) {
        let mut config = self.config.clone();
        match account {
            Some(index) if config.activate(index) => (
                config
                    .maildir
                    .clone()
                    .map_or(MailboxSource::Demo, MailboxSource::Maildir),
                config,
            ),
            _ => (self.source.clone(), config),
        }
    }

    fn handle_compose_key(&mut self, mut state: ComposeState, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match state.composer.summary(&base) {
                Ok(summary) => self.overlay = Some(Overlay::DraftReady { state, summary }),
                Err(error) => {
                    state.error = Some(error.to_string());
                    self.overlay = Some(Overlay::Compose(state));
                }
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            self.finish_composer(state, false);
            return;
        }
        match key.code {
            KeyCode::Esc => self.overlay = Some(Overlay::ComposeClose(state)),
            KeyCode::Tab if state.suggestion.is_some() => {
                if let (Some(suggestion), Some(value)) =
                    (state.suggestion.take(), state.value_mut())
                {
                    *value = suggestion;
                }
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Tab | KeyCode::Down => {
                state.move_next();
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::BackTab | KeyCode::Up => {
                state.move_previous();
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if state.field == ComposeState::FORMAT_FIELD =>
            {
                state.composer.format.toggle();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Enter if state.field == ComposeState::BODY_FIELD => {
                self.pending_editor = Some(state);
            }
            KeyCode::Enter if state.field == ComposeState::FORMAT_FIELD => {
                state.composer.format.toggle();
                state.move_next();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Enter => {
                state.move_next();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Backspace => {
                if let Some(value) = state.value_mut() {
                    value.pop();
                }
                state.error = None;
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && state.field != ComposeState::FORMAT_FIELD
                    && state.field != ComposeState::BODY_FIELD =>
            {
                if let Some(value) = state.value_mut() {
                    value.push(character);
                }
                state.error = None;
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            _ => self.overlay = Some(Overlay::Compose(state)),
        }
    }

    fn suggest(&self, state: &mut ComposeState) {
        state.suggestion = match state.field {
            0..=2 => state
                .value_mut()
                .and_then(|value| complete::address(value, &self.contacts)),
            5 => {
                let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                complete::path(&state.composer.attachments, &base)
            }
            _ => None,
        };
    }

    fn edit_composer_body(
        &mut self,
        mut state: ComposeState,
    ) -> color_eyre::Result<DefaultTerminal> {
        execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
        ratatui::restore();
        let result = self.run_editor(&state.composer.body_path);
        let terminal = ratatui::init();
        execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;

        if let Err(error) = result {
            state.error = Some(format!(
                "Unable to open $EDITOR: {error}. Check editor in config.toml."
            ));
        }
        self.overlay = Some(Overlay::Compose(state));
        Ok(terminal)
    }

    fn run_editor(&self, path: &Path) -> color_eyre::Result<()> {
        let command = self.config.editor_command();
        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| color_eyre::eyre::eyre!("editor command is empty"))?;
        let status = Command::new(program).args(arguments).arg(path).status()?;
        if !status.success() {
            return Err(color_eyre::eyre::eyre!("editor exited with {status}"));
        }
        Ok(())
    }

    fn finish_composer(&mut self, mut state: ComposeState, send: bool) {
        if send && self.sending {
            state.error = Some("Wait for the current send to finish.".into());
            self.overlay = Some(Overlay::Compose(state));
            return;
        }
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let contents = if send {
            state.composer.message(&base)
        } else {
            state.composer.draft_message(&base)
        };
        let contents = match contents {
            Ok(contents) => contents,
            Err(error) => {
                state.error = Some(error.to_string());
                self.overlay = Some(Overlay::Compose(state));
                return;
            }
        };
        let (source, config) = self.compose_context(state.account);
        if send {
            let delivery = match delivery::Delivery::queue(
                source.clone(),
                config.delivery_identity(),
                config.send_command.clone(),
                &contents,
            ) {
                Ok(delivery) => delivery,
                Err(error) => {
                    state.error = Some(format!(
                        "Unable to queue message: {error}. Nothing was sent."
                    ));
                    self.overlay = Some(Overlay::Compose(state));
                    return;
                }
            };
            if let Some(original) = &state.composer.original_draft
                && let Err(error) = fs::remove_file(original)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                self.notice = Some(format!(
                    "Message queued but not sent: unable to remove the original draft: {error}. Retry from Outbox."
                ));
                self.reload();
                return;
            }
            self.mark_answered(state.composer.replying_to(), state.account);
            self.start_delivery(delivery);
        } else {
            let saved = match &state.composer.original_draft {
                Some(path) if path.exists() => source.update_draft(path, &contents),
                _ => source.save_draft(&contents),
            };
            match saved {
                Ok(_) => {
                    let _ = state.composer.remove_body();
                    self.notice = Some("Draft saved".into());
                }
                Err(error) => {
                    state.error = Some(format!(
                        "Unable to save draft: {error}. Check the Maildir and retry."
                    ));
                    self.overlay = Some(Overlay::Compose(state));
                    return;
                }
            }
        }
        self.reload();
    }

    fn mark_answered(&mut self, id: Option<&str>, account: Option<usize>) {
        let Some(id) = id else {
            return;
        };
        let account = account.and_then(|index| self.config.accounts.get(index));
        let id = match account {
            Some(account) => format!("{}:{id}", account.id),
            None => id.to_owned(),
        };
        if let Some(index) = self.mails.iter().position(|mail| mail.id == id)
            && let Ok(Some(change)) = self.source.set_answered(&self.mails[index])
        {
            self.mails[index].source_path = Some(change.after_path().to_path_buf());
            self.refresh_fingerprint();
        }
    }

    fn visible_conversations(&self) -> Vec<usize> {
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

    fn selected_conversation(&self) -> Option<&Conversation> {
        let index = *self.visible_conversations().get(self.selected)?;
        self.conversations.get(index)
    }

    fn refresh_inbox(&mut self) {
        let count = self.visible_conversations().len();
        self.selected = self.selected.min(count.saturating_sub(1));
        if count > 0 {
            self.sync_workspace();
        }
    }
}

fn draw_mailbox_state(
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

fn status_line(hints: &[(&str, &str)]) -> Line<'static> {
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

fn action_line(message: &str, undo: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(format!(" {message} "), theme::title(true))];
    if undo {
        spans.push(Span::styled(" u ", theme::keycap()));
        spans.push(Span::styled(" undo", theme::muted()));
    }
    spans.push(Span::styled(" esc ", theme::keycap()));
    spans.push(Span::styled(" dismiss", theme::muted()));
    Line::from(spans)
}

fn mail_folder(mail: &Mail) -> &str {
    mail.mailbox.as_deref().unwrap_or("INBOX")
}

fn draw_no_matches(frame: &mut Frame, area: Rect, folder: &str, folder_empty: bool) {
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

const HELP: &[&str] = &[
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
    "     from: to: subject: body: in: is:unread is:starred",
    "     has:attachment \"exact phrase\" -exclude",
    "  p  accounts (a add · e edit · l sign in · x remove)",
    "",
    "compose",
    "  tab  next field      shift+tab  previous field",
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

fn draw_overlay(frame: &mut Frame, overlay: &Overlay) {
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
        Overlay::Command(_) => (64, 3),
        Overlay::DraftReady { .. } => (68, 11),
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
            frame.render_widget(
                Paragraph::new(format!(
                    "To: {}{cc}{bcc}\nSubject: {}\nFormat: {} · Attachments: {}\n\n s  Send     e  Edit     d  Save draft     esc  Back",
                    summary.to,
                    summary.subject,
                    summary.format.label(),
                    summary.attachments.len()
                ))
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
        Overlay::Folders {
            selected,
            items,
            moving,
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
                        .title(if *moving { " move to " } else { " folders " })
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
                        .title(" links · enter to open ")
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

fn draw_compose(frame: &mut Frame, area: Rect, state: &ComposeState) {
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

fn reader_is_docked(width: u16, reader_open: bool) -> bool {
    reader_open && width >= MIN_DOCK_WIDTH
}

fn action_key(key: KeyEvent, expected: char) -> bool {
    matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&expected))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

struct CliOptions {
    source: Option<MailboxSource>,
    config_path: Option<PathBuf>,
    doctor: bool,
    log: Option<PathBuf>,
}

fn parse_options(
    args: impl IntoIterator<Item = OsString>,
) -> color_eyre::Result<Option<CliOptions>> {
    let mut args = args.into_iter();
    let mut source = None;
    let mut config_path = None;
    let mut doctor = false;
    let mut log = None;

    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--maildir") => {
                let path = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--maildir needs a path"))?;
                source = Some(MailboxSource::Maildir(PathBuf::from(path)));
            }
            Some("--config") => {
                let path = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--config needs a path"))?;
                config_path = Some(PathBuf::from(path));
            }
            Some("--log") => {
                let path = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--log needs a path"))?;
                log = Some(PathBuf::from(path));
            }
            Some("--demo") => source = Some(MailboxSource::Demo),
            Some("doctor") => doctor = true,
            Some("--help" | "-h") => return Ok(None),
            Some(value) => return Err(color_eyre::eyre::eyre!("unknown option: {value}")),
            None => return Err(color_eyre::eyre::eyre!("option is not valid utf-8")),
        }
    }

    Ok(Some(CliOptions {
        source,
        config_path,
        doctor,
        log,
    }))
}

fn print_help() {
    println!("termail [doctor] [--config PATH] [--log PATH] [--demo | --maildir PATH]");
    println!();
    println!("  --demo          open the built-in mailbox");
    println!("  --maildir PATH  open a Maildir mailbox");
    println!("  --config PATH   load a config file");
    println!("  --log PATH      append connection errors to a file (or set TERMAIL_LOG)");
    println!("  doctor          check account, Maildir, editor, and sender setup");
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(relative) = value.strip_prefix("~/")
        && let Some(base) = BaseDirs::new()
    {
        return base.home_dir().join(relative);
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    use crate::{draft::Composer, mailbox::MailboxSource};

    use super::{
        ComposeState, MailboxState, Overlay, action_key, draw_mailbox_state, draw_overlay,
        parse_options, reader_is_docked,
    };

    struct TestMailbox(std::path::PathBuf);

    impl TestMailbox {
        fn new() -> Self {
            let root = std::env::temp_dir().join(crate::storage::unique_name("termail-app"));
            for folder in ["cur", "new", "tmp"] {
                std::fs::create_dir_all(root.join(folder)).unwrap();
            }
            Self(root)
        }

        fn app(&self) -> super::App {
            super::App::new(
                MailboxSource::Maildir(self.0.clone()),
                crate::config::Config {
                    email: Some("me@example.test".into()),
                    maildir: Some(self.0.clone()),
                    ..Default::default()
                },
            )
        }
    }

    impl Drop for TestMailbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn finish_load(app: &mut super::App) {
        loop {
            let load = app.load_rx.recv().await.unwrap();
            let current = load.generation == app.load_generation;
            app.apply_load(load);
            if current {
                break;
            }
        }
    }

    #[tokio::test]
    async fn missing_remote_password_keeps_cached_mail_available() {
        let mailbox = TestMailbox::new();
        std::fs::write(
            mailbox.0.join("new/cached"),
            b"From: reader@example.test\nSubject: cached\n\nbody",
        )
        .unwrap();
        let mut app = mailbox.app();
        app.config.remote = Some(crate::remote::tests::Server::new().remote());
        finish_load(&mut app).await;
        app.start_sync();
        assert!(
            matches!(app.overlay, Some(Overlay::AccountSetup(ref form)) if form.mode == crate::account_setup::Mode::Login)
        );
        assert!(app.syncing.is_empty());
        assert_eq!(app.mails.len(), 1);
        assert!(matches!(app.mailbox_state, MailboxState::Ready));
    }

    #[tokio::test]
    async fn first_run_opens_setup_without_loading_demo_mail() {
        let mailbox = TestMailbox::new();
        let config = crate::config::Config::load(Some(&mailbox.0.join("config.toml"))).unwrap();
        let mut app = super::App::new(MailboxSource::Demo, config);
        app.prepare_accounts(true, false);
        assert!(app.onboarding);
        let load = app.load_rx.recv().await.unwrap();
        app.apply_load(load);
        assert!(app.mails.is_empty());
        assert!(
            matches!(app.overlay, Some(Overlay::AccountSetup(ref form)) if form.mode == crate::account_setup::Mode::Add)
        );
        app.on_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.overlay.is_none());
        assert!(app.onboarding);
        assert!(!mailbox.0.join("config.toml").exists());
        app.on_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::AccountSetup(_))));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("add account"));
    }

    fn pending_setup(app: &mut super::App, request: u64) -> crate::config::Account {
        let mut form = crate::account_setup::tests::filled_form(&app.config);
        let account = form.build().unwrap();
        form.busy = true;
        form.request = request;
        app.setup_generation = request;
        app.checking_account = true;
        app.overlay = Some(Overlay::AccountSetup(Box::new(form)));
        account
    }

    #[tokio::test]
    async fn failed_and_cancelled_logins_never_save_account_settings() {
        let mailbox = TestMailbox::new();
        let config = crate::config::Config::load(Some(&mailbox.0.join("config.toml"))).unwrap();
        let mut app = super::App::new(MailboxSource::Demo, config);
        app.prepare_accounts(true, false);
        let account = pending_setup(&mut app, 1);
        app.finish_account_setup(super::SetupResult {
            request: 1,
            account,
            result: Err("Sign-in failed".into()),
        });
        assert!(
            matches!(app.overlay, Some(Overlay::AccountSetup(ref form)) if !form.busy && form.error.as_deref() == Some("Sign-in failed"))
        );
        assert!(!mailbox.0.join("config.toml").exists());
        assert!(app.config.accounts.is_empty());
        let account = pending_setup(&mut app, 2);
        app.on_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.finish_account_setup(super::SetupResult {
            request: 2,
            account,
            result: Ok(()),
        });
        assert!(app.overlay.is_none());
        assert!(app.onboarding);
        assert!(!app.checking_account);
        assert!(!mailbox.0.join("config.toml").exists());
        assert!(app.config.accounts.is_empty());
    }

    #[tokio::test]
    async fn verified_setup_persists_settings_and_restart_requests_password() {
        let mailbox = TestMailbox::new();
        let path = mailbox.0.join("config.toml");
        let config = crate::config::Config::load(Some(&path)).unwrap();
        let mut app = super::App::new(MailboxSource::Demo, config);
        app.prepare_accounts(true, false);
        let account = pending_setup(&mut app, 1);
        app.finish_account_setup(super::SetupResult {
            request: 1,
            account,
            result: Ok(()),
        });
        assert!(!app.onboarding);
        assert!(app.overlay.is_none());
        assert!(
            app.config
                .remote
                .as_ref()
                .unwrap()
                .session_password
                .is_some()
        );
        finish_load(&mut app).await;
        assert!(matches!(app.mailbox_state, MailboxState::Empty));
        let config = crate::config::Config::load(Some(&path)).unwrap();
        let source = MailboxSource::Maildir(config.maildir.clone().unwrap());
        let mut restarted = super::App::new(source, config);
        restarted.prepare_accounts(false, false);
        assert!(
            matches!(restarted.overlay, Some(Overlay::AccountSetup(ref form)) if form.mode == crate::account_setup::Mode::Login)
        );
        let before = std::fs::read(&path).unwrap();
        let Some(Overlay::AccountSetup(mut form)) = restarted.overlay.take() else {
            panic!()
        };
        form.values[3] = "another-fake-password".into();
        form.request = 2;
        let account = form.build().unwrap();
        restarted.setup_generation = 2;
        restarted.overlay = Some(Overlay::AccountSetup(form));
        restarted.finish_account_setup(super::SetupResult {
            request: 2,
            account,
            result: Ok(()),
        });
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(
            restarted
                .config
                .remote
                .as_ref()
                .unwrap()
                .password()
                .unwrap(),
            "another-fake-password"
        );
    }

    #[tokio::test]
    async fn account_form_accepts_paste_without_executing_newlines() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        app.open_account_setup(crate::account_setup::Mode::Add);
        app.handle_event(crossterm::event::Event::Paste("Tuna\r\nKılıç\u{1b}".into()));
        let Some(Overlay::AccountSetup(form)) = &app.overlay else {
            panic!()
        };
        assert_eq!(form.values[0], "TunaKılıç");
        assert!(!form.busy);
    }

    #[tokio::test]
    async fn single_account_menu_allows_adding_and_editing() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        app.config.path = Some(mailbox.0.join("config.toml"));
        let account = crate::account_setup::tests::filled_form(&app.config)
            .build()
            .unwrap();
        app.config.accounts.push(account);
        app.config.activate(0);
        app.open_accounts();
        assert!(
            matches!(app.overlay, Some(Overlay::Accounts { ref items, .. }) if items.len() == 1)
        );
        app.on_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(
            matches!(app.overlay, Some(Overlay::AccountSetup(ref form)) if form.mode == crate::account_setup::Mode::Add)
        );
        app.overlay = None;
        app.open_accounts();
        app.on_key_event(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(
            matches!(app.overlay, Some(Overlay::AccountSetup(ref form)) if form.mode == crate::account_setup::Mode::Edit)
        );
    }

    #[tokio::test]
    async fn demo_and_unrelated_maildirs_never_start_remote_sync() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        app.config.remote = Some(crate::remote::tests::Server::new().remote());
        app.source = MailboxSource::Demo;
        app.start_sync();
        assert!(app.syncing.is_empty());
        app.source = MailboxSource::Maildir(mailbox.0.join("unrelated"));
        app.start_sync();
        assert!(app.syncing.is_empty());
    }

    #[tokio::test]
    async fn empty_mailbox_detects_new_messages_and_allows_composing() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        finish_load(&mut app).await;
        assert!(matches!(app.mailbox_state, MailboxState::Empty));
        app.on_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Compose(_))));
        app.overlay = None;
        std::fs::write(
            mailbox.0.join("new/incoming"),
            b"From: ada@example.test\nSubject: hello\n\nhello",
        )
        .unwrap();
        app.check_mailbox_changes();
        finish_load(&mut app).await;
        assert_eq!(app.mails.len(), 1);
    }

    #[tokio::test]
    async fn ignores_stale_mailbox_loads() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        finish_load(&mut app).await;
        app.apply_load(super::MailboxLoad {
            generation: app.load_generation - 1,
            fingerprint: Some(999),
            result: MailboxSource::Demo.load(),
        });
        assert!(app.mails.is_empty());
        assert!(matches!(app.mailbox_state, MailboxState::Empty));
        assert_ne!(app.mailbox_fingerprint, 999);
    }

    #[tokio::test]
    async fn saving_a_draft_refreshes_the_mailbox() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        finish_load(&mut app).await;
        app.start_draft(crate::draft::Start::New);
        let Some(Overlay::Compose(state)) = app.overlay.take() else {
            panic!()
        };
        app.finish_composer(state, false);
        finish_load(&mut app).await;
        assert_eq!(app.mails.len(), 1);
        assert_eq!(app.mails[0].mailbox.as_deref(), Some("Drafts"));
    }

    #[tokio::test]
    async fn resumes_drafts_without_losing_headers_body_or_attachments() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        finish_load(&mut app).await;
        let attachment = mailbox.0.join("notes.txt");
        std::fs::write(&attachment, b"attached notes").unwrap();
        let mut composer = Composer::new(
            Some(&crate::mail::Mail::new(
                "parent@example.test",
                "ada@example.test",
                "Hello",
                "",
                None,
            )),
            Some("me@example.test"),
            None,
        )
        .unwrap();
        composer.cc = "cc@example.test".into();
        composer.bcc = "bcc@example.test".into();
        composer.format = crate::draft::MessageFormat::Html;
        composer.attachments = attachment.display().to_string();
        std::fs::write(&composer.body_path, "<p>Draft body</p>").unwrap();
        app.finish_composer(
            ComposeState {
                composer,
                field: 0,
                error: None,
                account: None,
                suggestion: None,
            },
            false,
        );
        finish_load(&mut app).await;
        let original = app.mails[0].source_path.clone().unwrap();
        std::fs::remove_file(attachment).unwrap();
        app.active_folder = "Drafts".into();
        app.open_mail(&app.mails[0].id.clone());
        let Some(Overlay::Compose(mut state)) = app.overlay.take() else {
            panic!()
        };
        assert_eq!(state.composer.format, crate::draft::MessageFormat::Html);
        assert_eq!(state.composer.cc, "cc@example.test");
        assert_eq!(state.composer.bcc, "bcc@example.test");
        let message = state.composer.message(&mailbox.0).unwrap();
        let parsed = mail_parser::MessageParser::default()
            .parse(&message)
            .unwrap();
        assert_eq!(parsed.in_reply_to().as_text(), Some("parent@example.test"));
        assert_eq!(parsed.body_html(0).unwrap().trim(), "<p>Draft body</p>");
        assert_eq!(
            parsed.attachments().next().unwrap().contents(),
            b"attached notes"
        );
        let extracted = std::path::PathBuf::from(&state.composer.attachments);
        state.composer.subject = "Updated".into();
        app.finish_composer(state, false);
        finish_load(&mut app).await;
        assert_eq!(app.mails.len(), 1);
        assert_eq!(app.mails[0].subject, "Updated");
        assert_eq!(app.mails[0].source_path.as_ref(), Some(&original));
        assert!(!extracted.exists());
    }

    #[tokio::test]
    async fn interrupt_preserves_the_composer() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        finish_load(&mut app).await;
        app.running = true;
        app.start_draft(crate::draft::Start::New);
        app.on_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.running);
        assert!(matches!(app.overlay, Some(Overlay::ComposeClose(_))));
    }

    #[tokio::test]
    async fn sending_a_resumed_draft_moves_it_to_sent() {
        let mailbox = TestMailbox::new();
        let mut app = mailbox.app();
        let sink = mailbox.0.join("sent.log");
        app.config.send_command = vec!["termail-test-sink".into(), sink.display().to_string()];
        finish_load(&mut app).await;
        app.start_draft(crate::draft::Start::New);
        let Some(Overlay::Compose(mut state)) = app.overlay.take() else {
            panic!()
        };
        state.composer.to = "ada@example.test".into();
        app.finish_composer(state, false);
        finish_load(&mut app).await;
        app.active_folder = "Drafts".into();
        app.open_mail(&app.mails[0].id.clone());
        let Some(Overlay::Compose(state)) = app.overlay.take() else {
            panic!()
        };
        app.finish_composer(state, true);
        assert!(app.sending);
        app.running = true;
        app.quit();
        assert!(app.running);
        let notice = app.delivery_rx.recv().await.unwrap();
        app.finish_delivery(notice);
        finish_load(&mut app).await;
        assert!(!app.sending);
        assert_eq!(app.mails.len(), 1);
        assert_eq!(app.mails[0].mailbox.as_deref(), Some("Sent"));
        assert!(!app.mails[0].unread);
        assert!(sink.exists());
    }

    #[tokio::test]
    async fn uncertain_delivery_is_not_retried_after_reopening() {
        let mailbox = TestMailbox::new();
        let source = MailboxSource::Maildir(mailbox.0.clone());
        let path = source
            .save_outbox(b"From: me@example.test\nTo: ada@example.test\n\nbody")
            .unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        std::fs::create_dir_all(mailbox.0.join(".Outbox/termail-state")).unwrap();
        std::fs::write(
            mailbox
                .0
                .join(".Outbox/termail-state")
                .join(format!("{name}.delivery")),
            "sending",
        )
        .unwrap();
        let mail = source.load().unwrap().mails.remove(0);
        let sink = mailbox.0.join("sent.log");
        let result = crate::delivery::Delivery {
            remote: None,
            account: "test".into(),
            source,
            mail,
            command: vec!["termail-test-sink".into(), sink.display().to_string()],
        }
        .run()
        .await;
        assert!(result.contains("unconfirmed"));
        assert!(path.exists());
        assert!(!sink.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn slow_delivery_keeps_navigation_responsive_and_preserves_its_account() {
        let personal = TestMailbox::new();
        let work = TestMailbox::new();
        let mut app = personal.app();
        finish_load(&mut app).await;
        app.config.send_command =
            vec!["sh".into(), "-c".into(), "cat >/dev/null; sleep 0.1".into()];
        app.start_draft(crate::draft::Start::New);
        let Some(Overlay::Compose(mut state)) = app.overlay.take() else {
            panic!()
        };
        state.composer.to = "ada@example.test".into();
        app.finish_composer(state, true);
        app.on_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help { .. })));
        app.source = MailboxSource::Maildir(work.0.clone());
        app.reload();
        let notice =
            tokio::time::timeout(std::time::Duration::from_secs(5), app.delivery_rx.recv())
                .await
                .unwrap()
                .unwrap();
        app.finish_delivery(notice);
        finish_load(&mut app).await;
        assert!(app.mails.is_empty());
        let personal_mail = MailboxSource::Maildir(personal.0.clone()).load().unwrap();
        assert_eq!(personal_mail.mails[0].mailbox.as_deref(), Some("Sent"));
    }

    #[tokio::test]
    async fn sent_copy_failure_does_not_resend_the_message() {
        let mailbox = TestMailbox::new();
        let source = MailboxSource::Maildir(mailbox.0.clone());
        source
            .save_outbox(b"From: me@example.test\nTo: ada@example.test\nSubject: hello\n\nbody")
            .unwrap();
        let mail = source.load().unwrap().mails.remove(0);
        let sink = mailbox.0.join("sent.log");
        let delivery = || crate::delivery::Delivery {
            remote: None,
            account: "test".into(),
            source: source.clone(),
            mail: mail.clone(),
            command: vec!["termail-test-sink".into(), sink.display().to_string()],
        };
        std::fs::write(mailbox.0.join(".Sent"), "blocked").unwrap();
        let result = delivery().run().await;
        assert!(result.contains("unable to move"), "{result}");
        let sent = std::fs::read(&sink).unwrap();
        std::fs::remove_file(mailbox.0.join(".Sent")).unwrap();
        assert_eq!(delivery().run().await, "Message sent");
        assert_eq!(std::fs::read(sink).unwrap(), sent);
        assert_eq!(
            source.load().unwrap().mails[0].mailbox.as_deref(),
            Some("Sent")
        );
    }

    #[tokio::test]
    async fn failed_sender_leaves_a_retryable_outbox_message() {
        let mailbox = TestMailbox::new();
        let source = MailboxSource::Maildir(mailbox.0.clone());
        source
            .save_outbox(b"From: me@example.test\nTo: ada@example.test\n\nbody")
            .unwrap();
        let mail = source.load().unwrap().mails.remove(0);
        let result = crate::delivery::Delivery {
            remote: None,
            account: "test".into(),
            source: source.clone(),
            mail: mail.clone(),
            command: Vec::new(),
        }
        .run()
        .await;
        assert!(result.contains("Send failed"));
        assert!(mail.source_path.as_ref().unwrap().exists());
        let result = crate::delivery::Delivery {
            remote: None,
            account: "test".into(),
            source,
            mail,
            command: vec![
                "termail-test-sink".into(),
                mailbox.0.join("sent.log").display().to_string(),
            ],
        }
        .run()
        .await;
        assert_eq!(result, "Message sent");
    }

    #[tokio::test]
    async fn unified_outbox_uses_the_message_account() {
        let personal = TestMailbox::new();
        let work = TestMailbox::new();
        let mut app = personal.app();
        finish_load(&mut app).await;
        app.config.send_command = vec!["wrong-account".into()];
        app.config.accounts = vec![crate::config::Account {
            id: "work".into(),
            maildir: Some(work.0.clone()),
            send_command: vec!["work-sender".into()],
            ..Default::default()
        }];
        app.source = MailboxSource::Unified(vec![("work".into(), work.0.clone())]);
        let mut mail = crate::mail::Mail::new("work:queued", "", "", "", None);
        mail.account = Some("work".into());
        mail.mailbox = Some("Outbox".into());
        let (source, command, account) = app.delivery_account(&mail).unwrap();
        assert_eq!(account, "work:");
        assert_eq!(source, MailboxSource::Maildir(work.0.clone()));
        assert_eq!(command, vec!["work-sender"]);
    }

    #[test]
    fn docks_readers_only_when_they_fit() {
        assert!(reader_is_docked(120, true));
        assert!(!reader_is_docked(100, true));
        assert!(!reader_is_docked(120, false));
    }

    #[test]
    fn accepts_shifted_action_keys() {
        for action in ['s', 'e', 'd'] {
            assert!(action_key(
                KeyEvent::new(KeyCode::Char(action), KeyModifiers::NONE),
                action
            ));
            assert!(action_key(
                KeyEvent::new(
                    KeyCode::Char(action.to_ascii_uppercase()),
                    KeyModifiers::SHIFT
                ),
                action
            ));
        }
        assert!(!action_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            's'
        ));
    }

    #[test]
    fn reads_maildir_source_from_arguments() {
        let options = parse_options([OsString::from("--maildir"), OsString::from("/tmp/mail")])
            .unwrap()
            .unwrap();

        assert_eq!(
            options.source,
            Some(MailboxSource::Maildir(PathBuf::from("/tmp/mail")))
        );
    }

    #[test]
    fn reads_doctor_command() {
        let options = parse_options([OsString::from("doctor")]).unwrap().unwrap();

        assert!(options.doctor);
    }

    #[test]
    fn renders_mailbox_states_with_recovery() {
        assert!(render_state(&MailboxState::Loading).contains("Loading demo mailbox"));
        assert!(render_state(&MailboxState::Empty).contains("No messages found"));

        let screen = render_state(&MailboxState::Error("permission denied".into()));
        assert!(screen.contains("Unable to load mailbox"));
        assert!(screen.contains("press r to retry"));
    }

    #[test]
    fn renders_help_and_command_overlays() {
        assert!(render_overlay(&Overlay::Help { scroll: 0 }).contains("message actions"));
        assert!(
            render_overlay(&Overlay::Command("filter unread".into())).contains(":filter unread_")
        );
    }

    #[test]
    fn renders_the_builtin_compose_form() {
        let composer = Composer::new(None, Some("Tuna <tuna@example.com>"), None).unwrap();
        let screen = render_overlay(&Overlay::Compose(ComposeState {
            composer,
            field: 0,
            error: None,
            account: None,
            suggestion: None,
        }));

        assert!(screen.contains("From"));
        assert!(screen.contains("headers"));
        assert!(screen.contains("To       _"));
        assert!(screen.contains("Plain text"));
        assert!(screen.contains("body"));
        assert!(screen.contains("$EDITOR"));
        assert!(screen.contains("ctrl+s"));
    }

    fn render_state(state: &MailboxState) -> String {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_mailbox_state(frame, frame.area(), state, &MailboxSource::Demo, 2))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn render_overlay(overlay: &Overlay) -> String {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_overlay(frame, overlay)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
}
