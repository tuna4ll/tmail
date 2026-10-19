use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use mail_builder::{MessageBuilder, headers::text::Text};
use mail_parser::{MessageParser, MimeHeaders};

use crate::mail::Mail;

#[derive(Clone, Debug)]
pub struct Summary {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub format: MessageFormat,
    pub attachments: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageFormat {
    Plain,
    Html,
}

impl MessageFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "Plain text",
            Self::Html => "HTML",
        }
    }

    const fn content_type(self) -> &'static str {
        match self {
            Self::Plain => "text/plain",
            Self::Html => "text/html",
        }
    }

    pub fn toggle(&mut self) {
        *self = match self {
            Self::Plain => Self::Html,
            Self::Html => Self::Plain,
        };
    }
}

#[derive(Debug)]
pub struct Composer {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub attachments: String,
    pub format: MessageFormat,
    pub body_path: PathBuf,
    pub original_draft: Option<PathBuf>,
    attachment_dir: Option<PathBuf>,
    reply_to: Option<String>,
    references: Vec<String>,
    message_id: String,
}

#[derive(Debug)]
pub struct DraftError(String);

impl fmt::Display for DraftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DraftError {}

impl Composer {
    pub fn new(
        reply_to: Option<&Mail>,
        identity: Option<&str>,
        signature: Option<&str>,
    ) -> io::Result<Self> {
        let name = crate::storage::unique_name("termail");
        let body_path = std::env::temp_dir().join(format!("{name}.txt"));
        let body = signature
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("\n\n-- \n{}", value.trim()))
            .unwrap_or_default();
        crate::storage::write_new(&body_path, body.as_bytes())?;
        let subject = reply_to
            .map(|mail| {
                if mail.subject.to_lowercase().starts_with("re:") {
                    mail.subject.clone()
                } else {
                    format!("Re: {}", mail.subject)
                }
            })
            .unwrap_or_default();
        let mut references = reply_to
            .map(|mail| mail.references.clone())
            .unwrap_or_default();
        if let Some(mail) = reply_to {
            references.push(mail.id.clone());
            references.dedup();
        }
        Ok(Self {
            from: identity.unwrap_or_default().into(),
            to: reply_to.map(|mail| mail.from.clone()).unwrap_or_default(),
            cc: String::new(),
            bcc: String::new(),
            subject,
            attachments: String::new(),
            format: MessageFormat::Plain,
            body_path,
            original_draft: None,
            attachment_dir: None,
            reply_to: reply_to.map(|mail| mail.id.clone()),
            references,
            message_id: format!("{name}@localhost"),
        })
    }

    pub fn resume(path: &Path) -> io::Result<Self> {
        let contents = fs::read(path)?;
        let parsed = MessageParser::default()
            .parse(&contents)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Unable to read draft."))?;
        let mut composer = Self::new(None, None, None)?;
        composer.from = parsed
            .from()
            .map(crate::mailbox::format_addresses)
            .unwrap_or_default()
            .join(", ");
        composer.to = parsed
            .to()
            .map(crate::mailbox::format_addresses)
            .unwrap_or_default()
            .join(", ");
        composer.cc = parsed
            .cc()
            .map(crate::mailbox::format_addresses)
            .unwrap_or_default()
            .join(", ");
        composer.bcc = parsed
            .bcc()
            .map(crate::mailbox::format_addresses)
            .unwrap_or_default()
            .join(", ");
        composer.subject = parsed.subject().unwrap_or_default().into();
        composer.reply_to = parsed.in_reply_to().as_text().map(str::to_owned);
        composer.references = parsed
            .references()
            .as_text_list()
            .unwrap_or_default()
            .iter()
            .map(|id| id.to_string())
            .collect();
        if let Some(id) = parsed.message_id() {
            composer.message_id = id.into();
        }
        let body = if parsed
            .html_part(0)
            .is_some_and(|part| matches!(part.body, mail_parser::PartType::Html(_)))
        {
            composer.format = MessageFormat::Html;
            parsed.body_html(0)
        } else {
            parsed.body_text(0)
        }
        .unwrap_or_default();
        fs::write(&composer.body_path, body.as_bytes())?;
        let mut attachments = Vec::new();
        for (index, part) in parsed.attachments().enumerate() {
            let directory = composer.body_path.with_extension("attachments");
            if composer.attachment_dir.is_none() {
                fs::create_dir(&directory)?;
                composer.attachment_dir = Some(directory.clone());
            }
            let directory = directory.join(index.to_string());
            fs::create_dir(&directory)?;
            let name =
                crate::mailbox::safe_file_name(part.attachment_name().unwrap_or("attachment"))
                    .replace(';', "_");
            let attachment = directory.join(name);
            crate::storage::write_new(&attachment, part.contents())?;
            attachments.push(attachment.to_string_lossy().into_owned());
        }
        composer.attachments = attachments.join("; ");
        composer.original_draft = Some(path.to_path_buf());
        Ok(composer)
    }

    pub fn summary(&self, base: &Path) -> Result<Summary, DraftError> {
        self.validate_identity()?;
        inspect(self.source()?.as_bytes(), base)
    }

    pub fn message(&self, base: &Path) -> Result<Vec<u8>, DraftError> {
        self.validate_identity()?;
        prepare(self.source()?.as_bytes(), base)
    }

    pub fn draft_message(&self, base: &Path) -> Result<Vec<u8>, DraftError> {
        prepare_message(self.source()?.as_bytes(), base, false)
    }

    pub fn body_stats(&self) -> io::Result<(usize, usize)> {
        let body = fs::read_to_string(&self.body_path)?;
        Ok((body.lines().count(), body.chars().count()))
    }

    pub fn remove_body(&self) -> io::Result<()> {
        if self.body_path.exists() {
            fs::remove_file(&self.body_path)?;
        }
        Ok(())
    }

    fn validate_identity(&self) -> Result<(), DraftError> {
        if self.from.trim().is_empty() {
            Err(DraftError(
                "Set email in config.toml before sending.".into(),
            ))
        } else {
            let from = crate::message::addresses(&self.from, "From").map_err(DraftError)?;
            if from.len() != 1 {
                return Err(DraftError("From must contain exactly one address.".into()));
            }
            for (field, value) in [("To", &self.to), ("Cc", &self.cc), ("Bcc", &self.bcc)] {
                crate::message::addresses(value, field).map_err(DraftError)?;
            }
            if self.subject.chars().any(char::is_control) {
                return Err(DraftError("Subject contains a control character.".into()));
            }
            Ok(())
        }
    }

    fn source(&self) -> Result<String, DraftError> {
        let body = fs::read_to_string(&self.body_path).map_err(|error| {
            DraftError(format!(
                "Unable to read message body {}: {error}",
                self.body_path.display()
            ))
        })?;
        let mut headers = vec![
            format!("From: {}", one_line(&self.from)),
            format!("To: {}", one_line(&self.to)),
            format!("Cc: {}", one_line(&self.cc)),
            format!("Bcc: {}", one_line(&self.bcc)),
            format!("Subject: {}", one_line(&self.subject)),
        ];
        headers.extend(
            self.attachments
                .split(';')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("Attach: {}", one_line(value))),
        );
        if let Some(reply_to) = &self.reply_to {
            headers.push(format!("In-Reply-To: <{}>", one_line(reply_to)));
        }
        if !self.references.is_empty() {
            headers.push(format!(
                "References: {}",
                self.references
                    .iter()
                    .map(|id| format!("<{}>", one_line(id)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        headers.push(format!("Message-ID: <{}>", self.message_id));
        headers.push("MIME-Version: 1.0".into());
        headers.push(format!(
            "Content-Type: {}; charset=utf-8",
            self.format.content_type()
        ));
        Ok(format!("{}\n\n{body}", headers.join("\n")))
    }
}

impl Drop for Composer {
    fn drop(&mut self) {
        let _ = self.remove_body();
        if let Some(directory) = &self.attachment_dir {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

pub fn inspect(contents: &[u8], base: &Path) -> Result<Summary, DraftError> {
    inspect_message(contents, base, true)
}

fn inspect_message(
    contents: &[u8],
    base: &Path,
    require_recipient: bool,
) -> Result<Summary, DraftError> {
    let text = String::from_utf8_lossy(contents);
    let (headers, _) = split_message(&text);
    let to = header_values(headers, "to").join(", ");
    let cc = header_values(headers, "cc").join(", ");
    let bcc = header_values(headers, "bcc").join(", ");
    if require_recipient && to.trim().is_empty() && cc.trim().is_empty() && bcc.trim().is_empty() {
        return Err(DraftError(
            "Add at least one recipient to To, Cc, or Bcc.".into(),
        ));
    }
    if require_recipient {
        for (field, value) in [("To", &to), ("Cc", &cc), ("Bcc", &bcc)] {
            crate::message::addresses(value, field).map_err(DraftError)?;
        }
    }
    let attachments: Vec<PathBuf> = header_values(headers, "attach")
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| resolve_path(value.trim(), base))
        .collect();
    if let Some(path) = attachments.iter().find(|path| !path.is_file()) {
        return Err(DraftError(format!(
            "Attachment not found: {}. Fix the Attach line and try again.",
            path.display()
        )));
    }
    Ok(Summary {
        to,
        cc,
        bcc,
        subject: header_values(headers, "subject")
            .into_iter()
            .next()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "(no subject)".into()),
        format: header_values(headers, "content-type")
            .into_iter()
            .next()
            .filter(|value| value.to_ascii_lowercase().starts_with("text/html"))
            .map(|_| MessageFormat::Html)
            .unwrap_or(MessageFormat::Plain),
        attachments,
    })
}

pub fn prepare(contents: &[u8], base: &Path) -> Result<Vec<u8>, DraftError> {
    prepare_message(contents, base, true)
}

fn prepare_message(
    contents: &[u8],
    base: &Path,
    require_recipient: bool,
) -> Result<Vec<u8>, DraftError> {
    let summary = inspect_message(contents, base, require_recipient)?;
    let text = String::from_utf8_lossy(contents);
    let (headers, body) = split_message(&text);
    let mut builder = MessageBuilder::new();
    for name in ["From", "To", "Cc", "Bcc"] {
        let value = header_values(headers, name).join(", ");
        match crate::message::addresses(&value, name) {
            Ok(addresses) if !addresses.is_empty() => {
                builder = builder.header(
                    name,
                    mail_builder::headers::address::Address::new_list(addresses),
                )
            }
            Ok(_) => {}
            Err(error) if require_recipient => return Err(DraftError(error)),
            Err(_) => builder = builder.header(name, Text::new(value)),
        }
    }
    builder = builder.subject(header_values(headers, "subject").join(" "));
    let parsed = MessageParser::default().parse(contents);
    if let Some(parsed) = &parsed {
        if let Some(id) = parsed.message_id() {
            builder = builder.message_id(id.to_owned());
        }
        if let Some(id) = parsed.in_reply_to().as_text() {
            builder = builder.in_reply_to(id.to_owned());
        }
        let references = parsed.references().as_text_list().unwrap_or_default();
        if !references.is_empty() {
            builder = builder.references(
                references
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }
    let body = body
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n");
    builder = match summary.format {
        MessageFormat::Plain => builder.text_body(body),
        MessageFormat::Html => builder.html_body(body),
    };
    for path in summary.attachments {
        let bytes = fs::read(&path).map_err(|error| {
            DraftError(format!(
                "Unable to read attachment {}: {error}",
                path.display()
            ))
        })?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment")
            .to_owned();
        let guessed = mime_guess::from_path(&path).first_or_octet_stream();
        let media_type = if guessed.type_() == "text" && std::str::from_utf8(&bytes).is_err() {
            "application/octet-stream".to_owned()
        } else {
            guessed.to_string()
        };
        builder = builder.attachment(media_type, name, bytes);
    }
    builder
        .write_to_vec()
        .map_err(|error| DraftError(error.to_string()))
}

fn split_message(message: &str) -> (&str, &str) {
    message
        .split_once("\r\n\r\n")
        .or_else(|| message.split_once("\n\n"))
        .unwrap_or((message, ""))
}

fn header_values(headers: &str, name: &str) -> Vec<String> {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
        .collect()
}

fn resolve_path(value: &str, base: &Path) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::mail::Mail;

    use super::{Composer, MessageFormat, inspect, prepare};

    #[test]
    fn keeps_headers_out_of_the_body_editor() {
        let mail = Mail::new(
            "first@example.com",
            "Ada <ada@example.com>",
            "Update",
            "hi",
            None,
        )
        .with_references(&["root@example.com"]);
        let mut composer =
            Composer::new(Some(&mail), Some("Tuna <tuna@example.com>"), Some("Tuna")).unwrap();
        composer.cc = "mert@example.com".into();
        let body = fs::read_to_string(&composer.body_path).unwrap();
        let message = String::from_utf8(composer.message(Path::new(".")).unwrap()).unwrap();

        assert!(!body.contains("Subject:"));
        assert!(body.contains("-- \nTuna"));
        let parsed = mail_parser::MessageParser::default()
            .parse(message.as_bytes())
            .unwrap();
        assert_eq!(
            parsed.to().unwrap().first().unwrap().name.as_deref(),
            Some("Ada")
        );
        assert_eq!(
            parsed.cc().unwrap().first().unwrap().address.as_deref(),
            Some("mert@example.com")
        );
        assert!(message.contains("Subject: Re: Update"));
        assert!(message.contains("In-Reply-To: <first@example.com>"));
        assert!(message.contains("<root@example.com> <first@example.com>"));
        composer.remove_body().unwrap();
    }

    #[test]
    fn builds_html_messages() {
        let mut composer = Composer::new(None, Some("tuna@example.com"), None).unwrap();
        composer.to = "ada@example.com".into();
        composer.format = MessageFormat::Html;
        fs::write(&composer.body_path, "<p>hello</p>").unwrap();

        let summary = composer.summary(Path::new(".")).unwrap();
        let message = String::from_utf8(composer.message(Path::new(".")).unwrap()).unwrap();

        assert_eq!(summary.format, MessageFormat::Html);
        let parsed = mail_parser::MessageParser::default()
            .parse(message.as_bytes())
            .unwrap();
        assert_eq!(parsed.body_html(0).unwrap(), "<p>hello</p>");
        assert!(parsed.date().is_some());
        composer.remove_body().unwrap();
    }

    #[test]
    fn validates_recipients_and_builds_attachments() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termail-draft-test-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), "hello").unwrap();
        let contents = b"To: ada@example.com\nCc: mert@example.com\nBcc: hidden@example.com\nSubject: Report\nAttach: notes.txt\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\nplease review";

        let summary = inspect(contents, &root).unwrap();
        assert_eq!(summary.to, "ada@example.com");
        assert_eq!(summary.attachments, [root.join("notes.txt")]);

        let message = String::from_utf8(prepare(contents, &root).unwrap()).unwrap();
        assert!(message.contains("Content-Type: multipart/mixed"));
        assert!(message.contains("filename=\"notes.txt\""));
        let parsed = mail_parser::MessageParser::default()
            .parse(message.as_bytes())
            .unwrap();
        assert_eq!(parsed.attachments().next().unwrap().contents(), b"hello");
        assert!(!message.contains("Attach: notes.txt"));

        let html = prepare(
            b"To: ada@example.com\nAttach: notes.txt\nContent-Type: text/html; charset=utf-8\n\n<p>hello</p>",
            &root,
        )
        .unwrap();
        let html = String::from_utf8(html).unwrap();
        let parsed = mail_parser::MessageParser::default()
            .parse(html.as_bytes())
            .unwrap();
        assert_eq!(parsed.body_html(0).unwrap().trim(), "<p>hello</p>");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_missing_recipients() {
        let error = inspect(b"To: \nSubject: hi\n\nbody", Path::new(".")).unwrap_err();
        assert!(error.to_string().contains("at least one recipient"));
    }

    #[test]
    fn removes_empty_attachment_headers() {
        let message = prepare(
            b"To: ada@example.com\nSubject: hi\nAttach: \n\nhello",
            Path::new("."),
        )
        .unwrap();
        let message = String::from_utf8(message).unwrap();

        assert!(!message.contains("Attach:"));
        assert!(message.ends_with("hello"));
    }

    #[test]
    fn saves_an_incomplete_structured_draft() {
        let composer = Composer::new(None, Some("tuna@example.com"), None).unwrap();
        let message = String::from_utf8(composer.draft_message(Path::new(".")).unwrap()).unwrap();

        let parsed = mail_parser::MessageParser::default()
            .parse(message.as_bytes())
            .unwrap();
        assert!(parsed.to().is_none());
        assert_eq!(parsed.subject().unwrap_or_default(), "");
        composer.remove_body().unwrap();
    }

    #[test]
    fn explains_missing_sender_identity() {
        let mut composer = Composer::new(None, None, None).unwrap();
        composer.to = "ada@example.com".into();

        let error = composer.summary(Path::new(".")).unwrap_err();

        assert!(error.to_string().contains("Set email"));
        composer.remove_body().unwrap();
    }

    #[test]
    fn encodes_unicode_headers_and_preserves_body_whitespace() {
        let mut composer =
            Composer::new(None, Some("Tuna Kılıç <tuna@example.com>"), None).unwrap();
        composer.to = "\"Doe, Ada\" <ada@example.com>".into();
        composer.subject = "Türkçe başlık: ğüşiöç ".repeat(12);
        let body = "\n\nMerhaba dünya!\nİkinci satır.\n";
        fs::write(&composer.body_path, body).unwrap();
        let bytes = composer.message(Path::new(".")).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        assert_eq!(parsed.subject(), Some(composer.subject.trim()));
        assert_eq!(
            parsed.from().unwrap().first().unwrap().name.as_deref(),
            Some("Tuna Kılıç")
        );
        assert_eq!(parsed.body_text(0).unwrap().replace("\r\n", "\n"), body);
        assert!(parsed.date().is_some());
        assert!(parsed.message_id().is_some());
        assert!(
            !bytes
                .windows(2)
                .any(|pair| pair[1] == b'\n' && pair[0] != b'\r')
        );
        assert!(
            bytes
                .split(|byte| *byte == b'\n')
                .all(|line| line.len() <= 998)
        );
    }

    #[test]
    fn invalid_recipients_can_be_saved_but_not_sent() {
        let mut composer = Composer::new(None, Some("tuna@example.com"), None).unwrap();
        composer.to = "unfinished-address".into();
        assert!(composer.message(Path::new(".")).is_err());
        assert!(composer.draft_message(Path::new(".")).is_ok());
        composer.to = "ada@example.com\r\nBcc: hidden@example.com".into();
        assert!(composer.summary(Path::new(".")).is_err());
    }

    #[test]
    fn resumes_quoted_names_and_unicode_attachments() {
        let directory = std::env::temp_dir().join(crate::storage::unique_name("termail-mime"));
        fs::create_dir(&directory).unwrap();
        let attachment = directory.join("Türkçe rapor.txt");
        fs::write(&attachment, [0, 1, 127, 128, 255]).unwrap();
        let draft = directory.join("draft.eml");
        let mut composer = Composer::new(None, Some("tuna@example.com"), None).unwrap();
        composer.to = "\"Doe, Ada\" <ada@example.com>".into();
        composer.attachments = attachment.display().to_string();
        fs::write(&draft, composer.draft_message(&directory).unwrap()).unwrap();
        let resumed = Composer::resume(&draft).unwrap();
        let bytes = resumed.message(&directory).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        assert_eq!(
            parsed.to().unwrap().first().unwrap().name.as_deref(),
            Some("Doe, Ada")
        );
        let part = parsed.attachments().next().unwrap();
        assert_eq!(
            mail_parser::MimeHeaders::attachment_name(part),
            Some("Türkçe rapor.txt")
        );
        assert_eq!(part.contents(), [0, 1, 127, 128, 255]);
        fs::remove_dir_all(directory).unwrap();
    }
}
