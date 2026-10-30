use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{message::Envelope, sender::FailureKind, storage};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ready,
    Sending,
    Sent,
    CopyingSent,
    CopiedSent,
    Failed(FailureKind),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    version: u32,
    pub account: String,
    pub envelope: Envelope,
    digest: String,
    pub state: State,
    pub attempts: u32,
    pub updated: u64,
    pub error: Option<String>,
}

pub struct Journal {
    file: File,
    pub record: Record,
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn status_path(root: &Path, message: &Path) -> Result<PathBuf, String> {
    let outbox = root.join(".Outbox");
    if ![outbox.join("new"), outbox.join("cur"), outbox.join("tmp")]
        .iter()
        .any(|parent| message.parent() == Some(parent.as_path()))
    {
        return Err("Message is outside the account Outbox.".into());
    }
    let name = message
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Invalid Outbox filename.")?;
    let name = storage::maildir_info(name).0;
    Ok(outbox
        .join("termail-state")
        .join(format!("{name}.delivery")))
}

fn read_record(value: &str) -> Result<Option<Record>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    if !value.ends_with('\n') {
        return Err(
            "Delivery status is incomplete. Check delivery before sending another copy.".into(),
        );
    }
    let line = value.lines().last().ok_or("Delivery status is empty.")?;
    let record: Record = serde_json::from_str(line)
        .map_err(|_| "Delivery status is unreadable. Message remains in Outbox.")?;
    if record.version != 1 {
        return Err("Unsupported delivery status version.".into());
    }
    Ok(Some(record))
}

impl Journal {
    pub fn open(
        root: &Path,
        message: &Path,
        account: &str,
        contents: &[u8],
    ) -> Result<Self, String> {
        let path = status_path(root, message)?;
        let directory = path.parent().ok_or("Invalid delivery status path.")?;
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        storage::sync_parent(directory).map_err(|error| error.to_string())?;
        let mut file = match storage::create(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| error.to_string())?,
            Err(error) => return Err(error.to_string()),
        };
        file.try_lock()
            .map_err(|_| "This message is already being sent by another process.")?;
        let mut value = String::new();
        file.read_to_string(&mut value)
            .map_err(|error| error.to_string())?;
        let envelope = crate::message::envelope(contents)?;
        let digest = format!("{:x}", Sha256::digest(contents));
        let legacy = matches!(value.as_str(), "sending" | "sent");
        let previous = if legacy { None } else { read_record(&value)? };
        let record = if let Some(record) = previous {
            if record.account != account {
                return Err("Queued message belongs to a different account. Restore its original account before retrying.".into());
            }
            if record.digest != digest || record.envelope != envelope {
                return Err("Queued message changed after it was saved. Nothing was sent.".into());
            }
            record
        } else {
            Record {
                version: 1,
                account: account.into(),
                envelope,
                digest,
                state: match value.as_str() {
                    "sending" => State::Sending,
                    "sent" => State::Sent,
                    _ => State::Ready,
                },
                attempts: u32::from(legacy),
                updated: 0,
                error: None,
            }
        };
        let mut journal = Self { file, record };
        if legacy {
            journal
                .file
                .seek(SeekFrom::End(0))
                .and_then(|_| journal.file.write_all(b"\n"))
                .map_err(|error| error.to_string())?;
        }
        if value.is_empty() || legacy {
            journal.persist()?;
            storage::sync_parent(&path).map_err(|error| error.to_string())?;
        }
        Ok(journal)
    }

    fn persist(&mut self) -> Result<(), String> {
        self.record.updated = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut bytes = serde_json::to_vec(&self.record).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        self.file
            .seek(SeekFrom::End(0))
            .and_then(|_| self.file.write_all(&bytes))
            .and_then(|_| self.file.sync_all())
            .map_err(|error| format!("Unable to save delivery status: {error}"))
    }

    pub fn transition(&mut self, state: State, error: Option<String>) -> Result<(), String> {
        if state == State::Sending {
            self.record.attempts += 1;
        }
        self.record.state = state;
        self.record.error = error;
        self.persist()
    }
}

/// Temporary failures are retried this many times before waiting for the user.
pub const AUTOMATIC_RETRIES: u32 = 6;

/// Whether a temporarily failed message is due for another automatic attempt. The wait
/// doubles from one minute up to an hour.
pub fn retry_due(root: &Path, message: &Path) -> bool {
    let Ok(path) = status_path(root, message) else {
        return false;
    };
    let Ok(Some(record)) = fs::read_to_string(path).map(|value| read_record(&value).ok().flatten())
    else {
        return false;
    };
    if record.state != State::Failed(FailureKind::Temporary) || record.attempts >= AUTOMATIC_RETRIES
    {
        return false;
    }
    let wait = (60u64 << record.attempts.saturating_sub(1).min(6)).min(3600);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.saturating_sub(record.updated) >= wait
}

pub fn describe(root: &Path, message: &Path) -> String {
    let result = status_path(root, message).and_then(|path| match fs::read_to_string(path) {
        Ok(value) if value == "sending" => {
            Ok("Delivery unconfirmed · check delivery before retrying".into())
        }
        Ok(value) if value == "sent" => Ok("Sent · retry to save the Sent copy".into()),
        Ok(value) => read_record(&value)?
            .map(|record| {
                let label = match record.state {
                    State::Ready => "Queued",
                    State::Sending | State::Failed(FailureKind::Uncertain) => {
                        "Delivery unconfirmed · check delivery before retrying"
                    }
                    State::Sent => "Sent · retry to save the Sent copy",
                    State::CopyingSent => "Sent · remote copy unconfirmed · retry to check",
                    State::CopiedSent => "Sent · remote copy saved · retry to finish locally",
                    State::Failed(FailureKind::Temporary)
                        if record.attempts < AUTOMATIC_RETRIES =>
                    {
                        "Temporary failure · retrying automatically"
                    }
                    State::Failed(FailureKind::Temporary) => "Temporary failure · press x to retry",
                    State::Failed(FailureKind::Configuration) => {
                        "Setup error · fix sender settings before retrying"
                    }
                    State::Failed(FailureKind::Permanent) => {
                        "Rejected · correct the message before sending again"
                    }
                };
                format!(
                    "{label} · {} attempts{}",
                    record.attempts,
                    record
                        .error
                        .map(|error| format!(" · {error}"))
                        .unwrap_or_default()
                )
            })
            .ok_or("Delivery status is empty.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok("Queued · press x to send".into())
        }
        Err(error) => Err(error.to_string()),
    });
    result.unwrap_or_else(|error| format!("Delivery status unavailable: {error}"))
}

pub fn recover(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root.join(".Outbox/tmp")) else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(status) = status_path(root, &path) else {
            continue;
        };
        if !status.is_file() {
            continue;
        }
        let recover = || -> Result<(), String> {
            let value = fs::read_to_string(&status).map_err(|error| error.to_string())?;
            let record = read_record(&value)?.ok_or("Incomplete queued message.")?;
            let contents = fs::read(&path).map_err(|error| error.to_string())?;
            let journal = Journal::open(root, &path, &record.account, &contents)?;
            if journal.record.state != State::Ready {
                return Err("Unpublished message has an unexpected delivery status.".into());
            }
            let destination = root.join(".Outbox/new").join(entry.file_name());
            if destination.exists() {
                return Err("A queued message with the same filename already exists.".into());
            }
            storage::rename(&path, &destination).map_err(|error| error.to_string())?;
            drop(journal);
            Ok(())
        };
        if let Err(error) = recover()
            && path.exists()
            && !error.contains("another process")
        {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{delivery::Delivery, mailbox::MailboxSource};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(storage::unique_name("termail-outbox"));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn queue(&self) -> Delivery {
            Delivery::queue(MailboxSource::Maildir(self.0.clone()), "personal:me@example.com".into(),
                vec!["termail-test-sink".into(), self.0.join("sent.log").display().to_string()],
                b"From: me@example.com\r\nTo: ada@example.com\r\nBcc: hidden@example.com\r\nSubject: hello\r\n\r\nbody").unwrap()
        }

        fn journal(&self, delivery: &Delivery) -> Journal {
            let path = delivery.mail.source_path.as_ref().unwrap();
            Journal::open(&self.0, path, &delivery.account, &fs::read(path).unwrap()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn queue_survives_restart_with_account_and_envelope() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let journal = fixture.journal(&delivery);
        assert_eq!(journal.record.state, State::Ready);
        assert_eq!(journal.record.account, "personal:me@example.com");
        assert_eq!(
            journal.record.envelope.recipients,
            ["ada@example.com", "hidden@example.com"]
        );
        assert_eq!(journal.record.attempts, 0);
        drop(journal);
        let source = delivery.source.clone();
        let command = delivery.command.clone();
        let account = delivery.account.clone();
        drop(delivery);
        let mail = source.load().unwrap().mails.remove(0);
        assert!(mail.delivery_status.as_deref().unwrap().contains("Queued"));
        assert_eq!(
            Delivery {
                remote: None,
                source: source.clone(),
                mail,
                command,
                account
            }
            .run()
            .await,
            "Message sent"
        );
        let wire = fs::read_to_string(fixture.0.join("sent.log")).unwrap();
        assert!(!wire.contains("hidden@example.com"));
        let sent = source.load().unwrap().mails.remove(0);
        assert!(
            fs::read_to_string(sent.source_path.unwrap())
                .unwrap()
                .contains("hidden@example.com")
        );
    }

    #[tokio::test]
    async fn locks_message_until_the_current_attempt_finishes() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let journal = fixture.journal(&delivery);
        let result = delivery.run().await;
        assert!(result.contains("another process"), "{result}");
        assert!(!fixture.0.join("sent.log").exists());
        drop(journal);
    }

    #[tokio::test]
    async fn refuses_changed_messages_and_account_mismatches() {
        let fixture = Fixture::new();
        let mut delivery = fixture.queue();
        delivery.account = "work:me@example.com".into();
        assert!(delivery.run().await.contains("different account"));
        let delivery = fixture.queue();
        let path = delivery.mail.source_path.as_ref().unwrap();
        let mut contents = fs::read(path).unwrap();
        contents.extend_from_slice(b"changed");
        fs::write(path, contents).unwrap();
        assert!(delivery.run().await.contains("changed"));
        assert!(!fixture.0.join("sent.log").exists());
    }

    #[tokio::test]
    async fn torn_journal_never_triggers_another_send() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let path = status_path(&fixture.0, delivery.mail.source_path.as_ref().unwrap()).unwrap();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"{\"state\":").unwrap();
        drop(file);
        let result = delivery.run().await;
        assert!(result.contains("incomplete"), "{result}");
        assert!(!fixture.0.join("sent.log").exists());
    }

    #[tokio::test]
    async fn confirmed_delivery_only_moves_the_local_copy() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let mut journal = fixture.journal(&delivery);
        journal.transition(State::Sending, None).unwrap();
        drop(journal);
        delivery.resolve(true).unwrap();
        assert_eq!(delivery.run().await, "Message sent");
        assert!(!fixture.0.join("sent.log").exists());
    }

    #[tokio::test]
    async fn explicit_non_delivery_confirmation_allows_a_new_attempt() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let mut journal = fixture.journal(&delivery);
        journal.transition(State::Sending, None).unwrap();
        journal
            .transition(
                State::Failed(FailureKind::Uncertain),
                Some("timeout".into()),
            )
            .unwrap();
        drop(journal);
        delivery.resolve(false).unwrap();
        assert_eq!(delivery.run().await, "Message sent");
        assert!(fixture.0.join("sent.log").exists());
    }

    #[tokio::test]
    async fn permanent_failures_are_not_retried_unchanged() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let mut journal = fixture.journal(&delivery);
        journal
            .transition(
                State::Failed(FailureKind::Permanent),
                Some("recipient rejected".into()),
            )
            .unwrap();
        drop(journal);
        assert!(delivery.resolve(false).is_err());
        assert!(delivery.run().await.contains("rejected"));
        assert!(!fixture.0.join("sent.log").exists());
    }

    #[test]
    fn failed_queue_publication_never_exposes_an_unbound_message() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join(".Outbox")).unwrap();
        fs::write(fixture.0.join(".Outbox/termail-state"), "blocked").unwrap();
        let source = MailboxSource::Maildir(fixture.0.clone());
        assert!(
            Delivery::queue(
                source.clone(),
                "personal".into(),
                Vec::new(),
                b"From: me@example.com\nTo: ada@example.com\n\nbody"
            )
            .is_err()
        );
        assert!(source.load().unwrap().mails.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn queued_message_and_metadata_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let path = delivery.mail.source_path.as_ref().unwrap();
        for path in [path.clone(), status_path(&fixture.0, path).unwrap()] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn recovers_a_fully_staged_message_without_sending_it() {
        let fixture = Fixture::new();
        let source = MailboxSource::Maildir(fixture.0.clone());
        let contents = b"From: me@example.com\nTo: ada@example.com\n\nbody";
        let temporary = source.stage_outbox(contents).unwrap();
        let journal = Journal::open(&fixture.0, &temporary, "personal", contents).unwrap();
        assert!(recover(&fixture.0).is_empty());
        assert!(temporary.exists());
        drop(journal);
        assert!(recover(&fixture.0).is_empty());
        assert!(!temporary.exists());
        let loaded = source.load().unwrap();
        assert_eq!(loaded.mails.len(), 1);
        assert_eq!(loaded.mails[0].mailbox.as_deref(), Some("Outbox"));
        assert!(!fixture.0.join("sent.log").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn temporary_failures_preserve_attempt_history_across_retries() {
        let fixture = Fixture::new();
        let delivery = fixture.queue();
        let failed = Delivery {
            remote: None,
            source: delivery.source.clone(),
            mail: delivery.mail.clone(),
            account: delivery.account.clone(),
            command: vec![
                "sh".into(),
                "-c".into(),
                "cat >/dev/null; printf 'try later' >&2; exit 75".into(),
            ],
        };
        assert!(failed.run().await.contains("try later"));
        let journal = fixture.journal(&delivery);
        assert_eq!(journal.record.state, State::Failed(FailureKind::Temporary));
        assert_eq!(journal.record.attempts, 1);
        assert_eq!(journal.record.error.as_deref(), Some("try later"));
        drop(journal);
        let status = status_path(&fixture.0, delivery.mail.source_path.as_ref().unwrap()).unwrap();
        assert_eq!(delivery.run().await, "Message sent");
        let value = fs::read_to_string(status).unwrap();
        let record = read_record(&value).unwrap().unwrap();
        assert_eq!(record.attempts, 2);
        assert_eq!(record.state, State::Sent);
        assert_eq!(value.lines().count(), 5);
    }

    #[tokio::test]
    async fn remote_copy_failure_never_repeats_smtp_delivery() {
        let fixture = Fixture::new();
        let mut delivery = fixture.queue();
        fixture
            .journal(&delivery)
            .transition(State::Sent, None)
            .unwrap();
        delivery.remote = Some(crate::remote::tests::Server::new().remote());
        assert!(delivery.run().await.contains("Message sent."));
        assert!(!fixture.0.join("sent.log").exists());
        let source = MailboxSource::Maildir(fixture.0.clone());
        let mail = source.load().unwrap().mails.remove(0);
        let delivery = Delivery {
            source,
            mail,
            account: "personal:me@example.com".into(),
            command: Vec::new(),
            remote: Some(crate::remote::tests::Server::new().remote()),
        };
        assert_eq!(fixture.journal(&delivery).record.state, State::Sent);
        fixture
            .journal(&delivery)
            .transition(State::CopiedSent, None)
            .unwrap();
        assert_eq!(delivery.run().await, "Message sent");
        assert!(!fixture.0.join("sent.log").exists());
    }
}
