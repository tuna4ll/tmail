use std::{fs, path::PathBuf};

use crate::{
    mail::Mail,
    mailbox::MailboxSource,
    outbox::{Journal, State},
    sender::{self, FailureKind},
    storage,
};

pub struct Delivery {
    pub source: MailboxSource,
    pub mail: Mail,
    pub command: Vec<String>,
    pub account: String,
}

impl Delivery {
    pub fn queue(
        source: MailboxSource,
        account: String,
        command: Vec<String>,
        contents: &[u8],
    ) -> Result<Self, String> {
        let MailboxSource::Maildir(root) = &source else {
            return Err("Choose a sender account before sending.".into());
        };
        crate::message::envelope(contents)?;
        let temporary = source
            .stage_outbox(contents)
            .map_err(|error| error.to_string())?;
        let journal = Journal::open(root, &temporary, &account, contents)?;
        let destination = root
            .join(".Outbox/new")
            .join(temporary.file_name().ok_or("Invalid queued filename.")?);
        storage::rename(&temporary, &destination)
            .map_err(|error| format!("Unable to publish queued message: {error}"))?;
        drop(journal);
        let mut mail = Mail::new("", "", "", "", None);
        mail.source_path = Some(destination);
        mail.mailbox = Some("Outbox".into());
        Ok(Self {
            source,
            mail,
            command,
            account,
        })
    }

    pub async fn run(self) -> String {
        match self.deliver().await {
            Ok(()) => "Message sent".into(),
            Err(error) => error,
        }
    }

    pub fn resolve(&self, delivered: bool) -> Result<(), String> {
        let (root, path) = self.paths()?;
        let contents = fs::read(&path).map_err(|error| error.to_string())?;
        let mut journal = Journal::open(&root, &path, &self.account, &contents)?;
        if !matches!(
            journal.record.state,
            State::Sending | State::Failed(FailureKind::Uncertain)
        ) {
            return Err("Only an unconfirmed delivery needs manual resolution.".into());
        }
        journal.transition(
            if delivered { State::Sent } else { State::Ready },
            Some(if delivered {
                "Delivery confirmed by user.".into()
            } else {
                "User confirmed the message was not delivered.".into()
            }),
        )
    }

    async fn deliver(&self) -> Result<(), String> {
        let (root, path) = self.paths()?;
        let contents =
            fs::read(&path).map_err(|error| format!("Unable to read queued message: {error}"))?;
        let mut journal = Journal::open(&root, &path, &self.account, &contents)?;
        match journal.record.state {
            State::Sending | State::Failed(FailureKind::Uncertain) => return Err(
                "Delivery is unconfirmed. Check the receiving mailbox before sending another copy."
                    .into(),
            ),
            State::Failed(FailureKind::Permanent) => {
                return Err(
                    "Message was rejected. Correct the message before sending another copy.".into(),
                );
            }
            State::Sent => {}
            _ => {
                journal.transition(State::Sending, None)?;
                if let Err(error) = sender::send(&self.command, &contents).await {
                    journal.transition(State::Failed(error.kind), Some(error.to_string()))?;
                    return Err(format!("Send failed: {error}. Message remains in Outbox."));
                }
                journal.transition(State::Sent, None).map_err(|error| {
                    format!("Message sent, but {error}. Check delivery before retrying.")
                })?;
            }
        }
        self.source.mark_sent(&self.mail)
            .map_err(|error| format!("Message sent, but unable to move it to Sent: {error}. Retry will only move the saved copy."))?;
        Ok(())
    }

    fn paths(&self) -> Result<(PathBuf, PathBuf), String> {
        let MailboxSource::Maildir(root) = &self.source else {
            return Err("Choose a sender account before sending.".into());
        };
        let path = self
            .mail
            .source_path
            .clone()
            .ok_or("Queued message has no path.")?;
        if ![root.join(".Outbox/new"), root.join(".Outbox/cur")]
            .iter()
            .any(|parent| path.parent() == Some(parent.as_path()))
        {
            return Err("Message is outside the account Outbox.".into());
        }
        Ok((root.clone(), path))
    }
}
