use std::time::Duration;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password_env: String,
    pub inbox: String,
    pub sent: String,
    pub sync_interval_secs: u64,
    pub batch_size: usize,
    pub timeout_secs: u64,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 465,
            username: String::new(),
            password_env: String::new(),
            inbox: "INBOX".into(),
            sent: "INBOX.Sent".into(),
            sync_interval_secs: 60,
            batch_size: 200,
            timeout_secs: 30,
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
        if self.password_env.is_empty()
            || !self
                .password_env
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("Set password_env to the name of an environment variable.".into());
        }
        for folder in [&self.inbox, &self.sent] {
            if folder.is_empty() || folder.chars().any(char::is_control) {
                return Err("Set valid IMAP inbox and sent folder names.".into());
            }
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

    pub fn password(&self) -> Result<String, String> {
        std::env::var(&self.password_env)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("Set {} before connecting.", self.password_env))
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
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
