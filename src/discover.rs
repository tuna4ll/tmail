//! Finds the IMAP and SMTP settings for an address, the way mail apps do: a list of
//! well-known providers first, then Thunderbird's provider database, the domain's own
//! autoconfig file, DNS service records, and the domain's mail servers.
//!
//! Lookups past the built-in list send the address's domain (never the address or a
//! password) to autoconfig.thunderbird.net, to the domain itself, and to Cloudflare's
//! DNS resolver.

use std::time::Duration;

use scraper::{Html, Selector};

use crate::remote::Security;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Where the settings came from, to show the user.
    pub source: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    /// The server wants the part before `@` as the username, not the whole address.
    pub local_username: bool,
    /// `gmail` or `outlook` when the provider supports OAuth2 sign-in.
    pub oauth: Option<&'static str>,
    /// Something the user must know to sign in, such as needing an app password.
    pub note: Option<&'static str>,
}

struct Preset {
    domains: &'static [&'static str],
    imap: (&'static str, u16, Security),
    smtp: (&'static str, u16, Security),
    oauth: Option<&'static str>,
    note: Option<&'static str>,
}

const APP_PASSWORD: &str =
    "This provider needs an app password: create one in its security settings.";

const PRESETS: &[Preset] = &[
    Preset {
        domains: &["gmail.com", "googlemail.com"],
        imap: ("imap.gmail.com", 993, Security::Tls),
        smtp: ("smtp.gmail.com", 465, Security::Tls),
        oauth: Some("gmail"),
        note: Some("Use OAuth2, or an app password if 2-step verification is on."),
    },
    Preset {
        domains: &[
            "outlook.com",
            "hotmail.com",
            "live.com",
            "msn.com",
            "outlook.de",
            "hotmail.de",
            "outlook.fr",
            "hotmail.fr",
            "hotmail.co.uk",
            "live.co.uk",
            "outlook.com.tr",
            "hotmail.com.tr",
        ],
        imap: ("outlook.office365.com", 993, Security::Tls),
        smtp: ("smtp.office365.com", 587, Security::Starttls),
        oauth: Some("outlook"),
        note: Some("Microsoft accounts sign in with OAuth2."),
    },
    Preset {
        domains: &["icloud.com", "me.com", "mac.com"],
        imap: ("imap.mail.me.com", 993, Security::Tls),
        smtp: ("smtp.mail.me.com", 587, Security::Starttls),
        oauth: None,
        note: Some(APP_PASSWORD),
    },
    Preset {
        domains: &[
            "yahoo.com",
            "ymail.com",
            "rocketmail.com",
            "yahoo.co.uk",
            "yahoo.de",
            "yahoo.fr",
        ],
        imap: ("imap.mail.yahoo.com", 993, Security::Tls),
        smtp: ("smtp.mail.yahoo.com", 465, Security::Tls),
        oauth: None,
        note: Some(APP_PASSWORD),
    },
    Preset {
        domains: &["aol.com"],
        imap: ("imap.aol.com", 993, Security::Tls),
        smtp: ("smtp.aol.com", 465, Security::Tls),
        oauth: None,
        note: Some(APP_PASSWORD),
    },
    Preset {
        domains: &["fastmail.com", "fastmail.fm", "fastmail.net"],
        imap: ("imap.fastmail.com", 993, Security::Tls),
        smtp: ("smtp.fastmail.com", 465, Security::Tls),
        oauth: None,
        note: Some(APP_PASSWORD),
    },
    Preset {
        domains: &["yandex.com", "yandex.ru", "ya.ru", "yandex.com.tr"],
        imap: ("imap.yandex.com", 993, Security::Tls),
        smtp: ("smtp.yandex.com", 465, Security::Tls),
        oauth: None,
        note: Some(APP_PASSWORD),
    },
    Preset {
        domains: &["gmx.com", "gmx.net", "gmx.de", "gmx.at", "gmx.ch"],
        imap: ("imap.gmx.net", 993, Security::Tls),
        smtp: ("mail.gmx.net", 587, Security::Starttls),
        oauth: None,
        note: Some("Turn on IMAP access in the GMX web settings first."),
    },
    Preset {
        domains: &["web.de"],
        imap: ("imap.web.de", 993, Security::Tls),
        smtp: ("smtp.web.de", 587, Security::Starttls),
        oauth: None,
        note: Some("Turn on IMAP access in the WEB.DE web settings first."),
    },
    Preset {
        domains: &["zoho.com", "zohomail.com"],
        imap: ("imap.zoho.com", 993, Security::Tls),
        smtp: ("smtp.zoho.com", 465, Security::Tls),
        oauth: None,
        note: None,
    },
    Preset {
        domains: &["mailbox.org"],
        imap: ("imap.mailbox.org", 993, Security::Tls),
        smtp: ("smtp.mailbox.org", 465, Security::Tls),
        oauth: None,
        note: None,
    },
    Preset {
        domains: &["posteo.de", "posteo.net"],
        imap: ("posteo.de", 993, Security::Tls),
        smtp: ("posteo.de", 465, Security::Tls),
        oauth: None,
        note: None,
    },
    Preset {
        domains: &["mail.ru", "inbox.ru", "list.ru", "bk.ru"],
        imap: ("imap.mail.ru", 993, Security::Tls),
        smtp: ("smtp.mail.ru", 465, Security::Tls),
        oauth: None,
        note: Some(APP_PASSWORD),
    },
    Preset {
        domains: &["proton.me", "protonmail.com", "protonmail.ch", "pm.me"],
        imap: ("127.0.0.1", 1143, Security::Starttls),
        smtp: ("127.0.0.1", 1025, Security::Starttls),
        oauth: None,
        note: Some(
            "Run Proton Mail Bridge and use the password it shows; trust its certificate when asked.",
        ),
    },
];

fn from_preset(preset: &Preset, source: &str) -> Found {
    Found {
        source: source.to_owned(),
        imap_host: preset.imap.0.into(),
        imap_port: preset.imap.1,
        imap_security: preset.imap.2,
        smtp_host: preset.smtp.0.into(),
        smtp_port: preset.smtp.1,
        smtp_security: preset.smtp.2,
        local_username: false,
        oauth: preset.oauth,
        note: preset.note,
    }
}

/// The built-in settings for a domain, without going online.
pub fn known(domain: &str) -> Option<Found> {
    let domain = domain.to_ascii_lowercase();
    PRESETS
        .iter()
        .find(|preset| preset.domains.contains(&domain.as_str()))
        .map(|preset| from_preset(preset, "built-in settings"))
}

/// Everything there is to try, in order. Returns the first complete result.
pub fn discover(address: &str) -> Result<Found, String> {
    let domain = address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .filter(|domain| domain.contains('.') && !domain.contains('/'))
        .ok_or("Enter a full email address first.")?;
    if let Some(found) = known(&domain) {
        return Ok(found);
    }
    let agent = agent();
    let sources = [
        (
            format!("https://autoconfig.thunderbird.net/v1.1/{domain}"),
            "Thunderbird's provider list",
        ),
        (
            format!("https://autoconfig.{domain}/mail/config-v1.1.xml"),
            "the domain's autoconfig",
        ),
        (
            format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml"),
            "the domain's autoconfig",
        ),
    ];
    for (url, source) in &sources {
        if let Some(found) = fetch_xml(&agent, url).and_then(|xml| parse_autoconfig(&xml, source)) {
            return Ok(found);
        }
    }
    // Custom domains hosted by Google or Microsoft show it in their mail servers.
    let exchangers = dns(&agent, &domain, "MX");
    for exchanger in &exchangers {
        let host = exchanger
            .split_whitespace()
            .last()
            .unwrap_or_default()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if host.ends_with("google.com") || host.ends_with("googlemail.com") {
            return Ok(from_preset(
                &PRESETS[0],
                "the domain's mail servers (Google)",
            ));
        }
        if host.ends_with("outlook.com") {
            return Ok(from_preset(
                &PRESETS[1],
                "the domain's mail servers (Microsoft 365)",
            ));
        }
        if let Some(base) = base_domain(&host)
            && base != domain
            && let Some(found) = fetch_xml(
                &agent,
                &format!("https://autoconfig.thunderbird.net/v1.1/{base}"),
            )
            .and_then(|xml| parse_autoconfig(&xml, "Thunderbird's provider list"))
        {
            return Ok(found);
        }
    }
    if let Some(found) = service_records(&agent, &domain) {
        return Ok(found);
    }
    Err(format!(
        "No settings found for {domain}. Enter the servers from your provider's help pages."
    ))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(6)))
        .build()
        .new_agent()
}

fn fetch_xml(agent: &ureq::Agent, url: &str) -> Option<String> {
    let mut response = agent.get(url).call().ok()?;
    response
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()
        .ok()
}

/// Reads a Mozilla autoconfig file (the format Thunderbird's database uses).
pub fn parse_autoconfig(xml: &str, source: &str) -> Option<Found> {
    let document = Html::parse_fragment(xml);
    let server = |selector: &str| {
        let selector = Selector::parse(selector).ok()?;
        document.select(&selector).next()
    };
    let text = |element: scraper::ElementRef<'_>, name: &str| -> Option<String> {
        let selector = Selector::parse(name).ok()?;
        element
            .select(&selector)
            .next()
            .map(|child| child.text().collect::<String>().trim().to_owned())
    };
    let security = |value: Option<String>, port: u16| match value
        .as_deref()
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("SSL" | "TLS") => Security::Tls,
        Some("STARTTLS") => Security::Starttls,
        _ if port == 993 || port == 465 => Security::Tls,
        _ => Security::Starttls,
    };
    let imap = server(r#"incomingserver[type="imap"]"#)?;
    let smtp = server(r#"outgoingserver[type="smtp"]"#)?;
    let imap_port: u16 = text(imap, "port")?.parse().ok()?;
    let smtp_port: u16 = text(smtp, "port")?.parse().ok()?;
    // Never fall back to plain text: only encrypted servers are offered.
    let plain = |value: &Option<String>| {
        value
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("plain"))
    };
    let imap_socket = text(imap, "sockettype");
    let smtp_socket = text(smtp, "sockettype");
    if plain(&imap_socket) || plain(&smtp_socket) {
        return None;
    }
    Some(Found {
        source: source.to_owned(),
        imap_host: text(imap, "hostname")?,
        imap_port,
        imap_security: security(imap_socket, imap_port),
        smtp_host: text(smtp, "hostname")?,
        smtp_port,
        smtp_security: security(smtp_socket, smtp_port),
        local_username: text(imap, "username").is_some_and(|value| value == "%EMAILLOCALPART%"),
        // OAuth2 needs endpoints and a client the database does not give.
        oauth: None,
        note: None,
    })
}

/// Answers from Cloudflare's DNS-over-HTTPS resolver, as text records.
fn dns(agent: &ureq::Agent, name: &str, kind: &str) -> Vec<String> {
    let url = format!("https://cloudflare-dns.com/dns-query?name={name}&type={kind}");
    let Ok(mut response) = agent
        .get(&url)
        .header("Accept", "application/dns-json")
        .call()
    else {
        return Vec::new();
    };
    let Ok(body) = response.body_mut().read_json::<serde_json::Value>() else {
        return Vec::new();
    };
    body["Answer"]
        .as_array()
        .map(|answers| {
            answers
                .iter()
                .filter_map(|answer| answer["data"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// RFC 6186 service records: `_imaps._tcp` or `_imap._tcp`, and `_submissions._tcp`
/// or `_submission._tcp`.
fn service_records(agent: &ureq::Agent, domain: &str) -> Option<Found> {
    let lookup = |service: &str| -> Option<(String, u16)> {
        let record = dns(agent, &format!("{service}.{domain}"), "SRV")
            .into_iter()
            .next()?;
        // "priority weight port target"
        let parts: Vec<&str> = record.split_whitespace().collect();
        let port = parts.get(2)?.parse().ok()?;
        let host = parts.get(3)?.trim_end_matches('.').to_owned();
        (host != "." && !host.is_empty()).then_some((host, port))
    };
    let (imap, imap_security) = lookup("_imaps._tcp")
        .map(|found| (found, Security::Tls))
        .or_else(|| lookup("_imap._tcp").map(|found| (found, Security::Starttls)))?;
    let (smtp, smtp_security) = lookup("_submissions._tcp")
        .map(|found| (found, Security::Tls))
        .or_else(|| lookup("_submission._tcp").map(|found| (found, Security::Starttls)))?;
    Some(Found {
        source: "the domain's DNS records".into(),
        imap_host: imap.0,
        imap_port: imap.1,
        imap_security,
        smtp_host: smtp.0,
        smtp_port: smtp.1,
        smtp_security,
        local_username: false,
        oauth: None,
        note: None,
    })
}

/// The registrable part of a host, roughly: the last two labels, or three for
/// country domains such as `example.co.uk`.
fn base_domain(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    if labels.len() < 2 {
        return None;
    }
    let keep = if labels.len() >= 3
        && labels[labels.len() - 2].len() <= 3
        && labels[labels.len() - 1].len() == 2
    {
        3
    } else {
        2
    };
    Some(labels[labels.len() - keep..].join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?><clientConfig version="1.1"><emailProvider id="x.test">
        <incomingServer type="pop3"><hostname>pop.x.test</hostname><port>995</port><socketType>SSL</socketType></incomingServer>
        <incomingServer type="imap"><hostname>imap.x.test</hostname><port>143</port><socketType>STARTTLS</socketType><username>%EMAILLOCALPART%</username></incomingServer>
        <outgoingServer type="smtp"><hostname>smtp.x.test</hostname><port>465</port><socketType>SSL</socketType></outgoingServer>
        </emailProvider></clientConfig>"#;

    #[test]
    fn reads_autoconfig_files() {
        let found = parse_autoconfig(XML, "test").unwrap();
        assert_eq!(
            (
                found.imap_host.as_str(),
                found.imap_port,
                found.imap_security
            ),
            ("imap.x.test", 143, Security::Starttls)
        );
        assert_eq!(
            (
                found.smtp_host.as_str(),
                found.smtp_port,
                found.smtp_security
            ),
            ("smtp.x.test", 465, Security::Tls)
        );
        assert!(found.local_username);
        assert!(
            parse_autoconfig(&XML.replace("STARTTLS", "plain"), "test").is_none(),
            "plain text is never offered"
        );
    }

    #[test]
    fn knows_common_providers_and_domains() {
        assert_eq!(known("GMAIL.com").unwrap().imap_host, "imap.gmail.com");
        assert_eq!(
            known("outlook.com.tr").unwrap().smtp_security,
            Security::Starttls
        );
        assert!(known("example.test").is_none());
        assert_eq!(
            base_domain("mx1.mail.example.co.uk").as_deref(),
            Some("example.co.uk")
        );
        assert_eq!(
            base_domain("mx.example.com").as_deref(),
            Some("example.com")
        );
        assert!(discover("not-an-address").is_err());
    }
}
