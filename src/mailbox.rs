use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    fmt, fs,
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use mail_parser::{Address, MessageParser, MimeHeaders, decoders::html::html_to_text};
use serde::{Deserialize, Serialize};

use crate::mail::{Attachment, Mail, demo_mails};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MailboxSource {
    Demo,
    Maildir(PathBuf),
    Unified(Vec<(String, PathBuf)>),
}

#[derive(Debug)]
pub struct LoadReport {
    pub mails: Vec<Mail>,
    /// Every folder in the Maildir, including empty ones.
    pub folders: Vec<String>,
    pub skipped: Vec<String>,
    pub attempted: usize,
}

#[derive(Debug)]
pub struct LoadError {
    message: String,
}

#[derive(Clone, Debug)]
pub struct MaildirChange {
    before: PathBuf,
    after: PathBuf,
}

/// A parsed message, keyed by the unique part of its file name so flag changes and
/// moves between folders reuse it. Size and modification time detect rewrites.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedMessage {
    modified: u128,
    length: u64,
    mail: Mail,
}

/// A message file found while scanning, before it is read.
struct Entry {
    path: PathBuf,
    base: String,
    unread: bool,
    starred: bool,
    modified: u128,
    seconds: u64,
    length: u64,
}

/// The Maildir++ folders under `root`: the Inbox itself and every `.Name` folder.
fn maildir_folders(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    fn walk(
        directory: &Path,
        names: &mut Vec<String>,
        found: &mut Vec<(String, PathBuf)>,
    ) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(directory)?.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(name) = name.strip_prefix('.') else {
                continue;
            };
            if name.is_empty() || !path.is_dir() {
                continue;
            }
            names.push(name.to_owned());
            if path.join("cur").is_dir() && path.join("new").is_dir() {
                found.push((names.join("."), path.clone()));
            }
            walk(&path, names, found)?;
            names.pop();
        }
        Ok(())
    }
    let mut found = Vec::new();
    if root.join("cur").is_dir() {
        found.push(("INBOX".to_owned(), root.to_path_buf()));
    }
    walk(root, &mut Vec::new(), &mut found)?;
    Ok(found)
}

fn scan_folder(folder: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for child in ["cur", "new"] {
        let directory = match fs::read_dir(folder.join(child)) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in directory.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .unwrap_or_default();
            let (base, flags, _) = crate::storage::maildir_info(&name);
            entries.push(Entry {
                path: entry.path(),
                base: base.to_owned(),
                unread: !flags.contains('S'),
                starred: flags.contains('F'),
                modified: modified.as_nanos(),
                seconds: modified.as_secs(),
                length: metadata.len(),
            });
        }
    }
    Ok(entries)
}

impl MailboxSource {
    pub fn label(&self) -> String {
        match self {
            Self::Demo => "demo mailbox".into(),
            Self::Maildir(path) => path.display().to_string(),
            Self::Unified(_) => "all accounts".into(),
        }
    }

    pub fn load(&self) -> Result<LoadReport, LoadError> {
        match self {
            Self::Demo => Ok(LoadReport {
                mails: demo_mails(),
                folders: Vec::new(),
                skipped: Vec::new(),
                attempted: 7,
            }),
            Self::Maildir(path) => load_maildir(path),
            Self::Unified(accounts) => load_unified(accounts),
        }
    }

    pub fn fingerprint(&self) -> Result<u64, LoadError> {
        match self {
            Self::Demo => Ok(0),
            Self::Maildir(path) => mailbox_fingerprint(path).map_err(Into::into),
            Self::Unified(accounts) => accounts
                .iter()
                .try_fold(0, |fingerprint, (id, path)| {
                    mailbox_fingerprint(path)
                        .map(|value| fingerprint ^ content_digest(id.as_bytes()) ^ value)
                })
                .map_err(Into::into),
        }
    }

    pub fn set_seen(&self, mail: &Mail, seen: bool) -> Result<Option<MaildirChange>, LoadError> {
        self.set_flag(mail, 'S', seen)
    }

    pub fn set_starred(
        &self,
        mail: &Mail,
        starred: bool,
    ) -> Result<Option<MaildirChange>, LoadError> {
        self.set_flag(mail, 'F', starred)
    }

    pub fn set_answered(&self, mail: &Mail) -> Result<Option<MaildirChange>, LoadError> {
        self.set_flag(mail, 'R', true)
    }

    pub fn archive(&self, mail: &Mail) -> Result<Option<MaildirChange>, LoadError> {
        self.move_to(mail, ".Archive")
    }

    pub fn trash(&self, mail: &Mail) -> Result<Option<MaildirChange>, LoadError> {
        self.move_to(mail, ".Trash")
    }

    /// Removes a message for good. Sync then expunges it on the server.
    pub fn delete(&self, mail: &Mail) -> Result<(), LoadError> {
        let Some(root) = self.root_for(mail)? else {
            return Ok(());
        };
        let path = mail.source_path.as_ref().ok_or_else(|| LoadError {
            message: "Message has no Maildir path.".into(),
        })?;
        if !path.starts_with(root) {
            return Err(LoadError {
                message: "Message is outside the active Maildir.".into(),
            });
        }
        fs::remove_file(path)?;
        crate::storage::sync_parent(path).map_err(Into::into)
    }

    /// Moves a message to a folder by its display name, such as `INBOX` or `Work.Clients`.
    pub fn move_to_named(
        &self,
        mail: &Mail,
        name: &str,
    ) -> Result<Option<MaildirChange>, LoadError> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(LoadError {
                message: "Invalid folder name.".into(),
            });
        }
        if name == "INBOX" {
            self.move_to(mail, "")
        } else {
            self.move_to(mail, &format!(".{name}"))
        }
    }

    pub fn mark_sent(&self, mail: &Mail) -> Result<Option<MaildirChange>, LoadError> {
        self.move_to(mail, ".Sent")
    }

    pub fn save_draft(&self, contents: &[u8]) -> Result<PathBuf, LoadError> {
        let Self::Maildir(root) = self else {
            return Err(LoadError {
                message: "Choose one account before saving drafts.".into(),
            });
        };
        save_message(root, ".Drafts", "new", contents, "").map_err(Into::into)
    }

    pub fn stage_outbox(&self, contents: &[u8]) -> Result<PathBuf, LoadError> {
        let Self::Maildir(root) = self else {
            return Err(LoadError {
                message: "Choose one account before queueing messages.".into(),
            });
        };
        save_message(root, ".Outbox", "tmp", contents, "").map_err(Into::into)
    }

    #[cfg(test)]
    pub fn save_outbox(&self, contents: &[u8]) -> Result<PathBuf, LoadError> {
        let temporary = self.stage_outbox(contents)?;
        let folder = temporary.parent().unwrap().parent().unwrap();
        let destination = folder.join("new").join(temporary.file_name().unwrap());
        crate::storage::rename(&temporary, &destination)?;
        Ok(destination)
    }

    pub fn update_draft(&self, path: &Path, contents: &[u8]) -> Result<PathBuf, LoadError> {
        let Self::Maildir(root) = self else {
            return Err(LoadError {
                message: "Choose one account before saving drafts.".into(),
            });
        };
        let folder = root.join(".Drafts");
        if ![folder.join("new"), folder.join("cur")]
            .iter()
            .any(|parent| path.parent() == Some(parent.as_path()))
        {
            return Err(LoadError {
                message: "Draft is outside the active Drafts folder.".into(),
            });
        }
        let temporary = save_message(root, ".Drafts", "tmp", contents, "")?;
        if let Err(error) = crate::storage::rename(&temporary, path) {
            let _ = fs::remove_file(temporary);
            return Err(error.into());
        }
        Ok(path.to_path_buf())
    }

    pub fn save_attachment(
        &self,
        mail: &Mail,
        index: usize,
        destination: &Path,
    ) -> Result<PathBuf, LoadError> {
        let Some(root) = self.root_for(mail)? else {
            return Err(LoadError {
                message: "Open a Maildir mailbox to save attachments.".into(),
            });
        };
        let source = mail.source_path.as_ref().ok_or_else(|| LoadError {
            message: "Message has no Maildir path.".into(),
        })?;
        if !source.starts_with(root) {
            return Err(LoadError {
                message: "Message is outside the active Maildir.".into(),
            });
        }
        let contents = fs::read(source)?;
        let parsed = MessageParser::default()
            .parse(&contents)
            .ok_or_else(|| LoadError {
                message: "Unable to parse the message attachment.".into(),
            })?;
        let part = *listed_attachments(&parsed)
            .get(index)
            .ok_or_else(|| LoadError {
                message: "Attachment is no longer available.".into(),
            })?;
        let fallback = format!("attachment-{}", index + 1);
        let name = safe_file_name(part.attachment_name().unwrap_or(&fallback));
        fs::create_dir_all(destination)?;
        let path = available_path(destination, &name);
        fs::write(&path, part.contents())?;
        Ok(path)
    }

    fn set_flag(
        &self,
        mail: &Mail,
        flag: char,
        enabled: bool,
    ) -> Result<Option<MaildirChange>, LoadError> {
        let Some(root) = self.root_for(mail)? else {
            return Ok(None);
        };
        let path = mail.source_path.as_ref().ok_or_else(|| LoadError {
            message: "Message has no Maildir path.".into(),
        })?;
        if !path.starts_with(root) {
            return Err(LoadError {
                message: "Message is outside the active Maildir.".into(),
            });
        }
        rename_with_flag(path, flag, enabled)
            .map(Some)
            .map_err(Into::into)
    }

    fn move_to(&self, mail: &Mail, folder: &str) -> Result<Option<MaildirChange>, LoadError> {
        let Some(root) = self.root_for(mail)? else {
            return Ok(None);
        };
        let path = mail.source_path.as_ref().ok_or_else(|| LoadError {
            message: "Message has no Maildir path.".into(),
        })?;
        if !path.starts_with(root) {
            return Err(LoadError {
                message: "Message is outside the active Maildir.".into(),
            });
        }
        move_to_folder(path, &root.join(folder))
            .map(Some)
            .map_err(Into::into)
    }

    fn root_for<'a>(&'a self, mail: &Mail) -> Result<Option<&'a PathBuf>, LoadError> {
        match self {
            Self::Demo => Ok(None),
            Self::Maildir(root) => Ok(Some(root)),
            Self::Unified(accounts) => {
                let account = mail.account.as_deref().ok_or_else(|| LoadError {
                    message: "Message has no account.".into(),
                })?;
                accounts
                    .iter()
                    .find(|(id, _)| id == account)
                    .map(|(_, path)| Some(path))
                    .ok_or_else(|| LoadError {
                        message: "Message account is no longer configured.".into(),
                    })
            }
        }
    }
}

impl MaildirChange {
    pub fn after_path(&self) -> &Path {
        &self.after
    }

    pub fn undo(self) -> Result<(), LoadError> {
        fs::rename(self.after, self.before).map_err(Into::into)
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LoadError {}

impl From<io::Error> for LoadError {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

fn load_maildir(path: &Path) -> Result<LoadReport, LoadError> {
    let mut mails = Vec::new();
    let mut folders = Vec::new();
    let mut skipped = crate::outbox::recover(path);
    let mut attempted = 0;
    let cache = load_cache(path).unwrap_or_default();
    let mut next_cache = BTreeMap::new();

    let mut dirty = false;
    for (mailbox, folder) in maildir_folders(path)? {
        folders.push(mailbox.clone());
        for entry in scan_folder(&folder)? {
            attempted += 1;
            if let Some(cached) = cache.get(&entry.base)
                && cached.modified == entry.modified
                && cached.length == entry.length
            {
                // Flags and location come from the file name, so renames reuse the parse.
                let mut mail = cached.mail.clone();
                mail.unread = entry.unread;
                mail.starred = entry.starred;
                mail.source_path = Some(entry.path.clone());
                mail.mailbox = Some(mailbox.clone());
                next_cache.insert(entry.base, cached.clone());
                mails.push(mail);
                continue;
            }
            dirty = true;
            let Ok(contents) = fs::read(&entry.path) else {
                skipped.push(entry.path.display().to_string());
                continue;
            };
            let Some(parsed) = MessageParser::default().parse(&contents) else {
                skipped.push(entry.path.display().to_string());
                continue;
            };
            let fallback_id = entry.path.display().to_string();
            let id = parsed.message_id().map(clean_id).unwrap_or(fallback_id);
            let from = parsed
                .from()
                .and_then(Address::first)
                .map(format_address)
                .unwrap_or_else(|| "unknown sender".into());
            let to = parsed.to().map(format_addresses).unwrap_or_default();
            let cc = parsed.cc().map(format_addresses).unwrap_or_default();
            let date = parsed.date().map(|date| date.to_rfc3339());
            let timestamp = parsed
                .date()
                .map(|date| date.to_timestamp())
                .or(Some(entry.seconds as i64));
            let reply_to = parsed.in_reply_to().as_text().map(clean_id);
            let references = parsed
                .references()
                .as_text_list()
                .unwrap_or_default()
                .iter()
                .map(|value| clean_id(value))
                .collect();
            let reply_address = parsed.reply_to().map(format_addresses).unwrap_or_default();
            let unread = entry.unread;
            let starred = entry.starred;
            let html = parsed.html_body_count() > 0;
            let html_body = parsed.body_html(0);
            let body = parsed
                .body_text(0)
                .map(|body| body.into_owned())
                .or_else(|| html_body.as_deref().map(readable_html))
                .unwrap_or_default();
            let links = html_body.as_deref().map(extract_links).unwrap_or_default();
            let attachments: Vec<Attachment> = listed_attachments(&parsed)
                .into_iter()
                .enumerate()
                .map(|(index, part)| {
                    let media_type = part
                        .content_type()
                        .map(|kind| {
                            let subtype = kind.c_subtype.as_deref().unwrap_or("octet-stream");
                            format!("{}/{subtype}", kind.c_type)
                        })
                        .unwrap_or_else(|| "application/octet-stream".into());
                    Attachment {
                        name: part
                            .attachment_name()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("attachment-{}", index + 1)),
                        media_type,
                        size: part.contents().len(),
                    }
                })
                .collect();

            let mail = Mail {
                id,
                from,
                to,
                cc,
                subject: parsed.subject().unwrap_or("(no subject)").into(),
                body,
                date,
                timestamp,
                reply_to,
                references,
                reply_address,
                unread,
                starred,
                attachment_count: attachments.len(),
                attachments,
                links,
                html,
                account: None,
                source_path: Some(entry.path.clone()),
                mailbox: Some(mailbox.clone()),
                delivery_status: None,
                partial: parsed
                    .header_raw(crate::imap_client::PARTIAL_HEADER)
                    .is_some(),
                security: crate::crypto::Kind::detect(&parsed),
                invite: parsed.parts.iter().find_map(|part| {
                    let kind = part.content_type()?;
                    let calendar = kind.c_type.eq_ignore_ascii_case("text")
                        && kind
                            .c_subtype
                            .as_deref()
                            .is_some_and(|subtype| subtype.eq_ignore_ascii_case("calendar"));
                    let named = part
                        .attachment_name()
                        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".ics"));
                    (calendar || named)
                        .then(|| crate::calendar::parse(&String::from_utf8_lossy(part.contents())))
                        .flatten()
                }),
                sender_check: parsed
                    .header_raw("Authentication-Results")
                    .and_then(|results| {
                        let address = parsed
                            .from()
                            .and_then(Address::first)
                            .and_then(|from| from.address())
                            .unwrap_or_default();
                        crate::links::sender_check(results, address)
                    }),
                unsubscribe: parsed.header_raw("List-Unsubscribe").and_then(|value| {
                    crate::mail::Unsubscribe::parse(
                        &value.replace(['\r', '\n'], ""),
                        parsed.header_raw("List-Unsubscribe-Post"),
                    )
                }),
            };
            next_cache.insert(
                entry.base,
                CachedMessage {
                    modified: entry.modified,
                    length: entry.length,
                    mail: mail.clone(),
                },
            );
            mails.push(mail);
        }
    }

    mails.sort_by_key(|mail| mail.timestamp.unwrap_or_default());
    for mail in &mut mails {
        if mail.mailbox.as_deref() == Some("Outbox")
            && let Some(source) = &mail.source_path
        {
            mail.delivery_status = Some(crate::outbox::describe(path, source));
        }
    }
    if dirty || next_cache.len() != cache.len() {
        let _ = save_cache(path, &next_cache);
    }
    Ok(LoadReport {
        mails,
        folders,
        skipped,
        attempted,
    })
}

fn load_unified(accounts: &[(String, PathBuf)]) -> Result<LoadReport, LoadError> {
    let mut report = LoadReport {
        mails: Vec::new(),
        folders: Vec::new(),
        skipped: Vec::new(),
        attempted: 0,
    };
    for (account, path) in accounts {
        let loaded = load_maildir(path)?;
        report.attempted += loaded.attempted;
        report.folders.extend(loaded.folders);
        report.skipped.extend(loaded.skipped);
        report
            .mails
            .extend(loaded.mails.into_iter().map(|mut mail| {
                mail.id = format!("{account}:{}", mail.id);
                mail.reply_to = mail.reply_to.map(|id| format!("{account}:{id}"));
                mail.references = mail
                    .references
                    .into_iter()
                    .map(|id| format!("{account}:{id}"))
                    .collect();
                mail.account = Some(account.clone());
                mail
            }));
    }
    report
        .mails
        .sort_by_key(|mail| mail.timestamp.unwrap_or_default());
    Ok(report)
}

/// Caches older than this belong to Maildirs that are no longer opened.
const CACHE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(60 * 60 * 24 * 30);

/// Creates, renames or deletes a folder of a Maildir that does not sync.
pub fn change_local_folder(
    root: &Path,
    change: &crate::imap_client::FolderChange,
) -> Result<String, String> {
    use crate::imap_client::{FolderChange, local_name};
    let children = |local: &str| -> Vec<String> {
        fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| name == local || name.starts_with(&format!("{local}.")))
            .collect()
    };
    match change {
        FolderChange::Create(name) => {
            let folder = root.join(local_name(name));
            if folder.exists() {
                return Err("That folder already exists.".into());
            }
            for child in ["cur", "new", "tmp"] {
                fs::create_dir_all(folder.join(child)).map_err(|error| error.to_string())?;
            }
            Ok(format!("Created {name}"))
        }
        FolderChange::Rename { local, name } => {
            let target = local_name(name);
            if root.join(&target).exists() {
                return Err("That folder already exists.".into());
            }
            for folder in children(local) {
                let renamed = format!("{target}{}", &folder[local.len()..]);
                fs::rename(root.join(&folder), root.join(renamed))
                    .map_err(|error| error.to_string())?;
            }
            Ok(format!("Renamed to {name}"))
        }
        FolderChange::Delete(local) => {
            for folder in children(local) {
                fs::remove_dir_all(root.join(folder)).map_err(|error| error.to_string())?;
            }
            Ok("Folder deleted".into())
        }
    }
}

pub(crate) fn cache_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join("termail-test-cache"));
    }
    ProjectDirs::from("com", "tunakilic", "termail").map(|dirs| dirs.cache_dir().to_path_buf())
}

fn cache_path(maildir: &Path) -> Option<PathBuf> {
    let mut hasher = DefaultHasher::new();
    maildir.hash(&mut hasher);
    cache_dir().map(|dir| dir.join(format!("maildir-v8-{:016x}.json", hasher.finish())))
}

/// Removes caches nobody has opened for a month.
pub fn prune_caches() {
    let Some(entries) = cache_dir().and_then(|dir| fs::read_dir(dir).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > CACHE_LIFETIME);
        if stale && entry.file_name().to_string_lossy().starts_with("maildir-") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn load_cache(maildir: &Path) -> io::Result<BTreeMap<String, CachedMessage>> {
    let Some(path) = cache_path(maildir) else {
        return Ok(BTreeMap::new());
    };
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let contents = fs::read(path)?;
    serde_json::from_slice(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn save_cache(maildir: &Path, cache: &BTreeMap<String, CachedMessage>) -> io::Result<()> {
    let Some(path) = cache_path(maildir) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    let temporary = path.with_file_name(crate::storage::unique_name("cache"));
    let contents = serde_json::to_vec(cache)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    // The cache holds message bodies, so it gets the same private mode as the mail.
    if let Err(error) = crate::storage::write_new(&temporary, &contents) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(temporary, path)
}

fn content_digest(contents: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    hasher.finish()
}

fn rename_with_flag(path: &Path, flag: char, enabled: bool) -> io::Result<MaildirChange> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid Maildir filename."))?;
    let (base, current, separator) = crate::storage::maildir_info(file_name);
    let mut flags: Vec<char> = current.chars().filter(|value| *value != flag).collect();
    if enabled {
        flags.push(flag);
    }
    flags.sort_unstable();
    flags.dedup();
    let file_name = format!("{base}{separator}{}", flags.into_iter().collect::<String>());
    let parent = if enabled && flag == 'S' && path.parent().is_some_and(|dir| dir.ends_with("new"))
    {
        path.parent().unwrap().parent().unwrap().join("cur")
    } else {
        path.parent().unwrap().to_path_buf()
    };
    let after = parent.join(file_name);
    if after != path && after.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "A message with the target flags already exists.",
        ));
    }
    fs::rename(path, &after)?;
    Ok(MaildirChange {
        before: path.to_path_buf(),
        after,
    })
}

fn move_to_folder(path: &Path, folder: &Path) -> io::Result<MaildirChange> {
    for child in ["cur", "new", "tmp"] {
        fs::create_dir_all(folder.join(child))?;
    }
    crate::storage::sync_parent(folder)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid Maildir filename."))?;
    let (base, flags, separator) = crate::storage::maildir_info(file_name);
    let mut flags: Vec<char> = flags.chars().collect();
    if folder.file_name().is_some_and(|name| name == ".Sent") {
        flags.push('S');
    }
    flags.sort_unstable();
    flags.dedup();
    let flags: String = flags.into_iter().collect();
    let mut after = folder.join("cur").join(format!("{base}{separator}{flags}"));
    if after.exists() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let unique = format!("{base}.termail-{nonce}{separator}{flags}");
        after = folder.join("cur").join(unique);
    }
    crate::storage::rename(path, &after)?;
    Ok(MaildirChange {
        before: path.to_path_buf(),
        after,
    })
}

fn mailbox_fingerprint(path: &Path) -> io::Result<u64> {
    let mut fingerprint = 0;
    fingerprint_dir(path, &mut fingerprint)?;
    Ok(fingerprint)
}

fn save_message(
    root: &Path,
    folder: &str,
    location: &str,
    contents: &[u8],
    suffix: &str,
) -> io::Result<PathBuf> {
    let folder = root.join(folder);
    for child in ["cur", "new", "tmp"] {
        fs::create_dir_all(folder.join(child))?;
    }
    crate::storage::sync_parent(&folder)?;
    crate::storage::sync_parent(&folder.join("tmp"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let name = format!("{nonce}.{}{suffix}", crate::storage::unique_name("termail"));
    let temporary = folder.join("tmp").join(&name);
    let destination = folder.join(location).join(name);
    crate::storage::write_new(&temporary, contents)?;
    if temporary != destination {
        crate::storage::rename(&temporary, &destination)?;
    }
    Ok(destination)
}

pub(crate) fn safe_file_name(value: &str) -> String {
    let mut name: String = value
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    name = name.trim_matches([' ', '.']).to_owned();
    if name.is_empty() {
        return "attachment".into();
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        name.insert(0, '_');
    }
    name
}

fn available_path(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    if !path.exists() {
        return path;
    }
    let name = Path::new(name);
    let stem = name
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let extension = name.extension().and_then(|value| value.to_str());
    for number in 2.. {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{stem} ({number}).{extension}")),
            None => directory.join(format!("{stem} ({number})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Hashes file names and directory modification times. Adding, removing, renaming or
/// replacing a message changes its directory, so no per-file metadata is needed.
fn fingerprint_dir(path: &Path, fingerprint: &mut u64) -> io::Result<()> {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    fs::metadata(path)?.modified()?.hash(&mut hasher);
    *fingerprint ^= hasher.finish();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir() {
            if name == "tmp" || name == "termail-sync" {
                continue;
            }
            fingerprint_dir(&entry.path(), fingerprint)?;
        } else {
            let mut hasher = DefaultHasher::new();
            path.hash(&mut hasher);
            name.hash(&mut hasher);
            *fingerprint ^= hasher.finish();
        }
    }
    Ok(())
}

fn clean_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .into()
}

/// The attachments a reader sees: pictures the HTML shows inline through `cid:` are part
/// of the message body, not files.
pub(crate) fn listed_attachments<'a>(
    message: &'a mail_parser::Message<'a>,
) -> Vec<&'a mail_parser::MessagePart<'a>> {
    let html: String = (0..message.html_body_count())
        .filter_map(|index| message.body_html(index))
        .collect::<Vec<_>>()
        .concat()
        .to_lowercase();
    message
        .attachments()
        .filter(|part| {
            !part.content_id().is_some_and(|id| {
                let id = clean_id(id).to_lowercase();
                !id.is_empty() && html.contains(&format!("cid:{id}"))
            })
        })
        .collect()
}

pub(crate) fn format_addresses(addresses: &Address<'_>) -> Vec<String> {
    addresses.iter().map(format_address).collect()
}

fn format_address(address: &mail_parser::Addr<'_>) -> String {
    match (address.name.as_deref(), address.address.as_deref()) {
        (Some(name), Some(email)) => crate::message::display_address(name, email),
        (None, Some(email)) => email.into(),
        (Some(name), None) => name.into(),
        (None, None) => "unknown".into(),
    }
}

fn extract_links(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut offset = 0;

    while let Some(position) = lower[offset..].find("href") {
        let start = offset + position + 4;
        let Some(equal) = html[start..].find('=') else {
            break;
        };
        let value_start = start + equal + 1;
        let value = html[value_start..].trim_start();
        let skipped = html[value_start..].len() - value.len();
        let Some(first) = value.chars().next() else {
            break;
        };
        let (link, consumed) = if matches!(first, '\'' | '"') {
            let rest = &value[first.len_utf8()..];
            let end = rest.find(first).unwrap_or(rest.len());
            (&rest[..end], end + first.len_utf8() * 2)
        } else {
            let end = value
                .find(|character: char| character.is_whitespace() || character == '>')
                .unwrap_or(value.len());
            (&value[..end], end)
        };
        if (link.starts_with("https://") || link.starts_with("http://"))
            && !links.iter().any(|existing| existing == link)
        {
            links.push(link.to_owned());
        }
        offset = value_start + skipped + consumed;
    }
    links
}

fn readable_html(html: &str) -> String {
    let mut prepared = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        prepared.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            prepared.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let tag = &rest[start..=start + end];
        let name = tag[1..tag.len() - 1]
            .trim()
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if tag.starts_with("</")
            && matches!(
                name.as_str(),
                "p" | "div" | "h1" | "h2" | "h3" | "li" | "tr"
            )
        {
            prepared.push_str("<br>");
        }
        prepared.push_str(tag);
        rest = &rest[start + end + 1..];
    }
    prepared.push_str(rest);
    html_to_text(&prepared)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{MailboxSource, readable_html};

    #[test]
    fn keeps_html_blocks_readable() {
        let text = readable_html("<h1>Launch</h1><p>Ready now.</p>");

        assert!(text.contains("Launch\n"));
        assert!(text.contains("Ready now."));
    }

    #[test]
    fn loads_maildir_headers_and_flags() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termail-maildir-{nonce}"));
        fs::create_dir_all(root.join("cur")).unwrap();
        fs::create_dir_all(root.join("new")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
        fs::write(
            root.join("cur/message!2,FS"),
            b"From: Ada <ada@example.com>\r\nTo: Tuna <tuna@example.com>\r\nSubject: Build ready\r\nMessage-ID: <root@example.com>\r\nDate: Thu, 1 Oct 2026 09:00:00 +0300\r\n\r\nPlease review it.",
        )
        .unwrap();
        fs::write(root.join("new/broken"), []).unwrap();

        let source = MailboxSource::Maildir(root.clone());
        let report = source.load().unwrap();

        assert_eq!(report.mails.len(), 1);
        assert_eq!(report.mails[0].id, "root@example.com");
        assert_eq!(report.mails[0].to, ["Tuna <tuna@example.com>"]);
        assert!(report.mails[0].starred);
        assert!(!report.mails[0].unread);
        assert_eq!(report.attempted, 2);
        assert_eq!(report.skipped.len(), 1);

        let fingerprint = source.fingerprint().unwrap();
        let change = source.set_seen(&report.mails[0], false).unwrap().unwrap();
        assert_ne!(source.fingerprint().unwrap(), fingerprint);
        assert!(source.load().unwrap().mails[0].unread);
        change.undo().unwrap();
        assert!(!source.load().unwrap().mails[0].unread);

        let change = source.archive(&report.mails[0]).unwrap().unwrap();
        assert_eq!(
            source.load().unwrap().mails[0].mailbox.as_deref(),
            Some("Archive")
        );
        change.undo().unwrap();
        assert_eq!(source.load().unwrap().mails.len(), 1);

        let draft = source.save_draft(b"To: ada@example.com\n\nhello").unwrap();
        assert!(draft.starts_with(root.join(".Drafts/new")));
        assert_eq!(fs::read(draft).unwrap(), b"To: ada@example.com\n\nhello");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_html_links_and_attachments() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termail-rich-{nonce}"));
        for child in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        fs::write(
            root.join("new/message"),
            b"From: ada@example.com\r\nSubject: Report\r\nMessage-ID: <rich@example.com>\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Read <a href=\"https://example.com/report\">the report</a>.</p>\r\n--x\r\nContent-Type: text/plain; name=notes.txt\r\nContent-Disposition: attachment; filename=notes.txt\r\n\r\nhello\r\n--x--\r\n",
        )
        .unwrap();

        let source = MailboxSource::Maildir(root.clone());
        let mail = source.load().unwrap().mails.remove(0);

        assert!(mail.html);
        assert!(mail.body.contains("Read the report"));
        assert_eq!(mail.links, ["https://example.com/report"]);
        assert_eq!(mail.attachments[0].name, "notes.txt");
        let saved = source
            .save_attachment(&mail, 0, &root.join("downloads"))
            .unwrap();
        assert_eq!(saved.file_name().unwrap(), "notes.txt");
        assert_eq!(fs::read_to_string(saved).unwrap().trim(), "hello");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_unified_account_messages_separate() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termail-unified-{nonce}"));
        let personal = root.join("personal");
        let work = root.join("work");
        for maildir in [&personal, &work] {
            for child in ["cur", "new", "tmp"] {
                fs::create_dir_all(maildir.join(child)).unwrap();
            }
            fs::write(
                maildir.join("new/message"),
                b"From: ada@example.com\r\nSubject: Hello\r\nMessage-ID: <same@example.com>\r\n\r\nhi",
            )
            .unwrap();
        }

        let report =
            MailboxSource::Unified(vec![("personal".into(), personal), ("work".into(), work)])
                .load()
                .unwrap();

        assert_eq!(report.mails.len(), 2);
        assert_eq!(report.mails[0].id, "personal:same@example.com");
        assert_eq!(report.mails[1].id, "work:same@example.com");
        fs::remove_dir_all(root).unwrap();
    }
}
