use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{remote::RemoteConfig, storage};

type Session = imap::Session<native_tls::TlsStream<TcpStream>>;

pub fn connect(remote: &RemoteConfig) -> Result<Session, String> {
    let password = remote.password()?;
    let tls = native_tls::TlsConnector::new().map_err(|_| "Unable to configure IMAP TLS.")?;
    connect_with_tls(remote, &password, &tls)
}

fn connect_with_tls(
    remote: &RemoteConfig,
    password: &str,
    tls: &native_tls::TlsConnector,
) -> Result<Session, String> {
    remote.validate()?;
    let addresses = (remote.imap_host.as_str(), remote.imap_port)
        .to_socket_addrs()
        .map_err(|_| "Unable to resolve IMAP host.")?;
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
        .map_err(|_| "Unable to set IMAP timeout.")?;
    stream
        .set_write_timeout(Some(remote.timeout()))
        .map_err(|_| "Unable to set IMAP timeout.")?;
    let stream = tls
        .connect(&remote.imap_host, stream)
        .map_err(|_| "IMAP TLS verification or handshake failed.")?;
    let mut client = imap::Client::new(stream);
    client
        .read_greeting()
        .map_err(|_| "Invalid IMAP greeting.")?;
    client
        .login(&remote.username, password)
        .map_err(|_| "IMAP login failed. Check the username and password.".into())
}

#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    server: String,
    folders: BTreeMap<String, FolderState>,
}

#[derive(Default, Serialize, Deserialize)]
struct FolderState {
    validity: u32,
    messages: BTreeMap<u32, String>,
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

pub fn sync(remote: &RemoteConfig, root: &Path) -> Result<String, String> {
    let mut session = connect(remote)?;
    let result = sync_session(&mut session, remote, root);
    let _ = session.logout();
    result
}

fn sync_session<T: Read + Write>(
    session: &mut imap::Session<T>,
    remote: &RemoteConfig,
    root: &Path,
) -> Result<String, String> {
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
        .map_err(|_| "This account is already syncing.")?;
    let _lock = Lock(file);
    let state_path = directory.join("state.json");
    let mut state: SyncState = match fs::read(&state_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| "Sync state is unreadable. Existing messages were left unchanged.")?,
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
    let mut downloaded = 0;
    let mut remaining = 0;
    for (name, local) in [
        (&remote.inbox, root.to_path_buf()),
        (&remote.sent, root.join(".Sent")),
    ] {
        let mailbox = session
            .examine(name)
            .map_err(|_| format!("Unable to open IMAP folder {name}."))?;
        let validity = mailbox
            .uid_validity
            .ok_or("IMAP server did not return UIDVALIDITY.")?;
        let folder = state.folders.entry(name.clone()).or_default();
        if folder.validity != validity {
            folder.messages.clear();
            folder.validity = validity;
        }
        let uids = session
            .uid_search("ALL")
            .map_err(|_| "Unable to list IMAP messages.")?;
        let mut pending: Vec<_> = uids
            .into_iter()
            .filter(|uid| !folder.messages.contains_key(uid))
            .collect();
        pending.sort_unstable_by(|a, b| b.cmp(a));
        remaining += pending.len().saturating_sub(remote.batch_size);
        let mut existing_ids = local_ids(&local)?;
        for uid in pending.into_iter().take(remote.batch_size) {
            let fetched = session
                .uid_fetch(uid.to_string(), "(UID FLAGS BODY.PEEK[])")
                .map_err(|_| "Unable to download IMAP message.")?;
            let Some(message) = fetched.iter().find(|message| message.uid == Some(uid)) else {
                continue;
            };
            let body = message.body().ok_or("IMAP response has no message body.")?;
            let id = mail_parser::MessageParser::default()
                .parse(body)
                .and_then(|parsed| parsed.message_id().map(str::to_owned));
            let flags = format!(
                "{}{}",
                if message.flags().contains(&imap::types::Flag::Flagged) {
                    "F"
                } else {
                    ""
                },
                if message.flags().contains(&imap::types::Flag::Seen) {
                    "S"
                } else {
                    ""
                }
            );
            let hash = format!("{:x}", Sha256::digest(format!("{}:{name}", state.server)));
            let filename = format!("termail-imap-{}-{validity}-{uid}!2,{flags}", &hash[..16]);
            if !id.as_ref().is_some_and(|id| existing_ids.contains(id)) {
                let destination = local.join("cur").join(&filename);
                if !destination.exists() {
                    let temporary = local.join("tmp").join(storage::unique_name("download"));
                    storage::write_new(&temporary, body).map_err(|error| error.to_string())?;
                    storage::rename(&temporary, &destination).map_err(|error| error.to_string())?;
                    downloaded += 1;
                }
                if let Some(id) = id {
                    existing_ids.insert(id);
                }
            }
            state
                .folders
                .get_mut(name)
                .unwrap()
                .messages
                .insert(uid, filename);
            save_state(&state_path, &state)?;
        }
        save_state(&state_path, &state)?;
    }
    Ok(format!(
        "Synced · {downloaded} new messages · {remaining} older messages remaining"
    ))
}

fn save_state(path: &Path, state: &SyncState) -> Result<(), String> {
    let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
    let temporary = path.with_file_name(storage::unique_name("sync-state"));
    storage::write_new(&temporary, &bytes).map_err(|error| error.to_string())?;
    storage::rename(&temporary, path).map_err(|error| error.to_string())
}

fn local_ids(root: &Path) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    for child in ["cur", "new"] {
        for entry in fs::read_dir(root.join(child)).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry.path().is_file() {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|error| error.to_string())?;
            if let Some(parsed) = mail_parser::MessageParser::default().parse(&bytes)
                && let Some(id) = parsed.message_id()
            {
                ids.insert(id.to_owned());
            }
        }
    }
    Ok(ids)
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
    if id.chars().any(|c| c.is_control() || "\"\\".contains(c)) {
        return Err(failure("Invalid Message-ID in Sent copy."));
    }
    session
        .examine(&remote.sent)
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
        .append_with_flags(&remote.sent, contents, &[imap::types::Flag::Seen])
        .map_err(|_| CopyError {
            message: "Unable to confirm the remote Sent copy. SMTP delivery will not be repeated."
                .into(),
            uncertain: true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::tests::Server;
    use std::io::{BufRead, BufReader};

    const MESSAGE: &[u8] = b"From: test@example.test\r\nTo: reader@example.test\r\nMessage-ID: <one@example.test>\r\nSubject: hello\r\n\r\nhello\r\n";

    struct Cache(std::path::PathBuf);

    impl Cache {
        fn new() -> Self {
            Self(std::env::temp_dir().join(storage::unique_name("imap-test")))
        }
    }

    impl Drop for Cache {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mock(
        server: Server,
        existing_copy: bool,
        drop_append: bool,
    ) -> std::thread::JoinHandle<Vec<String>> {
        server.spawn(move |stream| {
            let mut stream = BufReader::new(stream.unwrap());
            stream.get_mut().write_all(b"* OK local test\r\n").unwrap();
            let mut commands = Vec::new();
            let mut inbox = false;
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                commands.push(command.to_string());
                if command.starts_with("EXAMINE ") {
                    inbox = command == "EXAMINE \"INBOX\"";
                    stream
                        .get_mut()
                        .write_all(b"* 1 EXISTS\r\n* OK [UIDVALIDITY 7] stable\r\n")
                        .unwrap();
                } else if command.starts_with("UID SEARCH ") {
                    let found = if command.contains("HEADER Message-ID") {
                        existing_copy
                    } else {
                        inbox
                    };
                    stream
                        .get_mut()
                        .write_all(if found {
                            b"* SEARCH 1\r\n"
                        } else {
                            b"* SEARCH\r\n"
                        })
                        .unwrap();
                } else if command.starts_with("UID FETCH ") {
                    let section = if command.contains("BODY.PEEK[HEADER]") {
                        "HEADER"
                    } else {
                        ""
                    };
                    let response = format!(
                        "* 1 FETCH (UID 1 FLAGS (\\Seen \\Flagged) BODY[{section}] {{{}}}\r\n",
                        MESSAGE.len()
                    );
                    stream.get_mut().write_all(response.as_bytes()).unwrap();
                    stream.get_mut().write_all(MESSAGE).unwrap();
                    stream.get_mut().write_all(b")\r\n").unwrap();
                } else if command.starts_with("APPEND ") {
                    let length: usize = command
                        .rsplit_once('{')
                        .unwrap()
                        .1
                        .trim_end_matches('}')
                        .parse()
                        .unwrap();
                    stream.get_mut().write_all(b"+ continue\r\n").unwrap();
                    let mut bytes = vec![0; length];
                    stream.read_exact(&mut bytes).unwrap();
                    assert_eq!(bytes, MESSAGE);
                    let mut ending = [0; 2];
                    stream.read_exact(&mut ending).unwrap();
                    assert_eq!(&ending, b"\r\n");
                    if drop_append {
                        break;
                    }
                } else if command == "LOGOUT" {
                    stream
                        .get_mut()
                        .write_all(format!("* BYE closing\r\n{tag} OK logout\r\n").as_bytes())
                        .unwrap();
                    break;
                } else {
                    assert!(
                        command.starts_with("LOGIN "),
                        "unexpected command: {command}"
                    );
                }
                stream
                    .get_mut()
                    .write_all(format!("{tag} OK done\r\n").as_bytes())
                    .unwrap();
            }
            commands
        })
    }

    #[test]
    fn sync_downloads_once_with_peek_and_persists_uids() {
        let server = Server::new();
        let remote = server.remote();
        let tls = server.connector();
        let worker = mock(server, false, false);
        let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
        let cache = Cache::new();
        assert!(
            sync_session(&mut session, &remote, &cache.0)
                .unwrap()
                .contains("1 new")
        );
        assert!(
            sync_session(&mut session, &remote, &cache.0)
                .unwrap()
                .contains("0 new")
        );
        let state_path = cache.0.join("termail-sync/state.json");
        let mut state: SyncState = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state.folders.get_mut("INBOX").unwrap().validity = 6;
        save_state(&state_path, &state).unwrap();
        assert!(
            sync_session(&mut session, &remote, &cache.0)
                .unwrap()
                .contains("0 new")
        );
        state.server = "different-account".into();
        save_state(&state_path, &state).unwrap();
        assert!(
            sync_session(&mut session, &remote, &cache.0)
                .unwrap_err()
                .contains("different IMAP account")
        );
        session.logout().unwrap();
        let commands = worker.join().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|line| line.starts_with("UID FETCH"))
                .count(),
            2
        );
        assert!(commands.iter().any(|line| line.contains("BODY.PEEK[]")));
        let files = fs::read_dir(cache.0.join("cur"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().to_string_lossy().ends_with("!2,FS"));
        assert_eq!(fs::read(files[0].path()).unwrap(), MESSAGE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                files[0].metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn sent_copy_is_appended_once_and_existing_copy_is_verified() {
        for existing in [false, true] {
            let server = Server::new();
            let remote = server.remote();
            let tls = server.connector();
            let worker = mock(server, existing, false);
            let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
            assert!(copy_sent_session(&mut session, &remote, MESSAGE, true).is_ok());
            session.logout().unwrap();
            let commands = worker.join().unwrap();
            assert_eq!(
                commands
                    .iter()
                    .any(|command| command.starts_with("APPEND \"INBOX.Sent\"")),
                !existing
            );
            assert_eq!(
                commands
                    .iter()
                    .any(|command| command.contains("BODY.PEEK[HEADER]")),
                existing
            );
        }
    }

    #[test]
    fn uncertain_copy_is_checked_without_appending_again() {
        let server = Server::new();
        let remote = server.remote();
        let tls = server.connector();
        let worker = mock(server, false, false);
        let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
        assert!(
            copy_sent_session(&mut session, &remote, MESSAGE, false)
                .unwrap_err()
                .uncertain
        );
        session.logout().unwrap();
        assert!(
            !worker
                .join()
                .unwrap()
                .iter()
                .any(|command| command.starts_with("APPEND"))
        );
    }

    #[test]
    fn dropped_append_confirmation_is_uncertain() {
        let server = Server::new();
        let remote = server.remote();
        let tls = server.connector();
        let worker = mock(server, false, true);
        let mut session = connect_with_tls(&remote, "fake-password", &tls).unwrap();
        assert!(
            copy_sent_session(&mut session, &remote, MESSAGE, true)
                .unwrap_err()
                .uncertain
        );
        worker.join().unwrap();
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
        let server = Server::new();
        let remote = server.remote();
        let tls = server.connector();
        let worker = server.spawn(|stream| {
            let mut stream = BufReader::new(stream.unwrap());
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
}
