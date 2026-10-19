use email_address::EmailAddress;
use mail_builder::headers::address::Address;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub from: String,
    pub recipients: Vec<String>,
}

pub fn display_address(name: &str, address: &str) -> String {
    if name
        .chars()
        .any(|character| ",;<>@:\\[]\"()".contains(character))
    {
        format!(
            "\"{}\" <{address}>",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        format!("{name} <{address}>")
    }
}

pub fn addresses(value: &str, field: &str) -> Result<Vec<Address<'static>>, String> {
    if value.chars().any(char::is_control) {
        return Err(format!("{field} contains a control character."));
    }
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            parts.push(&value[start..index]);
            start = index + 1;
        }
    }
    parts.push(&value[start..]);
    if quoted || escaped {
        return Err(format!("{field} contains an unfinished quoted address."));
    }
    parts
        .into_iter()
        .map(|part| {
            let address: EmailAddress = part
                .trim()
                .parse()
                .map_err(|error| format!("Invalid {field} address: {error}"))?;
            let email = address.email();
            if !email.is_ascii() {
                return Err(format!(
                    "{field} requires an ASCII email address; international names are supported."
                ));
            }
            let source = format!("To: {}\r\n\r\n", part.trim());
            let parsed = mail_parser::MessageParser::default().parse(source.as_bytes());
            let name = parsed
                .as_ref()
                .and_then(|mail| mail.to())
                .and_then(|to| to.first())
                .and_then(|address| address.name.as_ref())
                .map(|name| name.to_string());
            Ok(Address::new_address(name, email))
        })
        .collect()
}

pub fn envelope(contents: &[u8]) -> Result<Envelope, String> {
    let parsed = mail_parser::MessageParser::default()
        .parse(contents)
        .ok_or("Unable to parse queued message.")?;
    let from = parsed.from().ok_or("Message has no sender.")?;
    let from: Vec<_> = from.iter().collect();
    if from.len() != 1 {
        return Err("Message must have exactly one sender.".into());
    }
    let from = from[0]
        .address
        .as_deref()
        .ok_or("Message has no sender address.")?;
    let validate = |address: &str| -> Result<String, String> {
        let values = addresses(address, "envelope")?;
        if values.len() != 1 {
            return Err("Invalid envelope address.".into());
        }
        Ok(values[0].unwrap_address().email.to_string())
    };
    let from = validate(from)?;
    let mut recipients = Vec::new();
    for field in [parsed.to(), parsed.cc(), parsed.bcc()]
        .into_iter()
        .flatten()
    {
        for address in field.iter() {
            let address = validate(
                address
                    .address
                    .as_deref()
                    .ok_or("Recipient has no email address.")?,
            )?;
            if !recipients.contains(&address) {
                recipients.push(address);
            }
        }
    }
    if recipients.is_empty() {
        return Err("Message has no recipients.".into());
    }
    Ok(Envelope { from, recipients })
}

pub fn wire_message(contents: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(contents.len());
    let mut headers = true;
    let mut hidden = false;
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        if headers {
            if line == b"\r\n" || line == b"\n" {
                headers = false;
                hidden = false;
            } else if !line.starts_with(b" ") && !line.starts_with(b"\t") {
                hidden = line.split(|byte| *byte == b':').next().is_some_and(|name| {
                    name.eq_ignore_ascii_case(b"bcc") || name.eq_ignore_ascii_case(b"resent-bcc")
                });
            }
        }
        if !hidden {
            output.extend_from_slice(line);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_every_address_without_silently_dropping_invalid_entries() {
        let values = addresses(
            "\"Doe, Ada\" <ada@example.com>, Tuna Kılıç <tuna@example.com>",
            "To",
        )
        .unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].unwrap_address().name.as_deref(), Some("Doe, Ada"));
        for value in [
            "invalid",
            "a@example.com, bad",
            "a@example.com,",
            "a@example.com\r\nBcc: hidden@example.com",
            "a@@example.com",
        ] {
            assert!(addresses(value, "To").is_err(), "{value}");
        }
    }

    #[test]
    fn keeps_hidden_recipients_in_envelope_only() {
        let source = b"From: me@example.com\r\nTo: ada@example.com\r\nBcc: hidden@example.com,\r\n other@example.com\r\nSubject: test\r\n\r\nBcc: body text\r\n";
        let envelope = envelope(source).unwrap();
        assert_eq!(
            envelope.recipients,
            ["ada@example.com", "hidden@example.com", "other@example.com"]
        );
        let wire = String::from_utf8(wire_message(source)).unwrap();
        assert!(!wire.contains("hidden@example.com"));
        assert!(!wire.contains("other@example.com"));
        assert!(wire.ends_with("Bcc: body text\r\n"));
    }
}
