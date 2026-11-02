use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct SessionPassword(pub zeroize::Zeroizing<String>);

impl std::fmt::Debug for SessionPassword {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    #[default]
    Auto,
    Tls,
    Starttls,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    #[default]
    Password,
    Xoauth2,
}

impl Security {
    pub fn starttls(self, port: u16) -> bool {
        match self {
            Self::Auto => matches!(port, 143 | 587 | 25),
            Self::Tls => false,
            Self::Starttls => true,
        }
    }

    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

impl Auth {
    fn is_password(&self) -> bool {
        *self == Self::Password
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    pub imap_host: String,
    pub imap_port: u16,
    #[serde(skip_serializing_if = "Security::is_auto")]
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    #[serde(skip_serializing_if = "Security::is_auto")]
    pub smtp_security: Security,
    pub username: String,
    #[serde(skip_serializing_if = "Auth::is_password")]
    pub auth: Auth,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password_env: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub password_command: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub keyring: bool,
    /// Seconds a secret from `password_command` or an OAuth2 refresh stays cached.
    #[serde(skip_serializing_if = "is_default_cache")]
    pub password_cache_secs: u64,
    /// Refreshes XOAUTH2 access tokens from a stored refresh token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth2: Option<OAuth2Config>,
    /// IMAP folders watched with IDLE; empty watches the Inbox.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub watch: Vec<String>,
    /// Messages larger than this many bytes download headers first; zero downloads all.
    #[serde(skip_serializing_if = "is_zero")]
    pub max_download_bytes: u64,
    /// A different SMTP sign-in, for the few providers that need one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub smtp_username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub smtp_password_env: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub smtp_password_command: Vec<String>,
    /// PEM certificates to trust for this account's servers, besides the system's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_certificate: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub smtp_session_password: Option<std::sync::Arc<SessionPassword>>,
    pub inbox: String,
    pub sent: String,
    pub sync_interval_secs: u64,
    pub batch_size: usize,
    pub timeout_secs: u64,
    #[serde(skip)]
    pub session_password: Option<std::sync::Arc<SessionPassword>>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: 993,
            imap_security: Security::Auto,
            smtp_host: String::new(),
            smtp_port: 465,
            smtp_security: Security::Auto,
            username: String::new(),
            auth: Auth::Password,
            password_env: String::new(),
            password_command: Vec::new(),
            keyring: false,
            password_cache_secs: DEFAULT_CACHE_SECS,
            oauth2: None,
            watch: Vec::new(),
            max_download_bytes: 0,
            smtp_username: String::new(),
            smtp_password_env: String::new(),
            smtp_password_command: Vec::new(),
            tls_certificate: None,
            smtp_session_password: None,
            inbox: "INBOX".into(),
            sent: String::new(),
            sync_interval_secs: 60,
            batch_size: 200,
            timeout_secs: 30,
            session_password: None,
        }
    }
}

const DEFAULT_CACHE_SECS: u64 = 300;

fn is_default_cache(value: &u64) -> bool {
    *value == DEFAULT_CACHE_SECS
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// OAuth2 endpoints and client used to turn a refresh token into access tokens.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuth2Config {
    /// `gmail` or `outlook` fill in the endpoints and scope.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    pub client_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub auth_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub token_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

impl OAuth2Config {
    fn preset(&self) -> (&'static str, &'static str, &'static str) {
        match self.provider.to_ascii_lowercase().as_str() {
            "gmail" | "google" => (
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                "https://mail.google.com/",
            ),
            "outlook" | "microsoft" | "office365" => (
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access",
            ),
            _ => ("", "", ""),
        }
    }

    pub fn auth_url(&self) -> String {
        pick(&self.auth_url, self.preset().0)
    }

    pub fn token_url(&self) -> String {
        pick(&self.token_url, self.preset().1)
    }

    pub fn scope(&self) -> String {
        pick(&self.scope, self.preset().2)
    }
}

fn pick(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.into()
    } else {
        value.into()
    }
}

/// Why talking to the server failed, so callers can react without reading messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// Credentials are missing or were rejected.
    Auth,
    /// The server lacks a capability the operation needs.
    Unsupported,
    /// The server's certificate is not trusted.
    Certificate,
    Other,
}

#[derive(Clone, Debug)]
pub struct RemoteError {
    pub kind: ErrorKind,
    pub message: String,
}

impl RemoteError {
    pub fn auth(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Auth,
            message: message.into(),
        }
    }

    pub fn certificate(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Certificate,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unsupported,
            message: message.into(),
        }
    }
}

impl From<String> for RemoteError {
    fn from(message: String) -> Self {
        Self {
            kind: ErrorKind::Other,
            message,
        }
    }
}

impl From<&str> for RemoteError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl From<RemoteError> for String {
    fn from(error: RemoteError) -> Self {
        error.message
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

type Cached = (zeroize::Zeroizing<String>, std::time::Instant);

/// Secrets from commands and token refreshes, so periodic syncs do not rerun them.
fn secret_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Cached>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Cached>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

impl RemoteConfig {
    fn cache_key(&self, kind: &str) -> String {
        format!("{kind}:{}@{}", self.username, self.imap_host)
    }

    fn cached(&self, kind: &str) -> Option<String> {
        let cache = secret_cache().lock().ok()?;
        let (value, expires) = cache.get(&self.cache_key(kind))?;
        (std::time::Instant::now() < *expires).then(|| value.to_string())
    }

    fn remember(&self, kind: &str, value: &str, lifetime: Duration) {
        if lifetime.is_zero() {
            return;
        }
        if let Ok(mut cache) = secret_cache().lock() {
            cache.insert(
                self.cache_key(kind),
                (
                    zeroize::Zeroizing::new(value.to_owned()),
                    std::time::Instant::now() + lifetime,
                ),
            );
        }
    }

    /// Drops cached secrets after the server rejected them.
    pub fn forget_cached(&self) {
        if let Ok(mut cache) = secret_cache().lock() {
            cache
                .retain(|key, _| !key.ends_with(&format!(":{}@{}", self.username, self.imap_host)));
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for host in [&self.imap_host, &self.smtp_host] {
            // IPv6 addresses contain colons; anything else with one is a URL or a port.
            let ipv6 = host.parse::<std::net::Ipv6Addr>().is_ok();
            if host.is_empty()
                || host
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "/@".contains(c))
                || (host.contains(':') && !ipv6)
            {
                return Err(
                    "IMAP and SMTP hosts must be hostnames without URLs or credentials.".into(),
                );
            }
        }
        if self.imap_port == 0 || self.smtp_port == 0 {
            return Err("IMAP and SMTP ports must be nonzero.".into());
        }
        if self.username.is_empty() || self.username.chars().any(char::is_control) {
            return Err("Set the remote account username.".into());
        }
        if self.smtp_username.chars().any(char::is_control) {
            return Err("Set a valid SMTP username.".into());
        }
        if ![&self.password_env, &self.smtp_password_env]
            .iter()
            .all(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return Err("Set password_env to the name of an environment variable.".into());
        }
        if self.inbox.is_empty() || self.inbox.chars().any(char::is_control) {
            return Err("Set valid IMAP inbox and sent folder names.".into());
        }
        if self.sent.chars().any(char::is_control) {
            return Err("Set valid IMAP inbox and sent folder names.".into());
        }
        if self.inbox.eq_ignore_ascii_case(&self.sent) {
            return Err("Inbox and Sent must be different IMAP folders.".into());
        }
        if !(1..=1000).contains(&self.batch_size) || !(1..=120).contains(&self.timeout_secs) {
            return Err("batch_size must be 1–1000 and timeout_secs must be 1–120.".into());
        }
        if self.sync_interval_secs != 0 && self.sync_interval_secs < 30 {
            return Err("sync_interval_secs must be zero or at least 30.".into());
        }
        if self
            .watch
            .iter()
            .any(|name| name.is_empty() || name.chars().any(char::is_control))
        {
            return Err("Set valid IMAP folder names in watch.".into());
        }
        if let Some(oauth) = &self.oauth2 {
            if self.auth != Auth::Xoauth2 {
                return Err("oauth2 settings need auth = \"xoauth2\".".into());
            }
            if oauth.client_id.is_empty() || oauth.token_url().is_empty() {
                return Err(
                    "Set oauth2 client_id and provider (gmail, outlook) or token_url.".into(),
                );
            }
        }
        Ok(())
    }

    /// Reports whether a secret source is available without running commands or
    /// touching the OS keyring, so the UI thread can call it cheaply.
    pub fn has_credentials(&self) -> bool {
        self.session_password
            .as_ref()
            .is_some_and(|password| !password.0.is_empty())
            || !self.password_command.is_empty()
            || self.keyring
            || (!self.password_env.is_empty()
                && std::env::var_os(&self.password_env).is_some_and(|value| !value.is_empty()))
    }

    /// The secret sent at sign-in: the password, or an access token for XOAUTH2.
    pub fn password(&self) -> Result<String, String> {
        let Some(oauth) = &self.oauth2 else {
            return self.stored_secret();
        };
        if let Some(token) = self.cached("access") {
            return Ok(token);
        }
        let refresh = self.stored_secret()?;
        let (token, lifetime) = refresh_access_token(oauth, &refresh)?;
        self.remember(
            "access",
            &token,
            lifetime.saturating_sub(Duration::from_secs(60)),
        );
        Ok(token)
    }

    /// The secret the account stores: a password, access token or OAuth2 refresh token.
    pub fn stored_secret(&self) -> Result<String, String> {
        if let Some(password) = &self.session_password
            && !password.0.is_empty()
        {
            return Ok(password.0.to_string());
        }
        if !self.password_env.is_empty()
            && let Some(value) = std::env::var(&self.password_env)
                .ok()
                .filter(|value| !value.is_empty())
        {
            return Ok(value);
        }
        if !self.password_command.is_empty() {
            if let Some(value) = self.cached("command") {
                return Ok(value);
            }
            let value = run_password_command(&self.password_command)?;
            self.remember(
                "command",
                &value,
                Duration::from_secs(self.password_cache_secs),
            );
            return Ok(value);
        }
        if self.keyring {
            return keyring_entry(self)?
                .get_password()
                .map_err(|_| "No password is stored in the system keyring. Sign in again.".into());
        }
        Err(if self.password_env.is_empty() {
            "Sign in to enter the account password.".into()
        } else {
            format!("Set {} before connecting.", self.password_env)
        })
    }

    /// Whether SMTP signs in with its own username instead of the IMAP one.
    pub fn separate_smtp(&self) -> bool {
        !self.smtp_username.is_empty() && self.smtp_username != self.username
    }

    pub fn smtp_login(&self) -> &str {
        if self.separate_smtp() {
            &self.smtp_username
        } else {
            &self.username
        }
    }

    /// The SMTP secret: the account's own, unless SMTP has a separate username.
    pub fn smtp_password(&self) -> Result<String, String> {
        if !self.separate_smtp() || self.oauth2.is_some() {
            return self.password();
        }
        if let Some(password) = &self.smtp_session_password
            && !password.0.is_empty()
        {
            return Ok(password.0.to_string());
        }
        if !self.smtp_password_env.is_empty()
            && let Some(value) = std::env::var(&self.smtp_password_env)
                .ok()
                .filter(|value| !value.is_empty())
        {
            return Ok(value);
        }
        if !self.smtp_password_command.is_empty() {
            if let Some(value) = self.cached("smtp-command") {
                return Ok(value);
            }
            let value = run_password_command(&self.smtp_password_command)?;
            self.remember(
                "smtp-command",
                &value,
                Duration::from_secs(self.password_cache_secs),
            );
            return Ok(value);
        }
        if self.keyring {
            return smtp_keyring_entry(self)?.get_password().map_err(|_| {
                "No SMTP password is stored in the system keyring. Sign in again.".into()
            });
        }
        Err("Sign in to enter the SMTP password.".into())
    }

    pub fn store_smtp_keyring_password(&self, password: &str) -> Result<(), String> {
        smtp_keyring_entry(self)?
            .set_password(password)
            .map_err(|_| "Unable to save the SMTP password in the system keyring.".into())
    }

    pub fn smtp_keyring_has_password(&self) -> bool {
        smtp_keyring_entry(self).is_ok_and(|entry| entry.get_password().is_ok())
    }

    /// The certificates from `tls_certificate`, as PEM blocks.
    pub fn extra_certificates(&self) -> Result<Vec<Vec<u8>>, String> {
        let Some(path) = &self.tls_certificate else {
            return Ok(Vec::new());
        };
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
        let blocks: Vec<Vec<u8>> = text
            .split_inclusive("-----END CERTIFICATE-----")
            .filter_map(|block| {
                let start = block.find("-----BEGIN CERTIFICATE-----")?;
                Some(block.as_bytes()[start..].to_vec())
            })
            .collect();
        if blocks.is_empty() {
            return Err(format!("{} holds no PEM certificate.", path.display()));
        }
        Ok(blocks)
    }

    /// A TLS connector that trusts the system's certificates and the account's own.
    pub fn tls_connector(&self) -> Result<native_tls::TlsConnector, String> {
        let mut builder = native_tls::TlsConnector::builder();
        for pem in self.extra_certificates()? {
            let certificate = native_tls::Certificate::from_pem(&pem)
                .map_err(|_| "The trusted certificate file is not valid PEM.".to_owned())?;
            builder.add_root_certificate(certificate);
        }
        builder
            .build()
            .map_err(crate::log::fail("Unable to configure TLS."))
    }

    pub fn keyring_has_password(&self) -> bool {
        keyring_entry(self).is_ok_and(|entry| entry.get_password().is_ok())
    }

    pub fn store_keyring_password(&self, password: &str) -> Result<(), String> {
        keyring_entry(self)?
            .set_password(password)
            .map_err(|_| "Unable to save the password in the system keyring.".into())
    }

    pub fn forget_keyring_password(&self) {
        if let Ok(entry) = keyring_entry(self) {
            let _ = entry.delete_credential();
        }
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// Exchanges a refresh token for an access token and its lifetime.
fn refresh_access_token(oauth: &OAuth2Config, refresh: &str) -> Result<(String, Duration), String> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", oauth.client_id.as_str()),
    ];
    if !oauth.client_secret.is_empty() {
        form.push(("client_secret", oauth.client_secret.as_str()));
    }
    let response = token_request(&oauth.token_url(), &form)?;
    let token = response["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or("The OAuth2 server returned no access token. Run termail oauth again.")?
        .to_owned();
    let lifetime = response["expires_in"].as_u64().unwrap_or(3600);
    Ok((token, Duration::from_secs(lifetime)))
}

pub(crate) fn token_request(url: &str, form: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    if !url.starts_with("https://") && !url.starts_with("http://127.0.0.1") {
        return Err("The OAuth2 token_url must use https.".into());
    }
    let agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();
    let mut response = agent
        .post(url)
        .send_form(form.iter().copied())
        .map_err(crate::log::fail("Unable to reach the OAuth2 server."))?;
    let status = response.status();
    let body: serde_json::Value = response.body_mut().read_json().map_err(crate::log::fail(
        "The OAuth2 server sent an unreadable reply.",
    ))?;
    if !status.is_success() {
        let reason = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("request refused");
        crate::log::write("oauth2", format_args!("{status} {reason}"));
        return Err(format!(
            "OAuth2 sign-in failed ({reason}). Run termail oauth again."
        ));
    }
    Ok(body)
}

fn smtp_keyring_entry(remote: &RemoteConfig) -> Result<keyring::Entry, String> {
    keyring::Entry::new(
        "termail",
        &format!("smtp:{}@{}", remote.smtp_login(), remote.smtp_host),
    )
    .map_err(|_| "The system keyring is unavailable.".into())
}

fn keyring_entry(remote: &RemoteConfig) -> Result<keyring::Entry, String> {
    keyring::Entry::new(
        "termail",
        &format!("{}@{}", remote.username, remote.imap_host),
    )
    .map_err(|_| "The system keyring is unavailable.".into())
}

fn run_password_command(command: &[String]) -> Result<String, String> {
    let (program, arguments) = command.split_first().ok_or("password_command is empty.")?;
    let output = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| format!("Unable to run password_command {program}."))?;
    if !output.status.success() {
        return Err(format!("password_command {program} failed."));
    }
    let mut output = zeroize::Zeroizing::new(output.stdout);
    let value = std::str::from_utf8(&output)
        .map_err(|_| "password_command printed invalid UTF-8.")?
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    use zeroize::Zeroize;
    output.zeroize();
    if value.is_empty() {
        return Err(format!("password_command {program} printed nothing."));
    }
    Ok(value)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        net::{TcpListener, TcpStream},
        thread::JoinHandle,
    };

    pub struct Server {
        listener: TcpListener,
        acceptor: native_tls::TlsAcceptor,
        pub pem: String,
        pub port: u16,
    }

    impl Server {
        pub fn new() -> Self {
            let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let pem = key.cert.pem();
            let identity = native_tls::Identity::from_pkcs8(
                pem.as_bytes(),
                key.signing_key.serialize_pem().as_bytes(),
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            Self {
                listener,
                acceptor: native_tls::TlsAcceptor::new(identity).unwrap(),
                pem,
                port,
            }
        }

        pub fn connector(&self) -> native_tls::TlsConnector {
            native_tls::TlsConnector::builder()
                .add_root_certificate(
                    native_tls::Certificate::from_pem(self.pem.as_bytes()).unwrap(),
                )
                .build()
                .unwrap()
        }

        pub fn remote(&self) -> RemoteConfig {
            RemoteConfig {
                imap_host: "localhost".into(),
                imap_port: self.port,
                smtp_host: "localhost".into(),
                smtp_port: self.port,
                username: "test@example.test".into(),
                password_env: "TERMAIL_TEST_MISSING_PASSWORD_8D5F".into(),
                timeout_secs: 2,
                ..Default::default()
            }
        }

        pub fn into_parts(self) -> (TcpListener, native_tls::TlsAcceptor) {
            (self.listener, self.acceptor)
        }

        pub fn spawn<T: Send + 'static>(
            self,
            run: impl FnOnce(Option<native_tls::TlsStream<TcpStream>>) -> T + Send + 'static,
        ) -> JoinHandle<T> {
            std::thread::spawn(move || {
                let (stream, _) = self.listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                run(self.acceptor.accept(stream).ok())
            })
        }
    }

    #[test]
    fn validates_remote_settings_without_credentials_in_urls() {
        let mut remote = Server::new().remote();
        assert!(remote.validate().is_ok());
        remote.imap_host = "imaps://user:secret@localhost".into();
        assert!(remote.validate().is_err());
        remote.imap_host = "localhost".into();
        remote.batch_size = 0;
        assert!(remote.validate().is_err());
        remote.batch_size = 200;
        remote.sync_interval_secs = 1;
        assert!(remote.validate().is_err());
        remote.sync_interval_secs = 0;
        assert!(remote.validate().is_ok());
    }

    #[test]
    fn missing_password_is_reported_without_connecting() {
        let remote = Server::new().remote();
        assert!(
            crate::imap_client::sync(&remote, std::path::Path::new("unused-test-maildir"))
                .unwrap_err()
                .message
                .contains(&remote.password_env)
        );
    }
}
