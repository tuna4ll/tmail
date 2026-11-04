//! Patches sent by mail, the way Git and the Linux kernel work: finding the series in
//! a conversation, applying it with `git am`, and fetching threads from public-inbox
//! archives such as lore.kernel.org.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::{App, Task, mail::Mail};

/// The `[patch]` table.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchConfig {
    /// The repository patches go into; the folder tmail started in when unset.
    pub repo: Option<PathBuf>,
    /// Extra arguments for `git am`; `["-3"]` when unset.
    pub am_args: Option<Vec<String>>,
    /// The public-inbox archive for `:lore`.
    pub archive: Option<String>,
}

/// Where a message sits in a series, from a subject such as `[PATCH v3 2/5] fix`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Position {
    pub version: u32,
    pub index: u32,
    pub total: u32,
}

pub fn position(subject: &str) -> Option<Position> {
    let subject = subject.trim_start();
    // Replies to a patch are review, not the patch.
    if subject.len() >= 3 && subject[..3].eq_ignore_ascii_case("re:") {
        return None;
    }
    let start = subject.find('[')?;
    let end = start + subject[start..].find(']')?;
    let tag = &subject[start + 1..end];
    if !tag
        .to_ascii_uppercase()
        .split_whitespace()
        .any(|word| word == "PATCH")
    {
        return None;
    }
    let mut found = Position {
        version: 1,
        index: 1,
        total: 1,
    };
    for word in tag.split_whitespace() {
        if let Some(number) = word
            .strip_prefix(['v', 'V'])
            .and_then(|rest| rest.parse().ok())
        {
            found.version = number;
        } else if let Some((index, total)) = word.split_once('/')
            && let (Ok(index), Ok(total)) = (index.parse(), total.parse())
        {
            found.index = index;
            found.total = total;
        }
    }
    Some(found)
}

/// Whether a message body carries a diff.
pub fn has_diff(body: &str) -> bool {
    body.contains("\ndiff --git ")
        || body.starts_with("diff --git ")
        || (body.contains("\n--- ") && body.contains("\n+++ ") && body.contains("\n@@ "))
}

/// The newest version of the series among `mails`, in order, with a note about
/// missing parts.
pub fn series<'a>(mails: &[&'a Mail]) -> (Vec<&'a Mail>, Option<String>) {
    let patches: Vec<(Position, &Mail)> = mails
        .iter()
        .filter_map(|mail| Some((position(&mail.subject)?, *mail)))
        .filter(|(position, mail)| position.index > 0 && has_diff(&mail.body))
        .collect();
    let Some(version) = patches.iter().map(|(position, _)| position.version).max() else {
        return (Vec::new(), None);
    };
    let mut chosen: Vec<(Position, &Mail)> = patches
        .into_iter()
        .filter(|(position, _)| position.version == version)
        .collect();
    chosen.sort_by_key(|(position, mail)| (position.index, mail.timestamp));
    chosen.dedup_by_key(|(position, _)| position.index);
    let total = chosen
        .iter()
        .map(|(position, _)| position.total)
        .max()
        .unwrap_or(0);
    let missing: Vec<String> = (1..=total)
        .filter(|index| !chosen.iter().any(|(position, _)| position.index == *index))
        .map(|index| index.to_string())
        .collect();
    let note = (!missing.is_empty()).then(|| {
        format!(
            "Part {} of {total} is missing from this conversation.",
            missing.join(", ")
        )
    });
    (chosen.into_iter().map(|(_, mail)| mail).collect(), note)
}

/// The messages as one mbox file (mboxrd), the input `git am` reads.
pub fn mbox(messages: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::new();
    for message in messages {
        output.extend_from_slice(b"From tmail Thu Jan  1 00:00:00 1970\n");
        let text = String::from_utf8_lossy(message).replace("\r\n", "\n");
        for line in text.split_inclusive('\n') {
            if line.trim_start_matches('>').starts_with("From ") {
                output.push(b'>');
            }
            output.extend_from_slice(line.as_bytes());
        }
        if !text.ends_with('\n') {
            output.push(b'\n');
        }
        output.push(b'\n');
    }
    output
}

/// Splits an mbox file into its messages.
pub fn split_mbox(text: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut current = String::new();
    let mut previous_blank = true;
    for line in text.split_inclusive('\n') {
        if previous_blank && line.starts_with("From ") {
            if !current.trim().is_empty() {
                messages.push(std::mem::take(&mut current));
            }
            current.clear();
            previous_blank = false;
            continue;
        }
        previous_blank = line.trim_end_matches(['\r', '\n']).is_empty();
        let unescaped = match line.strip_prefix('>') {
            Some(rest) if rest.trim_start_matches('>').starts_with("From ") => rest,
            _ => line,
        };
        current.push_str(unescaped);
    }
    if !current.trim().is_empty() {
        messages.push(current);
    }
    messages
}

/// The Message-ID in a lore address or in angle brackets, bare.
pub fn message_id(value: &str) -> String {
    let value = value.trim();
    let value = match value.split_once("://") {
        // https://lore.kernel.org/LIST/MESSAGE-ID/...
        Some((_, rest)) => rest.split('/').nth(2).unwrap_or_default(),
        None => value,
    };
    let value = value.trim_start_matches('<').trim_end_matches('>');
    decode(value)
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            output.push(byte);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"@.-_+=$".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Downloads a whole thread from a public-inbox archive and writes it into a Maildir
/// of its own, so nothing reaches your account or its server.
pub fn fetch_thread(archive: &str, id: &str, cache: &Path) -> Result<(PathBuf, usize), String> {
    use std::io::Read;
    let url = format!("{}/{}/t.mbox.gz", archive.trim_end_matches('/'), encode(id));
    let agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .new_agent();
    let response = agent
        .get(&url)
        .header("User-Agent", "tmail")
        .call()
        .map_err(|error| format!("Unable to fetch the thread: {error}"))?;
    let mut compressed = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut compressed)
        .map_err(|error| format!("Unable to fetch the thread: {error}"))?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(256 * 1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|_| "The archive sent something that is not a thread.".to_owned())?;
    let messages = split_mbox(&text);
    if messages.is_empty() {
        return Err("The archive has no messages for that ID.".into());
    }
    // Threads opened before are only a cache; drop the ones older than a week.
    if let Ok(entries) = fs::read_dir(cache) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 7 * 24 * 3600);
            if old {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    let root = cache.join(crate::storage::unique_name("thread"));
    for folder in ["cur", "new", "tmp"] {
        fs::create_dir_all(root.join(folder)).map_err(|error| error.to_string())?;
    }
    for (index, message) in messages.iter().enumerate() {
        fs::write(
            root.join("cur").join(format!(
                "{index:05}.lore{}S",
                crate::storage::MAILDIR_SEPARATOR
            )),
            message,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok((root, messages.len()))
}

/// Runs git in `repo` and returns what it printed.
fn git(repo: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()
        .map_err(|_| "Unable to run git. Install it first.".to_owned())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// The last lines of git's output that say something, on one line.
fn gist(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .collect();
    lines[lines.len().saturating_sub(2)..].join(" · ")
}

impl App {
    fn patch_repo(&self, argument: &str) -> PathBuf {
        if !argument.trim().is_empty() {
            return crate::cli::expand_path(argument.trim());
        }
        self.config
            .patch
            .repo
            .as_deref()
            .map(|path| crate::cli::expand_path(&path.to_string_lossy()))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The series in the selected conversation as raw messages.
    fn patch_series(&mut self) -> Option<(Vec<Vec<u8>>, String, Option<String>)> {
        let Some(conversation) = self.selected_conversation() else {
            self.notice = Some("Select a message first.".into());
            return None;
        };
        let mails = conversation.mails(&self.mails);
        let (patches, note) = series(&mails);
        if patches.is_empty() {
            self.notice = Some("This conversation has no patches.".into());
            return None;
        }
        let subject = patches[0].subject.clone();
        let mut messages = Vec::new();
        for mail in &patches {
            match mail.source_path.as_deref().map(fs::read) {
                Some(Ok(bytes)) => messages.push(bytes),
                _ => {
                    self.notice = Some("A patch file could not be read.".into());
                    return None;
                }
            }
        }
        Some((messages, subject, note))
    }

    /// `:patch apply [DIR]`: applies the series with `git am`.
    pub(crate) fn apply_patches(&mut self, argument: &str) {
        let Some((messages, _, note)) = self.patch_series() else {
            return;
        };
        let repo = self.patch_repo(argument);
        let mut arguments = self
            .config
            .patch
            .am_args
            .clone()
            .unwrap_or_else(|| vec!["-3".into()]);
        let count = messages.len();
        self.notice = Some(format!("Applying {count} patches in {}…", repo.display()));
        self.spawn_task(move || {
            let message = (|| {
                git(&repo, &["rev-parse", "--show-toplevel"])
                    .map_err(|_| format!("{} is not a Git repository.", repo.display()))?;
                let file = std::env::temp_dir()
                    .join(format!("{}.mbox", crate::storage::unique_name("series")));
                fs::write(&file, mbox(&messages)).map_err(|error| error.to_string())?;
                arguments.insert(0, "am".into());
                arguments.push(file.to_string_lossy().into_owned());
                let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
                let result = git(&repo, &arguments);
                let _ = fs::remove_file(&file);
                match result {
                    Ok(_) => Ok(format!(
                        "Applied {count} patches to {}{}",
                        repo.display(),
                        note.map(|note| format!(" · {note}")).unwrap_or_default()
                    )),
                    Err(output) => Err(format!(
                        "git am stopped: {} · fix it and run git am --continue, or git am --abort",
                        gist(&output)
                    )),
                }
            })();
            Task::Done {
                message: message.unwrap_or_else(|error| error),
                reload: false,
            }
        });
    }

    /// `:patch save`: writes the series as an mbox file to the download folder.
    pub(crate) fn save_patches(&mut self) {
        let Some((messages, subject, note)) = self.patch_series() else {
            return;
        };
        let name: String = subject
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .take(8)
            .collect::<Vec<_>>()
            .join("-");
        let path = self.config.download_dir().join(format!("{name}.mbox"));
        self.notice = Some(match fs::write(&path, mbox(&messages)) {
            Ok(()) => format!(
                "Saved {} patches to {}{}",
                messages.len(),
                path.display(),
                note.map(|note| format!(" · {note}")).unwrap_or_default()
            ),
            Err(error) => format!("Unable to save the series: {error}"),
        });
    }

    /// `:patch b4 [DIR]`: lets b4 fetch the newest version of the series and its
    /// review tags, then apply it.
    pub(crate) fn b4_patches(&mut self, argument: &str) {
        let Some(mail) = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
        else {
            self.notice = Some("Select a message first.".into());
            return;
        };
        let id = match &mail.account {
            Some(account) => mail
                .id
                .strip_prefix(&format!("{account}:"))
                .unwrap_or(&mail.id)
                .to_owned(),
            None => mail.id.clone(),
        };
        let repo = self.patch_repo(argument);
        self.notice = Some("Asking b4 for the series…".into());
        self.spawn_task(move || {
            let output = Command::new("b4")
                .arg("shazam")
                .arg(&id)
                .current_dir(&repo)
                .output();
            let message = match output {
                Err(_) => "Unable to run b4. Install it with pip install b4.".to_owned(),
                Ok(output) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    if output.status.success() {
                        format!(
                            "b4 applied the series to {} · {}",
                            repo.display(),
                            gist(&text)
                        )
                    } else {
                        format!("b4 stopped: {}", gist(&text))
                    }
                }
            };
            Task::Done {
                message,
                reload: false,
            }
        });
    }

    /// `:lore ID`: opens a thread from the public archive as a mailbox of its own.
    pub(crate) fn open_lore(&mut self, argument: &str) {
        let id = message_id(argument);
        if id.is_empty() {
            self.notice = Some("Give a Message-ID or a lore link: :lore ID".into());
            return;
        }
        let archive = self
            .config
            .patch
            .archive
            .clone()
            .unwrap_or_else(|| "https://lore.kernel.org/all".into());
        let Some(cache) = crate::mailbox::cache_dir().map(|dir| dir.join("lore")) else {
            self.notice = Some("There is no cache folder to keep the thread in.".into());
            return;
        };
        self.notice = Some(format!("Fetching the thread from {archive}…"));
        self.spawn_task(move || match fetch_thread(&archive, &id, &cache) {
            Ok((root, count)) => Task::OpenMaildir {
                path: root,
                message: format!("{count} messages from {archive} · p goes back to your accounts"),
            },
            Err(message) => Task::Done {
                message,
                reload: false,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_series_and_round_trips_mbox() {
        assert_eq!(
            position("[PATCH v3 2/5] net: fix"),
            Some(Position {
                version: 3,
                index: 2,
                total: 5
            })
        );
        assert_eq!(position("[RFC PATCH] idea").map(|p| p.total), Some(1));
        assert_eq!(position("Re: [PATCH 1/2] x"), None);
        assert_eq!(position("[PATCHES] nope"), None);
        let diff =
            "msg\n---\n a | 1 +\ndiff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n";
        let make = |subject: &str, body: &str| Mail::new(subject, "a@b", subject, body, None);
        let mails = [
            make("[PATCH v2 0/2] cover", "cover"),
            make("[PATCH v2 2/2] two", diff),
            make("[PATCH 1/2] old", diff),
            make("[PATCH v2 1/2] one", diff),
            make("Re: [PATCH v2 1/2] one", diff),
        ];
        let refs: Vec<&Mail> = mails.iter().collect();
        let (chosen, note) = series(&refs);
        let subjects: Vec<&str> = chosen.iter().map(|mail| mail.subject.as_str()).collect();
        assert_eq!(subjects, ["[PATCH v2 1/2] one", "[PATCH v2 2/2] two"]);
        assert!(note.is_none());
        let (_, note) = series(&refs[..2]);
        assert!(note.unwrap().contains("Part 1 of 2"));
        let messages = vec![
            b"Subject: a\r\n\r\nFrom here on\r\n>From quoted\r\n".to_vec(),
            b"Subject: b\n\nbody\n".to_vec(),
        ];
        let split = split_mbox(&String::from_utf8(mbox(&messages)).unwrap());
        assert_eq!(split.len(), 2);
        assert!(
            split[0].contains("\nFrom here on\n>From quoted\n"),
            "{:?}",
            split[0]
        );
        assert_eq!(
            message_id("https://lore.kernel.org/netdev/20260101.abc%2Bx@host.test/T/#t"),
            "20260101.abc+x@host.test"
        );
        assert_eq!(message_id("<a@b>"), "a@b");
        assert_eq!(encode("a/b@c"), "a%2Fb@c");
    }
}
