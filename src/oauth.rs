//! `termail oauth`: signs in through the provider's web page and stores the refresh
//! token in the system keyring. Uses PKCE and a one-shot loopback redirect (RFC 8252).

use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

use crate::{config::Config, remote::RemoteConfig};

pub fn run(config: &Config, account: Option<&str>) -> Result<(), String> {
    let remote = pick(config, account)?;
    let oauth = remote
        .oauth2
        .as_ref()
        .ok_or("This account has no [oauth2] settings.")?;
    let auth_url = oauth.auth_url();
    if auth_url.is_empty() {
        return Err("Set oauth2 provider (gmail, outlook) or auth_url.".into());
    }
    let verifier = random_token(64);
    let challenge = base64_url(&sha256(verifier.as_bytes()));
    let state = random_token(24);
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let redirect = format!(
        "http://127.0.0.1:{}/",
        listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port()
    );
    let mut query = vec![
        ("response_type", "code".to_owned()),
        ("client_id", oauth.client_id.clone()),
        ("redirect_uri", redirect.clone()),
        ("scope", oauth.scope()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256".into()),
        ("state", state.clone()),
        ("login_hint", remote.username.clone()),
    ];
    if auth_url.contains("google.com") {
        // Google only returns a refresh token for offline access with consent.
        query.push(("access_type", "offline".into()));
        query.push(("prompt", "consent".into()));
    }
    let url = format!(
        "{auth_url}{}{}",
        if auth_url.contains('?') { '&' } else { '?' },
        encode_query(&query)
    );
    println!("Opening the sign-in page for {}.", remote.username);
    println!("If no browser opens, visit:\n\n{url}\n");
    let _ = opener::open(&url);
    let code = wait_for_code(&listener, &state)?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", oauth.client_id.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    if !oauth.client_secret.is_empty() {
        form.push(("client_secret", oauth.client_secret.as_str()));
    }
    let response = crate::remote::token_request(&oauth.token_url(), &form)?;
    let refresh = response["refresh_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or("The provider returned no refresh token. Check the scope and client type.")?;
    remote.store_keyring_password(refresh)?;
    remote.forget_cached();
    println!("Signed in. The refresh token is in the system keyring.");
    if !remote.keyring {
        println!("Add `keyring = true` to this account's [remote] table so Termail reads it.");
    }
    Ok(())
}

fn pick(config: &Config, account: Option<&str>) -> Result<RemoteConfig, String> {
    if let Some(id) = account {
        return config
            .accounts
            .iter()
            .find(|candidate| candidate.id == id)
            .and_then(|candidate| candidate.remote.clone())
            .ok_or_else(|| format!("No account {id} with a [remote] table."));
    }
    config
        .remote
        .clone()
        .filter(|remote| remote.oauth2.is_some())
        .or_else(|| {
            config
                .accounts
                .iter()
                .filter_map(|candidate| candidate.remote.clone())
                .find(|remote| remote.oauth2.is_some())
        })
        .ok_or_else(|| "No account has [oauth2] settings.".into())
}

/// Waits up to five minutes for the browser to come back with the code.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    loop {
        if started.elapsed() > Duration::from_secs(300) {
            return Err("Sign-in timed out.".into());
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..count]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        let parameters = parse_query(target.split_once('?').map_or("", |(_, query)| query));
        let value = |name: &str| {
            parameters
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let (status, message, result) = if let Some(error) = value("error") {
            (
                "400 Bad Request",
                format!("Sign-in failed: {error}"),
                Some(Err(error)),
            )
        } else if value("state").as_deref() != Some(state) {
            ("400 Bad Request", "Unexpected request.".to_owned(), None)
        } else if let Some(code) = value("code") {
            (
                "200 OK",
                "Signed in to termail. You can close this tab.".to_owned(),
                Some(Ok(code)),
            )
        } else {
            ("404 Not Found", "Waiting for sign-in.".to_owned(), None)
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>termail</title><p style=\"font:16px system-ui;margin:3em\">{message}</p>"
        );
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        if let Some(result) = result {
            return result.map_err(|error| format!("The provider refused sign-in: {error}"));
        }
    }
}

fn encode_query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.replace('+', " ").into_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = std::str::from_utf8(
                &bytes[(index + 1).min(bytes.len())..(index + 3).min(bytes.len())],
            )
            .ok()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            output.push(byte);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).to_vec()
}

fn base64_url(bytes: &[u8]) -> String {
    crate::widgets::rich::base64(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// URL-safe random text from the operating system's generator.
fn random_token(length: usize) -> String {
    let mut bytes = vec![0u8; length];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        // Windows has no /dev/urandom; the hasher's keys come from the system RNG.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u128(Instant::now().elapsed().as_nanos());
            chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    base64_url(&bytes)[..length].to_owned()
}
