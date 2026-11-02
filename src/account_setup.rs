use crate::{
    config::{Account, Config},
    remote::{Auth, OAuth2Config, RemoteConfig, RemoteError, Security, SessionPassword},
    theme,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use zeroize::Zeroize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Add,
    Edit,
    Login,
}

/// Every line of the form. Which ones show depends on the mode and the sign-in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Name,
    Email,
    SignIn,
    Username,
    Password,
    ClientId,
    ClientSecret,
    AuthUrl,
    TokenUrl,
    Scope,
    ImapHost,
    ImapPort,
    ImapSecurity,
    SmtpHost,
    SmtpPort,
    SmtpSecurity,
    SmtpUsername,
    SmtpPassword,
    Inbox,
    Sent,
    MakeDefault,
    Remember,
}

const FIELD_COUNT: usize = 22;

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Email => "Email",
            Self::SignIn => "Sign-in",
            Self::Username => "Username",
            Self::Password => "Password",
            Self::ClientId => "OAuth client ID",
            Self::ClientSecret => "Client secret",
            Self::AuthUrl => "Auth URL",
            Self::TokenUrl => "Token URL",
            Self::Scope => "Scope",
            Self::ImapHost => "IMAP host",
            Self::ImapPort => "IMAP port",
            Self::ImapSecurity => "IMAP security",
            Self::SmtpHost => "SMTP host",
            Self::SmtpPort => "SMTP port",
            Self::SmtpSecurity => "SMTP security",
            Self::SmtpUsername => "SMTP username",
            Self::SmtpPassword => "SMTP password",
            Self::Inbox => "Inbox",
            Self::Sent => "Sent folder",
            Self::MakeDefault => "Use by default",
            Self::Remember => "Remember",
        }
    }

    fn secret(self) -> bool {
        matches!(
            self,
            Self::Password | Self::SmtpPassword | Self::ClientSecret
        )
    }

    fn is_text(self) -> bool {
        !matches!(
            self,
            Self::SignIn
                | Self::ImapSecurity
                | Self::SmtpSecurity
                | Self::MakeDefault
                | Self::Remember
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignIn {
    Password,
    Google,
    Microsoft,
    OtherOAuth,
}

impl SignIn {
    fn label(self) -> &'static str {
        match self {
            Self::Password => "Password",
            Self::Google => "OAuth2 · Google",
            Self::Microsoft => "OAuth2 · Microsoft",
            Self::OtherOAuth => "OAuth2 · other provider",
        }
    }

    fn next(self, back: bool) -> Self {
        const ALL: [SignIn; 4] = [
            SignIn::Password,
            SignIn::Google,
            SignIn::Microsoft,
            SignIn::OtherOAuth,
        ];
        let index = ALL.iter().position(|item| *item == self).unwrap_or(0);
        ALL[(index + if back { 3 } else { 1 }) % 4]
    }

    fn oauth(self) -> bool {
        self != Self::Password
    }
}

fn security_label(security: Security) -> &'static str {
    match security {
        Security::Auto => "Auto (by port)",
        Security::Tls => "TLS",
        Security::Starttls => "STARTTLS",
    }
}

fn next_security(security: Security, back: bool) -> Security {
    const ALL: [Security; 3] = [Security::Auto, Security::Tls, Security::Starttls];
    let index = ALL.iter().position(|item| *item == security).unwrap_or(0);
    ALL[(index + if back { 2 } else { 1 }) % 3]
}

/// What a key asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Submit,
    /// Look up server settings for this address.
    Discover(String),
    /// Trust the certificate the form is offering.
    Trust,
    /// Copy the sign-in page address.
    CopyLink,
}

/// Why a connection check failed, and what the form can offer.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub untrusted: Option<crate::certs::Presented>,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            untrusted: None,
        }
    }
}

pub struct Form {
    pub mode: Mode,
    pub account: Account,
    values: [String; FIELD_COUNT],
    pub field: Field,
    pub sign_in: SignIn,
    pub imap_security: Security,
    pub smtp_security: Security,
    pub make_default: bool,
    pub remember: bool,
    pub reveal: bool,
    pub error: Option<String>,
    /// Neutral news, such as where settings were found.
    pub info: Option<String>,
    pub busy: bool,
    pub request: u64,
    pub discovering: bool,
    /// The domain settings were last looked up for, so leaving the field again does
    /// not repeat it.
    looked_up: String,
    /// A certificate the user may choose to trust.
    pub untrusted: Option<crate::certs::Presented>,
    /// The provider's sign-in page while OAuth2 waits for the browser.
    pub oauth_link: Option<String>,
    /// A certificate file the user chose to trust during this setup.
    pub tls_certificate: Option<std::path::PathBuf>,
    cursor_back: usize,
}

impl Drop for Form {
    fn drop(&mut self) {
        for field in [Field::Password, Field::SmtpPassword, Field::ClientSecret] {
            self.values[field as usize].zeroize();
        }
    }
}

impl Form {
    pub fn new(config: &Config, mode: Mode) -> Self {
        let account = if mode == Mode::Add {
            let base = config.path.as_ref().and_then(|path| path.parent());
            let mut number = 1;
            while config
                .accounts
                .iter()
                .any(|account| account.id == format!("account-{number}"))
                || base.is_some_and(|root| {
                    root.join("mail").join(format!("account-{number}")).exists()
                })
            {
                number += 1;
            }
            let id = format!("account-{number}");
            let maildir = base.map(|root| root.join("mail").join(&id));
            Account {
                id,
                maildir,
                ..Default::default()
            }
        } else {
            config.current_account()
        };
        let remote = account.remote.clone().unwrap_or_default();
        let oauth = remote.oauth2.clone().unwrap_or_default();
        let sign_in = match (remote.auth, oauth.provider.to_ascii_lowercase().as_str()) {
            (Auth::Password, _) => SignIn::Password,
            (_, "gmail" | "google") => SignIn::Google,
            (_, "outlook" | "microsoft" | "office365") => SignIn::Microsoft,
            _ => SignIn::OtherOAuth,
        };
        let mut values: [String; FIELD_COUNT] = Default::default();
        let mut set = |field: Field, value: String| values[field as usize] = value;
        set(Field::Name, account.name.clone().unwrap_or_default());
        set(Field::Email, account.email.clone().unwrap_or_default());
        set(Field::Username, remote.username.clone());
        set(Field::ClientId, oauth.client_id.clone());
        set(Field::ClientSecret, oauth.client_secret.clone());
        set(Field::AuthUrl, oauth.auth_url.clone());
        set(Field::TokenUrl, oauth.token_url.clone());
        set(Field::Scope, oauth.scope.clone());
        set(Field::ImapHost, remote.imap_host.clone());
        set(Field::ImapPort, remote.imap_port.to_string());
        set(Field::SmtpHost, remote.smtp_host.clone());
        set(Field::SmtpPort, remote.smtp_port.to_string());
        set(Field::SmtpUsername, remote.smtp_username.clone());
        set(Field::Inbox, remote.inbox.clone());
        set(Field::Sent, remote.sent.clone());
        let make_default = config.maildir.is_none()
            || config.default_account.as_deref() == Some(account.id.as_str());
        let mut form = Self {
            mode,
            values,
            field: Field::Name,
            sign_in,
            imap_security: remote.imap_security,
            smtp_security: remote.smtp_security,
            make_default,
            remember: remote.keyring,
            reveal: false,
            error: None,
            info: None,
            busy: false,
            request: 0,
            discovering: false,
            looked_up: String::new(),
            untrusted: None,
            oauth_link: None,
            tls_certificate: None,
            cursor_back: 0,
            account,
        };
        form.field = form.visible()[0];
        form
    }

    pub fn value(&self, field: Field) -> &str {
        &self.values[field as usize]
    }

    pub fn set(&mut self, field: Field, value: impl Into<String>) {
        self.values[field as usize] = value.into();
    }

    /// The lines on screen, in order.
    pub fn visible(&self) -> Vec<Field> {
        use Field::*;
        let oauth = self.sign_in.oauth();
        if self.mode == Mode::Login {
            let mut fields = Vec::new();
            if !oauth {
                fields.push(Password);
                if self.separate_smtp() {
                    fields.push(SmtpPassword);
                }
            }
            fields.push(Remember);
            return fields;
        }
        let mut fields = vec![Name, Email, SignIn, Username];
        if oauth {
            fields.extend([ClientId, ClientSecret]);
            if self.sign_in == self::SignIn::OtherOAuth {
                fields.extend([AuthUrl, TokenUrl, Scope]);
            }
        } else {
            fields.push(Password);
        }
        fields.extend([
            ImapHost,
            ImapPort,
            ImapSecurity,
            SmtpHost,
            SmtpPort,
            SmtpSecurity,
        ]);
        if !oauth {
            fields.push(SmtpUsername);
            if self.separate_smtp() {
                fields.push(SmtpPassword);
            }
        }
        fields.extend([Inbox, Sent, MakeDefault]);
        if !oauth {
            fields.push(Remember);
        }
        fields
    }

    fn separate_smtp(&self) -> bool {
        let smtp = self.value(Field::SmtpUsername).trim();
        !smtp.is_empty() && smtp != self.username()
    }

    fn username(&self) -> String {
        let typed = self.value(Field::Username).trim();
        if typed.is_empty() {
            self.value(Field::Email).trim().to_owned()
        } else {
            typed.to_owned()
        }
    }

    pub fn move_field(&mut self, backwards: bool) {
        let visible = self.visible();
        let index = visible
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let count = visible.len();
        self.field = visible[if backwards {
            (index + count - 1) % count
        } else {
            (index + 1) % count
        }];
        self.cursor_back = 0;
        self.error = None;
    }

    pub fn input(&mut self, text: &str) {
        if self.field.is_text() {
            let back = self.cursor_back;
            crate::input::insert_text(&mut self.values[self.field as usize], back, text);
            self.error = None;
        }
    }

    /// Fills the server lines from discovered settings. Lines the user already typed in
    /// are kept unless they are the defaults.
    pub fn apply(&mut self, found: &crate::discover::Found) {
        self.set(Field::ImapHost, found.imap_host.clone());
        self.set(Field::ImapPort, found.imap_port.to_string());
        self.set(Field::SmtpHost, found.smtp_host.clone());
        self.set(Field::SmtpPort, found.smtp_port.to_string());
        self.imap_security = found.imap_security;
        self.smtp_security = found.smtp_security;
        if found.local_username && self.value(Field::Username).trim().is_empty() {
            let local = self
                .value(Field::Email)
                .split('@')
                .next()
                .unwrap_or_default()
                .to_owned();
            self.set(Field::Username, local);
        }
        let mut info = format!("Settings from {}.", found.source);
        if let Some(note) = found.note {
            info.push(' ');
            info.push_str(note);
        }
        self.info = Some(info);
    }

    /// Handles a key, returning what the app should do. Escape is left to the app.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('s' | 'S') if control => return Action::Submit,
            KeyCode::Char('r' | 'R') if control => {
                self.reveal = !self.reveal;
                return Action::None;
            }
            KeyCode::Char('t' | 'T') if control && self.untrusted.is_some() => {
                return Action::Trust;
            }
            KeyCode::Char('y' | 'Y') if control && self.oauth_link.is_some() => {
                return Action::CopyLink;
            }
            KeyCode::Char('f' | 'F') if control && self.mode == Mode::Add => {
                self.looked_up.clear();
                return self.lookup();
            }
            _ => {}
        }
        let visible = self.visible();
        let last = visible.last().copied() == Some(self.field);
        match key.code {
            KeyCode::Enter if self.mode == Mode::Login || last => Action::Submit,
            KeyCode::Tab | KeyCode::Down | KeyCode::Enter => {
                let leaving = self.field;
                self.move_field(false);
                if leaving == Field::Email && self.mode == Mode::Add {
                    return self.lookup();
                }
                Action::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_field(true);
                Action::None
            }
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right if !self.field.is_text() => {
                let back = key.code == KeyCode::Left;
                match self.field {
                    Field::SignIn => self.sign_in = self.sign_in.next(back),
                    Field::ImapSecurity => {
                        self.imap_security = next_security(self.imap_security, back)
                    }
                    Field::SmtpSecurity => {
                        self.smtp_security = next_security(self.smtp_security, back)
                    }
                    Field::MakeDefault => self.make_default = !self.make_default,
                    Field::Remember => self.remember = !self.remember,
                    _ => {}
                }
                self.error = None;
                Action::None
            }
            _ if self.field.is_text() => {
                let mut back = self.cursor_back;
                if crate::input::edit_text(&mut self.values[self.field as usize], &mut back, key) {
                    self.error = None;
                }
                self.cursor_back = back;
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Applies built-in settings right away and asks for an online lookup when the
    /// address's domain is not one of them.
    fn lookup(&mut self) -> Action {
        let email = self.value(Field::Email).trim().to_owned();
        let Some((_, domain)) = email.rsplit_once('@') else {
            return Action::None;
        };
        let domain = domain.to_ascii_lowercase();
        if domain == self.looked_up || !domain.contains('.') {
            return Action::None;
        }
        self.looked_up = domain.clone();
        if let Some(found) = crate::discover::known(&domain) {
            self.apply(&found);
            return Action::None;
        }
        self.discovering = true;
        self.info = Some(format!("Looking up the servers for {domain}…"));
        Action::Discover(email)
    }

    pub fn build(&self) -> Result<Account, (Field, String)> {
        let mut account = self.account.clone();
        if self.mode != Mode::Login {
            let email = self.value(Field::Email).trim();
            if !email_address::EmailAddress::is_valid(email) {
                return Err((Field::Email, "Enter a valid email address.".into()));
            }
            account.name = Some(self.value(Field::Name).trim().to_owned());
            account.email = Some(email.to_owned());
            let mut remote = account.remote.clone().unwrap_or_default();
            remote.username = self.username();
            remote.imap_host = self.value(Field::ImapHost).trim().to_owned();
            remote.imap_port = port(self.value(Field::ImapPort)).ok_or((
                Field::ImapPort,
                "Enter an IMAP port from 1 to 65535.".into(),
            ))?;
            remote.imap_security = self.imap_security;
            remote.smtp_host = self.value(Field::SmtpHost).trim().to_owned();
            remote.smtp_port = port(self.value(Field::SmtpPort)).ok_or((
                Field::SmtpPort,
                "Enter an SMTP port from 1 to 65535.".into(),
            ))?;
            remote.smtp_security = self.smtp_security;
            remote.inbox = self.value(Field::Inbox).trim().to_owned();
            remote.sent = self.value(Field::Sent).trim().to_owned();
            if self.sign_in.oauth() {
                let client_id = self.value(Field::ClientId).trim();
                if client_id.is_empty() {
                    return Err((
                        Field::ClientId,
                        "Enter the OAuth client ID you created for termail with your provider."
                            .into(),
                    ));
                }
                remote.auth = Auth::Xoauth2;
                remote.smtp_username.clear();
                remote.oauth2 = Some(OAuth2Config {
                    provider: match self.sign_in {
                        SignIn::Google => "gmail".into(),
                        SignIn::Microsoft => "outlook".into(),
                        _ => String::new(),
                    },
                    client_id: client_id.to_owned(),
                    client_secret: self.value(Field::ClientSecret).trim().to_owned(),
                    auth_url: self.value(Field::AuthUrl).trim().to_owned(),
                    token_url: self.value(Field::TokenUrl).trim().to_owned(),
                    scope: self.value(Field::Scope).trim().to_owned(),
                });
            } else {
                remote.auth = Auth::Password;
                remote.oauth2 = None;
                remote.smtp_username = self.value(Field::SmtpUsername).trim().to_owned();
            }
            remote.validate().map_err(|error| {
                let field = if error.contains("oauth2") || error.contains("token_url") {
                    Field::ClientId
                } else if error.contains("SMTP username") {
                    Field::SmtpUsername
                } else {
                    Field::ImapHost
                };
                (field, error)
            })?;
            if let Some(old) = &self.account.remote
                && (old.imap_host != remote.imap_host
                    || old.imap_port != remote.imap_port
                    || old.username != remote.username
                    || old.inbox != remote.inbox)
            {
                return Err((Field::ImapHost, "Add a new account to change the IMAP server, username or Inbox. This keeps cached mail separate.".into()));
            }
            account.remote = Some(remote);
        }
        let oauth = self.sign_in.oauth();
        let remote = account
            .remote
            .as_mut()
            .ok_or((Field::Password, "This account has no IMAP settings.".into()))?;
        if let Some(path) = &self.tls_certificate {
            remote.tls_certificate = Some(path.clone());
        }
        // A refresh token must outlive the session, so OAuth2 always uses the keyring.
        remote.keyring = self.remember || oauth;
        if oauth {
            return finish(account);
        }
        let password = self.value(Field::Password);
        if !password.is_empty() {
            remote.session_password = Some(session(password));
        }
        if password.is_empty()
            && (!remote.has_credentials() || self.remember && !remote.keyring_has_password())
        {
            return Err((Field::Password, "Enter the account password.".into()));
        }
        if remote.separate_smtp() {
            let smtp = self.value(Field::SmtpPassword);
            if !smtp.is_empty() {
                remote.smtp_session_password = Some(session(smtp));
            } else if remote.smtp_password_env.is_empty()
                && remote.smtp_password_command.is_empty()
                && !(self.remember && remote.smtp_keyring_has_password())
            {
                return Err((Field::SmtpPassword, "Enter the SMTP password.".into()));
            }
        }
        finish(account)
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let title = match self.mode {
            Mode::Add => " add account ",
            Mode::Edit => " account settings ",
            Mode::Login => " sign in ",
        };
        let block = Block::bordered()
            .title(title)
            .border_style(theme::border(true))
            .title_style(theme::title(true))
            .style(theme::text().bg(theme::surface()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .split(inner);
        let hint = if self.mode == Mode::Login {
            format!(
                "{} · {}",
                self.account.email.as_deref().unwrap_or_default(),
                self.value(Field::ImapHost)
            )
        } else {
            "Type your address and press tab: termail fills in the servers it can find.\nUsername defaults to your email address.".into()
        };
        frame.render_widget(
            Paragraph::new(hint)
                .style(theme::muted())
                .wrap(Wrap { trim: false }),
            rows[0],
        );
        let width = usize::from(rows[1].width.saturating_sub(18));
        let items: Vec<ListItem> = self
            .visible()
            .iter()
            .map(|&field| {
                let label = format!("{:<15} ", field.label());
                let value = match field {
                    Field::SignIn => format!("< {} >", self.sign_in.label()),
                    Field::ImapSecurity => format!("< {} >", security_label(self.imap_security)),
                    Field::SmtpSecurity => format!("< {} >", security_label(self.smtp_security)),
                    Field::MakeDefault => if self.make_default { "[x]" } else { "[ ]" }.into(),
                    Field::Remember => if self.remember { "[x]" } else { "[ ]" }.into(),
                    _ if field.secret() && !self.reveal => {
                        "•".repeat(self.value(field).chars().count())
                    }
                    _ => self.value(field).to_owned(),
                };
                // Long values show their end, where the typing happens.
                let value: String = {
                    let characters: Vec<char> = value.chars().collect();
                    characters[characters.len().saturating_sub(width)..]
                        .iter()
                        .collect()
                };
                let line = if field == self.field && field.is_text() {
                    let back = self.cursor_back.min(value.chars().count());
                    crate::ui::cursor_line(&label, &value, back)
                } else {
                    Line::from(vec![Span::raw(label), Span::raw(value)])
                };
                ListItem::new(line)
            })
            .collect();
        let mut selection = ListState::default()
            .with_selected(self.visible().iter().position(|field| *field == self.field));
        frame.render_stateful_widget(
            List::new(items)
                .style(theme::text())
                .highlight_style(theme::selection()),
            rows[1],
            &mut selection,
        );
        let message = if let Some(link) = self.oauth_link.as_ref().filter(|_| self.busy) {
            format!(
                "Finish signing in in the browser. If it did not open, ctrl+y copies the link:\n{link}"
            )
        } else if self.busy {
            "Checking IMAP and SMTP… No message is sent.".into()
        } else if let Some(error) = &self.error {
            error.clone()
        } else if let Some(presented) = &self.untrusted {
            format!(
                "{} uses a certificate this computer does not trust.\nSHA-256 {}\nCompare it with the server's, then press ctrl+t to trust it.",
                presented.host, presented.fingerprint
            )
        } else if let Some(info) = &self.info {
            info.clone()
        } else if self.sign_in.oauth() {
            "termail opens the provider's sign-in page when you save. The token stays in the system keyring.".into()
        } else if self.remember {
            "Password is saved in the system keyring.".into()
        } else if self.mode == Mode::Edit {
            "Password stays in memory. Leave it blank to keep the current one, or turn on Remember."
                .into()
        } else {
            "Password stays in memory until you quit. Turn on Remember to use the system keyring."
                .into()
        };
        let style = if self.error.is_some() && !self.busy {
            theme::title(true)
        } else {
            theme::muted()
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(style)
                .wrap(Wrap { trim: false }),
            rows[2],
        );
        let actions = if self.busy {
            "esc  Cancel"
        } else if self.mode == Mode::Login {
            "enter  Sign in · space  Remember · ctrl+r  show password · esc  Work offline"
        } else {
            "tab  Move · space/←→  Change choice · ctrl+s  Save and connect\nctrl+f  Find servers · ctrl+r  Show passwords · esc  Cancel"
        };
        frame.render_widget(
            Paragraph::new(actions)
                .style(theme::muted())
                .wrap(Wrap { trim: false }),
            rows[3],
        );
    }
}

fn port(value: &str) -> Option<u16> {
    value.trim().parse().ok().filter(|port| *port > 0)
}

fn session(value: &str) -> std::sync::Arc<SessionPassword> {
    std::sync::Arc::new(SessionPassword(zeroize::Zeroizing::new(value.to_owned())))
}

fn finish(account: Account) -> Result<Account, (Field, String)> {
    if account.maildir.is_none() {
        return Err((
            Field::Name,
            "Start with --config PATH to choose where settings are saved.".into(),
        ));
    }
    Ok(account)
}

/// Checks the account against its servers without sending mail. For OAuth2 it first
/// signs in through the browser and keeps the refresh token in the returned account.
/// An untrusted certificate comes back so the user can decide about it.
pub fn verify(
    mut account: Account,
    mode: Mode,
    cancelled: &std::sync::atomic::AtomicBool,
    show_link: &mut dyn FnMut(&str),
) -> Result<Account, Failure> {
    let stop = || -> Result<(), Failure> {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            Err("Sign-in cancelled.".to_owned().into())
        } else {
            Ok(())
        }
    };
    stop()?;
    let remote = account
        .remote
        .as_mut()
        .ok_or_else(|| Failure::from("Set IMAP and SMTP details before connecting.".to_owned()))?;
    if remote.oauth2.is_some() && (mode != Mode::Edit || !remote.keyring_has_password()) {
        let refresh = crate::oauth::authorize(remote, cancelled, show_link)?;
        remote.session_password = Some(session(&refresh));
        remote.forget_cached();
    }
    stop()?;
    let remote = account.remote.as_ref().expect("checked above");
    let imap = crate::imap_client::connect(remote).and_then(|mut session| {
        let folders = crate::imap_client::verify_folders(&mut session, remote);
        let _ = session.logout();
        folders.map_err(RemoteError::from)
    });
    untrusted(imap, remote, crate::certs::Protocol::Imap)?;
    stop()?;
    untrusted(
        crate::sender::check_connection(remote),
        remote,
        crate::certs::Protocol::Smtp,
    )?;
    Ok(account)
}

/// Turns a certificate error into an offer to trust the certificate the server shows.
fn untrusted(
    result: Result<(), RemoteError>,
    remote: &RemoteConfig,
    protocol: crate::certs::Protocol,
) -> Result<(), Failure> {
    let Err(error) = result else {
        return Ok(());
    };
    if error.kind != crate::remote::ErrorKind::Certificate {
        return Err(error.message.into());
    }
    let (host, port, security) = match protocol {
        crate::certs::Protocol::Imap => (&remote.imap_host, remote.imap_port, remote.imap_security),
        crate::certs::Protocol::Smtp => (&remote.smtp_host, remote.smtp_port, remote.smtp_security),
    };
    match crate::certs::fetch(host, port, protocol, security.starttls(port)) {
        Ok(presented) => Err(Failure {
            message: error.message,
            untrusted: Some(presented),
        }),
        Err(reason) => Err(format!("{} ({reason})", error.message).into()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    pub fn filled_form(config: &Config) -> Form {
        let mut form = Form::new(config, Mode::Add);
        form.set(Field::Name, "Test account");
        form.set(Field::Email, "test@example.test");
        form.set(Field::Password, "fake-password");
        form.set(Field::ImapHost, "localhost");
        form.set(Field::SmtpHost, "localhost");
        form
    }

    #[test]
    fn validates_fields_and_defaults_username_to_email() {
        let config = Config {
            path: Some(std::env::temp_dir().join("termail-form-test/config.toml")),
            ..Default::default()
        };
        let mut form = filled_form(&config);
        let account = form.build().unwrap();
        assert_eq!(
            account.remote.as_ref().unwrap().username,
            "test@example.test"
        );
        assert_eq!(
            account.remote.as_ref().unwrap().password().unwrap(),
            "fake-password"
        );
        assert!(account.maildir.unwrap().ends_with("mail/account-1"));
        form.set(Field::Email, "invalid");
        assert_eq!(form.build().unwrap_err().0, Field::Email);
        form.set(Field::Email, "test@example.test");
        form.set(Field::ImapPort, "65536");
        assert_eq!(form.build().unwrap_err().0, Field::ImapPort);
        form.set(Field::ImapPort, "993");
        form.set(Field::Password, "");
        assert_eq!(form.build().unwrap_err().0, Field::Password);
    }

    #[test]
    fn editing_cannot_rebind_an_existing_mail_cache() {
        let mut config = Config {
            path: Some(std::env::temp_dir().join("termail-form-test/config.toml")),
            ..Default::default()
        };
        config.accounts.push(filled_form(&config).build().unwrap());
        config.activate(0);
        let mut form = Form::new(&config, Mode::Edit);
        assert!(form.build().is_ok());
        form.set(Field::ImapHost, "other.example.test");
        assert!(form.build().unwrap_err().1.contains("new account"));
    }

    #[test]
    fn renders_masked_passwords_and_handles_small_terminals() {
        let config = Config::default();
        let mut form = filled_form(&config);
        form.field = Field::Password;
        for (width, height) in [(80, 30), (40, 12), (12, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| form.draw(frame, frame.area()))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(!text.contains("fake-password"));
            if width == 80 {
                assert!(text.contains("Password"));
                assert!(text.contains("•••"));
            }
        }
        form.mode = Mode::Login;
        form.field = Field::Password;
        form.move_field(false);
        assert_eq!(form.field, Field::Remember);
        form.move_field(false);
        assert_eq!(form.field, Field::Password);
    }
}
