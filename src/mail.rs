use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attachment {
    pub name: String,
    pub media_type: String,
    pub size: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Mail {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub date: Option<String>,
    pub timestamp: Option<i64>,
    pub reply_to: Option<String>,
    pub references: Vec<String>,
    #[serde(default)]
    pub reply_address: Vec<String>,
    pub unread: bool,
    pub starred: bool,
    pub attachment_count: usize,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub html: bool,
    #[serde(default)]
    pub account: Option<String>,
    pub source_path: Option<PathBuf>,
    pub mailbox: Option<String>,
    #[serde(skip)]
    pub delivery_status: Option<String>,
    /// Only the headers were downloaded; the body can be fetched on request.
    #[serde(default)]
    pub partial: bool,
    /// How the sender lets recipients leave a mailing list.
    #[serde(default)]
    pub unsubscribe: Option<Unsubscribe>,
    /// How the message is signed or encrypted.
    #[serde(default)]
    pub security: Option<crate::crypto::Kind>,
}

/// The `List-Unsubscribe` choices of a message (RFC 2369 and RFC 8058).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Unsubscribe {
    pub links: Vec<String>,
    /// The sender accepts a single HTTPS POST without a confirmation page.
    pub one_click: bool,
}

impl Unsubscribe {
    pub fn parse(header: &str, post: Option<&str>) -> Option<Self> {
        let links: Vec<String> = header
            .split(',')
            .filter_map(|part| {
                let link = part.trim().strip_prefix('<')?.strip_suffix('>')?.trim();
                let lower = link.to_ascii_lowercase();
                (lower.starts_with("https://")
                    || lower.starts_with("http://")
                    || lower.starts_with("mailto:"))
                .then(|| link.to_owned())
            })
            .collect();
        if links.is_empty() {
            return None;
        }
        let one_click = post.is_some_and(|value| {
            value
                .replace(char::is_whitespace, "")
                .eq_ignore_ascii_case("List-Unsubscribe=One-Click")
        });
        Some(Self { links, one_click })
    }

    pub fn https(&self) -> Option<&str> {
        self.links
            .iter()
            .find(|link| link.to_ascii_lowercase().starts_with("https://"))
            .map(String::as_str)
    }

    pub fn mailto(&self) -> Option<&str> {
        self.links
            .iter()
            .find(|link| link.to_ascii_lowercase().starts_with("mailto:"))
            .map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conversation {
    pub root_id: String,
    pub subject: String,
    pub mail_ids: Vec<String>,
    /// Positions of the messages in the slice the conversation was grouped from.
    pub positions: Vec<usize>,
}

impl Conversation {
    pub fn new(root_id: &str, subject: &str) -> Self {
        Self {
            root_id: root_id.into(),
            subject: subject.into(),
            mail_ids: Vec::new(),
            positions: Vec::new(),
        }
    }

    pub fn last_sender<'a>(&self, mails: &'a [Mail]) -> Option<&'a str> {
        self.mails(mails).last().map(|mail| mail.from.as_str())
    }

    #[cfg(test)]
    pub fn latest_mail_id(&self) -> Option<&str> {
        self.mail_ids.last().map(String::as_str)
    }

    pub fn mails<'a>(&self, mails: &'a [Mail]) -> Vec<&'a Mail> {
        self.positions
            .iter()
            .filter_map(|position| mails.get(*position))
            .collect()
    }

    pub fn is_unread(&self, mails: &[Mail]) -> bool {
        self.mails(mails).iter().any(|mail| mail.unread)
    }

    pub fn is_starred(&self, mails: &[Mail]) -> bool {
        self.mails(mails).iter().any(|mail| mail.starred)
    }

    pub fn matches(&self, mails: &[Mail], query: &str) -> bool {
        let query = query.to_lowercase();
        self.mails(mails).iter().any(|mail| mail.matches(&query))
    }
}

#[cfg(test)]
pub fn group_conversations(mails: &[Mail]) -> Vec<Conversation> {
    group_conversations_where(mails, |_| Some(0))
}

/// Groups the messages `rank` accepts. When several copies share a Message-ID, the
/// one with the lowest rank stands for all of them.
pub fn group_conversations_where(
    mails: &[Mail],
    rank: impl Fn(&Mail) -> Option<u8>,
) -> Vec<Conversation> {
    let mut chosen: HashMap<&str, (u8, usize)> = HashMap::new();
    for (position, mail) in mails.iter().enumerate() {
        let Some(value) = rank(mail) else {
            continue;
        };
        let entry = chosen.entry(mail.id.as_str()).or_insert((value, position));
        if value < entry.0 {
            *entry = (value, position);
        }
    }
    let mut pool: Vec<usize> = chosen.into_values().map(|(_, position)| position).collect();
    pool.sort_unstable();
    let subset: Vec<&Mail> = pool.iter().map(|position| &mails[*position]).collect();
    let roots = roots(&parents(&subset));
    let mut conversations: Vec<Conversation> = Vec::new();
    let mut by_root = HashMap::new();
    for (index, root) in roots.into_iter().enumerate() {
        let conversation = *by_root.entry(root).or_insert_with(|| {
            conversations.push(Conversation::new(&subset[root].id, &subset[root].subject));
            conversations.len() - 1
        });
        conversations[conversation]
            .mail_ids
            .push(subset[index].id.clone());
        conversations[conversation].positions.push(pool[index]);
    }
    conversations
}

/// Strips reply and forward prefixes, reporting whether there were any.
fn base_subject(subject: &str) -> (String, bool) {
    let mut rest = subject.trim();
    let mut reply = false;
    while let Some(prefix) = ["re:", "fwd:", "fw:", "aw:", "sv:", "ynt:", "ilt:"]
        .iter()
        .find(|prefix| {
            rest.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        })
    {
        rest = rest[prefix.len()..].trim_start();
        reply = true;
    }
    (rest.to_lowercase(), reply)
}

/// The index of each message's parent. Replies are linked by In-Reply-To and
/// References; replies from clients that drop those headers fall back to the most
/// recent earlier message with the same subject.
pub fn parents<M: std::borrow::Borrow<Mail>>(mails: &[M]) -> Vec<Option<usize>> {
    let mut index = HashMap::new();
    for (position, mail) in mails.iter().map(Borrow::borrow).enumerate() {
        index.entry(mail.id.as_str()).or_insert(position);
    }
    let mut latest_by_subject: HashMap<String, usize> = HashMap::new();
    mails
        .iter()
        .map(Borrow::borrow)
        .enumerate()
        .map(|(position, mail): (usize, &Mail)| {
            let explicit = mail
                .reply_to
                .as_deref()
                .and_then(|id| index.get(id))
                .or_else(|| {
                    mail.references
                        .iter()
                        .rev()
                        .find_map(|id| index.get(id.as_str()))
                })
                .copied()
                .filter(|parent| *parent != position);
            let (subject, reply) = base_subject(&mail.subject);
            let parent = explicit.or_else(|| {
                (reply && mail.reply_to.is_none() && mail.references.is_empty())
                    .then(|| latest_by_subject.get(&subject).copied())
                    .flatten()
            });
            if !subject.is_empty() {
                latest_by_subject.insert(subject, position);
            }
            parent
        })
        .collect()
}

/// The thread root of each message, given `parents`. Cycles stop at the first
/// message seen twice.
pub fn roots(parents: &[Option<usize>]) -> Vec<usize> {
    let mut roots: Vec<Option<usize>> = vec![None; parents.len()];
    for start in 0..parents.len() {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let mut current = start;
        let root = loop {
            if let Some(root) = roots[current] {
                break root;
            }
            if !seen.insert(current) {
                break current;
            }
            path.push(current);
            match parents[current] {
                Some(parent) => current = parent,
                None => break current,
            }
        };
        for position in path {
            roots[position] = Some(root);
        }
    }
    roots.into_iter().map(Option::unwrap_or_default).collect()
}

impl Mail {
    pub fn new(id: &str, from: &str, subject: &str, body: &str, reply_to: Option<&str>) -> Self {
        Self {
            id: id.into(),
            from: from.into(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: subject.into(),
            body: body.into(),
            date: None,
            timestamp: None,
            reply_to: reply_to.map(Into::into),
            references: Vec::new(),
            reply_address: Vec::new(),
            unread: false,
            starred: false,
            attachment_count: 0,
            attachments: Vec::new(),
            links: Vec::new(),
            html: false,
            account: None,
            source_path: None,
            mailbox: None,
            delivery_status: None,
            partial: false,
            unsubscribe: None,
            security: None,
        }
    }

    pub fn with_recipients(mut self, to: &[&str], cc: &[&str]) -> Self {
        self.to = to.iter().map(|value| (*value).into()).collect();
        self.cc = cc.iter().map(|value| (*value).into()).collect();
        self
    }

    pub fn with_date(mut self, date: &str, timestamp: i64) -> Self {
        self.date = Some(date.into());
        self.timestamp = Some(timestamp);
        self
    }

    pub fn with_references(mut self, references: &[&str]) -> Self {
        self.references = references.iter().map(|value| (*value).into()).collect();
        self
    }

    pub fn with_state(mut self, unread: bool, starred: bool) -> Self {
        self.unread = unread;
        self.starred = starred;
        self
    }

    pub fn with_attachments(mut self, count: usize) -> Self {
        self.attachment_count = count;
        self
    }

    /// Matches every space-separated term. Terms may be quoted phrases, take a
    /// `from:`, `to:`, `subject:`, `body:`, `in:`, `is:` or `has:` prefix, or start
    /// with `-` to exclude.
    pub fn matches(&self, query: &str) -> bool {
        search_terms(query)
            .iter()
            .all(|term| match term.strip_prefix('-') {
                Some(term) if !term.is_empty() => !self.matches_term(term),
                _ => self.matches_term(term),
            })
    }

    fn matches_term(&self, term: &str) -> bool {
        let contains = |value: &str, needle: &str| value.to_lowercase().contains(needle);
        match term.split_once(':') {
            Some(("from", value)) => contains(&self.from, value),
            Some(("to", value)) => self.to.iter().chain(&self.cc).any(|to| contains(to, value)),
            Some(("subject", value)) => contains(&self.subject, value),
            Some(("body", value)) => contains(&self.body, value),
            Some(("in", "all")) => true,
            Some(("in" | "folder", value)) => {
                self.mailbox.as_deref().unwrap_or("INBOX").to_lowercase() == value
            }
            Some(("is", "unread")) => self.unread,
            Some(("is", "read")) => !self.unread,
            Some(("is", "starred" | "flagged")) => self.starred,
            Some(("has", "attachment" | "attachments")) => self.attachment_count > 0,
            Some((filter @ ("before" | "after" | "on"), value)) => {
                let Some(date) = parse_date(value) else {
                    return false;
                };
                let Some(day) = self.timestamp.and_then(local_day) else {
                    return false;
                };
                match filter {
                    "before" => day < date,
                    "after" => day >= date,
                    _ => day == date,
                }
            }
            _ => {
                contains(&self.subject, term)
                    || contains(&self.from, term)
                    || contains(&self.body, term)
                    || self.to.iter().chain(&self.cc).any(|to| contains(to, term))
            }
        }
    }
}

/// A date for lists: the time today, the day this year, the full date before that.
pub fn short_date(timestamp: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(date) = Local.timestamp_opt(timestamp, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    if date.date_naive() == now.date_naive() {
        date.format("%H:%M").to_string()
    } else if date.year() == now.year() {
        date.format("%-d %b").to_string()
    } else {
        date.format("%-d %b %Y").to_string()
    }
}

/// A full date in the local time zone, for message headers.
pub fn long_date(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|date| date.format("%a %-d %b %Y %H:%M").to_string())
        .unwrap_or_default()
}

fn local_day(timestamp: i64) -> Option<chrono::NaiveDate> {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|date| date.date_naive())
}

/// Reads a search date: `2026-03-14`, `2026-03`, `today`, `yesterday`, or an age such as
/// `7d`, `2w`, `3m` or `1y`.
pub fn parse_date(value: &str) -> Option<chrono::NaiveDate> {
    use chrono::{Days, Local, Months, NaiveDate};
    let today = Local::now().date_naive();
    match value {
        "today" => return Some(today),
        "yesterday" => return today.checked_sub_days(Days::new(1)),
        _ => {}
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date);
    }
    if let Ok(date) = NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d") {
        return Some(date);
    }
    let unit = value.chars().last()?;
    let amount: u32 = value[..value.len() - unit.len_utf8()].parse().ok()?;
    match unit {
        'd' => today.checked_sub_days(Days::new(amount.into())),
        'w' => today.checked_sub_days(Days::new(u64::from(amount) * 7)),
        'm' => today.checked_sub_months(Months::new(amount)),
        'y' => today.checked_sub_months(Months::new(amount.checked_mul(12)?)),
        _ => None,
    }
}

/// Splits a search query into lowercased terms, keeping quoted phrases together.
pub fn search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in query.to_lowercase().chars() {
        match character {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !current.is_empty() {
                    terms.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

/// The bare, lowercased address from a display form such as `Name <a@b.test>`.
pub fn email_of(value: &str) -> String {
    value
        .rsplit_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map_or(value, |(address, _)| address)
        .trim()
        .to_lowercase()
}

pub fn demo_mails() -> Vec<Mail> {
    vec![
        Mail::new(
            "project-1",
            "ada@example.com",
            "project update",
            "hey,\nthe new build is ready.\ncan you review it?",
            None,
        )
        .with_recipients(&["tuna@tunakilic.com"], &[])
        .with_date("2026-09-29T09:14:00+03:00", 1_801_208_040)
        .with_state(true, true)
        .with_attachments(1),
        Mail::new(
            "project-2",
            "tuna@tunakilic.com",
            "re: project update",
            "sure, i'll check it tonight.",
            Some("project-1"),
        )
        .with_recipients(&["ada@example.com"], &[])
        .with_date("2026-09-29T18:42:00+03:00", 1_801_242_120)
        .with_references(&["project-1"]),
        Mail::new(
            "project-3",
            "ada@example.com",
            "re: project update",
            "great, i added the release notes too.",
            Some("project-2"),
        )
        .with_recipients(&["tuna@tunakilic.com"], &[])
        .with_date("2026-09-30T08:07:00+03:00", 1_801_290_420)
        .with_references(&["project-1", "project-2"])
        .with_state(true, false),
        Mail::new(
            "weekend-1",
            "mert@example.com",
            "weekend plans",
            "coffee on saturday?",
            None,
        )
        .with_recipients(&["tuna@tunakilic.com"], &[])
        .with_date("2026-09-28T17:31:00+03:00", 1_801_151_460),
        Mail::new(
            "weekend-2",
            "tuna@tunakilic.com",
            "re: weekend plans",
            "sounds good. same place at two?",
            Some("weekend-1"),
        )
        .with_recipients(&["mert@example.com"], &[])
        .with_date("2026-09-28T18:02:00+03:00", 1_801_153_320)
        .with_references(&["weekend-1"]),
        Mail::new(
            "pull-1",
            "github@github.com",
            "review requested",
            "ada requested your review on pull request #42.",
            None,
        )
        .with_recipients(&["tuna@tunakilic.com"], &[])
        .with_date("2026-09-30T10:18:00+03:00", 1_801_298_280)
        .with_state(true, false),
        Mail::new(
            "pull-2",
            "github@github.com",
            "pull request merged",
            "pull request #42 has been merged.",
            Some("pull-1"),
        )
        .with_recipients(&["tuna@tunakilic.com"], &[])
        .with_date("2026-09-30T12:46:00+03:00", 1_801_307_160)
        .with_references(&["pull-1"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::{Mail, demo_mails, group_conversations};

    #[test]
    fn groups_replies_with_their_root_mail() {
        let mails = demo_mails();
        let conversations = group_conversations(&mails);

        assert_eq!(conversations.len(), 3);
        assert_eq!(conversations[0].root_id, "project-1");
        assert_eq!(conversations[0].subject, "project update");
        assert_eq!(
            conversations[0].mail_ids,
            ["project-1", "project-2", "project-3"]
        );
        assert_eq!(conversations[0].latest_mail_id(), Some("project-3"));
        assert_eq!(
            conversations[0].last_sender(&mails),
            Some("ada@example.com")
        );
    }

    #[test]
    fn keeps_orphan_replies_in_their_own_conversation() {
        let mails = vec![Mail::new(
            "orphan",
            "ada@example.com",
            "missing parent",
            "hello",
            Some("missing"),
        )];
        let conversations = group_conversations(&mails);

        assert_eq!(conversations[0].root_id, "orphan");
    }

    #[test]
    fn uses_references_when_reply_to_is_missing() {
        let root = Mail::new("root", "ada@example.com", "update", "hello", None);
        let first = Mail::new(
            "first",
            "tuna@example.com",
            "re: update",
            "hi",
            Some("root"),
        );
        let second = Mail::new("second", "mert@example.com", "re: update", "hey", None)
            .with_references(&["root"]);
        let mails = vec![root, first, second];
        let conversations = group_conversations(&mails);

        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].mail_ids, ["root", "first", "second"]);
    }

    #[test]
    fn filters_messages_by_sender() {
        let mail = Mail::new("root", "Ada <ada@example.com>", "update", "hello", None);

        assert!(mail.matches("from:ada"));
        assert!(!mail.matches("from:mert"));
    }
}
