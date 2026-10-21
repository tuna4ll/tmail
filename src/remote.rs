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
            inbox: "INBOX".into(),
            sent: String::new(),
            sync_interval_secs: 60,
            batch_size: 200,
            timeout_secs: 30,
            session_password: None,
        }
    }
}

impl RemoteConfig {
    pub fn validate(&self) -> Result<(), String> {
        for host in [&self.imap_host, &self.smtp_host] {
            if host.is_empty()
                || host
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "/:@".contains(c))
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
        if !self
            .password_env
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
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

    pub fn password(&self) -> Result<String, String> {
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
            return run_password_command(&self.password_command);
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
                .contains(&remote.password_env)
        );
    }
}
