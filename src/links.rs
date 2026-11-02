//! Looks at a link before it opens: where it really goes, whether it hides that, and
//! which tracking parameters can go.

/// What to tell the user about a link before opening it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Assessment {
    /// The address to open, without tracking parameters when cleaning is on.
    pub url: String,
    /// Reasons to look twice. Empty for an ordinary link.
    pub warnings: Vec<String>,
    /// The link must not open at all, such as `javascript:`.
    pub blocked: bool,
}

/// Parameters that only identify the click for the sender's analytics.
const TRACKING: &[&str] = &[
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "oly_anon_id",
    "oly_enc_id",
    "vero_id",
    "ref_src",
];

pub fn assess(url: &str, shown: &str, clean: bool) -> Assessment {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let mut assessment = Assessment {
        url: url.to_owned(),
        ..Assessment::default()
    };
    if lower.starts_with("mailto:") {
        return assessment;
    }
    if !lower.starts_with("https://") && !lower.starts_with("http://") {
        assessment.blocked = true;
        assessment
            .warnings
            .push("This is not a web link, so termail does not open it.".into());
        return assessment;
    }
    let Some(parts) = Parts::parse(url) else {
        assessment.blocked = true;
        assessment.warnings.push("The address is malformed.".into());
        return assessment;
    };
    if clean {
        assessment.url = parts.without_tracking();
    }
    let host = parts.host.to_ascii_lowercase();
    if parts.userinfo {
        assessment.warnings.push(format!(
            "The address hides its real destination behind an @: it goes to {host}."
        ));
    }
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        assessment.warnings.push(format!(
            "The link goes to a bare IP address ({host}), not a named site."
        ));
    }
    if host.split('.').any(|label| label.starts_with("xn--")) {
        assessment.warnings.push(format!(
            "The site name {host} uses look-alike characters (punycode)."
        ));
    }
    if let Some(claimed) = shown_host(shown)
        && registrable(&claimed) != registrable(&host)
    {
        assessment
            .warnings
            .push(format!("The link shows {claimed} but leads to {host}."));
    }
    if lower.starts_with("http://") {
        assessment
            .warnings
            .push("The page is not encrypted (http).".into());
    }
    assessment
}

/// The site a link's visible text names, when it looks like an address at all.
fn shown_host(shown: &str) -> Option<String> {
    let text = shown
        .trim()
        .trim_end_matches(['.', ',', ')', '/'])
        .to_ascii_lowercase();
    let text = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .unwrap_or(&text);
    let host = text.split(['/', '?', '#', ':']).next()?;
    let looks_like_site = !host.contains(char::is_whitespace)
        && host.contains('.')
        && host
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    looks_like_site.then(|| host.trim_start_matches("www.").to_owned())
}

/// The part of a host its owner registered: `mail.example.co.uk` becomes
/// `example.co.uk`. A rough rule that is right for common domains.
fn registrable(host: &str) -> String {
    let labels: Vec<&str> = host
        .trim_start_matches("www.")
        .split('.')
        .filter(|l| !l.is_empty())
        .collect();
    if labels.len() < 2 {
        return host.to_owned();
    }
    let second = labels[labels.len() - 2];
    let keep = if labels.len() >= 3
        && labels[labels.len() - 1].len() == 2
        && matches!(
            second,
            "co" | "com" | "net" | "org" | "ac" | "gov" | "edu" | "ne" | "or"
        ) {
        3
    } else {
        2
    };
    labels[labels.len() - keep..].join(".")
}

struct Parts<'a> {
    scheme: &'a str,
    userinfo: bool,
    authority: &'a str,
    host: String,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

impl<'a> Parts<'a> {
    fn parse(url: &'a str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, Some(query)),
            None => (rest, None),
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        let userinfo = authority.contains('@');
        let host_port = authority.rsplit('@').next()?;
        let host = if host_port.starts_with('[') {
            host_port
                .split(']')
                .next()?
                .trim_start_matches('[')
                .to_owned()
        } else {
            host_port.split(':').next()?.to_owned()
        };
        (!host.is_empty()).then_some(Self {
            scheme,
            userinfo,
            authority,
            host,
            path,
            query,
            fragment,
        })
    }

    fn without_tracking(&self) -> String {
        let mut url = format!("{}://{}{}", self.scheme, self.authority, self.path);
        if let Some(query) = self.query {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| {
                    let name = pair
                        .split('=')
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    !name.is_empty()
                        && !name.starts_with("utm_")
                        && !TRACKING.contains(&name.as_str())
                })
                .collect();
            if !kept.is_empty() {
                url.push('?');
                url.push_str(&kept.join("&"));
            }
        }
        if let Some(fragment) = self.fragment {
            url.push('#');
            url.push_str(fragment);
        }
        url
    }
}

/// How the topmost `Authentication-Results` header judged the sender, as added by the
/// receiving server.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SenderCheck {
    pub verified: bool,
    pub failed: bool,
    pub detail: String,
}

/// Reads the receiving server's verdict. Only the first header counts: lower ones can
/// be written by anyone who sent the message.
pub fn sender_check(results: &str, from_address: &str) -> Option<SenderCheck> {
    let lower = results.to_ascii_lowercase();
    let result = |method: &str| -> Option<String> {
        let start = lower.find(&format!("{method}="))? + method.len() + 1;
        Some(
            lower[start..]
                .split(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let dmarc = result("dmarc");
    let dkim = result("dkim");
    let spf = result("spf");
    if dmarc.is_none() && dkim.is_none() && spf.is_none() {
        return None;
    }
    let from_domain = from_address
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let signed_domain = lower.find("header.d=").map(|start| {
        lower[start + 9..]
            .split(|c: char| c.is_whitespace() || c == ';' || c == ')')
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    let aligned = signed_domain
        .as_deref()
        .is_some_and(|domain| registrable(domain) == registrable(&from_domain));
    let pass = |value: &Option<String>| value.as_deref() == Some("pass");
    let fail = |value: &Option<String>| matches!(value.as_deref(), Some("fail" | "permerror"));
    let (verified, failed, detail) = if pass(&dmarc) {
        (true, false, "DMARC passed".to_owned())
    } else if fail(&dmarc) {
        (
            false,
            true,
            "DMARC failed: the sender may be forged".to_owned(),
        )
    } else if pass(&dkim) && aligned {
        (
            true,
            false,
            format!("signed by {}", signed_domain.unwrap_or_default()),
        )
    } else if fail(&dkim) && fail(&spf) {
        (
            false,
            true,
            "DKIM and SPF failed: the sender may be forged".to_owned(),
        )
    } else {
        (
            false,
            false,
            format!(
                "not verified (DKIM {}, SPF {})",
                dkim.as_deref().unwrap_or("none"),
                spf.as_deref().unwrap_or("none")
            ),
        )
    };
    Some(SenderCheck {
        verified,
        failed,
        detail,
    })
}

/// A display name that shows a different address than the one the mail comes from,
/// such as `"support@bank.test" <someone@else.test>`.
pub fn spoofed_name(from: &str) -> Option<String> {
    let (name, address) = from.rsplit_once('<')?;
    let address = address.trim_end_matches('>').trim().to_ascii_lowercase();
    let shown = name
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '(' || c == ')')
        .find(|word| word.contains('@') && word.contains('.'))?
        .to_ascii_lowercase();
    (shown != address).then(|| format!("The name shows {shown} but the mail comes from {address}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_links_that_hide_where_they_go() {
        let honest = assess(
            "https://www.example.com/a?utm_source=x&id=7&fbclid=1#top",
            "example.com",
            true,
        );
        assert_eq!(honest.url, "https://www.example.com/a?id=7#top");
        assert!(honest.warnings.is_empty() && !honest.blocked);
        let lie = assess("https://evil.test/login", "https://paypal.com/login", true);
        assert!(lie.warnings[0].contains("shows paypal.com but leads to evil.test"));
        assert!(
            assess("https://mail.example.co.uk/x", "example.co.uk", true)
                .warnings
                .is_empty()
        );
        assert!(
            !assess("https://paypal.com@evil.test/", "Click here", true)
                .warnings
                .is_empty()
        );
        assert!(
            !assess("https://xn--pypal-4ve.com/", "Click", true)
                .warnings
                .is_empty()
        );
        assert!(
            !assess("http://192.168.1.1/", "router", true)
                .warnings
                .is_empty()
        );
        assert!(assess("javascript:alert(1)", "Click", true).blocked);
        assert!(
            assess("mailto:a@b.test", "a@b.test", true)
                .warnings
                .is_empty()
        );
        assert!(
            assess("https://example.com/", "Read the report", true)
                .warnings
                .is_empty()
        );
    }

    #[test]
    fn reads_the_receiving_servers_verdict() {
        let pass = sender_check(
            "mx.test; dkim=pass header.d=bank.test; spf=pass; dmarc=pass",
            "a@bank.test",
        )
        .unwrap();
        assert!(pass.verified);
        let forged = sender_check(
            "mx.test; spf=fail; dkim=none; dmarc=fail (p=reject)",
            "a@bank.test",
        )
        .unwrap();
        assert!(forged.failed);
        let other_signer = sender_check(
            "mx.test; dkim=pass header.d=mailer.test; spf=softfail",
            "a@bank.test",
        )
        .unwrap();
        assert!(!other_signer.verified && !other_signer.failed);
        assert!(sender_check("mx.test; none", "a@b.test").is_none());
        assert!(spoofed_name("\"support@bank.test\" <x@evil.test>").is_some());
        assert!(spoofed_name("Ada <ada@example.test>").is_none());
        assert!(spoofed_name("ada@example.test <ada@example.test>").is_none());
    }
}
