// Full-text inverted index (M2.c): built alongside the vector index since
// both are over-fetch-then-filter secondary indexes (CLAUDE.md's M2 scope
// note). Tokenization is deliberately trivial for M2 — whitespace split,
// strip non-alphanumeric edges, lowercase — no stemming, no stopwords, no
// ranking/BM25. This is a minimal-viable choice, not a placeholder for
// something more that got forgotten; a real search engine's tokenizer is
// out of scope for proving the worker/query machinery works.
//
// Search is AND-only multi-term intersection, matching the project's
// existing AND-only `WHERE` philosophy (no `OR` support anywhere in the SQL
// subset either).

use std::collections::{HashMap, HashSet};

use crate::heap::RowId;

pub struct InvertedIndex {
    postings: HashMap<String, Vec<RowId>>,
    doc_terms: HashMap<RowId, HashSet<String>>,
}

impl Default for InvertedIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl InvertedIndex {
    pub fn new() -> Self {
        Self {
            postings: HashMap::new(),
            doc_terms: HashMap::new(),
        }
    }

    /// Insert or overwrite the indexed text for `id`.
    pub fn upsert(&mut self, id: RowId, text: &str) {
        self.remove(id);
        let terms = tokenize(text);
        for term in &terms {
            self.postings.entry(term.clone()).or_default().push(id);
        }
        self.doc_terms.insert(id, terms);
    }

    pub fn remove(&mut self, id: RowId) {
        let Some(terms) = self.doc_terms.remove(&id) else {
            return;
        };
        for term in terms {
            if let Some(list) = self.postings.get_mut(&term) {
                list.retain(|&existing| existing != id);
                if list.is_empty() {
                    self.postings.remove(&term);
                }
            }
        }
    }

    /// AND-only intersection of every term's posting list. An empty or
    /// all-stopword-stripped query matches nothing, not everything.
    pub fn search(&self, query: &str) -> Vec<RowId> {
        let terms = tokenize(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let mut sets: Vec<HashSet<RowId>> = terms
            .iter()
            .map(|t| {
                self.postings
                    .get(t)
                    .map(|list| list.iter().copied().collect())
                    .unwrap_or_default()
            })
            .collect();
        sets.sort_by_key(|s| s.len());

        let mut iter = sets.into_iter();
        let Some(mut result) = iter.next() else {
            return Vec::new();
        };
        for s in iter {
            result = result.intersection(&s).copied().collect();
            if result.is_empty() {
                break;
            }
        }
        result.into_iter().collect()
    }

    pub fn len(&self) -> usize {
        self.doc_terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc_terms.is_empty()
    }
}

/// Whitespace split, strip non-alphanumeric edges, lowercase — no stemming,
/// stopwords, or ranking (M2.c scope). `pub(crate)` since P3.b so the durable
/// full-text path (`sql/executor.rs`) derives the same token set a row's
/// on-disk B+tree entries are keyed by.
pub(crate) fn tokenize(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(page: u32, slot: u16) -> RowId {
        RowId {
            page_id: page,
            slot,
        }
    }

    #[test]
    fn empty_index_search_returns_nothing() {
        let idx = InvertedIndex::new();
        assert!(idx.search("hello").is_empty());
    }

    #[test]
    fn single_term_search_finds_matching_docs() {
        let mut idx = InvertedIndex::new();
        idx.upsert(rid(1, 0), "the quick brown fox");
        idx.upsert(rid(2, 0), "the lazy dog");

        let results = idx.search("fox");
        assert_eq!(results, vec![rid(1, 0)]);
    }

    #[test]
    fn multi_term_search_is_and_only_intersection() {
        let mut idx = InvertedIndex::new();
        idx.upsert(rid(1, 0), "rust database engine");
        idx.upsert(rid(2, 0), "rust programming language");
        idx.upsert(rid(3, 0), "python database driver");

        let mut results = idx.search("rust database");
        results.sort_by_key(|r| r.slot);
        assert_eq!(results, vec![rid(1, 0)]);
    }

    #[test]
    fn tokenization_lowercases_and_strips_punctuation() {
        let mut idx = InvertedIndex::new();
        idx.upsert(rid(1, 0), "Hello, World!");
        assert_eq!(idx.search("hello"), vec![rid(1, 0)]);
        assert_eq!(idx.search("world"), vec![rid(1, 0)]);
        assert_eq!(idx.search("HELLO"), vec![rid(1, 0)]);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let mut idx = InvertedIndex::new();
        idx.upsert(rid(1, 0), "some text");
        assert!(idx.search("   ").is_empty());
        assert!(idx.search("!!!").is_empty());
    }

    #[test]
    fn upsert_overwrites_previous_terms() {
        let mut idx = InvertedIndex::new();
        idx.upsert(rid(1, 0), "alpha");
        idx.upsert(rid(1, 0), "beta");
        assert!(idx.search("alpha").is_empty());
        assert_eq!(idx.search("beta"), vec![rid(1, 0)]);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn remove_drops_doc_from_all_postings() {
        let mut idx = InvertedIndex::new();
        idx.upsert(rid(1, 0), "shared term");
        idx.upsert(rid(2, 0), "shared other");
        idx.remove(rid(1, 0));
        assert_eq!(idx.search("shared"), vec![rid(2, 0)]);
        assert!(idx.search("term").is_empty());
    }
}
