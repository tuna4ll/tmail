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

mod accounts;
mod actions;
mod background;
mod cli;
mod compose;
mod inbox;
mod input;
mod ui;

use cli::*;
use inbox::*;
use input::*;
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

/// Results of blocking work done off the UI thread.
enum Task {
    Fingerprint {
        generation: u64,
        value: Option<u64>,
        /// Records the value instead of comparing it, after the app changed files itself.
        baseline: bool,
    },
    /// Shows a message and optionally reloads the mailbox.
    Done { message: String, reload: bool },
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
    account: config::Account,
    result: Result<(), String>,
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
}

impl Overlay {
    /// Moves the highlight in list overlays with j/k or the arrows. Returns whether the
    /// key was used.
    fn step_selection(&mut self, code: KeyCode) -> bool {
        let down = matches!(code, KeyCode::Down | KeyCode::Char('j'));
        if !down && !matches!(code, KeyCode::Up | KeyCode::Char('k')) {
            return false;
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
                    self.retry_due_deliveries();
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
            if let Some(state) = self.pending_editor.take() {
                terminal = self.edit_composer_body(state)?;
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
            Task::Done { message, reload } => {
                self.notice = Some(message);
                if reload {
                    self.reload();
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
        } else {
            self.running = false;
        }
    }
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
