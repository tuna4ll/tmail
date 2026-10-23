use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    remote::{Auth, RemoteConfig},
    storage,
};

type Session = imap::Session<native_tls::TlsStream<TcpStream>>;

/// Local folders that are never mirrored to the server.
const LOCAL_ONLY: [&str; 1] = [".Outbox"];
/// Messages fetched per round trip while downloading.
const FETCH_CHUNK: usize = 25;
/// Local deletions above this count are treated as a reset cache, not user intent.
const MASS_DELETE_LIMIT: usize = 20;

pub fn connect(remote: &RemoteConfig) -> Result<Session, String> {
    let secret = zeroize::Zeroizing::new(remote.password()?);
    let tls = native_tls::TlsConnector::new()
        .map_err(crate::log::fail("Unable to configure IMAP TLS."))?;
    connect_with_tls(remote, &secret, &tls)
}

fn connect_with_tls(
    remote: &RemoteConfig,
    secret: &str,
    tls: &native_tls::TlsConnector,
) -> Result<Session, String> {
    remote.validate()?;
    let addresses = (remote.imap_host.as_str(), remote.imap_port)
        .to_socket_addrs()
        .map_err(crate::log::fail("Unable to resolve IMAP host."))?;
    let mut stream = None;
    for address in addresses {
        if let Ok(socket) = TcpStream::connect_timeout(&address, remote.timeout()) {
            stream = Some(socket);
            break;
        }
    }
    let stream = stream.ok_or("Unable to connect to IMAP server.")?;
    stream
        .set_read_timeout(Some(remote.timeout()))
        .map_err(crate::log::fail("Unable to set IMAP timeout."))?;
    stream
        .set_write_timeout(Some(remote.timeout()))
        .map_err(crate::log::fail("Unable to set IMAP timeout."))?;
    let client = if remote.imap_security.starttls(remote.imap_port) {
        let mut client = imap::Client::new(stream);
        client
            .read_greeting()
            .map_err(crate::log::fail("Invalid IMAP greeting."))?;
        client
            .secure(&remote.imap_host, tls)
            .map_err(crate::log::fail(
                "IMAP STARTTLS verification or handshake failed.",
            ))?
    } else {
        let stream = tls
            .connect(&remote.imap_host, stream)
            .map_err(crate::log::fail(
                "IMAP TLS verification or handshake failed.",
            ))?;
        let mut client = imap::Client::new(stream);
        client
            .read_greeting()
            .map_err(crate::log::fail("Invalid IMAP greeting."))?;
        client
    };
    match remote.auth {
        Auth::Password => client
            .login(&remote.username, secret)
            .map_err(|(error, _)| {
                sign_in_failure(
                    "IMAP login failed. Check the username and password.",
                    &error,
                    secret,
                )
            }),
        Auth::Xoauth2 => client
            .authenticate(
                "XOAUTH2",
                &OAuth2 {
                    user: &remote.username,
                    token: secret,
                },
            )
            .map_err(|(error, _)| {
                sign_in_failure(
                    "IMAP OAuth2 sign-in failed. Refresh the access token.",
                    &error,
                    secret,
                )
            }),
    }
}

/// Logs a rejected sign-in without the secret, which some servers echo back.
fn sign_in_failure(message: &str, error: &imap::Error, secret: &str) -> String {
    let detail = error.to_string();
    let detail = if secret.is_empty() {
        detail
    } else {
        detail.replace(secret, "[redacted]")
    };
    crate::log::write("error", format_args!("{message} ({detail})"));
    message.into()
}

struct OAuth2<'a> {
    user: &'a str,
    token: &'a str,
}

impl imap::Authenticator for OAuth2<'_> {
    type Response = String;

    fn process(&self, _: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Junk,
}

impl Role {
    fn local(self) -> &'static str {
        match self {
            Self::Inbox => "",
            Self::Sent => ".Sent",
            Self::Drafts => ".Drafts",
            Self::Trash => ".Trash",
            Self::Archive => ".Archive",
            Self::Junk => ".Junk",
        }
    }

    fn attribute(self) -> &'static str {
        match self {
            Self::Inbox => "\\inbox",
            Self::Sent => "\\sent",
            Self::Drafts => "\\drafts",
            Self::Trash => "\\trash",
            Self::Archive => "\\archive",
            Self::Junk => "\\junk",
        }
    }

    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Inbox => &["inbox"],
            Self::Sent => &[
                "sent",
                "sent items",
                "sent messages",
                "sent mail",
                "gönderilmiş öğeler",
                "gönderilenler",
                "gönderilmiş",
            ],
            Self::Drafts => &["drafts", "draft", "taslaklar"],
            Self::Trash => &[
                "trash",
                "deleted items",
                "deleted messages",
                "bin",
                "çöp kutusu",
                "çöp",
                "silinmiş öğeler",
            ],
            Self::Archive => &["archive", "archives", "arşiv"],
            Self::Junk => &["junk", "spam", "junk e-mail", "junk email", "istenmeyen"],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub remote: String,
    pub local: String,
    pub role: Option<Role>,
}

/// Lists the account's folders, assigns well-known roles and the Maildir++ folder each
/// one mirrors. Missing Trash and Archive folders are created when `create` is set.
fn discover<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    create: bool,
) -> Result<Vec<Folder>, String> {
    let names = session
        .list(Some(""), Some("*"))
        .map_err(crate::log::fail("Unable to list IMAP folders."))?;
    let mut listed = Vec::new();
    let mut delimiter = None;
    for name in names.iter() {
        let attributes: Vec<String> = name
            .attributes()
            .iter()
            .map(|attribute| match attribute {
                imap::types::NameAttribute::NoSelect => "\\noselect".into(),
                imap::types::NameAttribute::Custom(value) => value.to_lowercase(),
                _ => String::new(),
            })
            .collect();
        if delimiter.is_none() {
            delimiter = name.delimiter().map(str::to_owned);
        }
        if attributes.iter().any(|attribute| {
            matches!(
                attribute.as_str(),
                "\\noselect" | "\\nonexistent" | "\\all" | "\\flagged" | "\\important"
            )
        }) {
            continue;
        }
        listed.push((
            name.name().to_owned(),
            name.delimiter().map(str::to_owned),
            attributes,
        ));
    }
    let mut roles: HashMap<String, Role> = HashMap::new();
    let inbox = listed
        .iter()
        .find(|(name, ..)| name.eq_ignore_ascii_case(&remote.inbox))
        .map(|(name, ..)| name.clone())
        .ok_or_else(|| format!("Unable to open IMAP folder {}.", remote.inbox))?;
    roles.insert(inbox.clone(), Role::Inbox);
    if !remote.sent.is_empty()
        && let Some((name, ..)) = listed.iter().find(|(name, ..)| *name == remote.sent)
    {
        roles.insert(name.clone(), Role::Sent);
    }
    for role in [
        Role::Sent,
        Role::Drafts,
        Role::Trash,
        Role::Archive,
        Role::Junk,
    ] {
        if roles.values().any(|assigned| *assigned == role) {
            continue;
        }
        let by_attribute = listed
            .iter()
            .find(|(name, _, attributes)| {
                !roles.contains_key(name)
                    && attributes.iter().any(|value| value == role.attribute())
            })
            .or_else(|| {
                listed.iter().find(|(name, separator, _)| {
                    !roles.contains_key(name)
                        && role.names().contains(
                            &last_component(name, separator.as_deref())
                                .to_lowercase()
                                .as_str(),
                        )
                })
            });
        if let Some((name, ..)) = by_attribute {
            roles.insert(name.clone(), role);
        }
    }
    if create {
        let prefix = match &delimiter {
            Some(separator)
                if listed
                    .iter()
                    .any(|(name, ..)| name.starts_with(&format!("{inbox}{separator}"))) =>
            {
                format!("{inbox}{separator}")
            }
            _ => String::new(),
        };
        for (role, name) in [(Role::Trash, "Trash"), (Role::Archive, "Archive")] {
            if roles.values().any(|assigned| *assigned == role) {
                continue;
            }
            let name = format!("{prefix}{name}");
            if session.create(&name).is_ok() {
                let _ = session.subscribe(&name);
                roles.insert(name.clone(), role);
                listed.push((name, delimiter.clone(), Vec::new()));
            }
        }
    }
    let mut folders = Vec::new();
    let mut used: HashSet<String> = roles
        .values()
        .map(|role| role.local())
        .chain(LOCAL_ONLY)
        .map(str::to_owned)
        .collect();
    for (name, separator, _) in &listed {
        let role = roles.get(name).copied();
        let local = match role {
            Some(role) => role.local().to_owned(),
            None => {
                let mut components: Vec<String> = match separator {
                    Some(separator) => name.split(separator.as_str()).map(decode_utf7).collect(),
                    None => vec![decode_utf7(name)],
                };
                if components.len() > 1 && components[0].eq_ignore_ascii_case(&inbox) {
                    components.remove(0);
                }
                let mut local = format!(
                    ".{}",
                    components
                        .iter()
                        .map(|component| {
                            crate::mailbox::safe_file_name(component).replace('.', "_")
                        })
                        .collect::<Vec<_>>()
                        .join(".")
                );
                while !used.insert(local.clone()) {
                    local.push('_');
                }
                local
            }
        };
        folders.push(Folder {
            remote: name.clone(),
            local,
            role,
        });
    }
    folders.sort_by_key(|folder| folder.role != Some(Role::Inbox));
    Ok(folders)
}

fn last_component(name: &str, separator: Option<&str>) -> String {
    let component = separator
        .and_then(|separator| name.rsplit(separator).next())
        .unwrap_or(name);
    decode_utf7(component)
}

/// Decodes an RFC 3501 modified UTF-7 mailbox name.
pub fn decode_utf7(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('-') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let encoded = &rest[start + 1..start + end];
        if encoded.is_empty() {
            output.push('&');
        } else {
            let mut bits = 0u32;
            let mut count = 0;
            let mut units = Vec::new();
            for byte in encoded.bytes() {
                let value = match byte {
                    b'A'..=b'Z' => byte - b'A',
                    b'a'..=b'z' => byte - b'a' + 26,
                    b'0'..=b'9' => byte - b'0' + 52,
                    b'+' => 62,
                    b',' => 63,
                    _ => return value.to_owned(),
                };
                bits = (bits << 6) | u32::from(value);
                count += 6;
                if count >= 16 {
                    count -= 16;
                    units.push((bits >> count) as u16);
                    bits &= (1 << count) - 1;
                }
            }
            output.push_str(&String::from_utf16_lossy(&units));
        }
        rest = &rest[start + end + 1..];
    }
    output.push_str(rest);
    output
}

#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    server: String,
    folders: BTreeMap<String, FolderState>,
}

#[derive(Default, Serialize, Deserialize)]
struct FolderState {
    validity: u32,
    messages: BTreeMap<u32, Entry>,
}

/// What both sides agreed on after the last sync. A zero `size` marks an entry whose
/// local file was never confirmed, so its absence is not treated as a deletion.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(from = "StoredEntry")]
struct Entry {
    flags: String,
    id: Option<String>,
    size: u64,
    modified: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredEntry {
    Legacy(String),
    Current {
        #[serde(default)]
        flags: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        size: u64,
        #[serde(default)]
        modified: u64,
    },
}

impl From<StoredEntry> for Entry {
    fn from(value: StoredEntry) -> Self {
        match value {
            StoredEntry::Legacy(file_name) => Self {
                flags: split_flags(&file_name).1.into(),
                ..Self::default()
            },
            StoredEntry::Current {
                flags,
                id,
                size,
                modified,
            } => Self {
                flags,
                id,
                size,
                modified,
            },
        }
    }
}

struct Lock(fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub fn initialize(root: &Path) -> Result<(), String> {
    for child in ["cur", "new", "tmp", ".Sent/cur", ".Sent/new", ".Sent/tmp"] {
        fs::create_dir_all(root.join(child))
            .map_err(|error| format!("Unable to create local mailbox: {error}"))?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct Synced {
    pub summary: String,
    pub changed: bool,
    pub downloaded: usize,
}

pub fn sync(remote: &RemoteConfig, root: &Path) -> Result<Synced, String> {
    let mut session = connect(remote)?;
    let result = sync_session(&mut session, remote, root);
    let _ = session.logout();
    result
}

/// Splits a Maildir filename into its unique part and its info flags.
fn split_flags(file_name: &str) -> (&str, &str) {
    let (base, flags, _) = storage::maildir_info(file_name);
    (base, flags)
}

fn separator() -> &'static str {
    storage::MAILDIR_SEPARATOR
}

fn base_name(server: &str, folder: &str, validity: u32, uid: u32) -> String {
    let hash = format!("{:x}", Sha256::digest(format!("{server}:{folder}")));
    format!("termail-imap-{}-{validity}-{uid}", &hash[..16])
}

fn normalize_flags(flags: &str) -> String {
    let mut flags: Vec<char> = flags
        .chars()
        .filter(|flag| "DFRS".contains(*flag))
        .collect();
    flags.sort_unstable();
    flags.dedup();
    flags.into_iter().collect()
}

fn remote_flags(flags: &[imap::types::Flag<'_>]) -> String {
    use imap::types::Flag;
    let mut value = String::new();
    for (flag, letter) in [
        (Flag::Draft, 'D'),
        (Flag::Flagged, 'F'),
        (Flag::Answered, 'R'),
        (Flag::Seen, 'S'),
    ] {
        if flags.contains(&flag) {
            value.push(letter);
        }
    }
    value
}

fn imap_flag(letter: char) -> &'static str {
    match letter {
        'D' => "\\Draft",
        'F' => "\\Flagged",
        'R' => "\\Answered",
        _ => "\\Seen",
    }
}

fn imap_flags(flags: &str) -> Vec<imap::types::Flag<'static>> {
    use imap::types::Flag;
    flags
        .chars()
        .map(|letter| match letter {
            'D' => Flag::Draft,
            'F' => Flag::Flagged,
            'R' => Flag::Answered,
            _ => Flag::Seen,
        })
        .collect()
}

fn local_folder(root: &Path, path: &Path) -> Option<String> {
    let folder = path.parent()?.parent()?;
    if folder == root {
        return Some(String::new());
    }
    folder
        .strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().into_owned())
}

fn file_stamp(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|time| time.as_nanos() as u64)
                .unwrap_or_default();
            (metadata.len(), modified)
        })
        .unwrap_or_default()
}

/// Finds every message Termail downloaded, wherever it lives in the Maildir now.
fn scan_local(root: &Path) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let mut folders = vec![root.to_path_buf()];
    if let Ok(entries) = fs::read_dir(root) {
        folders.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        }));
    }
    for folder in folders {
        for child in ["cur", "new"] {
            let Ok(entries) = fs::read_dir(folder.join(child)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("termail-imap-") {
                    index.insert(split_flags(&name).0.to_owned(), entry.path());
                }
            }
        }
    }
    index
}

fn set_local_flags(path: &Path, flags: &str) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("Invalid Maildir filename."))?;
    let base = split_flags(name).0;
    let directory = path.parent().unwrap();
    let directory = if flags.contains('S') && directory.ends_with("new") {
        directory.parent().unwrap().join("cur")
    } else {
        directory.to_path_buf()
    };
    let after = directory.join(format!("{base}{}{flags}", separator()));
    if after != path {
        storage::rename(path, &after)?;
    }
    Ok(after)
}

fn message_id(contents: &[u8]) -> Option<String> {
    mail_parser::MessageParser::default()
        .parse_headers(contents)
        .and_then(|parsed| parsed.message_id().map(str::to_owned))
}

fn searchable_id(id: &str) -> bool {
    !id.is_empty() && !id.chars().any(|c| c.is_control() || "\"\\".contains(c))
}

struct Capabilities {
    moves: bool,
    uidplus: bool,
}

#[derive(Default)]
struct Report {
    downloaded: usize,
    remaining: usize,
    uploaded: usize,
    changed: usize,
    removed: usize,
    skipped_deletes: bool,
}

fn sync_session<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    root: &Path,
) -> Result<Synced, String> {
    initialize(root)?;
    let directory = root.join("termail-sync");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let file = match storage::create(&directory.join("lock")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.join("lock"))
            .map_err(|error| error.to_string())?,
        Err(error) => return Err(error.to_string()),
    };
    file.try_lock()
        .map_err(crate::log::fail("This account is already syncing."))?;
    let _lock = Lock(file);
    let state_path = directory.join("state.json");
    let mut state: SyncState = match fs::read(&state_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(crate::log::fail(
            "Sync state is unreadable. Existing messages were left unchanged.",
        ))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SyncState::default(),
        Err(error) => return Err(error.to_string()),
    };
    let server = format!(
        "{}:{}:{}",
        remote.imap_host, remote.imap_port, remote.username
    );
    if !state.server.is_empty() && state.server != server {
        return Err("This local mailbox belongs to a different IMAP account.".into());
    }
    state.server = server;
    let capabilities = session
        .capabilities()
        .map(|capabilities| Capabilities {
            moves: capabilities.has_str("MOVE"),
            uidplus: capabilities.has_str("UIDPLUS"),
        })
        .map_err(crate::log::fail("Unable to read IMAP capabilities."))?;
    let folders = discover(session, remote, true)?;
    let mut report = Report::default();

    // Push and pull changes on known messages first, so moves land before any folder
    // looks for new messages.
    for folder in &folders {
        reconcile(
            session,
            &mut state,
            folder,
            &folders,
            root,
            &capabilities,
            &mut report,
        )?;
        save_state(&state_path, &state)?;
    }
    for folder in &folders {
        fetch_new(
            session,
            &mut state,
            &state_path,
            folder,
            root,
            remote.batch_size,
            &mut report,
        )?;
        save_state(&state_path, &state)?;
    }
    let mut summary = format!("Synced · {} new messages", report.downloaded);
    if report.changed + report.removed + report.uploaded > 0 {
        summary.push_str(&format!(
            " · {} updates",
            report.changed + report.removed + report.uploaded
        ));
    }
    summary.push_str(&format!(" · {} older messages remaining", report.remaining));
    if report.skipped_deletes {
        summary.push_str(" · many local messages were missing, so they will be downloaded again");
    }
    Ok(Synced {
        summary,
        changed: report.downloaded + report.changed + report.removed + report.uploaded > 0,
        downloaded: report.downloaded,
    })
}

fn reconcile<T: Read + Write>(
    session: &mut imap::Session<T>,
    state: &mut SyncState,
    folder: &Folder,
    folders: &[Folder],
    root: &Path,
    capabilities: &Capabilities,
    report: &mut Report,
) -> Result<(), String> {
    let mailbox = session
        .select(&folder.remote)
        .map_err(crate::log::fail(format!(
            "Unable to open IMAP folder {}.",
            folder.remote
        )))?;
    let validity = mailbox
        .uid_validity
        .ok_or("IMAP server did not return UIDVALIDITY.")?;
    let index = scan_local(root);
    let server = state.server.clone();
    let entry = state.folders.entry(folder.remote.clone()).or_default();
    if entry.validity != validity {
        if entry.validity != 0 {
            for uid in entry.messages.keys() {
                let base = base_name(&server, &folder.remote, entry.validity, *uid);
                if let Some(path) = index.get(&base)
                    && local_folder(root, path).as_deref() == Some(folder.local.as_str())
                {
                    let _ = fs::remove_file(path);
                }
            }
        }
        entry.messages.clear();
        entry.validity = validity;
    }
    if entry.messages.is_empty() {
        return Ok(());
    }
    let mut current = HashMap::new();
    if mailbox.exists > 0 {
        let fetched = session
            .uid_fetch("1:*", "(UID FLAGS)")
            .map_err(crate::log::fail("Unable to read IMAP message flags."))?;
        for message in fetched.iter() {
            if let Some(uid) = message.uid {
                current.insert(uid, remote_flags(message.flags()));
            }
        }
    }
    let missing = entry
        .messages
        .iter()
        .filter(|(uid, value)| {
            value.size > 0
                && current.contains_key(uid)
                && !index.contains_key(&base_name(&server, &folder.remote, validity, **uid))
        })
        .count();
    let mass_missing = missing > MASS_DELETE_LIMIT && missing * 2 > entry.messages.len();
    report.skipped_deletes |= mass_missing;
    let trash = folders
        .iter()
        .find(|candidate| candidate.role == Some(Role::Trash))
        .map(|candidate| candidate.remote.clone());
    let mut expunge = Vec::new();
    let mut forget = Vec::new();
    let mut updates = Vec::new();
    for (uid, synced) in &entry.messages {
        let base = base_name(&server, &folder.remote, validity, *uid);
        let local = index.get(&base);
        let Some(remote_flags) = current.get(uid) else {
            // Gone on the server: drop the local copy if it still sits in this folder.
            if let Some(path) = local
                && local_folder(root, path).as_deref() == Some(folder.local.as_str())
            {
                let _ = fs::remove_file(path);
                report.removed += 1;
            }
            forget.push(*uid);
            continue;
        };
        let Some(path) = local else {
            if synced.size == 0 || mass_missing {
                forget.push(*uid);
                continue;
            }
            // Deleted locally: keep it recoverable in Trash unless it is already there.
            match &trash {
                Some(trash) if !matches!(folder.role, Some(Role::Trash) | Some(Role::Drafts)) => {
                    move_message(session, *uid, trash, capabilities, &mut expunge)?;
                }
                _ => {
                    session
                        .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                        .map_err(crate::log::fail("Unable to delete an IMAP message."))?;
                    expunge.push(*uid);
                }
            }
            report.removed += 1;
            forget.push(*uid);
            continue;
        };
        let location = local_folder(root, path).unwrap_or_default();
        if location != folder.local {
            if let Some(target) = folders.iter().find(|candidate| candidate.local == location) {
                move_message(session, *uid, &target.remote, capabilities, &mut expunge)?;
                report.changed += 1;
                forget.push(*uid);
            }
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let local_flags = normalize_flags(split_flags(&name).1);
        let (size, modified) = file_stamp(path);
        if synced.size > 0 && (size != synced.size || modified != synced.modified) {
            // The local copy was rewritten (an edited draft): replace the server copy.
            let contents = fs::read(path).map_err(|error| error.to_string())?;
            session
                .append_with_flags(&folder.remote, &contents, &imap_flags(&local_flags))
                .map_err(crate::log::fail("Unable to upload an edited message."))?;
            session
                .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                .map_err(crate::log::fail("Unable to replace an edited message."))?;
            expunge.push(*uid);
            let _ = fs::remove_file(path);
            report.uploaded += 1;
            forget.push(*uid);
            continue;
        }
        let mut merged = String::new();
        let mut add = Vec::new();
        let mut remove = Vec::new();
        for flag in ['D', 'F', 'R', 'S'] {
            let was = synced.flags.contains(flag);
            let here = local_flags.contains(flag);
            let there = remote_flags.contains(flag);
            let value = if synced.size == 0 {
                // First pass over a legacy entry: the server state is the baseline.
                there || here
            } else if here != was {
                here
            } else {
                there
            };
            if value {
                merged.push(flag);
            }
            if value && !there {
                add.push(imap_flag(flag));
            } else if !value && there {
                remove.push(imap_flag(flag));
            }
        }
        for (operation, flags) in [("+FLAGS.SILENT", &add), ("-FLAGS.SILENT", &remove)] {
            if !flags.is_empty() {
                session
                    .uid_store(
                        uid.to_string(),
                        format!("{operation} ({})", flags.join(" ")),
                    )
                    .map_err(crate::log::fail("Unable to update IMAP message flags."))?;
                report.changed += 1;
            }
        }
        if merged != local_flags {
            report.changed += 1;
        }
        let path = set_local_flags(path, &merged).map_err(|error| error.to_string())?;
        let (size, modified) = file_stamp(&path);
        let id = synced.id.clone().or_else(|| {
            fs::read(&path)
                .ok()
                .and_then(|contents| message_id(&contents))
        });
        updates.push((
            *uid,
            Entry {
                flags: merged,
                id,
                size,
                modified,
            },
        ));
    }
    for uid in forget {
        entry.messages.remove(&uid);
    }
    for (uid, value) in updates {
        entry.messages.insert(uid, value);
    }
    if !expunge.is_empty() {
        expunge_messages(session, &expunge, capabilities)?;
    }
    Ok(())
}

fn move_message<T: Read + Write>(
    session: &mut imap::Session<T>,
    uid: u32,
    target: &str,
    capabilities: &Capabilities,
    expunge: &mut Vec<u32>,
) -> Result<(), String> {
    if capabilities.moves {
        session
            .uid_mv(uid.to_string(), target)
            .map_err(crate::log::fail(format!(
                "Unable to move a message to {target}."
            )))
    } else {
        session
            .uid_copy(uid.to_string(), target)
            .map_err(crate::log::fail(format!(
                "Unable to copy a message to {target}."
            )))?;
        session
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .map_err(crate::log::fail("Unable to remove a moved message."))?;
        expunge.push(uid);
        Ok(())
    }
}

fn expunge_messages<T: Read + Write>(
    session: &mut imap::Session<T>,
    uids: &[u32],
    capabilities: &Capabilities,
) -> Result<(), String> {
    let result = if capabilities.uidplus {
        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        session.uid_expunge(set).map(|_| ())
    } else {
        session.expunge().map(|_| ())
    };
    result.map_err(crate::log::fail("Unable to expunge deleted IMAP messages."))
}

fn fetch_new<T: Read + Write>(
    session: &mut imap::Session<T>,
    state: &mut SyncState,
    state_path: &Path,
    folder: &Folder,
    root: &Path,
    batch_size: usize,
    report: &mut Report,
) -> Result<(), String> {
    if LOCAL_ONLY.contains(&folder.local.as_str()) {
        return Ok(());
    }
    let local_root = if folder.local.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&folder.local)
    };
    for child in ["cur", "new", "tmp"] {
        fs::create_dir_all(local_root.join(child)).map_err(|error| error.to_string())?;
    }
    let mailbox = session
        .select(&folder.remote)
        .map_err(crate::log::fail(format!(
            "Unable to open IMAP folder {}.",
            folder.remote
        )))?;
    let validity = mailbox
        .uid_validity
        .ok_or("IMAP server did not return UIDVALIDITY.")?;
    let server = state.server.clone();
    let known: HashSet<String> = state
        .folders
        .iter()
        .flat_map(|(name, entry)| {
            let server = &server;
            entry
                .messages
                .keys()
                .map(move |uid| base_name(server, name, entry.validity, *uid))
        })
        .collect();
    let mut entry = state.folders.remove(&folder.remote).unwrap_or_default();
    entry.validity = validity;
    let mut uids = session
        .uid_search("ALL")
        .map_err(crate::log::fail("Unable to list IMAP messages."))?;

    // Local messages the server has not seen: adopt the server copy when one exists,
    // otherwise upload them.
    let mut uploaded = false;
    for child in ["cur", "new"] {
        let Ok(files) = fs::read_dir(local_root.join(child)) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let name = file.file_name().to_string_lossy().into_owned();
            if !path.is_file() || known.contains(split_flags(&name).0) {
                continue;
            }
            let Ok(contents) = fs::read(&path) else {
                continue;
            };
            let flags = normalize_flags(split_flags(&name).1);
            let id = message_id(&contents);
            let existing = match &id {
                Some(id) if searchable_id(id) => session
                    .uid_search(format!("HEADER Message-ID \"<{id}>\""))
                    .map_err(crate::log::fail("Unable to search IMAP messages."))?
                    .into_iter()
                    .filter(|uid| !entry.messages.contains_key(uid))
                    .max(),
                _ => None,
            };
            if let Some(uid) = existing {
                let fetched = session
                    .uid_fetch(uid.to_string(), "(UID FLAGS)")
                    .map_err(crate::log::fail("Unable to read IMAP message flags."))?;
                let there = fetched
                    .iter()
                    .find(|message| message.uid == Some(uid))
                    .map(|message| remote_flags(message.flags()))
                    .unwrap_or_default();
                let base = base_name(&server, &folder.remote, validity, uid);
                let destination =
                    local_root
                        .join("cur")
                        .join(format!("{base}{}{}", separator(), flags));
                storage::rename(&path, &destination).map_err(|error| error.to_string())?;
                let (size, modified) = file_stamp(&destination);
                entry.messages.insert(
                    uid,
                    Entry {
                        flags: there,
                        id,
                        size,
                        modified,
                    },
                );
                uids.remove(&uid);
            } else if name.starts_with("termail-imap-") {
                // It came from the server and is no longer where sync left it; the
                // server copy wherever it lives now is downloaded instead.
                fs::remove_file(&path).map_err(|error| error.to_string())?;
            } else {
                session
                    .append_with_flags(&folder.remote, &contents, &imap_flags(&flags))
                    .map_err(crate::log::fail(format!(
                        "Unable to upload a message to {}.",
                        folder.remote
                    )))?;
                fs::remove_file(&path).map_err(|error| error.to_string())?;
                report.uploaded += 1;
                uploaded = true;
            }
        }
    }
    if uploaded {
        uids = session
            .uid_search("ALL")
            .map_err(crate::log::fail("Unable to list IMAP messages."))?;
    }
    let mut pending: Vec<u32> = uids
        .into_iter()
        .filter(|uid| !entry.messages.contains_key(uid))
        .collect();
    pending.sort_unstable_by(|a, b| b.cmp(a));
    report.remaining += pending.len().saturating_sub(batch_size);
    pending.truncate(batch_size);
    for chunk in pending.chunks(FETCH_CHUNK) {
        let set = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetched = session
            .uid_fetch(set, "(UID FLAGS BODY.PEEK[])")
            .map_err(crate::log::fail("Unable to download IMAP message."))?;
        for message in fetched.iter() {
            let Some(uid) = message.uid.filter(|uid| chunk.contains(uid)) else {
                continue;
            };
            let Some(body) = message.body() else {
                continue;
            };
            let flags = remote_flags(message.flags());
            let base = base_name(&server, &folder.remote, validity, uid);
            let destination = local_root
                .join("cur")
                .join(format!("{base}{}{flags}", separator()));
            if !destination.exists() {
                let temporary = local_root
                    .join("tmp")
                    .join(storage::unique_name("download"));
                storage::write_new(&temporary, body).map_err(|error| error.to_string())?;
                storage::rename(&temporary, &destination).map_err(|error| error.to_string())?;
                report.downloaded += 1;
            }
            let (size, modified) = file_stamp(&destination);
            entry.messages.insert(
                uid,
                Entry {
                    flags,
                    id: message_id(body),
                    size,
                    modified,
                },
            );
        }
        state
            .folders
            .insert(folder.remote.clone(), std::mem::take(&mut entry));
        save_state(state_path, state)?;
        entry = state.folders.remove(&folder.remote).unwrap_or_default();
    }
    state.folders.insert(folder.remote.clone(), entry);
    Ok(())
}

fn save_state(path: &Path, state: &SyncState) -> Result<(), String> {
    let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
    let temporary = path.with_file_name(storage::unique_name("sync-state"));
    storage::write_new(&temporary, &bytes).map_err(|error| error.to_string())?;
    storage::rename(&temporary, path).map_err(|error| error.to_string())
}

pub struct CopyError {
    pub message: String,
    pub uncertain: bool,
}

pub fn copy_sent(
    remote: &RemoteConfig,
    contents: &[u8],
    allow_append: bool,
) -> Result<(), CopyError> {
    let mut session = connect(remote).map_err(|message| CopyError {
        message,
        uncertain: false,
    })?;
    let result = copy_sent_session(&mut session, remote, contents, allow_append);
    let _ = session.logout();
    result
}

fn sent_folder<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
) -> Result<String, String> {
    discover(session, remote, false)?
        .into_iter()
        .find(|folder| folder.role == Some(Role::Sent))
        .map(|folder| folder.remote)
        .ok_or_else(|| "The account has no Sent folder.".into())
}

fn copy_sent_session<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    contents: &[u8],
    allow_append: bool,
) -> Result<(), CopyError> {
    let failure = |message: &str| CopyError {
        message: message.into(),
        uncertain: false,
    };
    let parsed = mail_parser::MessageParser::default()
        .parse(contents)
        .ok_or_else(|| failure("Unable to parse Sent copy."))?;
    let id = parsed
        .message_id()
        .ok_or_else(|| failure("Sent copy has no Message-ID."))?;
    if !searchable_id(id) {
        return Err(failure("Invalid Message-ID in Sent copy."));
    }
    let sent = sent_folder(session, remote).map_err(|message| failure(&message))?;
    session
        .examine(&sent)
        .map_err(|_| failure("Unable to open the remote Sent folder."))?;
    let found = session
        .uid_search(format!("HEADER Message-ID \"<{id}>\""))
        .map_err(|_| failure("Unable to check the remote Sent folder."))?;
    for uid in found {
        let headers = session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[HEADER])")
            .map_err(|_| failure("Unable to verify the remote Sent copy."))?;
        for header in headers.iter().filter_map(|message| message.header()) {
            if mail_parser::MessageParser::default()
                .parse(header)
                .is_some_and(|message| message.message_id() == Some(id))
            {
                return Ok(());
            }
        }
    }
    if !allow_append {
        return Err(CopyError { message: "Sent copy is unconfirmed. SMTP delivery will not be repeated; check the remote Sent folder.".into(), uncertain: true });
    }
    session
        .append_with_flags(&sent, contents, &[imap::types::Flag::Seen])
        .map_err(|_| CopyError {
            message: "Unable to confirm the remote Sent copy. SMTP delivery will not be repeated."
                .into(),
            uncertain: true,
        })
}

/// Checks that the account's Inbox can be opened and its folders listed.
pub fn verify_folders(session: &mut Session, remote: &RemoteConfig) -> Result<(), String> {
    discover(session, remote, false)?;
    session
        .examine(&remote.inbox)
        .map(|_| ())
        .map_err(crate::log::fail(
            "Unable to open the Inbox. Check the folder name.",
        ))
}

/// Blocks until the server reports a change in the Inbox, the timeout passes, or
/// `stop` is set. Returns `Ok(true)` when the mailbox changed.
pub fn wait_for_change(
    remote: &RemoteConfig,
    timeout: std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<bool, String> {
    let mut session = connect(remote)?;
    let result = watch_session(&mut session, remote, timeout, stop);
    let _ = session.logout();
    result
}

fn watch_session(
    session: &mut Session,
    remote: &RemoteConfig,
    timeout: std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<bool, String> {
    let supported = session
        .capabilities()
        .map(|capabilities| capabilities.has_str("IDLE"))
        .unwrap_or(false);
    if !supported {
        return Err("IDLE is not supported.".into());
    }
    session
        .select(&remote.inbox)
        .map_err(crate::log::fail("Unable to open the Inbox."))?;
    let started = std::time::Instant::now();
    let mut changed = false;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) && started.elapsed() < timeout {
        let idle = session
            .idle()
            .map_err(crate::log::fail("Unable to start IDLE."))?;
        match idle.wait_with_timeout(std::time::Duration::from_secs(60)) {
            Ok(imap::extensions::idle::WaitOutcome::MailboxChanged) => {
                changed = true;
                break;
            }
            Ok(imap::extensions::idle::WaitOutcome::TimedOut) => {}
            Err(_) => return Err("The IDLE connection was interrupted.".into()),
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_imap::{self, State, mail};
    use crate::remote::tests::Server;

    struct Cache(PathBuf);

    impl Cache {
        fn new() -> Self {
            Self(std::env::temp_dir().join(storage::unique_name("imap-test")))
        }

        fn files(&self, folder: &str) -> Vec<String> {
            let mut names = Vec::new();
            for child in ["cur", "new"] {
                if let Ok(entries) = fs::read_dir(self.0.join(folder).join(child)) {
                    names.extend(
                        entries
                            .flatten()
                            .map(|entry| entry.file_name().to_string_lossy().into_owned()),
                    );
                }
            }
            names.sort();
            names
        }
    }

    impl Drop for Cache {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn start(state: &fake_imap::Shared) -> (RemoteConfig, native_tls::TlsConnector) {
        let server = Server::new();
        let remote = server.remote();
        let tls = server.connector();
        fake_imap::serve(server, state.clone());
        (remote, tls)
    }

    fn run(remote: &RemoteConfig, tls: &native_tls::TlsConnector, cache: &Cache) -> String {
        let mut session = connect_with_tls(remote, "fake-password", tls).unwrap();
        let result = sync_session(&mut session, remote, &cache.0).unwrap();
        session.logout().unwrap();
        result.summary
    }

    fn standard() -> fake_imap::Shared {
        State::new(&[
            ("INBOX", "\\HasChildren"),
            ("INBOX.Sent", "\\Sent"),
            ("INBOX.Trash", "\\Trash"),
            ("INBOX.Drafts", "\\Drafts"),
        ])
    }

    #[test]
    fn sync_downloads_once_with_peek_and_standard_names() {
        let state = standard();
        state.lock().unwrap().deliver(
            "INBOX",
            &mail("one@example.test", "hello"),
            &["\\Seen", "\\Flagged"],
        );
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        assert!(run(&remote, &tls, &cache).contains("1 new"));
        assert!(run(&remote, &tls, &cache).contains("0 new"));
        let files = cache.files("");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with(&format!("{}FS", separator())));
        let commands = state.lock().unwrap().commands.clone();
        assert!(commands.iter().any(|line| line.contains("BODY.PEEK[]")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = cache.0.join("cur").join(&files[0]);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Archive did not exist, so it is created next to the other folders.
        assert!(state.lock().unwrap().folders.contains_key("INBOX.Archive"));
    }

    #[test]
    fn local_changes_reach_the_server_and_remote_changes_come_back() {
        let state = standard();
        let first = state
            .lock()
            .unwrap()
            .deliver("INBOX", &mail("one@example.test", "one"), &[]);
        let second =
            state
                .lock()
                .unwrap()
                .deliver("INBOX", &mail("two@example.test", "two"), &["\\Seen"]);
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        run(&remote, &tls, &cache);

        // Mark the first message read and archive the second locally.
        let files = cache.files("");
        let one = files
            .iter()
            .find(|name| name.contains(&format!("-{first}:")))
            .unwrap();
        let two = files
            .iter()
            .find(|name| name.contains(&format!("-{second}:")))
            .unwrap();
        let source = MailboxPath(cache.0.clone());
        source.flag(one, 'S');
        source.move_to(two, ".Archive");
        // Meanwhile another client stars the first message.
        state
            .lock()
            .unwrap()
            .folders
            .get_mut("INBOX")
            .unwrap()
            .messages[0]
            .flags
            .push("\\Flagged".into());
        run(&remote, &tls, &cache);
        {
            let state = state.lock().unwrap();
            let flags = &state.message("INBOX", first).unwrap().flags;
            assert!(flags.contains(&"\\Seen".into()));
            assert_eq!(state.folders["INBOX"].messages.len(), 1);
            assert_eq!(state.folders["INBOX.Archive"].messages.len(), 1);
        }
        let inbox = cache.files("");
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].ends_with("FS"), "{inbox:?}");
        let archive = cache.files(".Archive");
        assert_eq!(archive.len(), 1);
        assert!(archive[0].starts_with("termail-imap-"));
        // The archived copy was adopted, not downloaded a second time.
        run(&remote, &tls, &cache);
        assert_eq!(cache.files(".Archive").len(), 1);
    }

    #[test]
    fn deletions_flow_both_ways_and_reset_caches_are_safe() {
        let state = standard();
        for index in 0..3 {
            state.lock().unwrap().deliver(
                "INBOX",
                &mail(&format!("{index}@example.test"), "x"),
                &[],
            );
        }
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        run(&remote, &tls, &cache);
        // Server removes uid 1; the user deletes uid 2 locally.
        state
            .lock()
            .unwrap()
            .folders
            .get_mut("INBOX")
            .unwrap()
            .messages
            .retain(|message| message.uid != 1);
        let local = cache.files("");
        let two = local.iter().find(|name| name.contains("-2:")).unwrap();
        fs::remove_file(cache.0.join("cur").join(two)).unwrap();
        run(&remote, &tls, &cache);
        let state_now = state.lock().unwrap();
        assert_eq!(state_now.folders["INBOX"].messages.len(), 1);
        assert_eq!(state_now.folders["INBOX.Trash"].messages.len(), 1);
        drop(state_now);
        assert_eq!(cache.files("").len(), 1);
    }

    #[test]
    fn local_drafts_and_sent_copies_are_uploaded_once() {
        let state = standard();
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        run(&remote, &tls, &cache);
        let sent = mail("sent@example.test", "sent");
        state
            .lock()
            .unwrap()
            .deliver("INBOX.Sent", &sent, &["\\Seen"]);
        storage::write_new(&cache.0.join(".Sent/cur/123.local:2,S"), &sent).unwrap();
        fs::create_dir_all(cache.0.join(".Drafts/cur")).unwrap();
        storage::write_new(
            &cache.0.join(".Drafts/cur/124.local:2,"),
            &mail("draft@example.test", "draft"),
        )
        .unwrap();
        run(&remote, &tls, &cache);
        run(&remote, &tls, &cache);
        let state = state.lock().unwrap();
        assert_eq!(state.folders["INBOX.Sent"].messages.len(), 1);
        assert_eq!(state.folders["INBOX.Drafts"].messages.len(), 1);
        assert_eq!(cache.files(".Sent").len(), 1);
        assert_eq!(cache.files(".Drafts").len(), 1);
        assert!(cache.files(".Drafts")[0].starts_with("termail-imap-"));
    }

    #[test]
    fn legacy_state_never_deletes_server_mail() {
        let state = standard();
        state
            .lock()
            .unwrap()
            .deliver("INBOX.Sent", &mail("old@example.test", "old"), &["\\Seen"]);
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        initialize(&cache.0).unwrap();
        fs::create_dir_all(cache.0.join("termail-sync")).unwrap();
        let server = format!(
            "{}:{}:{}",
            remote.imap_host, remote.imap_port, remote.username
        );
        fs::write(
            cache.0.join("termail-sync/state.json"),
            format!(r#"{{"server":"{server}","folders":{{"INBOX.Sent":{{"validity":7,"messages":{{"1":"missing!2,S"}}}}}}}}"#),
        )
        .unwrap();
        run(&remote, &tls, &cache);
        assert_eq!(
            state.lock().unwrap().folders["INBOX.Sent"].messages.len(),
            1
        );
        assert_eq!(cache.files(".Sent").len(), 1);
    }

    #[test]
    fn folders_are_discovered_by_attribute_and_name() {
        let state = State::new(&[
            ("INBOX", ""),
            ("[Gmail]", "\\Noselect"),
            ("[Gmail]/All Mail", "\\All"),
            ("[Gmail]/Sent Mail", "\\Sent"),
            ("Deleted Items", ""),
            ("Projekt &AMcA9g-zet", ""),
        ]);
        let (remote, tls) = start(&state);
        let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
        let folders = discover(&mut session, &remote, false).unwrap();
        let find = |name: &str| folders.iter().find(|folder| folder.remote == name).cloned();
        assert!(find("[Gmail]/All Mail").is_none());
        assert_eq!(find("[Gmail]/Sent Mail").unwrap().local, ".Sent");
        assert_eq!(find("Deleted Items").unwrap().role, Some(Role::Trash));
        assert_eq!(find("Projekt &AMcA9g-zet").unwrap().local, ".Projekt Çözet");
        assert_eq!(decode_utf7("Gelen &- Giden"), "Gelen & Giden");
    }

    #[test]
    fn sent_copy_is_appended_once_and_uncertain_copies_are_not_repeated() {
        let state = standard();
        let (remote, tls) = start(&state);
        let message = mail("sent-once@example.test", "sent");
        let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
        assert!(
            copy_sent_session(&mut session, &remote, &message, false)
                .err()
                .unwrap()
                .uncertain
        );
        assert!(copy_sent_session(&mut session, &remote, &message, true).is_ok());
        assert!(copy_sent_session(&mut session, &remote, &message, true).is_ok());
        session.logout().unwrap();
        assert_eq!(
            state.lock().unwrap().folders["INBOX.Sent"].messages.len(),
            1
        );
    }

    #[test]
    fn rejects_untrusted_tls_before_login() {
        let server = Server::new();
        let remote = server.remote();
        let worker = server.spawn(|stream| assert!(stream.is_none()));
        let error = connect_with_tls(
            &remote,
            "fake-password",
            &native_tls::TlsConnector::new().unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.contains("TLS"));
        worker.join().unwrap();
    }

    #[test]
    fn login_rejection_does_not_expose_password() {
        use std::io::BufRead;
        let server = Server::new();
        let remote = server.remote();
        let tls = server.connector();
        let worker = server.spawn(|stream| {
            let mut stream = std::io::BufReader::new(stream.unwrap());
            stream.get_mut().write_all(b"* OK local test\r\n").unwrap();
            let mut line = String::new();
            stream.read_line(&mut line).unwrap();
            let tag = line.split_whitespace().next().unwrap();
            stream
                .get_mut()
                .write_all(format!("{tag} NO fake-password rejected\r\n").as_bytes())
                .unwrap();
        });
        let error = connect_with_tls(&remote, "fake-password", &tls)
            .err()
            .unwrap();
        assert!(error.contains("login failed"));
        assert!(!error.contains("fake-password"));
        worker.join().unwrap();
    }

    /// Mirrors what the UI does to Maildir files.
    struct MailboxPath(PathBuf);

    impl MailboxPath {
        fn path(&self, name: &str) -> PathBuf {
            let cur = self.0.join("cur").join(name);
            if cur.exists() {
                cur
            } else {
                self.0.join("new").join(name)
            }
        }

        fn flag(&self, name: &str, flag: char) {
            let path = self.path(name);
            let (base, flags) = split_flags(name);
            let flags = normalize_flags(&format!("{flags}{flag}"));
            fs::rename(
                &path,
                self.0
                    .join("cur")
                    .join(format!("{base}{}{flags}", separator())),
            )
            .unwrap();
        }

        fn move_to(&self, name: &str, folder: &str) {
            fs::create_dir_all(self.0.join(folder).join("cur")).unwrap();
            fs::rename(self.path(name), self.0.join(folder).join("cur").join(name)).unwrap();
        }
    }
}
