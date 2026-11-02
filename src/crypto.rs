//! OpenPGP (RFC 3156) and S/MIME (RFC 8551) through the user's `gpg` and `gpgsm`.
//!
//! Signing and decrypting may ask for a passphrase, so they run while the terminal
//! is handed to the program. Decrypted content is only kept in memory.

use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

use mail_parser::MimeHeaders;
use serde::Deserialize;

/// The `[crypto]` table.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CryptoConfig {
    /// The OpenPGP program; `gpg` when unset.
    pub gpg: Option<String>,
    /// The S/MIME program; `gpgsm` when unset.
    pub gpgsm: Option<String>,
    /// The key to sign with; the sender address when unset.
    pub key: Option<String>,
    /// New messages start signed.
    pub sign: bool,
    /// New messages start encrypted.
    pub encrypt: bool,
    /// New messages use S/MIME instead of OpenPGP.
    pub smime: bool,
    /// A GnuPG home folder other than the default, passed as `--homedir`.
    pub home: Option<PathBuf>,
}

impl CryptoConfig {
    fn program(&self, standard: Standard) -> String {
        match standard {
            Standard::OpenPgp => self.gpg.clone().unwrap_or_else(|| "gpg".into()),
            Standard::Smime => self.gpgsm.clone().unwrap_or_else(|| "gpgsm".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Standard {
    #[default]
    OpenPgp,
    Smime,
}

/// What a composed message gets before it is queued.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Protection {
    pub sign: bool,
    pub encrypt: bool,
    pub standard: Standard,
}

impl Protection {
    pub fn from_config(config: &CryptoConfig) -> Self {
        Self {
            sign: config.sign,
            encrypt: config.encrypt,
            standard: if config.smime {
                Standard::Smime
            } else {
                Standard::OpenPgp
            },
        }
    }

    pub fn is_active(self) -> bool {
        self.sign || self.encrypt
    }

    pub fn label(self) -> String {
        let kind = match self.standard {
            Standard::OpenPgp => "OpenPGP",
            Standard::Smime => "S/MIME",
        };
        match (self.sign, self.encrypt) {
            (false, false) => "None".into(),
            (true, false) => format!("Sign · {kind}"),
            (false, true) => format!("Encrypt · {kind}"),
            (true, true) => format!("Sign and encrypt · {kind}"),
        }
    }

    /// Cycles none → sign → encrypt → both, then switches the standard.
    pub fn next(&mut self) {
        (self.sign, self.encrypt) = match (self.sign, self.encrypt) {
            (false, false) => (true, false),
            (true, false) => (false, true),
            (false, true) => (true, true),
            (true, true) => {
                self.standard = match self.standard {
                    Standard::OpenPgp => Standard::Smime,
                    Standard::Smime => Standard::OpenPgp,
                };
                (false, false)
            }
        };
    }

    pub fn previous(&mut self) {
        for _ in 0..7 {
            self.next();
        }
    }
}

/// How a received message is protected, read from its top-level headers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, serde::Serialize)]
pub enum Kind {
    PgpEncrypted,
    PgpSigned,
    SmimeEncrypted,
    SmimeSigned,
    /// An armored block inside a plain-text body.
    InlinePgp,
}

impl Kind {
    pub fn detect(message: &mail_parser::Message<'_>) -> Option<Self> {
        let kind = message.content_type()?;
        let ctype = kind.c_type.to_ascii_lowercase();
        let subtype = kind
            .c_subtype
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let protocol = kind
            .attribute("protocol")
            .unwrap_or_default()
            .to_ascii_lowercase();
        match (ctype.as_str(), subtype.as_str()) {
            ("multipart", "encrypted") if protocol == "application/pgp-encrypted" => {
                Some(Self::PgpEncrypted)
            }
            ("multipart", "signed") if protocol == "application/pgp-signature" => {
                Some(Self::PgpSigned)
            }
            ("multipart", "signed") if protocol.ends_with("pkcs7-signature") => {
                Some(Self::SmimeSigned)
            }
            ("application", "pkcs7-mime" | "x-pkcs7-mime") => Some(Self::SmimeEncrypted),
            _ => {
                let text = message.body_text(0)?;
                (text.contains("-----BEGIN PGP MESSAGE-----")
                    || text.contains("-----BEGIN PGP SIGNED MESSAGE-----"))
                .then_some(Self::InlinePgp)
            }
        }
    }

    /// Opening needs a secret key, which may ask for a passphrase.
    pub fn needs_secret(self) -> bool {
        matches!(
            self,
            Self::PgpEncrypted | Self::SmimeEncrypted | Self::InlinePgp
        )
    }
}

/// A message after decryption and verification.
pub struct Opened {
    /// A whole message with the original headers and the readable content.
    pub content: Vec<u8>,
    /// One line for the reader, such as `encrypted · signed by Ada <ada@x>`.
    pub status: String,
}

/// Decrypts and verifies `contents`.
pub fn open(contents: &[u8], kind: Kind, config: &CryptoConfig) -> Result<Opened, String> {
    let (headers, body) = split(contents);
    match kind {
        Kind::PgpEncrypted | Kind::SmimeEncrypted => {
            let standard = if kind == Kind::PgpEncrypted {
                Standard::OpenPgp
            } else {
                Standard::Smime
            };
            let input = if standard == Standard::OpenPgp {
                let boundary = boundary(headers).ok_or("The encrypted message has no parts.")?;
                let parts = parts(body, &boundary);
                let encrypted = parts.get(1).ok_or("The encrypted part is missing.")?;
                split(encrypted).1.to_vec()
            } else {
                decode_base64(body).ok_or("The S/MIME content is not valid base64.")?
            };
            let (output, status) = run(config, standard, &["--decrypt"], &input, &[])?;
            let mut report = Report::read(&status);
            if !report.decrypted {
                return Err(report.failure("Unable to decrypt this message."));
            }
            let mut inner = output;
            let inner_kind = mail_parser::MessageParser::default()
                .parse(&inner)
                .and_then(|message| Kind::detect(&message))
                .filter(|kind| matches!(kind, Kind::PgpSigned | Kind::SmimeSigned));
            if let Some(inner_kind) = inner_kind {
                let verified = open(&inner, inner_kind, config)?;
                report.signature = Some(verified.status);
                inner = split(&verified.content).1.to_vec();
                let mut joined = content_headers_of(&verified.content);
                joined.extend_from_slice(&inner);
                inner = joined;
            }
            Ok(Opened {
                content: join(headers, &inner),
                status: report.describe(true),
            })
        }
        Kind::PgpSigned | Kind::SmimeSigned => {
            let boundary = boundary(headers).ok_or("The signed message has no parts.")?;
            let parts = parts(body, &boundary);
            let (signed, signature) = match (parts.first(), parts.get(1)) {
                (Some(signed), Some(signature)) => (signed, signature),
                _ => return Err("The signature part is missing.".into()),
            };
            let standard = if kind == Kind::PgpSigned {
                Standard::OpenPgp
            } else {
                Standard::Smime
            };
            let (signature_headers, signature_body) = split(signature);
            let signature = if standard == Standard::Smime
                || header_value(signature_headers, "content-transfer-encoding")
                    .is_some_and(|value| value.eq_ignore_ascii_case("base64"))
            {
                decode_base64(signature_body).ok_or("The signature is not valid base64.")?
            } else {
                signature_body.to_vec()
            };
            let data = Temporary::new(&canonical(signed))?;
            let signature_file = Temporary::new(&signature)?;
            let (_, status) = run(
                config,
                standard,
                &["--verify"],
                &[],
                &[signature_file.path(), data.path()],
            )?;
            Ok(Opened {
                content: join(headers, signed),
                status: Report::read(&status).describe(false),
            })
        }
        Kind::InlinePgp => {
            let message = mail_parser::MessageParser::default()
                .parse(contents)
                .ok_or("Unable to read the message.")?;
            let text = message.body_text(0).ok_or("The message has no text.")?;
            let start = [
                "-----BEGIN PGP MESSAGE-----",
                "-----BEGIN PGP SIGNED MESSAGE-----",
            ]
            .iter()
            .filter_map(|marker| text.find(marker))
            .min()
            .ok_or("The message has no OpenPGP block.")?;
            let end = ["-----END PGP MESSAGE-----", "-----END PGP SIGNATURE-----"]
                .iter()
                .filter_map(|marker| {
                    text[start..]
                        .find(marker)
                        .map(|at| start + at + marker.len())
                })
                .max()
                .ok_or("The OpenPGP block is incomplete.")?;
            let (output, status) = run(
                config,
                Standard::OpenPgp,
                &["--decrypt"],
                text[start..end].as_bytes(),
                &[],
            )?;
            let report = Report::read(&status);
            if !report.decrypted && report.signature.is_none() {
                return Err(report.failure("Unable to read the OpenPGP block."));
            }
            let mut plain = text[..start].to_owned();
            plain.push_str(&String::from_utf8_lossy(&output));
            plain.push_str(&text[end..]);
            let mut content = strip_content_headers(headers);
            content.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n\r\n");
            content.extend_from_slice(plain.as_bytes());
            Ok(Opened {
                content,
                status: report.describe(report.decrypted),
            })
        }
    }
}

/// Signs and/or encrypts a composed message. `recipients` are every address the
/// message goes to; the sender is added so the Sent copy stays readable.
pub fn protect(
    contents: &[u8],
    protection: Protection,
    config: &CryptoConfig,
) -> Result<Vec<u8>, String> {
    if !protection.is_active() {
        return Ok(contents.to_vec());
    }
    let envelope = crate::message::envelope(contents)?;
    let (headers, body) = split(contents);
    let mut entity = content_headers_of(contents);
    entity.extend_from_slice(body);
    let entity = canonical(&entity);
    let outer = strip_content_headers(headers);
    let signer = config.key.clone().unwrap_or_else(|| envelope.from.clone());
    let boundary = format!("termail-{}", crate::storage::unique_name("part"));
    let standard = protection.standard;
    let mut recipients = envelope.recipients.clone();
    if !recipients.contains(&envelope.from) {
        recipients.push(envelope.from.clone());
    }
    let recipient_args: Vec<String> = recipients
        .iter()
        .flat_map(|address| ["--recipient".to_owned(), address.clone()])
        .collect();

    let mut message = outer;
    message.extend_from_slice(b"MIME-Version: 1.0\r\n");
    match (standard, protection.encrypt) {
        (Standard::OpenPgp, false) => {
            let signature = sign(config, standard, &signer, &entity)?;
            message.extend_from_slice(format!(
                "Content-Type: multipart/signed; micalg=pgp-sha256;\r\n protocol=\"application/pgp-signature\"; boundary=\"{boundary}\"\r\n\r\nThis is an OpenPGP/MIME signed message (RFC 4880 and 3156)\r\n--{boundary}\r\n"
            ).as_bytes());
            message.extend_from_slice(&entity);
            message.extend_from_slice(format!(
                "\r\n--{boundary}\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\nContent-Description: OpenPGP digital signature\r\nContent-Disposition: attachment; filename=\"signature.asc\"\r\n\r\n"
            ).as_bytes());
            message.extend_from_slice(&canonical(&signature));
            message.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        }
        (Standard::OpenPgp, true) => {
            let mut arguments = vec!["--armor".to_owned(), "--encrypt".to_owned()];
            if protection.sign {
                arguments.extend([
                    "--sign".to_owned(),
                    "--local-user".to_owned(),
                    signer.clone(),
                ]);
            }
            arguments.extend(recipient_args);
            let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
            let (encrypted, status) = run(config, standard, &arguments, &entity, &[])?;
            if encrypted.is_empty() {
                return Err(Report::read(&status).failure("Unable to encrypt the message."));
            }
            message.extend_from_slice(format!(
                "Content-Type: multipart/encrypted;\r\n protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\"\r\n\r\nThis is an OpenPGP/MIME encrypted message (RFC 4880 and 3156)\r\n--{boundary}\r\nContent-Type: application/pgp-encrypted\r\nContent-Description: PGP/MIME version identification\r\n\r\nVersion: 1\r\n\r\n--{boundary}\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Description: OpenPGP encrypted message\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n"
            ).as_bytes());
            message.extend_from_slice(&canonical(&encrypted));
            message.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        }
        (Standard::Smime, encrypt) => {
            let mut inner = entity.clone();
            if protection.sign {
                let signature = sign(config, standard, &signer, &entity)?;
                let signed_boundary = format!("{boundary}-signed");
                inner = format!(
                    "Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\";\r\n micalg=sha-256; boundary=\"{signed_boundary}\"\r\n\r\n--{signed_boundary}\r\n"
                )
                .into_bytes();
                inner.extend_from_slice(&entity);
                inner.extend_from_slice(format!(
                    "\r\n--{signed_boundary}\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7s\"\r\n\r\n{}\r\n--{signed_boundary}--\r\n",
                    wrap_base64(&signature)
                ).as_bytes());
            }
            if encrypt {
                let mut arguments = vec!["--encrypt".to_owned()];
                arguments.extend(recipient_args);
                let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
                let (encrypted, status) = run(config, standard, &arguments, &inner, &[])?;
                if encrypted.is_empty() {
                    return Err(Report::read(&status).failure("Unable to encrypt the message."));
                }
                message.extend_from_slice(format!(
                    "Content-Type: application/pkcs7-mime; smime-type=enveloped-data;\r\n name=\"smime.p7m\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7m\"\r\n\r\n{}\r\n",
                    wrap_base64(&encrypted)
                ).as_bytes());
            } else {
                message.extend_from_slice(&inner);
            }
        }
    }
    Ok(message)
}

fn sign(
    config: &CryptoConfig,
    standard: Standard,
    signer: &str,
    entity: &[u8],
) -> Result<Vec<u8>, String> {
    let mut arguments = vec!["--detach-sign", "--local-user", signer];
    if standard == Standard::OpenPgp {
        arguments.extend(["--armor", "--digest-algo", "SHA256"]);
    }
    let (signature, status) = run(config, standard, &arguments, entity, &[])?;
    if signature.is_empty() {
        return Err(Report::read(&status).failure("Unable to sign the message."));
    }
    Ok(signature)
}

/// Runs the program with machine-readable status on stderr. Returns stdout and the
/// status lines.
fn run(
    config: &CryptoConfig,
    standard: Standard,
    arguments: &[&str],
    input: &[u8],
    files: &[&std::path::Path],
) -> Result<(Vec<u8>, String), String> {
    let program = config.program(standard);
    let mut command = Command::new(&program);
    if let Some(home) = &config.home {
        command.arg("--homedir").arg(home);
    }
    command
        .args(["--status-fd", "2", "--no-tty"])
        .args(arguments)
        .args(files)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(tty) = terminal_name() {
        command.env("GPG_TTY", tty);
    }
    let mut child = command
        .spawn()
        .map_err(|_| format!("Unable to run {program}. Install it or set [crypto] in config."))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Unable to write to the program.")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child
        .wait_with_output()
        .map_err(|error| format!("{program} stopped: {error}"))?;
    let _ = writer.join();
    let status = String::from_utf8_lossy(&output.stderr).into_owned();
    crate::log::write(
        "crypto",
        format_args!(
            "{program} {} → {}",
            arguments.first().unwrap_or(&""),
            output.status
        ),
    );
    Ok((output.stdout, status))
}

/// The terminal device, so a text-mode pinentry can ask for the passphrase.
fn terminal_name() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("GPG_TTY") {
        return Some(PathBuf::from(value));
    }
    std::fs::read_link("/proc/self/fd/0")
        .ok()
        .filter(|path| path.starts_with("/dev/"))
}

/// The parts of `--status-fd` output a reader cares about.
#[derive(Default)]
struct Report {
    decrypted: bool,
    signature: Option<String>,
    problem: Option<String>,
}

impl Report {
    fn read(status: &str) -> Self {
        let mut report = Self::default();
        let mut trust = None;
        for line in status.lines() {
            let Some(rest) = line.strip_prefix("[GNUPG:] ") else {
                continue;
            };
            let mut words = rest.splitn(3, ' ');
            let keyword = words.next().unwrap_or_default();
            let key = words.next().unwrap_or_default();
            let user = words.next().unwrap_or(key);
            match keyword {
                "DECRYPTION_OKAY" => report.decrypted = true,
                "GOODSIG" => report.signature = Some(format!("signed by {user}")),
                "EXPKEYSIG" => report.signature = Some(format!("signed by {user} · key expired")),
                "REVKEYSIG" => report.signature = Some(format!("signed by {user} · key revoked")),
                "BADSIG" => report.signature = Some(format!("BAD SIGNATURE claiming {user}")),
                "ERRSIG" => {
                    report
                        .signature
                        .get_or_insert_with(|| format!("signed with unknown key {key}"));
                }
                "TRUST_UNDEFINED" | "TRUST_NEVER" => trust = Some("key not verified"),
                "NO_SECKEY" => report.problem = Some("no secret key for this message".into()),
                "DECRYPTION_FAILED" => {
                    report
                        .problem
                        .get_or_insert_with(|| "decryption failed".into());
                }
                "FAILURE" | "ERROR" => {
                    report.problem.get_or_insert_with(|| rest.to_owned());
                }
                _ => {}
            }
        }
        if let (Some(signature), Some(trust)) = (&mut report.signature, trust)
            && signature.starts_with("signed by")
        {
            signature.push_str(&format!(" · {trust}"));
        }
        report
    }

    fn describe(&self, encrypted: bool) -> String {
        match (encrypted, &self.signature) {
            (true, Some(signature)) => format!("encrypted · {signature}"),
            (true, None) => "encrypted · not signed".into(),
            (false, Some(signature)) => signature.clone(),
            (false, None) => self
                .problem
                .clone()
                .unwrap_or_else(|| "signature could not be checked".into()),
        }
    }

    fn failure(&self, fallback: &str) -> String {
        match &self.problem {
            Some(problem) => format!("{fallback} ({problem})"),
            None => fallback.into(),
        }
    }
}

/// A private temporary file that is removed when dropped.
struct Temporary(PathBuf);

impl Temporary {
    fn new(contents: &[u8]) -> Result<Self, String> {
        let path = std::env::temp_dir().join(crate::storage::unique_name("termail-crypto"));
        crate::storage::write_new(&path, contents).map_err(|error| error.to_string())?;
        Ok(Self(path))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Splits a message or part at the first blank line.
fn split(contents: &[u8]) -> (&[u8], &[u8]) {
    for (index, window) in contents.windows(2).enumerate() {
        if window == b"\n\n" {
            return (&contents[..index + 1], &contents[index + 2..]);
        }
        if window == b"\n\r" && contents.get(index + 2) == Some(&b'\n') {
            return (&contents[..index + 1], &contents[index + 3..]);
        }
    }
    (contents, &[])
}

/// Header lines with their continuations, as (name, whole field).
fn fields(headers: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut fields: Vec<(String, Vec<u8>)> = Vec::new();
    for line in headers.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b" ") || line.starts_with(b"\t") {
            if let Some((_, field)) = fields.last_mut() {
                field.extend_from_slice(line);
            }
            continue;
        }
        let name = line
            .split(|byte| *byte == b':')
            .next()
            .map(|name| String::from_utf8_lossy(name).trim().to_ascii_lowercase())
            .unwrap_or_default();
        fields.push((name, line.to_vec()));
    }
    fields
}

fn is_content_field(name: &str) -> bool {
    name.starts_with("content-") || name == "mime-version"
}

fn strip_content_headers(headers: &[u8]) -> Vec<u8> {
    fields(headers)
        .into_iter()
        .filter(|(name, _)| !is_content_field(name))
        .flat_map(|(_, field)| field)
        .collect()
}

/// The content headers of a message followed by the blank line.
fn content_headers_of(contents: &[u8]) -> Vec<u8> {
    let mut output: Vec<u8> = fields(split(contents).0)
        .into_iter()
        .filter(|(name, _)| name.starts_with("content-"))
        .flat_map(|(_, field)| field)
        .collect();
    if output.is_empty() {
        output.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
    }
    output.extend_from_slice(b"\r\n");
    output
}

fn join(headers: &[u8], entity: &[u8]) -> Vec<u8> {
    let mut content = strip_content_headers(headers);
    content.extend_from_slice(entity);
    content
}

fn header_value(headers: &[u8], name: &str) -> Option<String> {
    fields(headers).into_iter().find_map(|(field, line)| {
        (field == name).then(|| {
            let text = String::from_utf8_lossy(&line).replace(['\r', '\n'], "");
            text.split_once(':')
                .map(|(_, value)| value.trim().to_owned())
                .unwrap_or_default()
        })
    })
}

fn boundary(headers: &[u8]) -> Option<String> {
    let content_type = header_value(headers, "content-type")?;
    let lower = content_type.to_ascii_lowercase();
    let start = lower.find("boundary=")? + "boundary=".len();
    let rest = &content_type[start..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([';', ' ']).next()?,
    };
    (!value.is_empty()).then(|| value.to_owned())
}

/// The raw bytes of each part between `--boundary` lines, without the line break
/// before the next boundary (it belongs to the boundary, RFC 2046 5.1.1).
fn parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let marker = format!("--{boundary}");
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive(|byte| *byte == b'\n') {
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.starts_with(marker.as_bytes()) {
            let closing = trimmed[marker.len()..].starts_with(b"--");
            starts.push((offset, offset + line.len(), closing));
        }
        offset += line.len();
    }
    let mut parts = Vec::new();
    for pair in starts.windows(2) {
        let (_, begin, closing) = pair[0];
        if closing {
            break;
        }
        let mut end = pair[1].0;
        if body[..end].ends_with(b"\r\n") {
            end -= 2;
        } else if body[..end].ends_with(b"\n") {
            end -= 1;
        }
        parts.push(&body[begin..end.max(begin)]);
    }
    parts
}

/// Line endings as CRLF, the form signatures are computed over.
fn canonical(contents: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(contents.len() + contents.len() / 40);
    let mut previous = 0;
    for &byte in contents {
        if byte == b'\n' && previous != b'\r' {
            output.push(b'\r');
        }
        output.push(byte);
        previous = byte;
    }
    output
}

fn wrap_base64(bytes: &[u8]) -> String {
    let encoded = crate::widgets::rich::base64(bytes);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("\r\n")
}

pub fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for &byte in input {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway keyring with one OpenPGP key, or `None` when GnuPG is missing.
    fn keyring() -> Option<(CryptoConfig, PathBuf)> {
        let home = std::env::temp_dir().join(crate::storage::unique_name("termail-gnupg"));
        std::fs::create_dir_all(&home).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).ok()?;
        }
        let generated = Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args([
                "--batch",
                "--passphrase",
                "",
                "--quick-gen-key",
                "Ada <ada@example.test>",
                "ed25519",
                "sign",
                "never",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !generated.success() {
            return None;
        }
        // ed25519 cannot encrypt; add an encryption subkey.
        let fingerprint = String::from_utf8(
            Command::new("gpg")
                .arg("--homedir")
                .arg(&home)
                .args(["--with-colons", "--list-keys", "ada@example.test"])
                .output()
                .ok()?
                .stdout,
        )
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("fpr:")
                .map(|rest| rest.trim_matches(':').to_owned())
        })?;
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args([
                "--batch",
                "--passphrase",
                "",
                "--quick-add-key",
                &fingerprint,
                "cv25519",
                "encr",
                "never",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        Some((
            CryptoConfig {
                home: Some(home.clone()),
                ..CryptoConfig::default()
            },
            home,
        ))
    }

    fn message() -> Vec<u8> {
        b"From: Ada <ada@example.test>\r\nTo: ada@example.test\r\nSubject: hi\r\nMessage-ID: <1@example.test>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nsecret h\xc3\xa9llo\r\n".to_vec()
    }

    fn roundtrip(config: &CryptoConfig, sign: bool, encrypt: bool) -> (String, String) {
        let protection = Protection {
            sign,
            encrypt,
            standard: Standard::OpenPgp,
        };
        let protected = protect(&message(), protection, config).unwrap();
        let parsed = mail_parser::MessageParser::default()
            .parse(&protected)
            .unwrap();
        let kind = Kind::detect(&parsed).unwrap();
        let opened = open(&protected, kind, config).unwrap();
        let text = mail_parser::MessageParser::default()
            .parse(&opened.content)
            .unwrap()
            .body_text(0)
            .unwrap()
            .into_owned();
        (text, opened.status)
    }

    #[test]
    fn openpgp_signs_encrypts_and_catches_tampering() {
        let Some((config, home)) = keyring() else {
            eprintln!("gpg could not create a test key; skipping");
            return;
        };
        let (text, status) = roundtrip(&config, true, false);
        assert!(text.contains("secret héllo"));
        assert!(status.starts_with("signed by Ada"), "{status}");
        let (text, status) = roundtrip(&config, false, true);
        assert!(text.contains("secret héllo"));
        assert_eq!(status, "encrypted · not signed");
        let (_, status) = roundtrip(&config, true, true);
        assert!(status.starts_with("encrypted · signed by Ada"), "{status}");
        let signed = protect(
            &message(),
            Protection {
                sign: true,
                encrypt: false,
                standard: Standard::OpenPgp,
            },
            &config,
        )
        .unwrap();
        let tampered = String::from_utf8(signed)
            .unwrap()
            .replace("secret", "Secret");
        let opened = open(tampered.as_bytes(), Kind::PgpSigned, &config).unwrap();
        assert!(opened.status.contains("BAD"), "{}", opened.status);
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(&home)
            .args(["--kill", "gpg-agent"])
            .status();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn splits_parts_at_their_boundaries() {
        let body = b"preamble\r\n--b\r\nfirst\r\n--b\r\nsecond\r\n--b--\r\nepilogue";
        let parts = parts(body, "b");
        assert_eq!(parts, vec![&b"first"[..], &b"second"[..]]);
        assert_eq!(decode_base64(b"aGVs\r\nbG8="), Some(b"hello".to_vec()));
        assert_eq!(canonical(b"a\nb\r\nc"), b"a\r\nb\r\nc".to_vec());
    }
}
