use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
    process::Stdio,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Configuration,
    Temporary,
    Permanent,
    Uncertain,
}

#[derive(Debug)]
pub struct SendError {
    message: String,
    pub kind: FailureKind,
}

impl SendError {
    fn failed(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            kind: FailureKind::Configuration,
        }
    }

    fn uncertain(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            kind: FailureKind::Uncertain,
        }
    }
}

pub async fn send_smtp(
    remote: crate::remote::RemoteConfig,
    message: Vec<u8>,
) -> Result<(), SendError> {
    tokio::task::spawn_blocking(move || {
        remote.validate().map_err(SendError::failed)?;
        let transport = smtp_transport(&remote).map_err(SendError::failed)?;
        smtp_with_transport(&transport, &message)
    })
    .await
    .map_err(|_| SendError::uncertain("SMTP worker stopped. Check delivery before retrying."))?
}

pub fn check_connection(
    remote: &crate::remote::RemoteConfig,
) -> Result<(), crate::remote::RemoteError> {
    remote.validate()?;
    let secret = remote
        .smtp_password()
        .map_err(crate::remote::RemoteError::auth)?;
    check_transport(&smtp_transport(remote)?, remote, &secret)
}

fn smtp_transport(remote: &crate::remote::RemoteConfig) -> Result<lettre::SmtpTransport, String> {
    use lettre::transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{Certificate, Tls, TlsParameters},
    };
    let secret = remote.smtp_password()?;
    let mut parameters = TlsParameters::builder(remote.smtp_host.clone());
    for pem in remote.extra_certificates()? {
        parameters = parameters.add_root_certificate(
            Certificate::from_pem(&pem)
                .map_err(|_| "The trusted certificate file is not valid PEM.")?,
        );
    }
    let parameters = parameters
        .build()
        .map_err(|_| "Unable to configure SMTP TLS.")?;
    let builder = lettre::SmtpTransport::builder_dangerous(&remote.smtp_host).tls(
        if remote.smtp_security.starttls(remote.smtp_port) {
            Tls::Required(parameters)
        } else {
            Tls::Wrapper(parameters)
        },
    );
    let mechanisms = match remote.auth {
        crate::remote::Auth::Password => vec![Mechanism::Plain, Mechanism::Login],
        crate::remote::Auth::Xoauth2 => vec![Mechanism::Xoauth2],
    };
    Ok(builder
        .port(remote.smtp_port)
        .timeout(Some(remote.timeout()))
        .credentials(Credentials::new(remote.smtp_login().to_owned(), secret))
        .authentication(mechanisms)
        .build())
}

/// Signs in to SMTP without sending anything, and says what went wrong in words that
/// point at the fix: the address, the certificate or the password.
fn check_transport(
    transport: &lettre::SmtpTransport,
    remote: &crate::remote::RemoteConfig,
    secret: &str,
) -> Result<(), crate::remote::RemoteError> {
    use crate::remote::RemoteError;
    let host = format!("{}:{}", remote.smtp_host, remote.smtp_port);
    let error = match transport.test_connection() {
        Ok(true) => return Ok(()),
        Ok(false) => return Err(format!("{host} did not accept the connection.").into()),
        Err(error) => error,
    };
    let detail = if secret.is_empty() {
        error.to_string()
    } else {
        error.to_string().replace(secret, "[redacted]")
    };
    crate::log::write("smtp", &detail);
    let reason = crate::imap_client::short(&detail);
    if error.is_tls() {
        return Err(crate::imap_client::tls_failure(
            "SMTP",
            &remote.smtp_host,
            &detail,
        ));
    }
    if let Some(code) = error.status().map(|code| code.to_string()) {
        if matches!(code.as_str(), "535" | "534" | "530" | "454") {
            return Err(RemoteError::auth(format!(
                "SMTP rejected the username or password ({code}). The server said: {reason}.{}",
                crate::imap_client::sign_in_hint(&detail.to_lowercase())
            )));
        }
        return Err(format!("SMTP refused the sign-in ({code}): {reason}").into());
    }
    if error.is_timeout() {
        return Err(
            format!("{host} did not answer in time. Check the server name and port.").into(),
        );
    }
    Err(
        format!("Unable to reach {host}: {reason}. Check the server name, port and security.")
            .into(),
    )
}

fn smtp_with_transport(transport: &lettre::SmtpTransport, message: &[u8]) -> Result<(), SendError> {
    use lettre::Transport;
    let envelope = crate::message::envelope(message).map_err(SendError::failed)?;
    let from = envelope
        .from
        .parse()
        .map_err(|_| SendError::failed("Invalid SMTP sender."))?;
    let recipients = envelope
        .recipients
        .iter()
        .map(|address| address.parse())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SendError::failed("Invalid SMTP recipient."))?;
    let envelope =
        lettre::address::Envelope::new(Some(from), recipients).map_err(SendError::failed)?;
    transport
        .send_raw(&envelope, &crate::message::wire_message(message))
        .map(|_| ())
        .map_err(|error| {
            crate::log::write("smtp", &error);
            let code = error.status().map(|code| code.to_string());
            let mut cause = std::error::Error::source(&error);
            let mut tls_error = error.is_tls();
            while let Some(source) = cause {
                tls_error |= source.is::<native_tls::Error>()
                    || source.is::<native_tls::HandshakeError<std::net::TcpStream>>();
                cause = source.source();
            }
            let kind = if tls_error
                || code
                    .as_deref()
                    .is_some_and(|code| code == "535" || code == "534" || code == "530")
            {
                FailureKind::Configuration
            } else if error.is_transient() {
                FailureKind::Temporary
            } else if error.is_permanent() {
                FailureKind::Permanent
            } else {
                FailureKind::Uncertain
            };
            SendError {
                kind,
                message: match code {
                    Some(code) => format!("SMTP rejected the request ({code})."),
                    None if tls_error => "SMTP TLS verification failed.".into(),
                    None => "SMTP connection failed or ended before delivery was confirmed.".into(),
                },
            }
        })
}

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SendError {}

#[cfg(test)]
mod smtp_tests {

    #[test]
    fn explains_smtp_rejections_with_the_server_reason_and_separate_login() {
        let server = Server::new();
        let mut remote = server.remote();
        let file = std::env::temp_dir().join(crate::storage::unique_name("smtp-cert"));
        std::fs::write(&file, &server.pem).unwrap();
        remote.tls_certificate = Some(file.clone());
        remote.smtp_username = "relay@example.test".into();
        remote.smtp_session_password = Some(std::sync::Arc::new(crate::remote::SessionPassword(
            zeroize::Zeroizing::new("smtp-secret".into()),
        )));
        let worker = server.spawn(|stream| {
            let mut stream = BufReader::new(stream.unwrap());
            stream.get_mut().write_all(b"220 hi\r\n").unwrap();
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let reply: &[u8] = if line.starts_with("EHLO") {
                    b"250-hi\r\n250 AUTH PLAIN LOGIN\r\n"
                } else if line.starts_with("AUTH") {
                    auth = line.clone();
                    b"535 5.7.8 smtp-secret rejected: app password required\r\n"
                } else {
                    b"221 bye\r\n"
                };
                stream.get_mut().write_all(reply).unwrap();
                if line.starts_with("QUIT") {
                    break;
                }
            }
            auth
        });
        let error = check_connection(&remote).unwrap_err();
        assert_eq!(error.kind, crate::remote::ErrorKind::Auth);
        assert!(error.message.contains("535") && error.message.contains("app password"));
        assert!(!error.message.contains("smtp-secret"));
        let auth = worker.join().unwrap();
        assert!(auth.contains(&crate::widgets::rich::base64(
            b"\0relay@example.test\0smtp-secret"
        )));
        let _ = std::fs::remove_file(file);
    }

    use super::*;
    use crate::remote::tests::Server;
    use lettre::transport::smtp::{
        authentication::Credentials,
        client::{Certificate, Tls, TlsParameters},
    };
    use std::io::{BufRead, BufReader};

    const MESSAGE: &[u8] = b"From: test@example.test\r\nTo: visible@example.test\r\nBcc: hidden@example.test\r\nMessage-ID: <test@example.test>\r\n\r\nhello\r\n";

    fn transport(server: &Server, trust: bool) -> lettre::SmtpTransport {
        let mut tls = TlsParameters::builder("localhost".into());
        if trust {
            tls = tls.add_root_certificate(Certificate::from_pem(server.pem.as_bytes()).unwrap());
        }
        lettre::SmtpTransport::relay("localhost")
            .unwrap()
            .port(server.port)
            .tls(Tls::Wrapper(tls.build().unwrap()))
            .credentials(Credentials::new(
                "test@example.test".into(),
                "fake-password".into(),
            ))
            .timeout(Some(Duration::from_secs(2)))
            .build()
    }

    fn exchange(
        rejection: Option<&'static str>,
        drop_data: bool,
    ) -> (Result<(), SendError>, String, String) {
        let server = Server::new();
        let transport = transport(&server, true);
        let worker = server.spawn(move |stream| {
            let mut stream = BufReader::new(stream.unwrap());
            stream
                .get_mut()
                .write_all(b"220 localhost ESMTP\r\n")
                .unwrap();
            let mut recipients = String::new();
            let mut body = String::new();
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let reply = if line.starts_with("EHLO ") {
                    "250-localhost\r\n250 AUTH PLAIN\r\n"
                } else if line.starts_with("AUTH ") {
                    rejection.unwrap_or("235 authenticated\r\n")
                } else if line.starts_with("RCPT TO:") {
                    recipients.push_str(&line);
                    "250 recipient ok\r\n"
                } else if line == "DATA\r\n" {
                    stream.get_mut().write_all(b"354 continue\r\n").unwrap();
                    loop {
                        let mut line = String::new();
                        assert!(stream.read_line(&mut line).unwrap() > 0);
                        if line == ".\r\n" {
                            break;
                        }
                        body.push_str(&line);
                    }
                    if drop_data {
                        break;
                    }
                    "250 queued\r\n"
                } else if line == "QUIT\r\n" {
                    stream.get_mut().write_all(b"221 bye\r\n").unwrap();
                    break;
                } else {
                    "250 ok\r\n"
                };
                if stream.get_mut().write_all(reply.as_bytes()).is_err() {
                    break;
                }
            }
            (recipients, body)
        });
        let result = smtp_with_transport(&transport, MESSAGE);
        drop(transport);
        let (recipients, body) = worker.join().unwrap();
        (result, recipients, body)
    }

    #[test]
    fn sends_over_verified_tls_without_bcc_headers() {
        let (result, recipients, body) = exchange(None, false);
        result.unwrap();
        assert!(recipients.contains("visible@example.test"));
        assert!(recipients.contains("hidden@example.test"));
        assert!(!body.contains("Bcc:"));
        assert!(!body.contains("hidden@example.test"));
        assert!(body.contains("hello"));
    }

    #[test]
    fn classifies_auth_rejection_without_exposing_server_text() {
        let error = exchange(Some("535 fake-password rejected\r\n"), false)
            .0
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::Configuration);
        assert!(!error.to_string().contains("fake-password"));
    }

    #[test]
    fn dropped_data_confirmation_is_uncertain() {
        let error = exchange(None, true).0.unwrap_err();
        assert_eq!(error.kind, FailureKind::Uncertain);
    }

    #[test]
    fn rejects_untrusted_tls() {
        let server = Server::new();
        let transport = transport(&server, false);
        let worker = server.spawn(|stream| assert!(stream.is_none()));
        let error = smtp_with_transport(&transport, MESSAGE).unwrap_err();
        assert_eq!(error.kind, FailureKind::Configuration);
        worker.join().unwrap();
    }

    #[test]
    fn connection_check_authenticates_without_sending_mail() {
        let server = Server::new();
        let server_remote = server.remote();
        let transport = transport(&server, true);
        let worker = server.spawn(|stream| {
            let mut stream = BufReader::new(stream.unwrap());
            stream
                .get_mut()
                .write_all(b"220 localhost ESMTP\r\n")
                .unwrap();
            let mut authenticated = false;
            let mut checked = false;
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let response = if line.starts_with("EHLO ") {
                    "250-localhost\r\n250 AUTH PLAIN\r\n"
                } else if line.starts_with("AUTH ") {
                    authenticated = true;
                    "235 authenticated\r\n"
                } else if line == "NOOP\r\n" {
                    checked = true;
                    "250 ok\r\n"
                } else if line == "QUIT\r\n" {
                    stream.get_mut().write_all(b"221 bye\r\n").unwrap();
                    break;
                } else {
                    panic!("Connection check must not send mail");
                };
                stream.get_mut().write_all(response.as_bytes()).unwrap();
            }
            assert!(authenticated);
            assert!(checked);
        });
        check_transport(&transport, &server_remote, "fake-password").unwrap();
        drop(transport);
        worker.join().unwrap();
    }
}

pub async fn send(command: &[String], message: &[u8]) -> Result<(), SendError> {
    send_with_timeout(command, message, Duration::from_secs(60)).await
}

async fn send_with_timeout(
    command: &[String],
    message: &[u8],
    deadline: Duration,
) -> Result<(), SendError> {
    let envelope = crate::message::envelope(message).map_err(SendError::failed)?;
    let message = crate::message::wire_message(message);
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| SendError::failed("Configure send_command before sending."))?;
    if program == "tmail-test-sink" {
        let path = arguments
            .first()
            .ok_or_else(|| SendError::failed("tmail-test-sink needs an output path."))?;
        return write_test_sink(Path::new(path), &message).map_err(SendError::uncertain);
    }
    if arguments.iter().any(|argument| argument == "--") {
        return Err(SendError::failed(
            "Remove -- from send_command; Tmail supplies the recipient arguments.",
        ));
    }

    let mut child = Command::new(program)
        .args(
            arguments
                .iter()
                .filter(|argument| !matches!(argument.as_str(), "-t" | "--read-recipients")),
        )
        .arg("-f")
        .arg(&envelope.from)
        .arg("--")
        .args(&envelope.recipients)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(SendError::failed)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SendError::uncertain("Unable to open sender input."))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| SendError::uncertain("Unable to read sender errors."))?;
    let operation = async {
        let write = async move {
            stdin.write_all(&message).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<(), io::Error>(())
        };
        let read = async move {
            let mut output = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stderr.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                let keep = count.min(16384_usize.saturating_sub(output.len()));
                output.extend_from_slice(&buffer[..keep]);
            }
            Ok::<_, io::Error>(output)
        };
        tokio::join!(write, read, child.wait())
    };
    let (written, stderr, status) = match tokio::time::timeout(deadline, operation).await {
        Ok(output) => output,
        Err(_) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(SendError::uncertain(
                "Sender timed out. Delivery could not be confirmed.",
            ));
        }
    };
    let status = status.map_err(SendError::uncertain)?;
    if status.success() {
        written.map_err(SendError::uncertain)?;
        Ok(())
    } else {
        let stderr = stderr.unwrap_or_default();
        let error = String::from_utf8_lossy(&stderr)
            .chars()
            .filter(|c| !c.is_control() || *c == ' ')
            .collect::<String>();
        let kind = match status.code() {
            Some(64 | 66 | 68 | 72 | 73 | 77 | 78) => FailureKind::Configuration,
            Some(65 | 67) => FailureKind::Permanent,
            Some(69 | 75) => FailureKind::Temporary,
            _ => FailureKind::Uncertain,
        };
        Err(SendError {
            kind,
            message: if error.trim().is_empty() {
                format!("Sender exited with {status}.")
            } else {
                error
            },
        })
    }
}

fn write_test_sink(path: &Path, message: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(message)?;
    file.write_all(b"\n\n--- tmail message ---\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::send;

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_without_blocking_the_runtime() {
        let send = super::send_with_timeout(
            &["sh".into(), "-c".into(), "exec sleep 5".into()],
            b"From: me@example.com\nTo: ada@example.com\nSubject: test\n\nbody",
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(send.kind, super::FailureKind::Uncertain);
        assert!(send.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn writes_messages_to_test_sink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tmail-sink-{nonce}.eml"));
        send(
            &["tmail-test-sink".into(), path.display().to_string()],
            b"From: me@example.com\nTo: ada@example.com\nSubject: hello\n\nbody",
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert!(contents.contains("Subject: hello"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_bcc_only_recipients_as_arguments_without_leaking_headers() {
        let command = vec!["sh".into(), "-c".into(),
            "body=$(cat); case \"$body\" in *hidden@example.com*) exit 65;; esac; test \"$1\" = -f && test \"$2\" = me@example.com && test \"$3\" = -- && test \"$4\" = hidden@example.com".into(), "sender".into()];
        send(
            &command,
            b"From: me@example.com\r\nBcc: hidden@example.com\r\nSubject: test\r\n\r\nbody",
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn classifies_sender_failures_conservatively() {
        for (code, kind) in [
            (75, super::FailureKind::Temporary),
            (67, super::FailureKind::Permanent),
            (78, super::FailureKind::Configuration),
            (1, super::FailureKind::Uncertain),
        ] {
            let command = vec![
                "sh".into(),
                "-c".into(),
                format!("cat >/dev/null; exit {code}"),
            ];
            let result = send(
                &command,
                b"From: me@example.com\nTo: ada@example.com\n\nbody",
            )
            .await
            .unwrap_err();
            assert_eq!(result.kind, kind);
        }
    }
}
