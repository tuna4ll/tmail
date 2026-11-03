//! A word index over every loaded message, so search stays instant in large mailboxes.
//! Plain search words match words that start with them.

use std::collections::{HashMap, HashSet};

use crate::mail::{Mail, search_terms};

#[derive(Debug, Default)]
pub struct Index {
    /// Every word, sorted, with the positions of the messages that contain it.
    words: Vec<(String, Vec<u32>)>,
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

impl Index {
    pub fn build(mails: &[Mail]) -> Self {
        let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
        for (position, mail) in mails.iter().enumerate() {
            let position = position as u32;
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
        let mut words: Vec<(String, Vec<u32>)> = postings.into_iter().collect();
        words.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Self { words }
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

/// The index for the loaded messages and the candidates of the last query, built the
/// first time a search needs them.
#[derive(Debug, Default)]
pub struct Cache {
    index: Option<(usize, Index)>,
    query: String,
    candidates: Option<HashSet<u32>>,
}

impl Cache {
    /// Drops everything after the messages changed.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn candidates(&mut self, mails: &[Mail], query: &str) -> Option<&HashSet<u32>> {
        let stale = !self
            .index
            .as_ref()
            .is_some_and(|(count, _)| *count == mails.len());
        if stale {
            self.index = Some((mails.len(), Index::build(mails)));
        }
        if stale || self.query != query {
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
}
