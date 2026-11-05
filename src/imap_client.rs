use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    remote::{Auth, RemoteConfig, RemoteError},
    storage,
};

type Session = imap::Session<native_tls::TlsStream<TcpStream>>;

/// Local folders that are never mirrored to the server.
const LOCAL_ONLY: [&str; 1] = [".Outbox"];
/// Messages fetched per round trip while downloading.
const FETCH_CHUNK: usize = 25;
/// Local deletions above this count are treated as a reset cache, not user intent.
const MASS_DELETE_LIMIT: usize = 20;

pub fn connect(remote: &RemoteConfig) -> Result<Session, RemoteError> {
    let secret = zeroize::Zeroizing::new(remote.password().map_err(RemoteError::auth)?);
    let tls = remote.tls_connector()?;
    connect_with_tls(remote, &secret, &tls)
}

fn connect_with_tls(
    remote: &RemoteConfig,
    secret: &str,
    tls: &native_tls::TlsConnector,
) -> Result<Session, RemoteError> {
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
    let mut stream = stream.ok_or("Unable to connect to IMAP server.")?;
    stream
        .set_read_timeout(Some(remote.timeout()))
        .map_err(crate::log::fail("Unable to set IMAP timeout."))?;
    stream
        .set_write_timeout(Some(remote.timeout()))
        .map_err(crate::log::fail("Unable to set IMAP timeout."))?;
    let starttls = remote.imap_security.starttls(remote.imap_port);
    if starttls {
        start_tls(&mut stream)?;
    }
    let stream = tls.connect(&remote.imap_host, stream).map_err(|error| {
        tls_failure("IMAP", &remote.imap_host, &error.to_string(), || {
            crate::certs::fetch(
                &remote.imap_host,
                remote.imap_port,
                crate::certs::Protocol::Imap,
                starttls,
            )
            .is_ok()
        })
    })?;
    let mut client = imap::Client::new(stream);
    if !starttls {
        client
            .read_greeting()
            .map_err(crate::log::fail("Invalid IMAP greeting."))?;
    }
    let result = match remote.auth {
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
    };
    if result.is_err() {
        remote.forget_cached();
    }
    result.map_err(RemoteError::auth)
}

/// Upgrades a plain connection before any credentials are sent. Reads byte by byte so
/// nothing that belongs to the TLS handshake is buffered away.
fn start_tls(stream: &mut TcpStream) -> Result<(), String> {
    fn line(stream: &mut TcpStream) -> Result<String, String> {
        let mut bytes = Vec::new();
        let mut byte = [0];
        while bytes.len() < 8192 {
            stream
                .read_exact(&mut byte)
                .map_err(crate::log::fail("Invalid IMAP greeting."))?;
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n") {
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        Err("Invalid IMAP greeting.".into())
    }
    let greeting = line(stream)?;
    if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
        return Err("Invalid IMAP greeting.".into());
    }
    stream
        .write_all(b"t0 STARTTLS\r\n")
        .map_err(crate::log::fail("IMAP STARTTLS request failed."))?;
    loop {
        let reply = line(stream)?;
        if let Some(status) = reply.strip_prefix("t0 ") {
            return if status.to_ascii_uppercase().starts_with("OK") {
                Ok(())
            } else {
                Err("The IMAP server refused STARTTLS.".into())
            };
        }
    }
}

/// A TLS failure, telling an untrusted certificate apart from other problems so the
/// user can be offered to trust it. Some TLS libraries, such as Windows' Schannel,
/// describe a certificate they reject in vague words; `retried` makes a handshake
/// without checking the certificate, so when that one succeeds, the certificate was
/// the problem.
pub fn tls_failure(
    protocol: &str,
    host: &str,
    detail: &str,
    retried: impl FnOnce() -> bool,
) -> RemoteError {
    crate::log::write(
        "error",
        format_args!("{protocol} TLS with {host}: {detail}"),
    );
    let lower = detail.to_lowercase();
    if lower.contains("certif")
        || lower.contains("self signed")
        || lower.contains("self-signed")
        || lower.contains("issuer")
        || retried()
    {
        RemoteError::certificate(format!(
            "{host} uses a certificate this computer does not trust."
        ))
    } else {
        format!(
            "{protocol} TLS handshake with {host} failed: {}",
            short(detail)
        )
        .into()
    }
}

/// A server reply cut to one readable line.
pub fn short(detail: &str) -> String {
    let line: String = detail
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(160).collect::<String>())
    } else {
        line
    }
}

/// Explains a rejected sign-in with the server's own reason, without the secret,
/// which some servers echo back.
fn sign_in_failure(message: &str, error: &imap::Error, secret: &str) -> String {
    let detail = error.to_string();
    let detail = if secret.is_empty() {
        detail
    } else {
        detail.replace(secret, "[redacted]")
    };
    crate::log::write("error", format_args!("{message} ({detail})"));
    let reason = short(
        detail
            .trim_start_matches("No Response: ")
            .trim_start_matches("Bad Response: "),
    );
    let hint = sign_in_hint(&detail.to_lowercase());
    if reason.is_empty() {
        format!("{message}{hint}")
    } else {
        format!("{message} The server said: {reason}.{hint}")
    }
}

/// What usually fixes a rejected sign-in, from the words servers use.
pub fn sign_in_hint(reason: &str) -> &'static str {
    if reason.contains("application-specific")
        || reason.contains("app password")
        || reason.contains("web login required")
        || reason.contains("webloginrequired")
    {
        " Create an app password in the account's security settings and use it here."
    } else if reason.contains("basicauthblocked")
        || reason.contains("basic auth")
        || reason.contains("logondenied")
    {
        " This provider wants OAuth2 sign-in: choose it on the Sign-in line."
    } else if reason.contains("too many")
        || reason.contains("rate limit")
        || reason.contains("try again later")
    {
        " Wait a few minutes before trying again."
    } else {
        ""
    }
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
    discover_layout(session, remote, create).map(|(folders, _)| folders)
}

/// Where new folders go: the hierarchy delimiter and the prefix they need.
struct Layout {
    delimiter: Option<String>,
    prefix: String,
}

fn discover_layout<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    create: bool,
) -> Result<(Vec<Folder>, Layout), String> {
    let names = session
        .list(Some(""), Some("*"))
        .map_err(crate::log::fail("Unable to list IMAP folders."))?;
    let mut listed = Vec::new();
    let mut delimiter = None;
    for name in names.iter() {
        let attributes: Vec<String> = name
            .attributes()
            .iter()
            .map(|attribute| {
                use imap_proto::NameAttribute;
                match attribute {
                    NameAttribute::NoSelect => "\\noselect".into(),
                    NameAttribute::All => "\\all".into(),
                    NameAttribute::Archive => "\\archive".into(),
                    NameAttribute::Drafts => "\\drafts".into(),
                    NameAttribute::Flagged => "\\flagged".into(),
                    NameAttribute::Junk => "\\junk".into(),
                    NameAttribute::Sent => "\\sent".into(),
                    NameAttribute::Trash => "\\trash".into(),
                    NameAttribute::Extension(value) => value.to_lowercase(),
                    _ => String::new(),
                }
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
    if create {
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
    Ok((folders, Layout { delimiter, prefix }))
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

/// Encodes a folder name in IMAP's modified UTF-7.
pub fn encode_utf7(value: &str) -> String {
    let mut output = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, output: &mut String| {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
        let encoded = crate::widgets::rich::base64(&bytes)
            .trim_end_matches('=')
            .replace('/', ",");
        output.push('&');
        output.push_str(&encoded);
        output.push('-');
        pending.clear();
    };
    for character in value.chars() {
        if (' '..='~').contains(&character) {
            flush(&mut pending, &mut output);
            if character == '&' {
                output.push_str("&-");
            } else {
                output.push(character);
            }
        } else {
            let mut units = [0; 2];
            pending.extend_from_slice(character.encode_utf16(&mut units));
        }
    }
    flush(&mut pending, &mut output);
    output
}

#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    server: String,
    folders: BTreeMap<String, FolderState>,
    /// Messages that were in the Maildir before the first sync. They stay local until
    /// the user asks to upload them, so pointing Tmail at an existing Maildir never
    /// copies a whole archive to the server by surprise.
    #[serde(default)]
    held: BTreeSet<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct FolderState {
    validity: u32,
    messages: BTreeMap<u32, Entry>,
    /// HIGHESTMODSEQ after the last sync, when the server supports CONDSTORE.
    #[serde(default)]
    modseq: u64,
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
    digest: Option<String>,
    /// Only the headers were downloaded; the body is fetched on request.
    partial: bool,
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
        #[serde(default)]
        digest: Option<String>,
        #[serde(default)]
        partial: bool,
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
                digest,
                partial,
            } => Self {
                flags,
                id,
                size,
                modified,
                digest,
                partial,
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

/// Takes the account's sync lock. `wait` blocks until a running sync finishes.
fn lock_account(root: &Path, wait: bool) -> Result<Lock, String> {
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
    if wait {
        file.lock()
            .map_err(crate::log::fail("Unable to lock the local mailbox."))?;
    } else {
        file.try_lock()
            .map_err(crate::log::fail("This account is already syncing."))?;
    }
    Ok(Lock(file))
}

fn state_path(root: &Path) -> PathBuf {
    root.join("termail-sync").join("state.json")
}

/// Messages the user deleted for good in tmail, by Maildir name, until the next sync
/// has deleted them on the server too.
fn deleted_path(root: &Path) -> PathBuf {
    root.join("termail-sync").join("deleted")
}

/// Records a permanent deletion made in tmail. Sync holds back deletions when most of
/// a folder vanishes at once, in case the local copy was wiped by accident; a folder
/// emptied on purpose, such as Trash, must still be emptied on the server.
pub fn remember_deleted(root: &Path, file: &Path) {
    use std::io::Write;
    let Some(name) = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return;
    };
    if !root.join("termail-sync").is_dir() {
        return;
    }
    let base = storage::maildir_info(&name).0;
    if let Ok(mut list) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(deleted_path(root))
    {
        let _ = writeln!(list, "{base}");
    }
}

fn deleted_bases(root: &Path) -> std::collections::HashSet<String> {
    fs::read_to_string(deleted_path(root))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Forgets recorded deletions that sync has carried out.
fn prune_deleted(root: &Path, state: &SyncState) {
    let path = deleted_path(root);
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let pending: std::collections::HashSet<String> = state
        .folders
        .iter()
        .flat_map(|(folder, entry)| {
            entry
                .messages
                .keys()
                .map(|uid| base_name(&state.server, folder, entry.validity, *uid))
        })
        .collect();
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| pending.contains(*line))
        .collect();
    if kept.is_empty() {
        let _ = fs::remove_file(&path);
    } else {
        let _ = fs::write(&path, kept.join("\n") + "\n");
    }
}

/// Loads the sync state, reporting whether this is the account's first sync.
fn load_state(root: &Path, remote: &RemoteConfig) -> Result<(SyncState, bool), String> {
    let mut state: SyncState = match fs::read(state_path(root)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(crate::log::fail(
            "Sync state is unreadable. Existing messages were left unchanged.",
        ))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                SyncState {
                    server: server_key(remote),
                    ..SyncState::default()
                },
                true,
            ));
        }
        Err(error) => return Err(error.to_string()),
    };
    let server = server_key(remote);
    if !state.server.is_empty() && state.server != server {
        return Err("This local mailbox belongs to a different IMAP account.".into());
    }
    state.server = server;
    Ok((state, false))
}

fn server_key(remote: &RemoteConfig) -> String {
    format!(
        "{}:{}:{}",
        remote.imap_host, remote.imap_port, remote.username
    )
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
    /// Local messages from before the first sync that wait for an explicit upload.
    pub held: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncOptions {
    /// Uploads messages that were held back at the first sync.
    pub upload_held: bool,
}

pub fn sync(remote: &RemoteConfig, root: &Path) -> Result<Synced, RemoteError> {
    sync_with(remote, root, SyncOptions::default())
}

pub fn sync_with(
    remote: &RemoteConfig,
    root: &Path,
    options: SyncOptions,
) -> Result<Synced, RemoteError> {
    let mut session = connect(remote)?;
    let result = sync_session(&mut session, remote, root, options);
    let _ = session.logout();
    result.map_err(Into::into)
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

fn content_digest(contents: &[u8]) -> String {
    format!("{:x}", Sha256::digest(contents))[..32].to_owned()
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

fn append<T: Read + Write>(
    session: &mut imap::Session<T>,
    folder: &str,
    contents: &[u8],
    flags: Vec<imap::types::Flag<'static>>,
) -> imap::Result<()> {
    session
        .append(folder, contents)
        .flags(flags)
        .finish()
        .map(|_| ())
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

/// Finds every message Tmail downloaded, wherever it lives in the Maildir now.
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
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Invalid Maildir path."))?;
    let directory = match directory.parent() {
        Some(folder) if flags.contains('S') && directory.ends_with("new") => folder.join("cur"),
        _ => directory.to_path_buf(),
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

/// Quotes a value for an IMAP command, or refuses values that cannot be quoted.
fn quoted(value: &str) -> Option<String> {
    (!value.chars().any(|c| c.is_control()))
        .then(|| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

struct Capabilities {
    moves: bool,
    uidplus: bool,
    condstore: bool,
}

fn capabilities<T: Read + Write>(session: &mut imap::Session<T>) -> Result<Capabilities, String> {
    let capabilities = session
        .capabilities()
        .map(|capabilities| Capabilities {
            moves: capabilities.has_str("MOVE"),
            uidplus: capabilities.has_str("UIDPLUS"),
            condstore: capabilities.has_str("CONDSTORE") && capabilities.has_str("ENABLE"),
        })
        .map_err(crate::log::fail("Unable to read IMAP capabilities."))?;
    if capabilities.condstore {
        // Turns on HIGHESTMODSEQ in SELECT replies; a refusal only costs speed.
        let condstore = session.run_command_and_check_ok("ENABLE CONDSTORE").is_ok();
        return Ok(Capabilities {
            condstore,
            ..capabilities
        });
    }
    Ok(capabilities)
}

#[derive(Default)]
struct Report {
    downloaded: usize,
    remaining: usize,
    uploaded: usize,
    changed: usize,
    removed: usize,
    held: usize,
    skipped_deletes: bool,
}

fn sync_session<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    root: &Path,
    options: SyncOptions,
) -> Result<Synced, String> {
    initialize(root)?;
    let _lock = lock_account(root, false)?;
    let state_path = state_path(root);
    let (mut state, first) = load_state(root, remote)?;
    let capabilities = capabilities(session)?;
    let folders = discover(session, remote, true)?;
    let mut report = Report::default();
    let context = Context {
        root,
        folders: &folders,
        capabilities: &capabilities,
        state_path: &state_path,
        batch_size: remote.batch_size,
        max_bytes: remote.max_download_bytes,
        hold_new: first,
        upload_held: options.upload_held,
    };

    // Push and pull changes on known messages first, so moves land before any folder
    // looks for new messages.
    for folder in &folders {
        reconcile(session, &mut state, folder, &context, &mut report)?;
        save_state(&state_path, &state)?;
    }
    for folder in &folders {
        fetch_new(session, &mut state, folder, &context, &mut report)?;
        save_state(&state_path, &state)?;
    }
    prune_deleted(root, &state);
    report.held = state.held.len();
    let mut summary = format!("Synced · {} new messages", report.downloaded);
    if report.changed + report.removed + report.uploaded > 0 {
        summary.push_str(&format!(
            " · {} updates",
            report.changed + report.removed + report.uploaded
        ));
    }
    summary.push_str(&format!(" · {} older messages remaining", report.remaining));
    if report.held > 0 {
        summary.push_str(&format!(
            " · {} local messages kept off the server (:upload to send them)",
            report.held
        ));
    }
    if report.skipped_deletes {
        summary.push_str(" · many local messages were missing, so they will be downloaded again");
    }
    Ok(Synced {
        summary,
        changed: report.downloaded + report.changed + report.removed + report.uploaded > 0,
        downloaded: report.downloaded,
        held: report.held,
    })
}

struct Context<'a> {
    root: &'a Path,
    folders: &'a [Folder],
    capabilities: &'a Capabilities,
    state_path: &'a Path,
    batch_size: usize,
    max_bytes: u64,
    hold_new: bool,
    upload_held: bool,
}

/// The server's current flags for every message in the selected folder. With
/// CONDSTORE only messages changed since the last sync are fetched.
fn current_flags<T: Read + Write>(
    session: &mut imap::Session<T>,
    entry: &FolderState,
    exists: u32,
    modseq: Option<u64>,
    condstore: bool,
) -> Result<HashMap<u32, String>, String> {
    let mut current = HashMap::new();
    if exists == 0 {
        return Ok(current);
    }
    let incremental = condstore
        && entry.modseq > 0
        && modseq.is_some()
        && entry.messages.values().all(|value| value.size > 0);
    if !incremental {
        let fetched = session
            .uid_fetch("1:*", "(UID FLAGS)")
            .map_err(crate::log::fail("Unable to read IMAP message flags."))?;
        for message in fetched.iter() {
            if let Some(uid) = message.uid {
                current.insert(uid, remote_flags(message.flags()));
            }
        }
        return Ok(current);
    }
    let existing = session
        .uid_search("ALL")
        .map_err(crate::log::fail("Unable to list IMAP messages."))?;
    for uid in existing {
        if let Some(value) = entry.messages.get(&uid) {
            current.insert(uid, value.flags.clone());
        }
    }
    if modseq != Some(entry.modseq) {
        let fetched = session
            .uid_fetch(
                "1:*",
                format!("(UID FLAGS) (CHANGEDSINCE {})", entry.modseq),
            )
            .map_err(crate::log::fail("Unable to read changed IMAP flags."))?;
        for message in fetched.iter() {
            if let Some(uid) = message.uid
                && current.contains_key(&uid)
            {
                current.insert(uid, remote_flags(message.flags()));
            }
        }
    }
    Ok(current)
}

fn reconcile<T: Read + Write>(
    session: &mut imap::Session<T>,
    state: &mut SyncState,
    folder: &Folder,
    context: &Context<'_>,
    report: &mut Report,
) -> Result<(), String> {
    let root = context.root;
    let folders = context.folders;
    let capabilities = context.capabilities;
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
        entry.modseq = 0;
    }
    if entry.messages.is_empty() {
        entry.modseq = mailbox.highest_mod_seq.unwrap_or_default();
        return Ok(());
    }
    let current = current_flags(
        session,
        entry,
        mailbox.exists,
        mailbox.highest_mod_seq,
        capabilities.condstore,
    )?;
    let deleted = deleted_bases(root);
    let missing = entry
        .messages
        .iter()
        .filter(|(uid, value)| {
            value.size > 0
                && !deleted.contains(&base_name(&server, &folder.remote, validity, **uid))
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
            if synced.size == 0 || mass_missing && !deleted.contains(&base) {
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
        let mut digest = synced.digest.clone();
        if synced.size > 0 && (size != synced.size || modified != synced.modified) {
            // Copying or restoring a Maildir moves timestamps without changing a byte,
            // so only a real content change in Drafts replaces the server copy.
            let contents = fs::read(path).map_err(|error| error.to_string())?;
            let now = content_digest(&contents);
            let edited = digest.as_deref() != Some(now.as_str());
            if edited && folder.role == Some(Role::Drafts) && !synced.partial {
                append(session, &folder.remote, &contents, imap_flags(&local_flags))
                    .map_err(crate::log::fail("Unable to upload an edited message."))?;
                // Without the local file a failed delete below is retried as a local
                // deletion, never as a second upload.
                let _ = fs::remove_file(path);
                forget.push(*uid);
                report.uploaded += 1;
                session
                    .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                    .map_err(crate::log::fail("Unable to replace an edited message."))?;
                expunge.push(*uid);
                continue;
            }
            digest = Some(now);
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
        let mut id = synced.id.clone();
        if id.is_none() || digest.is_none() {
            let contents = fs::read(&path).unwrap_or_default();
            id = id.or_else(|| message_id(&contents));
            digest = digest.or_else(|| Some(content_digest(&contents)));
        }
        updates.push((
            *uid,
            Entry {
                flags: merged,
                id,
                size,
                modified,
                digest,
                partial: synced.partial,
            },
        ));
    }
    for uid in forget {
        entry.messages.remove(&uid);
    }
    for (uid, value) in updates {
        entry.messages.insert(uid, value);
    }
    entry.modseq = mailbox.highest_mod_seq.unwrap_or_default();
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

/// Expunges exactly `uids`. Without UIDPLUS a plain EXPUNGE would also remove messages
/// another client marked deleted, so those are unmarked around it and marked again.
fn expunge_messages<T: Read + Write>(
    session: &mut imap::Session<T>,
    uids: &[u32],
    capabilities: &Capabilities,
) -> Result<(), String> {
    let fail = crate::log::fail("Unable to expunge deleted IMAP messages.");
    if capabilities.uidplus {
        return session.uid_expunge(uid_set(uids)).map(|_| ()).map_err(fail);
    }
    let others: Vec<u32> = match session.uid_search("DELETED") {
        Ok(found) => found
            .into_iter()
            .filter(|uid| !uids.contains(uid))
            .collect(),
        Err(error) => return Err(fail(error)),
    };
    if !others.is_empty() {
        session
            .uid_store(uid_set(&others), "-FLAGS.SILENT (\\Deleted)")
            .map_err(crate::log::fail(
                "Unable to protect messages other apps deleted.",
            ))?;
    }
    let result = session.expunge().map(|_| ()).map_err(fail);
    if !others.is_empty() {
        session
            .uid_store(uid_set(&others), "+FLAGS.SILENT (\\Deleted)")
            .map_err(crate::log::fail(
                "Unable to restore deletion marks of other apps.",
            ))?;
    }
    result
}

fn fetch_new<T: Read + Write>(
    session: &mut imap::Session<T>,
    state: &mut SyncState,
    folder: &Folder,
    context: &Context<'_>,
    report: &mut Report,
) -> Result<(), String> {
    if LOCAL_ONLY.contains(&folder.local.as_str()) {
        return Ok(());
    }
    let local_root = folder_root(context.root, &folder.local);
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
            let base = split_flags(&name).0.to_owned();
            if !path.is_file() || known.contains(&base) {
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
                let target = base_name(&server, &folder.remote, validity, uid);
                let destination =
                    local_root
                        .join("cur")
                        .join(format!("{target}{}{}", separator(), flags));
                storage::rename(&path, &destination).map_err(|error| error.to_string())?;
                let (size, modified) = file_stamp(&destination);
                entry.messages.insert(
                    uid,
                    Entry {
                        flags: there,
                        id,
                        size,
                        modified,
                        digest: Some(content_digest(&contents)),
                        partial: false,
                    },
                );
                state.held.remove(&base);
                uids.remove(&uid);
            } else if name.starts_with("termail-imap-") {
                // It came from the server and is no longer where sync left it; the
                // server copy wherever it lives now is downloaded instead.
                fs::remove_file(&path).map_err(|error| error.to_string())?;
            } else if context.hold_new || (state.held.contains(&base) && !context.upload_held) {
                state.held.insert(base);
            } else {
                append(session, &folder.remote, &contents, imap_flags(&flags)).map_err(
                    crate::log::fail(format!("Unable to upload a message to {}.", folder.remote)),
                )?;
                fs::remove_file(&path).map_err(|error| error.to_string())?;
                state.held.remove(&base);
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
    report.remaining += pending.len().saturating_sub(context.batch_size);
    pending.truncate(context.batch_size);
    for chunk in pending.chunks(FETCH_CHUNK) {
        report.downloaded += download(
            session,
            &mut entry,
            &Target {
                server: &server,
                folder: &folder.remote,
                local_root: &local_root,
                validity,
            },
            chunk,
            context.max_bytes,
        )?;
        state
            .folders
            .insert(folder.remote.clone(), std::mem::take(&mut entry));
        save_state(context.state_path, state)?;
        entry = state.folders.remove(&folder.remote).unwrap_or_default();
    }
    state.folders.insert(folder.remote.clone(), entry);
    Ok(())
}

fn folder_root(root: &Path, local: &str) -> PathBuf {
    if local.is_empty() {
        root.to_path_buf()
    } else {
        root.join(local)
    }
}

struct Target<'a> {
    server: &'a str,
    folder: &'a str,
    local_root: &'a Path,
    validity: u32,
}

/// Downloads `uids` from the selected folder. Messages above `max_bytes` arrive as
/// headers with a short note, and their body follows on request.
fn download<T: Read + Write>(
    session: &mut imap::Session<T>,
    entry: &mut FolderState,
    target: &Target<'_>,
    uids: &[u32],
    max_bytes: u64,
) -> Result<usize, String> {
    let mut large = HashMap::new();
    if max_bytes > 0 {
        let sizes = session
            .uid_fetch(uid_set(uids), "(UID RFC822.SIZE)")
            .map_err(crate::log::fail("Unable to read IMAP message sizes."))?;
        for message in sizes.iter() {
            if let (Some(uid), Some(size)) = (message.uid, message.size)
                && u64::from(size) > max_bytes
            {
                large.insert(uid, size);
            }
        }
    }
    let (heavy, light): (Vec<u32>, Vec<u32>) = uids.iter().partition(|uid| large.contains_key(uid));
    let mut downloaded = 0;
    for (set, query) in [
        (light, "(UID FLAGS BODY.PEEK[])"),
        (heavy, "(UID FLAGS BODY.PEEK[HEADER])"),
    ] {
        if set.is_empty() {
            continue;
        }
        let fetched = session
            .uid_fetch(uid_set(&set), query)
            .map_err(crate::log::fail("Unable to download IMAP message."))?;
        for message in fetched.iter() {
            let Some(uid) = message.uid.filter(|uid| set.contains(uid)) else {
                continue;
            };
            let partial = large.get(&uid).copied();
            let contents = match partial {
                Some(size) => match message.header() {
                    Some(header) => partial_message(header, size),
                    None => continue,
                },
                None => match message.body() {
                    Some(body) => body.to_vec(),
                    None => continue,
                },
            };
            let flags = remote_flags(message.flags());
            let base = base_name(target.server, target.folder, target.validity, uid);
            let destination = target
                .local_root
                .join("cur")
                .join(format!("{base}{}{flags}", separator()));
            if !destination.exists() {
                write_message(target.local_root, &destination, &contents)?;
                downloaded += 1;
            }
            let (size, modified) = file_stamp(&destination);
            entry.messages.insert(
                uid,
                Entry {
                    flags,
                    id: message_id(&contents),
                    size,
                    modified,
                    digest: Some(content_digest(&contents)),
                    partial: partial.is_some(),
                },
            );
        }
    }
    Ok(downloaded)
}

fn write_message(local_root: &Path, destination: &Path, contents: &[u8]) -> Result<(), String> {
    let temporary = local_root
        .join("tmp")
        .join(storage::unique_name("download"));
    storage::write_new(&temporary, contents).map_err(|error| error.to_string())?;
    storage::rename(&temporary, destination).map_err(|error| error.to_string())
}

/// Header of a message marking it partial.
pub const PARTIAL_HEADER: &str = "X-Termail-Partial";

/// The original headers with a plain-text note in place of a body that was not
/// downloaded. Content headers are replaced so the note parses on its own.
fn partial_message(header: &[u8], size: u32) -> Vec<u8> {
    let text = String::from_utf8_lossy(header);
    let mut output = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        if line.trim_end().is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if !skipping {
                output.push_str(line);
            }
            continue;
        }
        let name = line
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        skipping = name == "content-type" || name == "content-transfer-encoding";
        if !skipping {
            output.push_str(line);
        }
    }
    let kilobytes = size.div_ceil(1024);
    output.push_str(&format!(
        "{PARTIAL_HEADER}: {size}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThis message is {kilobytes} KB and only its headers were downloaded.\r\nPress D to download the full message.\r\n"
    ));
    output.into_bytes()
}

/// Locates the folder and UID a downloaded file came from.
fn find_entry<'a>(
    state: &'a mut SyncState,
    base: &str,
) -> Option<(String, u32, u32, &'a mut Entry)> {
    let server = state.server.clone();
    state.folders.iter_mut().find_map(|(name, folder)| {
        let validity = folder.validity;
        folder.messages.iter_mut().find_map(|(uid, entry)| {
            (base_name(&server, name, validity, *uid) == base)
                .then(|| (name.clone(), validity, *uid, entry))
        })
    })
}

/// Replaces a partial message with the full one from the server.
pub fn download_full(
    remote: &RemoteConfig,
    root: &Path,
    path: &Path,
) -> Result<PathBuf, RemoteError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Invalid Maildir filename.")?
        .to_owned();
    let base = split_flags(&name).0.to_owned();
    let mut session = connect(remote)?;
    let result = download_full_in(&mut session, remote, root, path, &base);
    let _ = session.logout();
    result.map_err(Into::into)
}

fn download_full_in<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    root: &Path,
    path: &Path,
    base: &str,
) -> Result<PathBuf, String> {
    {
        let _lock = lock_account(root, true)?;
        let (mut state, _) = load_state(root, remote)?;
        let (folder, validity, uid, _) =
            find_entry(&mut state, base).ok_or("This message is no longer synced.")?;
        let mailbox = session.examine(&folder).map_err(crate::log::fail(format!(
            "Unable to open IMAP folder {folder}."
        )))?;
        if mailbox.uid_validity != Some(validity) {
            return Err("The folder changed on the server. Sync, then try again.".into());
        }
        let fetched = session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .map_err(crate::log::fail("Unable to download IMAP message."))?;
        let body = fetched
            .iter()
            .find(|message| message.uid == Some(uid))
            .and_then(|message| message.body())
            .ok_or("The server no longer has this message.")?
            .to_vec();
        let local_root = path
            .parent()
            .and_then(Path::parent)
            .ok_or("Invalid Maildir path.")?;
        write_message(local_root, path, &body)?;
        let (size, modified) = file_stamp(path);
        if let Some((_, _, _, entry)) = find_entry(&mut state, base) {
            entry.partial = false;
            entry.size = size;
            entry.modified = modified;
            entry.digest = Some(content_digest(&body));
        }
        save_state(&state_path(root), &state)?;
        Ok(path.to_path_buf())
    }
}

/// A folder change made on the server and mirrored locally.
pub enum FolderChange {
    /// Creates a folder. `/` separates levels.
    Create(String),
    /// Renames the local Maildir folder to a new name.
    Rename { local: String, name: String },
    /// Deletes the local Maildir folder and its messages.
    Delete(String),
}

/// The local Maildir++ folder for a display name such as `Projects/2026`.
pub fn local_name(name: &str) -> String {
    format!(
        ".{}",
        name.split('/')
            .map(|component| crate::mailbox::safe_file_name(component.trim()).replace('.', "_"))
            .collect::<Vec<_>>()
            .join(".")
    )
}

pub fn change_folder(
    remote: &RemoteConfig,
    root: &Path,
    change: &FolderChange,
) -> Result<String, RemoteError> {
    let mut session = connect(remote)?;
    let result = change_folder_in(&mut session, remote, root, change);
    let _ = session.logout();
    result.map_err(Into::into)
}

fn change_folder_in<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    root: &Path,
    change: &FolderChange,
) -> Result<String, String> {
    {
        let _lock = lock_account(root, true)?;
        let (mut state, _) = load_state(root, remote)?;
        let (folders, layout) = discover_layout(session, remote, false)?;
        let remote_name = |name: &str| {
            let components: Vec<String> = name
                .split('/')
                .map(|component| encode_utf7(component.trim()))
                .collect();
            format!(
                "{}{}",
                layout.prefix,
                components.join(layout.delimiter.as_deref().unwrap_or("/"))
            )
        };
        let existing = |local: &str| {
            folders
                .iter()
                .find(|folder| folder.local == local)
                .cloned()
                .ok_or_else(|| "That folder is not on the server.".to_owned())
        };
        match change {
            FolderChange::Create(name) => {
                let target = remote_name(name);
                session
                    .create(&target)
                    .map_err(crate::log::fail("The server refused to create the folder."))?;
                let _ = session.subscribe(&target);
                let local = folder_root(root, &local_name(name));
                for child in ["cur", "new", "tmp"] {
                    fs::create_dir_all(local.join(child)).map_err(|error| error.to_string())?;
                }
                Ok(format!("Created {name}"))
            }
            FolderChange::Rename { local, name } => {
                let folder = existing(local)?;
                if folder.role.is_some() {
                    return Err("Standard folders cannot be renamed.".into());
                }
                let target = remote_name(name);
                session
                    .rename(&folder.remote, &target)
                    .map_err(crate::log::fail("The server refused to rename the folder."))?;
                // Children move with their parent on the server; move their files too.
                let delimiter = layout.delimiter.clone().unwrap_or_default();
                for moved in folders.iter().filter(|candidate| {
                    candidate.remote == folder.remote
                        || (!delimiter.is_empty()
                            && candidate
                                .remote
                                .starts_with(&format!("{}{delimiter}", folder.remote)))
                }) {
                    let new_remote = format!("{target}{}", &moved.remote[folder.remote.len()..]);
                    let new_local =
                        format!("{}{}", local_name(name), &moved.local[folder.local.len()..]);
                    relocate(root, &mut state, moved, &new_remote, &new_local)?;
                }
                save_state(&state_path(root), &state)?;
                Ok(format!("Renamed to {name}"))
            }
            FolderChange::Delete(local) => {
                let folder = existing(local)?;
                if folder.role.is_some() {
                    return Err("Standard folders cannot be deleted.".into());
                }
                session.delete(&folder.remote).map_err(crate::log::fail(
                    "The server refused to delete the folder. Delete its subfolders first.",
                ))?;
                state.folders.remove(&folder.remote);
                save_state(&state_path(root), &state)?;
                let _ = fs::remove_dir_all(folder_root(root, &folder.local));
                Ok("Folder deleted".into())
            }
        }
    }
}

/// Moves a folder's files and sync state to a new server and local name.
fn relocate(
    root: &Path,
    state: &mut SyncState,
    folder: &Folder,
    new_remote: &str,
    new_local: &str,
) -> Result<(), String> {
    let from = folder_root(root, &folder.local);
    let to = folder_root(root, new_local);
    for child in ["cur", "new", "tmp"] {
        fs::create_dir_all(to.join(child)).map_err(|error| error.to_string())?;
    }
    let server = state.server.clone();
    let mut renames = HashMap::new();
    if let Some(entry) = state.folders.get(&folder.remote) {
        for uid in entry.messages.keys() {
            renames.insert(
                base_name(&server, &folder.remote, entry.validity, *uid),
                base_name(&server, new_remote, entry.validity, *uid),
            );
        }
    }
    for child in ["cur", "new"] {
        let Ok(files) = fs::read_dir(from.join(child)) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let (base, flags) = split_flags(&name);
            let renamed = match renames.get(base) {
                Some(new_base) if name.contains(separator()) => {
                    format!("{new_base}{}{flags}", separator())
                }
                Some(new_base) => new_base.clone(),
                None => name.clone(),
            };
            storage::rename(&file.path(), &to.join(child).join(renamed))
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(entry) = state.folders.remove(&folder.remote) {
        state.folders.insert(new_remote.to_owned(), entry);
    }
    let _ = fs::remove_dir_all(&from);
    Ok(())
}

/// Searches the server for messages that were never downloaded and downloads up to
/// `limit` of the newest matches. Returns how many arrived.
pub fn search_server(
    remote: &RemoteConfig,
    root: &Path,
    query: &str,
    limit: usize,
) -> Result<usize, RemoteError> {
    let criteria = server_criteria(query).ok_or("Nothing to search for on the server.")?;
    let mut session = connect(remote)?;
    let result = search_server_in(&mut session, remote, root, &criteria, limit);
    let _ = session.logout();
    result.map_err(Into::into)
}

fn search_server_in<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    root: &Path,
    criteria: &str,
    limit: usize,
) -> Result<usize, String> {
    {
        let _lock = lock_account(root, true)?;
        let (mut state, _) = load_state(root, remote)?;
        let folders = discover(session, remote, false)?;
        let server = state.server.clone();
        let mut found = 0;
        for folder in &folders {
            if found >= limit
                || LOCAL_ONLY.contains(&folder.local.as_str())
                || matches!(folder.role, Some(Role::Trash | Role::Junk))
            {
                continue;
            }
            let mailbox = session
                .examine(&folder.remote)
                .map_err(crate::log::fail(format!(
                    "Unable to open IMAP folder {}.",
                    folder.remote
                )))?;
            let Some(validity) = mailbox.uid_validity else {
                continue;
            };
            let mut entry = state.folders.remove(&folder.remote).unwrap_or_default();
            if entry.validity != validity {
                // Not synced yet or reset; a normal sync handles this folder first.
                state.folders.insert(folder.remote.clone(), entry);
                continue;
            }
            let mut matches: Vec<u32> = session
                .uid_search(criteria)
                .map_err(crate::log::fail("The server could not run this search."))?
                .into_iter()
                .filter(|uid| !entry.messages.contains_key(uid))
                .collect();
            matches.sort_unstable_by(|a, b| b.cmp(a));
            matches.truncate(limit - found);
            let local_root = folder_root(root, &folder.local);
            for chunk in matches.chunks(FETCH_CHUNK) {
                found += download(
                    session,
                    &mut entry,
                    &Target {
                        server: &server,
                        folder: &folder.remote,
                        local_root: &local_root,
                        validity,
                    },
                    chunk,
                    remote.max_download_bytes,
                )?;
            }
            state.folders.insert(folder.remote.clone(), entry);
            save_state(&state_path(root), &state)?;
        }
        Ok(found)
    }
}

/// Translates a Tmail search into IMAP SEARCH criteria. Filters that only make sense
/// locally (folders, read state) are left out.
fn server_criteria(query: &str) -> Option<String> {
    let mut criteria = Vec::new();
    for term in crate::mail::search_terms(query) {
        let (negate, term) = match term.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest.to_owned()),
            _ => (false, term),
        };
        let criterion = match term.split_once(':') {
            Some(("from", value)) => quoted(value).map(|value| format!("FROM {value}")),
            Some(("to", value)) => quoted(value).map(|value| format!("TO {value}")),
            Some(("subject", value)) => quoted(value).map(|value| format!("SUBJECT {value}")),
            Some(("body", value)) => quoted(value).map(|value| format!("BODY {value}")),
            Some(("before", value)) => imap_date(value).map(|date| format!("BEFORE {date}")),
            Some(("after", value)) => imap_date(value).map(|date| format!("SINCE {date}")),
            Some(("is" | "in" | "folder" | "has", _)) => None,
            _ => quoted(&term).map(|value| format!("TEXT {value}")),
        };
        if let Some(criterion) = criterion {
            criteria.push(if negate {
                format!("NOT {criterion}")
            } else {
                criterion
            });
        }
    }
    (!criteria.is_empty()).then(|| criteria.join(" "))
}

fn imap_date(value: &str) -> Option<String> {
    let date = crate::mail::parse_date(value)?;
    Some(date.format("%-d-%b-%Y").to_string())
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
    let mut session = connect(remote).map_err(|error| CopyError {
        message: error.message,
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
    append(session, &sent, contents, vec![imap::types::Flag::Seen]).map_err(|_| CopyError {
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

/// Blocks until the server reports a change in `folder`, the timeout passes, or
/// `stop` is set. Returns `Ok(true)` when the folder changed.
pub fn wait_for_change(
    remote: &RemoteConfig,
    folder: &str,
    timeout: std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<bool, RemoteError> {
    let mut session = connect(remote)?;
    let result = watch_session(&mut session, folder, timeout, stop);
    let _ = session.logout();
    result
}

fn watch_session(
    session: &mut Session,
    folder: &str,
    timeout: std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<bool, RemoteError> {
    let supported = session
        .capabilities()
        .map(|capabilities| capabilities.has_str("IDLE"))
        .unwrap_or(false);
    if !supported {
        return Err(RemoteError::unsupported("IDLE is not supported."));
    }
    session
        .select(folder)
        .map_err(crate::log::fail(format!("Unable to open {folder}.")))?;
    let started = std::time::Instant::now();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) && started.elapsed() < timeout {
        match session
            .idle()
            .timeout(std::time::Duration::from_secs(60))
            .keepalive(false)
            .wait_while(imap::extensions::idle::stop_on_any)
        {
            Ok(imap::extensions::idle::WaitOutcome::MailboxChanged) => return Ok(true),
            Ok(imap::extensions::idle::WaitOutcome::TimedOut) => {}
            Err(_) => return Err("The IDLE connection was interrupted.".into()),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {

    /// Runs against a real IMAP and SMTP server, such as the GreenMail container the
    /// CI starts. Skipped unless asked for: `cargo test -- --ignored real_server`.
    /// Settings come from TMAIL_IT_HOST, _IMAP_PORT, _SMTP_PORT, _USER, _PASSWORD
    /// and _CA (a PEM file to trust).
    #[test]
    #[ignore]
    fn real_server_round_trip() {
        let variable = |name: &str, fallback: &str| {
            std::env::var(format!("TMAIL_IT_{name}")).unwrap_or_else(|_| fallback.to_owned())
        };
        let user = variable("USER", "ada@localhost");
        let remote = RemoteConfig {
            imap_host: variable("HOST", "localhost"),
            imap_port: variable("IMAP_PORT", "3993").parse().unwrap(),
            smtp_host: variable("HOST", "localhost"),
            smtp_port: variable("SMTP_PORT", "3465").parse().unwrap(),
            username: user.clone(),
            tls_certificate: std::env::var_os("TMAIL_IT_CA").map(PathBuf::from),
            sync_interval_secs: 0,
            session_password: Some(std::sync::Arc::new(crate::remote::SessionPassword(
                zeroize::Zeroizing::new(variable("PASSWORD", "secret")),
            ))),
            ..Default::default()
        };
        let root = std::env::temp_dir().join(storage::unique_name("real-server"));
        let subject = storage::unique_name("tmail-it");
        let message = format!(
            "From: {user}\r\nTo: {user}\r\nSubject: {subject}\r\nMessage-ID: <{subject}@tmail.test>\r\nDate: Mon, 2 Mar 2026 10:00:00 +0000\r\n\r\nhello from the integration test\r\n"
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(crate::sender::send_smtp(
                remote.clone(),
                message.clone().into_bytes(),
            ))
            .unwrap();
        // Delivery is asynchronous on most servers.
        let arrived = (0..20).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            sync(&remote, &root).unwrap();
            scan_local(&root)
                .values()
                .any(|path| fs::read_to_string(path).is_ok_and(|text| text.contains(&subject)))
        });
        assert!(arrived, "the sent message never reached the Inbox");
        let path = scan_local(&root)
            .into_values()
            .find(|path| fs::read_to_string(path).is_ok_and(|text| text.contains(&subject)))
            .unwrap();
        // Mark it read and file it in a new folder; both must reach the server.
        let read = set_local_flags(&path, "S").unwrap();
        sync(&remote, &root).unwrap();
        change_folder(&remote, &root, &FolderChange::Create("Tmail Test".into())).unwrap();
        let folder = root.join(local_name("Tmail Test"));
        storage::rename(&read, &folder.join("cur").join(read.file_name().unwrap())).unwrap();
        sync(&remote, &root).unwrap();
        let mut session = connect(&remote).unwrap();
        let (folders, _) = discover_layout(&mut session, &remote, false).unwrap();
        let target = folders
            .iter()
            .find(|candidate| candidate.local == local_name("Tmail Test"))
            .unwrap();
        session.examine(&target.remote).unwrap();
        let found = session
            .uid_search(format!("SUBJECT \"{subject}\" SEEN"))
            .unwrap();
        assert_eq!(
            found.len(),
            1,
            "the read message is in the new folder on the server"
        );
        // GreenMail starts with only an inbox; real servers come with a Sent folder.
        let _ = session.create("Sent");
        let _ = session.logout();
        // The Sent copy is saved once, and IDLE notices new mail.
        copy_sent(&remote, message.as_bytes(), true)
            .map_err(|error| error.message)
            .unwrap();
        copy_sent(&remote, message.as_bytes(), true)
            .map_err(|error| error.message)
            .unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let watcher = {
            let remote = remote.clone();
            std::thread::spawn(move || {
                wait_for_change(&remote, "INBOX", std::time::Duration::from_secs(30), &stop)
            })
        };
        std::thread::sleep(std::time::Duration::from_secs(2));
        runtime
            .block_on(crate::sender::send_smtp(
                remote.clone(),
                message.replace(&subject, "idle").into_bytes(),
            ))
            .unwrap();
        assert!(
            watcher.join().unwrap().unwrap(),
            "IDLE reported the new message"
        );
        change_folder(
            &remote,
            &root,
            &FolderChange::Delete(local_name("Tmail Test")),
        )
        .unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_trusted_self_signed_certificate_lets_the_connection_through() {
        let server = Server::new();
        let mut remote = server.remote();
        let port = server.port;
        fake_imap::serve(server, standard());
        remote.session_password = Some(std::sync::Arc::new(crate::remote::SessionPassword(
            zeroize::Zeroizing::new("pw".into()),
        )));
        let error = connect(&remote).err().unwrap();
        assert_eq!(
            error.kind,
            crate::remote::ErrorKind::Certificate,
            "{}",
            error.message
        );
        // "localhost" may resolve to IPv6 first while the server listens on IPv4.
        let presented =
            crate::certs::fetch("localhost", port, crate::certs::Protocol::Imap, false).unwrap();
        let file = std::env::temp_dir().join(storage::unique_name("trusted-cert"));
        fs::write(&file, &presented.pem).unwrap();
        remote.tls_certificate = Some(file.clone());
        connect(&remote).unwrap().logout().unwrap();
        let _ = fs::remove_file(file);
    }

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
        let result = sync_session(&mut session, remote, &cache.0, SyncOptions::default()).unwrap();
        session.logout().unwrap();
        result.summary
    }

    #[test]
    fn copied_or_restored_files_are_not_uploaded_again() {
        let state = standard();
        state
            .lock()
            .unwrap()
            .deliver("INBOX", &mail("one@example.test", "one"), &["\\Seen"]);
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        run(&remote, &tls, &cache);
        // Copying a Maildir without keeping times touches every file.
        for name in cache.files("") {
            let path = cache.0.join("cur").join(name);
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
                .unwrap();
        }
        run(&remote, &tls, &cache);
        let state = state.lock().unwrap();
        assert_eq!(state.folders["INBOX"].messages.len(), 1);
        assert!(
            !state
                .commands
                .iter()
                .any(|command| command.starts_with("APPEND"))
        );
    }

    #[test]
    fn expunge_without_uidplus_keeps_messages_others_deleted() {
        let state = standard();
        state.lock().unwrap().capabilities = "IMAP4rev1 IDLE".into();
        state
            .lock()
            .unwrap()
            .deliver("INBOX", &mail("mine@example.test", "mine"), &[]);
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        run(&remote, &tls, &cache);
        state.lock().unwrap().deliver(
            "INBOX",
            &mail("theirs@example.test", "theirs"),
            &["\\Deleted"],
        );
        fs::remove_file(cache.0.join("cur").join(&cache.files("")[0])).unwrap();
        run(&remote, &tls, &cache);
        let state = state.lock().unwrap();
        let inbox = &state.folders["INBOX"].messages;
        assert_eq!(inbox.len(), 1, "the other client's message must survive");
        assert!(inbox[0].flags.iter().any(|flag| flag == "\\Deleted"));
        assert_eq!(state.folders["INBOX.Trash"].messages.len(), 1);
    }

    #[test]
    fn first_sync_keeps_existing_local_mail_off_the_server() {
        let state = standard();
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        initialize(&cache.0).unwrap();
        fs::write(
            cache.0.join("cur").join(format!("1.old{}S", separator())),
            mail("old@example.test", "old"),
        )
        .unwrap();
        assert!(run(&remote, &tls, &cache).contains("kept off the server"));
        run(&remote, &tls, &cache);
        assert!(state.lock().unwrap().folders["INBOX"].messages.is_empty());
        let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
        let synced = sync_session(
            &mut session,
            &remote,
            &cache.0,
            SyncOptions { upload_held: true },
        )
        .unwrap();
        assert_eq!(synced.held, 0);
        assert_eq!(state.lock().unwrap().folders["INBOX"].messages.len(), 1);
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
            .find(|name| name.contains(&format!("-{first}{}", &storage::MAILDIR_SEPARATOR[..1])))
            .unwrap();
        let two = files
            .iter()
            .find(|name| name.contains(&format!("-{second}{}", &storage::MAILDIR_SEPARATOR[..1])))
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
        let two = local
            .iter()
            .find(|name| name.contains(&format!("-2{}", &storage::MAILDIR_SEPARATOR[..1])))
            .unwrap();
        fs::remove_file(cache.0.join("cur").join(two)).unwrap();
        run(&remote, &tls, &cache);
        let state_now = state.lock().unwrap();
        assert_eq!(state_now.folders["INBOX"].messages.len(), 1);
        assert_eq!(state_now.folders["INBOX.Trash"].messages.len(), 1);
        drop(state_now);
        assert_eq!(cache.files("").len(), 1);
    }

    #[test]
    fn emptying_trash_in_tmail_empties_it_on_the_server() {
        // More than the mass-deletion guard allows, all deleted on purpose.
        let state = standard();
        for index in 0..(MASS_DELETE_LIMIT + 5) {
            state.lock().unwrap().deliver(
                "INBOX.Trash",
                &mail(&format!("trash-{index}@example.test"), "old"),
                &["\\Seen"],
            );
        }
        let (remote, tls) = start(&state);
        let cache = Cache::new();
        run(&remote, &tls, &cache);
        let source = crate::mailbox::MailboxSource::Maildir(cache.0.clone());
        for name in cache.files(".Trash") {
            let mut message = crate::mail::Mail::new(&name, "a@example.test", "old", "", None);
            message.source_path = Some(cache.0.join(".Trash/cur").join(&name));
            source.delete(&message).unwrap();
        }
        run(&remote, &tls, &cache);
        assert!(
            state.lock().unwrap().folders["INBOX.Trash"]
                .messages
                .is_empty()
        );
        // Nothing comes back as new mail, and the record of deletions is cleared.
        run(&remote, &tls, &cache);
        assert!(cache.files(".Trash").is_empty());
        assert!(!deleted_path(&cache.0).exists());
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
        let sent_name = format!("123.local{}S", storage::MAILDIR_SEPARATOR);
        storage::write_new(&cache.0.join(".Sent/cur").join(sent_name), &sent).unwrap();
        fs::create_dir_all(cache.0.join(".Drafts/cur")).unwrap();
        storage::write_new(
            &cache
                .0
                .join(".Drafts/cur")
                .join(format!("124.local{}", storage::MAILDIR_SEPARATOR)),
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
        let worker = server.spawn(crate::remote::tests::refused);
        let error = connect_with_tls(
            &remote,
            "fake-password",
            &native_tls::TlsConnector::new().unwrap(),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind, crate::remote::ErrorKind::Certificate);
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
        assert!(error.message.contains("login failed"));
        assert!(!error.message.contains("fake-password"));
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
