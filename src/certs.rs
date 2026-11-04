//! Reads a server's certificate so the user can decide to trust it, for self-hosted
//! servers and local bridges that use their own certificates.
//!
//! Fetching skips verification on purpose: nothing but the handshake happens on this
//! connection, no credentials are sent, and the user compares the fingerprint before
//! anything is saved.

use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    Imap,
    Smtp,
}

/// A certificate as the server presented it.
#[derive(Clone, Debug)]
pub struct Presented {
    pub host: String,
    pub pem: String,
    /// SHA-256 of the DER bytes, as colon-separated hex.
    pub fingerprint: String,
}

pub fn fetch(
    host: &str,
    port: u16,
    protocol: Protocol,
    starttls: bool,
) -> Result<Presented, String> {
    let timeout = Duration::from_secs(10);
    // Like the mail connection itself, try every address: "localhost" often resolves
    // to IPv6 first while a local bridge listens on IPv4 only.
    let mut stream = (host, port)
        .to_socket_addrs()
        .map_err(|_| format!("Unable to resolve {host}."))?
        .find_map(|address| TcpStream::connect_timeout(&address, timeout).ok())
        .ok_or_else(|| format!("Unable to connect to {host}:{port}."))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    if starttls {
        match protocol {
            Protocol::Imap => {
                expect_line(&mut stream, "* ")?;
                stream
                    .write_all(b"t0 STARTTLS\r\n")
                    .map_err(|e| e.to_string())?;
                expect_tagged(&mut stream, "t0 OK")?;
            }
            Protocol::Smtp => {
                expect_line(&mut stream, "220")?;
                stream
                    .write_all(b"EHLO tmail\r\n")
                    .map_err(|e| e.to_string())?;
                // EHLO answers with several 250- lines and a final "250 ".
                loop {
                    let line = read_line(&mut stream)?;
                    if line.starts_with("250 ") {
                        break;
                    }
                    if !line.starts_with("250-") {
                        return Err(format!("{host} did not accept EHLO."));
                    }
                }
                stream
                    .write_all(b"STARTTLS\r\n")
                    .map_err(|e| e.to_string())?;
                expect_line(&mut stream, "220")?;
            }
        }
    }
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|error| error.to_string())?;
    let tls = connector
        .connect(host, stream)
        .map_err(|error| format!("The TLS handshake with {host} failed: {error}"))?;
    let certificate = tls
        .peer_certificate()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{host} sent no certificate."))?;
    let der = certificate.to_der().map_err(|error| error.to_string())?;
    Ok(Presented {
        host: host.to_owned(),
        pem: pem(&der),
        fingerprint: fingerprint(&der),
    })
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn pem(der: &[u8]) -> String {
    let encoded = crate::widgets::rich::base64(der);
    let mut text = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        text.push_str(&String::from_utf8_lossy(chunk));
        text.push('\n');
    }
    text.push_str("-----END CERTIFICATE-----\n");
    text
}

fn read_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while bytes.len() < 4096 {
        stream
            .read_exact(&mut byte)
            .map_err(|_| "The server closed the connection.".to_owned())?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\n") {
            return Ok(String::from_utf8_lossy(&bytes).trim_end().to_owned());
        }
    }
    Err("The server sent a line that is too long.".into())
}

fn expect_line(stream: &mut TcpStream, prefix: &str) -> Result<(), String> {
    let line = read_line(stream)?;
    if line.starts_with(prefix) {
        Ok(())
    } else {
        Err(format!(
            "Unexpected reply: {}",
            crate::imap_client::short(&line)
        ))
    }
}

fn expect_tagged(stream: &mut TcpStream, prefix: &str) -> Result<(), String> {
    loop {
        let line = read_line(stream)?;
        if line.starts_with(prefix) {
            return Ok(());
        }
        if line.starts_with("t0 ") {
            return Err("The server refused STARTTLS.".into());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fingerprints_match_sha256() {
        assert_eq!(
            super::fingerprint(b"abc"),
            "BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
        );
        let pem = super::pem(&[7; 100]);
        assert!(
            pem.starts_with("-----BEGIN CERTIFICATE-----\n")
                && pem.lines().all(|line| line.len() <= 64)
        );
    }
}
