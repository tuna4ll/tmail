//! An in-memory IMAP server that speaks just enough of RFC 3501 for sync tests.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    sync::{Arc, Mutex},
};

use crate::remote::tests::Server;

#[derive(Clone, Default)]
pub struct Message {
    pub uid: u32,
    pub flags: Vec<String>,
    pub body: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct Folder {
    pub attributes: String,
    pub validity: u32,
    pub next_uid: u32,
    pub messages: Vec<Message>,
}

#[derive(Default)]
pub struct State {
    pub folders: BTreeMap<String, Folder>,
    pub capabilities: String,
    pub commands: Vec<String>,
}

pub type Shared = Arc<Mutex<State>>;

impl State {
    pub fn new(folders: &[(&str, &str)]) -> Shared {
        let mut state = Self {
            capabilities: "IMAP4rev1 MOVE UIDPLUS IDLE".into(),
            ..Self::default()
        };
        for (name, attributes) in folders {
            state.add_folder(name, attributes);
        }
        Arc::new(Mutex::new(state))
    }

    pub fn add_folder(&mut self, name: &str, attributes: &str) {
        self.folders.insert(
            name.into(),
            Folder {
                attributes: attributes.into(),
                validity: 7,
                next_uid: 1,
                messages: Vec::new(),
            },
        );
    }

    pub fn deliver(&mut self, folder: &str, body: &[u8], flags: &[&str]) -> u32 {
        let folder = self.folders.get_mut(folder).unwrap();
        let uid = folder.next_uid;
        folder.next_uid += 1;
        folder.messages.push(Message {
            uid,
            flags: flags.iter().map(|flag| (*flag).into()).collect(),
            body: body.to_vec(),
        });
        uid
    }

    pub fn message(&self, folder: &str, uid: u32) -> Option<&Message> {
        self.folders[folder]
            .messages
            .iter()
            .find(|message| message.uid == uid)
    }
}

pub fn mail(id: &str, subject: &str) -> Vec<u8> {
    format!(
        "From: a@example.test\r\nTo: b@example.test\r\nMessage-ID: <{id}>\r\nSubject: {subject}\r\n\r\nbody of {subject}\r\n"
    )
    .into_bytes()
}

/// Serves connections until the test process ends.
pub fn serve(server: Server, state: Shared) {
    let (listener, acceptor) = server.into_parts();
    serve_with(listener, acceptor, state);
}

pub fn serve_with(
    listener: std::net::TcpListener,
    acceptor: native_tls::TlsAcceptor,
    state: Shared,
) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let state = state.clone();
            let acceptor = acceptor.clone();
            std::thread::spawn(move || {
                if let Ok(stream) = acceptor.accept(stream) {
                    let _ = session(BufReader::new(stream), &state);
                }
            });
        }
    });
}

fn tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(&character) = chars.peek() {
        if character == ' ' {
            chars.next();
        } else if character == '"' {
            chars.next();
            let mut token = String::new();
            while let Some(character) = chars.next() {
                match character {
                    '\\' => token.extend(chars.next()),
                    '"' => break,
                    _ => token.push(character),
                }
            }
            tokens.push(token);
        } else if character == '(' {
            let mut depth = 0;
            let mut token = String::new();
            for character in chars.by_ref() {
                token.push(character);
                depth += i32::from(character == '(') - i32::from(character == ')');
                if depth == 0 {
                    break;
                }
            }
            tokens.push(token);
        } else {
            let mut token = String::new();
            while let Some(&character) = chars.peek() {
                if character == ' ' {
                    break;
                }
                token.push(character);
                chars.next();
            }
            tokens.push(token);
        }
    }
    tokens
}

fn in_set(set: &str, uid: u32, max: u32) -> bool {
    set.split(',').any(|part| match part.split_once(':') {
        Some((start, end)) => {
            let start: u32 = start.parse().unwrap_or(1);
            let end = if end == "*" {
                max
            } else {
                end.parse().unwrap_or(0)
            };
            (start.min(end)..=start.max(end)).contains(&uid)
        }
        None => part.parse() == Ok(uid),
    })
}

fn flag_list(value: &str) -> Vec<String> {
    value
        .trim_matches(['(', ')'])
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn session<S: Read + Write>(mut stream: BufReader<S>, state: &Shared) -> std::io::Result<()> {
    stream.get_mut().write_all(b"* OK fake imap ready\r\n")?;
    let mut selected: Option<String> = None;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end().to_owned();
        let Some((tag, command)) = line.split_once(' ') else {
            continue;
        };
        let mut state = state.lock().unwrap();
        state.commands.push(command.to_owned());
        let words = tokens(command);
        let verb = words[0].to_uppercase();
        let (verb, args) = if verb == "UID" {
            (format!("UID {}", words[1].to_uppercase()), &words[2..])
        } else {
            (verb, &words[1..])
        };
        let mut out = Vec::new();
        let mut status = "OK done".to_owned();
        match verb.as_str() {
            "LOGIN" => {}
            "CAPABILITY" => {
                out.extend(format!("* CAPABILITY {}\r\n", state.capabilities).bytes());
            }
            "LIST" => {
                for (name, folder) in &state.folders {
                    out.extend(
                        format!("* LIST ({}) \".\" \"{name}\"\r\n", folder.attributes).bytes(),
                    );
                }
            }
            "SELECT" | "EXAMINE" => match state.folders.get(&args[0]) {
                Some(folder) => {
                    out.extend(
                        format!(
                            "* {} EXISTS\r\n* OK [UIDVALIDITY {}] ok\r\n",
                            folder.messages.len(),
                            folder.validity
                        )
                        .bytes(),
                    );
                    selected = Some(args[0].clone());
                }
                None => status = "NO no such folder".into(),
            },
            "CREATE" => {
                let name = args[0].clone();
                state.add_folder(&name, "");
            }
            "SUBSCRIBE" => {}
            "APPEND" => {
                let name = args[0].clone();
                let flags = args
                    .iter()
                    .find(|arg| arg.starts_with('('))
                    .map(|value| flag_list(value))
                    .unwrap_or_default();
                let length: usize = args
                    .last()
                    .unwrap()
                    .trim_matches(['{', '}'])
                    .parse()
                    .unwrap();
                stream.get_mut().write_all(b"+ go\r\n")?;
                let mut body = vec![0; length];
                stream.read_exact(&mut body)?;
                let mut ending = String::new();
                stream.read_line(&mut ending)?;
                let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
                state.deliver(&name, &body, &flags);
            }
            "UID SEARCH" => {
                let folder = &state.folders[selected.as_ref().unwrap()];
                let header = args
                    .iter()
                    .position(|arg| arg.eq_ignore_ascii_case("HEADER"))
                    .map(|index| args[index + 2].to_lowercase());
                let uids: Vec<String> = folder
                    .messages
                    .iter()
                    .filter(|message| {
                        header.as_ref().is_none_or(|value| {
                            String::from_utf8_lossy(&message.body)
                                .to_lowercase()
                                .replace("\r\n ", " ")
                                .replace("\r\n\t", " ")
                                .replace("\n ", " ")
                                .replace("message-id:", "message-id: ")
                                .replace("message-id:  ", "message-id: ")
                                .contains(&format!("message-id: {value}"))
                        })
                    })
                    .map(|message| message.uid.to_string())
                    .collect();
                out.extend(
                    format!("* SEARCH {}\r\n", uids.join(" "))
                        .trim_end()
                        .bytes(),
                );
                out.extend(b"\r\n");
            }
            "UID FETCH" => {
                let folder = &state.folders[selected.as_ref().unwrap()];
                let max = folder.messages.iter().map(|m| m.uid).max().unwrap_or(0);
                let items = args[1].to_uppercase();
                for (index, message) in folder.messages.iter().enumerate() {
                    if !in_set(&args[0], message.uid, max) {
                        continue;
                    }
                    let mut response =
                        format!("* {} FETCH (UID {}", index + 1, message.uid).into_bytes();
                    if items.contains("FLAGS") {
                        response.extend(format!(" FLAGS ({})", message.flags.join(" ")).bytes());
                    }
                    let section = if items.contains("BODY.PEEK[HEADER]") {
                        let end = message
                            .body
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|position| position + 4)
                            .unwrap_or(message.body.len());
                        Some(("HEADER", &message.body[..end]))
                    } else if items.contains("BODY.PEEK[]") {
                        Some(("", &message.body[..]))
                    } else {
                        None
                    };
                    if let Some((name, bytes)) = section {
                        response.extend(format!(" BODY[{name}] {{{}}}\r\n", bytes.len()).bytes());
                        response.extend(bytes);
                    }
                    response.extend(b")\r\n");
                    out.extend(response);
                }
            }
            "UID STORE" => {
                let name = selected.clone().unwrap();
                let folder = state.folders.get_mut(&name).unwrap();
                let max = folder.messages.iter().map(|m| m.uid).max().unwrap_or(0);
                let flags = flag_list(&args[2]);
                for message in &mut folder.messages {
                    if !in_set(&args[0], message.uid, max) {
                        continue;
                    }
                    if args[1].starts_with('+') {
                        for flag in &flags {
                            if !message.flags.contains(flag) {
                                message.flags.push(flag.clone());
                            }
                        }
                    } else {
                        message.flags.retain(|flag| !flags.contains(flag));
                    }
                }
            }
            "UID MOVE" | "UID COPY" => {
                let name = selected.clone().unwrap();
                let max = state.folders[&name]
                    .messages
                    .iter()
                    .map(|m| m.uid)
                    .max()
                    .unwrap_or(0);
                let moving: Vec<Message> = state.folders[&name]
                    .messages
                    .iter()
                    .filter(|message| in_set(&args[0], message.uid, max))
                    .cloned()
                    .collect();
                for message in &moving {
                    let flags: Vec<&str> = message.flags.iter().map(String::as_str).collect();
                    state.deliver(&args[1], &message.body, &flags);
                }
                if verb == "UID MOVE" {
                    state
                        .folders
                        .get_mut(&name)
                        .unwrap()
                        .messages
                        .retain(|message| !in_set(&args[0], message.uid, max));
                }
            }
            "UID EXPUNGE" | "EXPUNGE" => {
                let name = selected.clone().unwrap();
                let folder = state.folders.get_mut(&name).unwrap();
                let max = folder.messages.iter().map(|m| m.uid).max().unwrap_or(0);
                folder.messages.retain(|message| {
                    !message.flags.iter().any(|flag| flag == "\\Deleted")
                        || (verb == "UID EXPUNGE" && !in_set(&args[0], message.uid, max))
                });
            }
            "IDLE" => {
                drop(state);
                stream.get_mut().write_all(b"+ idling\r\n")?;
                stream.get_mut().write_all(b"* 1 EXISTS\r\n")?;
                let mut done = String::new();
                stream.read_line(&mut done)?;
                stream
                    .get_mut()
                    .write_all(format!("{tag} OK idle done\r\n").as_bytes())?;
                continue;
            }
            "LOGOUT" => {
                stream
                    .get_mut()
                    .write_all(format!("* BYE bye\r\n{tag} OK logout\r\n").as_bytes())?;
                return Ok(());
            }
            _ => status = "BAD unknown".into(),
        }
        out.extend(format!("{tag} {status}\r\n").bytes());
        stream.get_mut().write_all(&out)?;
    }
}
