//! A word index over every loaded message, so search stays instant in large mailboxes.
//! Plain search words match words that start with them.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use crate::mail::{Mail, search_terms};

#[derive(Debug, Default, PartialEq)]
pub struct Index {
    /// Every word, sorted, with the positions of the messages that contain it.
    words: Vec<(String, Vec<u32>)>,
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

fn add_mail(postings: &mut HashMap<String, Vec<u32>>, position: u32, mail: &Mail) {
    let fields = [&mail.subject, &mail.from, &mail.body]
        .into_iter()
        .chain(&mail.to)
        .chain(&mail.cc);
    for field in fields {
        for word in tokens(field) {
            let list = postings.entry(word).or_default();
            if list.last() != Some(&position) {
                list.push(position);
            }
        }
    }
}

/// What stays the same for a message file while its flags change: the Maildir name
/// before `:2,`, or the Message-ID for messages without a file.
fn key(mail: &Mail) -> String {
    mail.source_path
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| {
            let name = name.to_string_lossy();
            name.split(':').next().unwrap_or_default().to_owned()
        })
        .unwrap_or_else(|| mail.id.clone())
        .replace('\n', " ")
}

const HEADER: &str = "termail-search 2";

impl Index {
    pub fn build(mails: &[Mail]) -> Self {
        let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
        for (position, mail) in mails.iter().enumerate() {
            add_mail(&mut postings, position as u32, mail);
        }
        Self::from_postings(postings)
    }

    fn from_postings(postings: HashMap<String, Vec<u32>>) -> Self {
        let mut words: Vec<(String, Vec<u32>)> = postings.into_iter().collect();
        words.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Self { words }
    }

    /// The index saved at `path`, brought up to date with `mails`: words of messages
    /// seen before are read back, only new messages are read word by word, and the
    /// file is written again when anything changed. A missing or damaged file means
    /// a fresh start.
    pub fn load_or_build(path: &Path, mails: &[Mail]) -> Self {
        let keys: Vec<String> = mails.iter().map(key).collect();
        let positions: HashMap<&str, u32> = keys
            .iter()
            .enumerate()
            .map(|(position, key)| (key.as_str(), position as u32))
            .collect();
        let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
        let mut covered = vec![false; mails.len()];
        let mut changed = true;
        if let Ok(text) = fs::read_to_string(path)
            && let Some(saved) = read_saved(&text)
        {
            let (saved_keys, lines) = saved;
            let mapped: Vec<Option<u32>> = saved_keys
                .iter()
                .map(|key| positions.get(key.as_str()).copied())
                .collect();
            for position in mapped.iter().flatten() {
                covered[*position as usize] = true;
            }
            changed = mapped.iter().any(Option::is_none) || covered.iter().any(|seen| !seen);
            for line in lines {
                let Some((word, list)) = line.split_once('\t') else {
                    continue;
                };
                // Saved as gaps between ascending numbers, which keeps the file small.
                let mut previous = 0usize;
                let found: Vec<u32> = list
                    .split(',')
                    .filter_map(|gap| gap.parse::<usize>().ok())
                    .filter_map(|gap| {
                        previous += gap;
                        mapped.get(previous).copied().flatten()
                    })
                    .collect();
                if !found.is_empty() {
                    postings.entry(word.to_owned()).or_default().extend(found);
                }
            }
        }
        for (position, mail) in mails.iter().enumerate() {
            if !covered[position] {
                add_mail(&mut postings, position as u32, mail);
            }
        }
        for list in postings.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        let index = Self::from_postings(postings);
        if changed {
            let _ = index.save(path, &keys);
        }
        index
    }

    fn save(&self, path: &Path, keys: &[String]) -> std::io::Result<()> {
        let mut text = String::with_capacity(self.words.len() * 16);
        text.push_str(HEADER);
        text.push('\n');
        text.push_str(&keys.len().to_string());
        text.push('\n');
        for key in keys {
            text.push_str(key);
            text.push('\n');
        }
        for (word, positions) in &self.words {
            text.push_str(word);
            text.push('\t');
            let mut previous = 0;
            for (index, position) in positions.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                text.push_str(&(position - previous).to_string());
                previous = *position;
            }
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, text)?;
        fs::rename(temporary, path)
    }

    /// Messages with a word starting with `prefix`.
    fn starting_with(&self, prefix: &str) -> HashSet<u32> {
        let start = self
            .words
            .partition_point(|(word, _)| word.as_str() < prefix);
        self.words[start..]
            .iter()
            .take_while(|(word, _)| word.starts_with(prefix))
            .flat_map(|(_, positions)| positions.iter().copied())
            .collect()
    }

    /// The messages that can match `query`, or `None` when the query has no plain words
    /// to narrow it down with. Field filters and exclusions are checked afterwards.
    pub fn candidates(&self, query: &str) -> Option<HashSet<u32>> {
        let mut result: Option<HashSet<u32>> = None;
        for term in search_terms(query) {
            if term.starts_with('-') || term.contains(':') {
                continue;
            }
            for word in tokens(&term) {
                let found = self.starting_with(&word);
                result = Some(match result {
                    Some(current) => current.intersection(&found).copied().collect(),
                    None => found,
                });
            }
        }
        result
    }
}

/// The keys and the word lines of a saved index, if it is one.
fn read_saved(text: &str) -> Option<(Vec<String>, std::str::Lines<'_>)> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let count: usize = lines.next()?.parse().ok()?;
    let mut keys = Vec::with_capacity(count.min(1 << 22));
    for _ in 0..count {
        keys.push(lines.next()?.to_owned());
    }
    Some((keys, lines))
}

/// The index for the loaded messages and the candidates of the last query, built the
/// first time a search needs them.
#[derive(Debug, Default)]
pub struct Cache {
    index: Option<(usize, Index)>,
    query: String,
    candidates: Option<HashSet<u32>>,
    /// Whether `candidates` belong to `query`.
    fresh: bool,
}

impl Cache {
    /// Forgets the last query, after the conversations were grouped again.
    pub fn clear(&mut self) {
        self.query.clear();
        self.candidates = None;
        self.fresh = false;
    }

    /// Uses an index built while loading, for exactly these `count` messages.
    pub fn set_index(&mut self, index: Option<Index>, count: usize) {
        *self = Self::default();
        self.index = index.map(|index| (count, index));
    }

    pub fn candidates(&mut self, mails: &[Mail], query: &str) -> Option<&HashSet<u32>> {
        let stale = !self
            .index
            .as_ref()
            .is_some_and(|(count, _)| *count == mails.len());
        if stale {
            self.index = Some((mails.len(), Index::build(mails)));
        }
        if stale || !self.fresh || self.query != query {
            self.fresh = true;
            self.query = query.to_owned();
            self.candidates = self
                .index
                .as_ref()
                .and_then(|(_, index)| index.candidates(query));
        }
        self.candidates.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_words_by_their_start() {
        let mails = vec![
            Mail::new(
                "1",
                "Ada <ada@example.test>",
                "Invoice for March",
                "Please pay soon",
                None,
            ),
            Mail::new(
                "2",
                "Grace <grace@example.test>",
                "Lunch",
                "Paying for lunch today",
                None,
            ),
        ];
        let index = Index::build(&mails);
        assert_eq!(index.candidates("pay"), Some(HashSet::from([0, 1])));
        assert_eq!(index.candidates("invoice march"), Some(HashSet::from([0])));
        assert_eq!(
            index.candidates("voice"),
            Some(HashSet::new()),
            "only word starts match"
        );
        assert_eq!(index.candidates("GRACE"), Some(HashSet::from([1])));
        assert_eq!(
            index.candidates("from:ada"),
            None,
            "field filters are checked later"
        );
        assert_eq!(index.candidates("\"for lunch\""), Some(HashSet::from([1])));
    }

    #[test]
    fn a_saved_index_catches_up_with_the_mailbox() {
        let mail = |name: &str, subject: &str| {
            let mut mail = Mail::new(name, "a@b.test", subject, "body words", None);
            mail.source_path = Some(format!("/m/cur/{name}:2,S").into());
            mail
        };
        let path =
            std::env::temp_dir().join(format!("{}.idx", crate::storage::unique_name("search")));
        let first = vec![mail("1", "alpha"), mail("2", "beta"), mail("3", "gamma")];
        assert_eq!(Index::load_or_build(&path, &first), Index::build(&first));
        // One message gone, one new, the rest in another order and with new flags.
        let mut second = vec![mail("4", "delta"), mail("3", "gamma"), mail("1", "alpha")];
        second[1].source_path = Some("/m/cur/3:2,RS".into());
        assert_eq!(Index::load_or_build(&path, &second), Index::build(&second));
        assert_eq!(Index::load_or_build(&path, &second), Index::build(&second));
        std::fs::write(&path, "not an index").unwrap();
        assert_eq!(Index::load_or_build(&path, &second), Index::build(&second));
        let _ = std::fs::remove_file(path);
    }
}
