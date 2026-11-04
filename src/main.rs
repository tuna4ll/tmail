mod account_setup;
mod calendar;
mod certs;
mod complete;
mod completions;
mod config;
mod contacts;
mod crypto;
mod delivery;
mod discover;
mod doctor;
mod draft;
#[cfg(test)]
mod fake_imap;
mod i18n;
mod imap_client;
mod keymap;
mod links;
mod log;
mod mail;
mod mailbox;
mod markdown;
mod message;
mod mouse;
mod oauth;
mod outbox;
mod patch;
mod remote;
mod script;
mod search;
mod sender;
mod settings;
mod storage;
mod terminal;
mod theme;
mod trackers;
mod widgets;

mod accounts;
mod actions;
mod background;
mod cli;
mod compose;
mod hooks;
mod inbox;
mod input;
mod reload;
mod ui;

use cli::*;
use inbox::*;
use input::*;
use mouse::clickable_hints;
use ui::*;

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

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
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

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    terminal::install();
    let Some(options) = parse_options(std::env::args_os().skip(1))? else {
        print_help();
        return Ok(());
    };
    if let Some(text) = &options.print {
        print!("{text}");
        return Ok(());
    }
    log::init(options.log.clone())?;
    mailbox::prune_caches();
    let config = Config::load(options.config_path.as_deref())?;
    theme::init(&theme::effective(&config.theme, config.ui.accessible))
        .map_err(color_eyre::eyre::Error::msg)?;
    settings::init(config.ui.clone()).map_err(color_eyre::eyre::Error::msg)?;
    settings::init_compose(config.compose.clone()).map_err(color_eyre::eyre::Error::msg)?;
    settings::init_reader(config.reader.clone()).map_err(color_eyre::eyre::Error::msg)?;
    keymap::Keymap::from_config(&config.keys).map_err(color_eyre::eyre::Error::msg)?;
    i18n::init(
        &config.ui.language,
        config.path.as_deref().and_then(Path::parent),
    )
    .map_err(color_eyre::eyre::Error::msg)?;
    if let Some(account) = &options.oauth {
        if let Err(error) = oauth::run(&config, account.as_deref()) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }
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

    let picker = terminal::graphics_picker();
    let terminal = terminal::enter()?;

    let mut app = App::new(source, config);
    app.picker = Some(picker);
    app.prepare_accounts(needs_setup, explicit_source);
    app.load_script();
    let result = app.run(terminal).await;

    terminal::leave()?;

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
    delivery_tx: mpsc::UnboundedSender<(Option<PathBuf>, String)>,
    delivery_rx: mpsc::UnboundedReceiver<(Option<PathBuf>, String)>,
    /// Outbox messages being delivered right now.
    sending: std::collections::HashSet<PathBuf>,
    task_tx: mpsc::UnboundedSender<Task>,
    task_rx: mpsc::UnboundedReceiver<Task>,
    /// Results of older mailbox scans are ignored once a newer one started.
    fingerprint_generation: u64,
    fingerprint_busy: bool,
    last_retry_check: std::time::Instant,
    sync_tx: mpsc::UnboundedSender<(String, Result<imap_client::Synced, remote::RemoteError>)>,
    sync_rx: mpsc::UnboundedReceiver<(String, Result<imap_client::Synced, remote::RemoteError>)>,
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
    /// Whether the conversations were grouped across every folder for `in:all`.
    built_everywhere: bool,
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
    search_cache: std::cell::RefCell<search::Cache>,
    /// When config.toml last changed, to notice edits.
    config_modified: Option<std::time::SystemTime>,
    searching: bool,
    filter: InboxFilter,
    undo: Option<UndoAction>,
    confirm_delete: Option<String>,
    notice: Option<String>,
    mailbox_fingerprint: u64,
    pending_editor: Option<ComposeState>,
    /// Work that needs the terminal, such as a passphrase prompt.
    pending_terminal: Option<TerminalJob>,
    keymap: keymap::Keymap,
    /// Characters between the text cursor and the end of the focused field.
    cursor_back: usize,
    /// Status-line cells that press a key when clicked.
    hint_regions: Vec<(Rect, KeyEvent)>,
    /// The cell under the pointer.
    hover: Option<(u16, u16)>,
    last_click: Option<(std::time::Instant, u16, u16)>,
    overlay_geometry: mouse::OverlayGeometry,
    /// Pane widths, which the pane borders can be dragged to change.
    inbox_width: u16,
    reader_width: u16,
    dragging: Option<mouse::Divider>,
    workspace_area: Option<Rect>,
    reader_area: Option<Rect>,
    reader_docked: bool,
    /// The reader window after the user moved or resized it.
    popup: Option<Rect>,
    popup_maximized: bool,
    screen_width: u16,
    screen_height: u16,
    /// q was pressed once with ui.confirm_quit on.
    confirm_quit: bool,
    /// Accounts whose server rejected the sign-in; they wait for a new password
    /// instead of retrying, which could get the account locked.
    auth_failed: std::collections::HashSet<String>,
    address_book: Vec<contacts::Contact>,
    script: Option<script::Engine>,
    /// Plugins from `plugins/` next to the config, each with only its permissions.
    plugins: Vec<script::Engine>,
    /// The folder the script last heard about.
    script_folder: String,
    /// Decrypted or verified content by message, kept only in memory.
    opened: std::collections::HashMap<String, Result<(Vec<u8>, String), String>>,
    overlay: Option<Overlay>,
    config: Config,
    active_folder: String,
    account_label: String,
}

/// Work that runs with the terminal restored, so programs such as pinentry can use it.
enum TerminalJob {
    /// Signs or encrypts a message, then queues it.
    Protect {
        state: Box<ComposeState>,
        contents: Vec<u8>,
    },
    /// Decrypts a message for the reader.
    Open(String),
}

/// Results of blocking work done off the UI thread.
enum Task {
    Fingerprint {
        generation: u64,
        value: Option<u64>,
        /// Records the value instead of comparing it, after the app changed files itself.
        baseline: bool,
    },
    /// Background work that had nothing to report.
    Nothing,
    /// Shows a Maildir that was just prepared, such as a thread from an archive.
    OpenMaildir { path: PathBuf, message: String },
    /// Shows a message and optionally reloads the mailbox.
    Done { message: String, reload: bool },
    /// A signed message was checked.
    Opened {
        mail_id: String,
        result: Result<(Vec<u8>, String), String>,
    },
    /// Server settings were looked up for an address in the account form.
    Discovered {
        email: String,
        result: Result<discover::Found, String>,
    },
    /// The OAuth2 sign-in page for a connection check, in case no browser opened.
    SetupLink { request: u64, url: String },
    /// A partial message finished downloading.
    Downloaded {
        mail_id: String,
        result: Result<(), String>,
    },
}

struct MailboxLoad {
    generation: u64,
    fingerprint: Option<u64>,
    result: Result<LoadReport, LoadError>,
}

struct SetupResult {
    request: u64,
    /// The checked account, with any OAuth2 token it got, or why the check failed.
    result: Result<config::Account, account_setup::Failure>,
}

struct UndoAction {
    changes: Vec<MaildirChange>,
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
        /// The highlighted folder is deleted when x is pressed again.
        deleting: bool,
    },
    /// Asks before opening a link that looks deceptive.
    LinkWarning {
        url: String,
        warnings: Vec<String>,
    },
    /// Confirms leaving a mailing list.
    Unsubscribe {
        unsubscribe: mail::Unsubscribe,
    },
    /// Actions for the message under the pointer, opened with a right click.
    Menu {
        x: u16,
        y: u16,
        items: Vec<mouse::MenuItem>,
        selected: usize,
    },
    /// The address book, filtered by what was typed.
    Contacts {
        selected: usize,
        filter: String,
        items: Vec<contacts::Contact>,
    },
    /// Asks for one line of text, such as a folder name.
    Prompt {
        kind: PromptKind,
        input: String,
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
    /// The built-in editor for the body of the message being composed.
    BodyEditor {
        state: Box<ComposeState>,
        editor: widgets::editor::Editor,
    },
}

impl Overlay {
    /// Moves the highlight in list overlays with j/k or the arrows. Returns whether the
    /// key was used.
    fn step_selection(&mut self, code: KeyCode) -> bool {
        let down = matches!(code, KeyCode::Down | KeyCode::Char('j'));
        if !down && !matches!(code, KeyCode::Up | KeyCode::Char('k')) {
            return false;
        }
        if let Self::Menu {
            selected, items, ..
        } = self
        {
            *selected = if down {
                (*selected + 1).min(items.len().saturating_sub(1))
            } else {
                selected.saturating_sub(1)
            };
            return true;
        }
        let (selected, count) = match self {
            Self::Folders {
                selected,
                items,
                deleting,
                ..
            } => {
                *deleting = false;
                (selected, items.len())
            }
            Self::Links { selected, items } => (selected, items.len()),
            Self::Contacts {
                selected,
                filter,
                items,
            } => {
                let count = contacts_matching(items, filter).len();
                (selected, count)
            }
            Self::Attachments { selected, items } => (selected, items.len()),
            Self::Accounts {
                selected,
                items,
                removing,
            } => {
                // Moving to another account cancels a pending removal.
                *removing = false;
                (selected, items.len())
            }
            _ => return false,
        };
        *selected = if down {
            (*selected + 1).min(count.saturating_sub(1))
        } else {
            selected.saturating_sub(1)
        };
        true
    }
}

struct ComposeState {
    composer: draft::Composer,
    field: usize,
    error: Option<String>,
    /// The account to send from when composing in the all-accounts view.
    account: Option<usize>,
    /// The focused field's value with its last entry completed; tab accepts it.
    suggestion: Option<String>,
    /// Signing and encryption applied when the message is sent.
    protection: crypto::Protection,
    autosave: Autosave,
}

/// Keeps an open message in Drafts while it is being written, so a crash or a closed
/// terminal loses at most a few seconds of it.
#[derive(Default)]
struct Autosave {
    /// What the message looked like when it was last saved, and when it opened.
    saved: Option<u64>,
    opened: Option<u64>,
    last: Option<std::time::Instant>,
    /// Shown in the composer, such as "Saved to Drafts at 14:02".
    status: Option<String>,
    /// The draft file was made by autosave, so discarding the message removes it.
    created: bool,
}

impl ComposeState {
    const FIELD_COUNT: usize = 8;
    const FORMAT_FIELD: usize = 4;
    const SECURITY_FIELD: usize = 5;
    const FILES_FIELD: usize = 6;
    const BODY_FIELD: usize = 7;

    fn value_mut(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.composer.to),
            1 => Some(&mut self.composer.cc),
            2 => Some(&mut self.composer.bcc),
            3 => Some(&mut self.composer.subject),
            Self::FILES_FIELD => Some(&mut self.composer.attachments),
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

/// The contacts whose name or address contains the filter.
fn contacts_matching<'a>(
    items: &'a [contacts::Contact],
    filter: &str,
) -> Vec<&'a contacts::Contact> {
    let filter = filter.to_lowercase();
    items
        .iter()
        .filter(|contact| {
            contact.name.to_lowercase().contains(&filter)
                || contact.email.to_lowercase().contains(&filter)
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PromptKind {
    NewFolder,
    /// Renames the folder with this display name.
    RenameFolder(String),
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
        let (task_tx, task_rx) = mpsc::unbounded_channel();
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
            sending: Default::default(),
            task_tx,
            task_rx,
            fingerprint_generation: 0,
            fingerprint_busy: false,
            last_retry_check: std::time::Instant::now(),
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
            built_everywhere: false,
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
            search_cache: Default::default(),
            config_modified: config.path.as_deref().and_then(|path| {
                fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
            }),
            searching: false,
            filter: match settings::with(|ui| ui.start_filter.clone()).as_str() {
                "unread" => InboxFilter::Unread,
                "starred" => InboxFilter::Starred,
                _ => InboxFilter::All,
            },
            undo: None,
            confirm_delete: None,
            notice: None,
            mailbox_fingerprint: 0,
            pending_editor: None,
            pending_terminal: None,
            keymap: keymap::Keymap::from_config(&config.keys).unwrap_or_default(),
            opened: Default::default(),
            cursor_back: 0,
            hint_regions: Vec::new(),
            hover: None,
            last_click: None,
            overlay_geometry: Default::default(),
            inbox_width: settings::with(|ui| ui.inbox_width),
            reader_width: settings::with(|ui| ui.reader_width),
            dragging: None,
            workspace_area: None,
            reader_area: None,
            reader_docked: false,
            popup: None,
            popup_maximized: false,
            screen_width: 0,
            screen_height: 0,
            confirm_quit: false,
            auth_failed: Default::default(),
            address_book: Vec::new(),
            script: None,
            plugins: Vec::new(),
            script_folder: String::new(),
            overlay: None,
            config,
            active_folder: settings::with(|ui| ui.start_folder.clone()),
            account_label,
        };
        app.address_book = app
            .config
            .contacts_path()
            .map(|path| contacts::load(&path))
            .unwrap_or_default();
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
                terminal::clear(&mut terminal)?;
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
                    self.retry_due_deliveries();
                    self.run_script_timers();
                    self.autosave_draft();
                    self.reload_config_if_changed();
                },
                task = self.task_rx.recv() => {
                    if let Some(task) = task {
                        self.finish_task(task);
                    }
                }
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
                    if let Some((path, notice)) = result {
                        self.finish_delivery(path, notice);
                    }
                }
                event = event_stream.next() => {
                    if let Some(Ok(event)) = event {
                        self.handle_event(event);
                    }
                }
            }
            if self.pending_editor.is_some() || self.pending_terminal.is_some() {
                // The event reader keeps reading the terminal in the background; stop it
                // so the editor or a passphrase prompt gets every key, and start a new one
                // afterwards.
                drop(event_stream);
                if let Some(state) = self.pending_editor.take() {
                    terminal = self.edit_composer_body(state)?;
                }
                if let Some(job) = self.pending_terminal.take() {
                    terminal = self.run_terminal_job(job)?;
                }
                event_stream = EventStream::new();
            }
        }

        Ok(())
    }

    /// Runs blocking work on the thread pool and hands its result back to the UI loop.
    fn spawn_task(&self, work: impl FnOnce() -> Task + Send + 'static) {
        let sender = self.task_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = sender.send(work());
        });
    }

    fn finish_task(&mut self, task: Task) {
        match task {
            Task::Fingerprint {
                generation,
                value,
                baseline,
            } => {
                if generation != self.fingerprint_generation {
                    return;
                }
                self.fingerprint_busy = false;
                let Some(value) = value else {
                    return;
                };
                if baseline || self.mailbox_fingerprint == 0 {
                    self.mailbox_fingerprint = value;
                } else if value != self.mailbox_fingerprint
                    && matches!(
                        self.mailbox_state,
                        MailboxState::Ready | MailboxState::Empty
                    )
                {
                    self.notice = Some("Mailbox changed · refreshing".into());
                    self.reload();
                }
            }
            Task::Nothing => {}
            Task::OpenMaildir { path, message } => {
                self.source = MailboxSource::Maildir(path);
                self.filter = InboxFilter::All;
                self.query.clear();
                self.undo = None;
                self.mailbox_fingerprint = 0;
                self.reload();
                self.notice = Some(message);
            }
            Task::Done { message, reload } => {
                self.notice = Some(message);
                if reload {
                    self.reload();
                }
            }
            Task::Opened { mail_id, result } => self.show_opened(mail_id, result),
            Task::Discovered { email, result } => {
                if let Some(Overlay::AccountSetup(form)) = &mut self.overlay
                    && form.value(account_setup::Field::Email).trim() == email
                {
                    form.discovering = false;
                    match result {
                        Ok(found)
                            if form.value(account_setup::Field::ImapHost).trim().is_empty() =>
                        {
                            form.apply(&found);
                        }
                        Ok(_) => {}
                        Err(message) => form.info = Some(message),
                    }
                }
            }
            Task::SetupLink { request, url } => {
                if let Some(Overlay::AccountSetup(form)) = &mut self.overlay
                    && form.request == request
                {
                    form.oauth_link = Some(url);
                }
            }
            Task::Downloaded { mail_id, result } => match result {
                Ok(()) => {
                    self.notice = Some("Full message downloaded".into());
                    if self
                        .reader
                        .as_ref()
                        .is_some_and(|reader| reader.mail_id() == mail_id)
                    {
                        self.reader = Some(Reader::new(mail_id));
                    }
                    self.reload();
                }
                Err(error) => self.notice = Some(error),
            },
        }
    }

    fn quit(&mut self) {
        if !self.sending.is_empty() || self.checking_account {
            self.notice =
                Some("Wait for the current connection or send to finish before quitting.".into());
        } else if settings::with(|ui| ui.confirm_quit) && !self.confirm_quit {
            self.confirm_quit = true;
            self.notice = Some("Press q again to quit".into());
        } else {
            self.running = false;
        }
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn rejected_passwords_open_sign_in_and_stop_retrying() {
        let mailbox = TestMailbox::new();
        let remote = crate::remote::RemoteConfig {
            imap_host: "imap.example.test".into(),
            smtp_host: "smtp.example.test".into(),
            username: "me@example.test".into(),
            session_password: Some(std::sync::Arc::new(crate::remote::SessionPassword(
                zeroize::Zeroizing::new("wrong".into()),
            ))),
            ..Default::default()
        };
        let config = crate::config::Config {
            email: Some("me@example.test".into()),
            maildir: Some(mailbox.0.clone()),
            remote: Some(remote),
            ..Default::default()
        };
        let mut app = super::App::new(MailboxSource::Maildir(mailbox.0.clone()), config);
        finish_load(&mut app).await;
        let id = app.config.current_account().id;
        app.finish_sync(
            id.clone(),
            Err(crate::remote::RemoteError::auth(
                "The server said: Invalid credentials.",
            )),
        );
        assert!(app.auth_failed.contains(&id));
        assert!(matches!(
            &app.overlay,
            Some(Overlay::AccountSetup(form)) if form.mode == crate::account_setup::Mode::Login
                && form.error.as_deref().is_some_and(|error| error.contains("Invalid credentials"))
        ));
        app.overlay = None;
        app.last_sync.clear();
        app.sync_if_due();
        assert!(
            app.syncing.is_empty(),
            "no retry with the rejected password"
        );
    }

    #[tokio::test]
    async fn the_reader_window_moves_and_resizes_with_the_mouse() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mailbox = TestMailbox::new();
        std::fs::write(
            mailbox.0.join("new/1"),
            "From: a@example.test\r\nSubject: hello\r\nMessage-ID: <1@x>\r\n\r\nbody\r\n",
        )
        .unwrap();
        let mut app = mailbox.app();
        finish_load(&mut app).await;
        let mut terminal = Terminal::new(TestBackend::new(140, 44)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let id = app.mails[0].id.clone();
        app.open_mail(&id);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let area = app.reader.as_ref().unwrap().frame_area();
        let mouse = |kind, column, row| {
            crossterm::event::Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 10,
            area.y,
        ));
        app.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 4,
            area.y + 2,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 4,
            area.y + 2,
        ));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let moved = app.reader.as_ref().unwrap().frame_area();
        assert_eq!((moved.x, moved.y), (area.x - 6, area.y + 2));
        let corner = (moved.right() - 1, moved.bottom() - 1);
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            corner.0,
            corner.1,
        ));
        app.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            moved.x + 60,
            moved.y + 20,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            moved.x + 60,
            moved.y + 20,
        ));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let sized = app.reader.as_ref().unwrap().frame_area();
        assert_eq!((sized.width, sized.height), (61, 21));
    }

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
        let _ = account;
        app.finish_account_setup(super::SetupResult {
            request: 1,
            result: Err("Sign-in failed".to_owned().into()),
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
            result: Ok(account),
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
            result: Ok(account),
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
        form.set(
            crate::account_setup::Field::Password,
            "another-fake-password",
        );
        form.request = 2;
        let account = form.build().unwrap();
        restarted.setup_generation = 2;
        restarted.overlay = Some(Overlay::AccountSetup(form));
        restarted.finish_account_setup(super::SetupResult {
            request: 2,
            result: Ok(account),
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
        assert_eq!(form.value(crate::account_setup::Field::Name), "TunaKılıç");
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
        let task = app.task_rx.recv().await.unwrap();
        app.finish_task(task);
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
                protection: crate::crypto::Protection::default(),
                autosave: Default::default(),
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
        assert!(!app.sending.is_empty());
        app.running = true;
        app.quit();
        assert!(app.running);
        let notice = app.delivery_rx.recv().await.unwrap();
        app.finish_delivery(notice.0, notice.1);
        finish_load(&mut app).await;
        assert!(app.sending.is_empty());
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
        app.finish_delivery(notice.0, notice.1);
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

    #[tokio::test]
    async fn reader_opens_as_a_window_unless_docking_is_chosen() {
        assert!(!reader_is_docked(160, true));
        let popup = super::App::new(MailboxSource::Demo, Default::default())
            .popup_rect(ratatui::layout::Rect::new(0, 0, 100, 40));
        assert_eq!((popup.width, popup.height), (84, 36));
        assert_eq!((popup.x, popup.y), (8, 2));
        let tiny = super::ui::clamp_rect(
            ratatui::layout::Rect::new(90, 30, 5, 2),
            ratatui::layout::Rect::new(0, 0, 100, 40),
        );
        assert_eq!(tiny, ratatui::layout::Rect::new(70, 30, 30, 8));
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
        assert!(render_overlay(Overlay::Help { scroll: 0 }).contains("message actions"));
        assert!(
            render_overlay(Overlay::Command("filter unread".into())).contains(":filter unread ")
        );
    }

    #[test]
    fn renders_the_builtin_compose_form() {
        let composer = Composer::new(None, Some("Tuna <tuna@example.com>"), None).unwrap();
        let screen = render_overlay(Overlay::Compose(ComposeState {
            composer,
            field: 0,
            error: None,
            account: None,
            suggestion: None,
            protection: crate::crypto::Protection::default(),
            autosave: Default::default(),
        }));

        assert!(screen.contains("From"));
        assert!(screen.contains("headers"));
        assert!(screen.contains("To        "));
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

    fn render_overlay(mut overlay: Overlay) -> String {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_overlay(frame, &mut overlay, 0);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
}
