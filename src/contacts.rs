//! The address book: a vCard file (RFC 6350) other apps can import and export.

use std::{fs, io, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub name: String,
    pub email: String,
}

impl Contact {
    /// The form used in address fields, such as `Ada Lovelace <ada@example.test>`.
    pub fn address(&self) -> String {
        crate::message::display_address(&self.name, &self.email)
    }
}

/// Every contact with an email address. A missing file is an empty book.
pub fn load(path: &Path) -> Vec<Contact> {
    fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

pub fn parse(text: &str) -> Vec<Contact> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = lines.last_mut()
        {
            last.push_str(&line[1..]);
        } else {
            lines.push(line.to_owned());
        }
    }
    let mut contacts = Vec::new();
    let mut name = String::new();
    let mut emails: Vec<String> = Vec::new();
    for line in lines {
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        // Grouped properties look like `item1.EMAIL`.
        let property = head
            .split(';')
            .next()
            .unwrap_or_default()
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match property.as_str() {
            "BEGIN" => {
                name.clear();
                emails.clear();
            }
            "FN" => {
                name = value
                    .replace("\\,", ",")
                    .replace("\\;", ";")
                    .trim()
                    .to_owned()
            }
            "EMAIL" if value.contains('@') => emails.push(value.trim().to_owned()),
            "END" => {
                for email in emails.drain(..) {
                    contacts.push(Contact {
                        name: name.clone(),
                        email,
                    });
                }
            }
            _ => {}
        }
    }
    contacts
}

/// Adds a contact unless the address is already in the book.
pub fn add(path: &Path, contact: &Contact) -> io::Result<bool> {
    let existing = load(path);
    if existing
        .iter()
        .any(|known| known.email.eq_ignore_ascii_case(&contact.email))
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str("\r\n");
    }
    text.push_str(&card(contact));
    fs::write(path, text)?;
    Ok(true)
}

/// Removes every card that holds the address.
pub fn remove(path: &Path, email: &str) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut kept = String::new();
    let mut card = String::new();
    for line in text.split_inclusive('\n') {
        card.push_str(line);
        if line.trim_end().eq_ignore_ascii_case("END:VCARD") {
            let holds = parse(&card)
                .iter()
                .any(|contact| contact.email.eq_ignore_ascii_case(email));
            if !holds {
                kept.push_str(&card);
            }
            card.clear();
        }
    }
    kept.push_str(&card);
    fs::write(path, kept)
}

fn card(contact: &Contact) -> String {
    let name = if contact.name.is_empty() {
        contact.email.clone()
    } else {
        contact.name.replace(',', "\\,").replace(';', "\\;")
    };
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{name}\r\nEMAIL;TYPE=INTERNET:{}\r\nEND:VCARD\r\n",
        contact.email
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_writes_and_removes_cards() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Ada\\, Countess\r\nitem1.EMAIL;TYPE=work:ada@example.test\r\nEMAIL:ada@home.test\r\nEND:VCARD\r\nBEGIN:VCARD\r\nFN:No mail\r\nEND:VCARD\r\n";
        let contacts = parse(text);
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].name, "Ada, Countess");
        let path =
            std::env::temp_dir().join(format!("{}.vcf", crate::storage::unique_name("contacts")));
        fs::write(&path, text).unwrap();
        let grace = Contact {
            name: "Grace".into(),
            email: "grace@example.test".into(),
        };
        assert!(add(&path, &grace).unwrap());
        assert!(!add(&path, &grace).unwrap(), "no duplicates");
        remove(&path, "ADA@example.test").unwrap();
        let left = load(&path);
        assert_eq!(left, vec![grace]);
        let _ = fs::remove_file(path);
    }
}
