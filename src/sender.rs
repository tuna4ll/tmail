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

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SendError {}

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
    if program == "termail-test-sink" {
        let path = arguments
            .first()
            .ok_or_else(|| SendError::failed("termail-test-sink needs an output path."))?;
        return write_test_sink(Path::new(path), &message).map_err(SendError::uncertain);
    }
    if arguments.iter().any(|argument| argument == "--") {
        return Err(SendError::failed(
            "Remove -- from send_command; Termail supplies the recipient arguments.",
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
    file.write_all(b"\n\n--- termail message ---\n")?;
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
        let path = std::env::temp_dir().join(format!("termail-sink-{nonce}.eml"));
        send(
            &["termail-test-sink".into(), path.display().to_string()],
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
