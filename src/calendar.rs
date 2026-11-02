//! Meeting invitations (iCalendar, RFC 5545) and the replies that accept or decline
//! them (iMIP, RFC 6047).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Invite {
    /// REQUEST for an invitation, CANCEL when it was called off.
    pub method: String,
    pub uid: String,
    pub sequence: String,
    pub summary: String,
    pub location: String,
    pub organizer: String,
    pub organizer_name: String,
    /// When it starts and ends, ready to show.
    pub when: String,
    /// The original DTSTART and DTEND lines, copied into replies.
    start_line: String,
    end_line: String,
}

impl Invite {
    pub fn cancelled(&self) -> bool {
        self.method.eq_ignore_ascii_case("CANCEL")
    }
}

/// Lines with their continuations joined, as (name, parameters, value).
fn properties(text: &str) -> Vec<(String, String, String)> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = lines.last_mut()
        {
            last.push_str(&line[1..]);
        } else {
            lines.push(line.to_owned());
        }
    }
    lines
        .into_iter()
        .filter_map(|line| {
            let (head, value) = line.split_once(':')?;
            let (name, parameters) = head.split_once(';').unwrap_or((head, ""));
            Some((
                name.to_ascii_uppercase(),
                parameters.to_owned(),
                value.to_owned(),
            ))
        })
        .collect()
}

fn unescape(value: &str) -> String {
    value
        .replace("\\n", "\n")
        .replace("\\N", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
}

fn parameter<'a>(parameters: &'a str, name: &str) -> Option<&'a str> {
    parameters.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim_matches('"'))
    })
}

/// A date-time for people: local time for UTC values, the given zone otherwise.
fn time(value: &str, parameters: &str) -> String {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
    if parameter(parameters, "VALUE").is_some_and(|kind| kind.eq_ignore_ascii_case("DATE"))
        || value.len() == 8
    {
        return NaiveDate::parse_from_str(value, "%Y%m%d")
            .map(|date| format!("{} (all day)", date.format("%a %-d %b %Y")))
            .unwrap_or_else(|_| value.to_owned());
    }
    let utc = value.ends_with('Z');
    let Ok(naive) = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
    else {
        return value.to_owned();
    };
    if utc {
        let local = Utc.from_utc_datetime(&naive).with_timezone(&Local);
        return local.format("%a %-d %b %Y %H:%M").to_string();
    }
    match parameter(parameters, "TZID") {
        Some(zone) => format!("{} ({zone})", naive.format("%a %-d %b %Y %H:%M")),
        None => naive.format("%a %-d %b %Y %H:%M").to_string(),
    }
}

pub fn parse(text: &str) -> Option<Invite> {
    let mut invite = Invite::default();
    let mut in_event = false;
    let mut start = None;
    let mut end = None;
    for (name, parameters, value) in properties(text) {
        match name.as_str() {
            "METHOD" if !in_event => invite.method = value.trim().to_owned(),
            "BEGIN" if value.eq_ignore_ascii_case("VEVENT") => in_event = true,
            "END" if value.eq_ignore_ascii_case("VEVENT") => break,
            _ if !in_event => {}
            "UID" => invite.uid = value,
            "SEQUENCE" => invite.sequence = value,
            "SUMMARY" => invite.summary = unescape(&value),
            "LOCATION" => invite.location = unescape(&value),
            "ORGANIZER" => {
                invite.organizer = value
                    .trim_start_matches("mailto:")
                    .trim_start_matches("MAILTO:")
                    .to_owned();
                invite.organizer_name = parameter(&parameters, "CN").unwrap_or_default().to_owned();
            }
            "DTSTART" => {
                invite.start_line = format!(
                    "DTSTART{}{parameters}:{value}",
                    if parameters.is_empty() { "" } else { ";" }
                );
                start = Some((value, parameters));
            }
            "DTEND" => {
                invite.end_line = format!(
                    "DTEND{}{parameters}:{value}",
                    if parameters.is_empty() { "" } else { ";" }
                );
                end = Some((value, parameters));
            }
            _ => {}
        }
    }
    if invite.uid.is_empty() || start.is_none() {
        return None;
    }
    invite.when = match (start, end) {
        (Some((start, start_parameters)), Some((end, end_parameters))) => {
            let shown = time(&start, &start_parameters);
            // On the same day the end is only a time, such as 10:00–11:00.
            let same_day = start.len() > 8
                && start.get(..8) == end.get(..8)
                && start.ends_with('Z') == end.ends_with('Z')
                && start_parameters == end_parameters;
            match end.get(9..13).filter(|_| same_day && !start.ends_with('Z')) {
                Some(clock) => match shown.split_once(" (") {
                    Some((date, zone)) => format!("{date}–{}:{} ({zone}", &clock[..2], &clock[2..]),
                    None => format!("{shown}–{}:{}", &clock[..2], &clock[2..]),
                },
                None => format!("{shown} to {}", time(&end, &end_parameters)),
            }
        }
        (Some((start, parameters)), None) => time(&start, &parameters),
        _ => String::new(),
    };
    if invite.method.is_empty() {
        invite.method = "PUBLISH".into();
    }
    Some(invite)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    Accepted,
    Tentative,
    Declined,
}

impl Answer {
    fn partstat(self) -> &'static str {
        match self {
            Self::Accepted => "ACCEPTED",
            Self::Tentative => "TENTATIVE",
            Self::Declined => "DECLINED",
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            Self::Accepted => "Accepted",
            Self::Tentative => "Tentatively accepted",
            Self::Declined => "Declined",
        }
    }
}

fn fold(line: &str) -> String {
    // iCalendar lines stay under 75 octets; continuations start with a space.
    let mut output = String::new();
    let mut length = 0;
    for character in line.chars() {
        if length + character.len_utf8() > 74 {
            output.push_str("\r\n ");
            length = 1;
        }
        output.push(character);
        length += character.len_utf8();
    }
    output
}

/// The iMIP reply that tells the organizer the answer of `attendee`.
pub fn reply(invite: &Invite, attendee: &str, name: &str, answer: Answer) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//termail//EN".to_owned(),
        "METHOD:REPLY".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{}", invite.uid),
        format!("DTSTAMP:{stamp}"),
        invite.start_line.clone(),
    ];
    if !invite.end_line.is_empty() {
        lines.push(invite.end_line.clone());
    }
    if !invite.sequence.is_empty() {
        lines.push(format!("SEQUENCE:{}", invite.sequence));
    }
    lines.push(format!(
        "SUMMARY:{}",
        invite
            .summary
            .replace(',', "\\,")
            .replace(';', "\\;")
            .replace('\n', "\\n")
    ));
    lines.push(format!("ORGANIZER:mailto:{}", invite.organizer));
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(";CN=\"{}\"", name.replace('"', ""))
    };
    lines.push(format!(
        "ATTENDEE;PARTSTAT={}{name}:mailto:{attendee}",
        answer.partstat()
    ));
    lines.push("END:VEVENT".to_owned());
    lines.push("END:VCALENDAR".to_owned());
    lines
        .iter()
        .map(|line| fold(line))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Istanbul\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:abc-123@example.test\r\nSEQUENCE:2\r\nSUMMARY:Team sync\\, weekly\r\nLOCATION:Room 2\r\nORGANIZER;CN=\"Boss\":mailto:boss@example.test\r\nDTSTART;TZID=Europe/Istanbul:20260303T100000\r\nDTEND;TZID=Europe/Istanbul:20260303T110000\r\nDESCRIPTION:long text that is fol\r\n ded over lines\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn reads_invitations_and_answers_them() {
        let invite = parse(INVITE).unwrap();
        assert_eq!(invite.summary, "Team sync, weekly");
        assert_eq!(invite.organizer, "boss@example.test");
        assert_eq!(invite.when, "Tue 3 Mar 2026 10:00–11:00 (Europe/Istanbul)");
        assert!(!invite.cancelled());
        let reply = reply(&invite, "ada@example.test", "Ada", Answer::Accepted);
        assert!(reply.contains("METHOD:REPLY") && reply.contains("UID:abc-123@example.test"));
        assert!(reply.contains("ATTENDEE;PARTSTAT=ACCEPTED;CN=\"Ada\":mailto:ada@example.test"));
        assert!(reply.contains("DTSTART;TZID=Europe/Istanbul:20260303T100000"));
        assert!(reply.lines().all(|line| line.len() <= 75));
        assert!(parse("BEGIN:VCALENDAR\r\nEND:VCALENDAR").is_none());
        let all_day =
            parse("BEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20260303\r\nEND:VEVENT").unwrap();
        assert!(all_day.when.contains("all day"));
    }
}
