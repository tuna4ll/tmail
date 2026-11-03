//! Finds what a message uses to watch its reader: invisible pictures that report when
//! and where it was opened, and links that pass through a click counter first.

/// Hosts that exist to count opens and clicks, and the service behind them.
const SERVICES: &[(&str, &str)] = &[
    ("list-manage.com", "Mailchimp"),
    ("mailchi.mp", "Mailchimp"),
    ("mandrillapp.com", "Mandrill"),
    ("hubspotemail.net", "HubSpot"),
    ("hubspotlinks.com", "HubSpot"),
    ("hs-analytics.net", "HubSpot"),
    ("track.hubspot.com", "HubSpot"),
    ("sendgrid.net", "SendGrid"),
    ("sparkpostmail.com", "SparkPost"),
    ("exct.net", "Salesforce Marketing Cloud"),
    ("exacttarget.com", "Salesforce Marketing Cloud"),
    ("klaviyo.com", "Klaviyo"),
    ("klclick.com", "Klaviyo"),
    ("klclick1.com", "Klaviyo"),
    ("mailtrack.io", "Mailtrack"),
    ("getnotify.com", "Notify"),
    ("yesware.com", "Yesware"),
    ("mixmax.com", "Mixmax"),
    ("superhuman.com", "Superhuman"),
    ("mailfoogae.appspot.com", "Streak"),
    ("streak.com", "Streak"),
    ("bananatag.com", "Bananatag"),
    ("createsend.com", "Campaign Monitor"),
    ("cmail19.com", "Campaign Monitor"),
    ("cmail20.com", "Campaign Monitor"),
    ("convertkit-mail.com", "ConvertKit"),
    ("convertkit-mail2.com", "ConvertKit"),
    ("eotrx.substackcdn.com", "Substack"),
    ("mlsend.com", "MailerLite"),
    ("sendibt2.com", "Brevo"),
    ("sendibt3.com", "Brevo"),
    ("sendibm1.com", "Brevo"),
    ("customeriomail.com", "Customer.io"),
    ("intercom-mail.com", "Intercom"),
    ("via.intercom.io", "Intercom"),
    ("links.iterable.com", "Iterable"),
    ("awstrack.me", "Amazon SES"),
    ("pstmrk.it", "Postmark"),
    ("mjt.lu", "Mailjet"),
    ("google-analytics.com", "Google Analytics"),
    ("mailgun.org", "Mailgun"),
    ("emltrk.com", "Litmus"),
    ("returnpath.net", "Return Path"),
];

fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The tracking service behind an address, if the host belongs to one.
pub fn service(url: &str) -> Option<&'static str> {
    let host = host(url)?;
    SERVICES.iter().find_map(|(domain, name)| {
        (host == *domain || host.ends_with(&format!(".{domain}"))).then_some(*name)
    })
}

/// Whether a remote picture only reports that the message was opened: a known
/// tracking host, a picture of a pixel or two, a hidden one, or an address that names
/// itself an open counter.
pub fn is_pixel(url: &str, width: Option<&str>, height: Option<&str>, style: &str) -> bool {
    let tiny = |value: Option<&str>| {
        value
            .and_then(|value| value.trim().trim_end_matches("px").parse::<u32>().ok())
            .is_some_and(|value| value <= 2)
    };
    let style = style.to_ascii_lowercase().replace(' ', "");
    let hidden = style.contains("display:none")
        || style.contains("visibility:hidden")
        || style.contains("opacity:0;")
        || style.ends_with("opacity:0")
        || ["width:0", "width:1px", "height:0", "height:1px"]
            .iter()
            .any(|rule| style.contains(rule));
    let path = url.to_ascii_lowercase();
    let named = [
        "/track/open",
        "/open.aspx",
        "/open.php",
        "/pixel.gif",
        "/beacon",
        "/wf/open",
        "/e/o/",
        "/trk/open",
    ]
    .iter()
    .any(|part| path.contains(part));
    service(url).is_some() || tiny(width) || tiny(height) || hidden || named
}

/// The address a click-counting link sends you on to, when it carries it in the open,
/// such as `https://click.example/?url=https%3A%2F%2Fshop.test%2F`.
pub fn destination(url: &str) -> Option<String> {
    service(url)?;
    let query = url.split_once('?')?.1.split('#').next()?;
    query.split('&').find_map(|pair| {
        let value = decode(pair.split_once('=')?.1);
        let lower = value.to_ascii_lowercase();
        (lower.starts_with("https://") || lower.starts_with("http://")).then_some(value)
    })
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        output.push(byte);
                        index += 3;
                        continue;
                    }
                    Err(_) => output.push(b'%'),
                }
            }
            b'+' => output.push(b' '),
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// One line for the reader, such as `blocked 3 trackers (Mailchimp, HubSpot)`.
pub fn summary(pixels: &[String], links: &[String]) -> Option<String> {
    let mut services: Vec<&str> = Vec::new();
    for url in pixels.iter().chain(links) {
        let name = service(url).unwrap_or("unknown");
        if !services.contains(&name) {
            services.push(name);
        }
    }
    let counted: Vec<&str> = links.iter().filter_map(|url| service(url)).collect();
    let mut parts = Vec::new();
    match pixels.len() {
        0 => {}
        1 => parts.push("blocked 1 tracking pixel".to_owned()),
        count => parts.push(format!("blocked {count} tracking pixels")),
    }
    match counted.len() {
        0 => {}
        1 => parts.push("1 link counted by the sender".to_owned()),
        count => parts.push(format!("{count} links counted by the sender")),
    }
    if parts.is_empty() {
        return None;
    }
    let named: Vec<&str> = services
        .into_iter()
        .filter(|name| *name != "unknown")
        .collect();
    let mut text = parts.join(" · ");
    if !named.is_empty() {
        text.push_str(&format!(" ({})", named.join(", ")));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_pixels_and_click_counters() {
        assert!(is_pixel(
            "https://x.list-manage.com/track/open.php?u=1",
            None,
            None,
            ""
        ));
        assert!(is_pixel(
            "https://shop.test/a.gif",
            Some("1"),
            Some("1"),
            ""
        ));
        assert!(is_pixel(
            "https://shop.test/a.gif",
            None,
            None,
            "display: none"
        ));
        assert!(!is_pixel(
            "https://shop.test/banner.png",
            Some("600"),
            None,
            "width:600px"
        ));
        assert_eq!(
            service("https://u123.ct.sendgrid.net/ls/click?x"),
            Some("SendGrid")
        );
        assert_eq!(service("https://notsendgrid.net/"), None);
        assert_eq!(
            destination("https://click.mjt.lu/r?url=https%3A%2F%2Fshop.test%2Fa%3Fb%3D1&x=2")
                .as_deref(),
            Some("https://shop.test/a?b=1")
        );
        assert_eq!(
            destination("https://shop.test/?url=https%3A%2F%2Fevil.test"),
            None
        );
        assert_eq!(
            summary(
                &[
                    "https://x.list-manage.com/o".into(),
                    "https://a.test/p.gif".into()
                ],
                &[
                    "https://hubspotlinks.com/x".into(),
                    "https://plain.test/".into()
                ]
            )
            .as_deref(),
            Some("blocked 2 tracking pixels · 1 link counted by the sender (Mailchimp, HubSpot)")
        );
    }
}
