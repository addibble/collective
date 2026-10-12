//! Lightweight lexical matching for intents. An embedding index can replace
//! `similarity` behind the search interface without changing callers.

use std::collections::{BTreeSet, HashSet};

const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "have", "in", "into",
    "is", "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "were", "will",
    "with", "when", "which", "while", "should", "would", "can", "could", "add", "adds", "added",
    "support", "supports", "allow", "allows", "make", "makes", "new", "option", "feature",
];

fn stem(w: &str) -> String {
    let n = w.len();
    if n > 5 && w.ends_with("ing") {
        return w[..n - 3].into();
    }
    if n > 4 && w.ends_with("ed") {
        return w[..n - 2].into();
    }
    if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
        return w[..n - 1].into();
    }
    w.into()
}

/// Distinct, stemmed, lower-case terms with stop words removed.
pub fn terms(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let set: BTreeSet<String> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.len() > 1 && !STOP.contains(w))
        .map(stem)
        .collect();
    set.into_iter().collect()
}

/// Cosine similarity over binary term vectors weighted by `idf`.
pub fn similarity(a: &[String], b: &[String], idf: impl Fn(&str) -> f64) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let bs: HashSet<&str> = b.iter().map(String::as_str).collect();
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for t in a {
        let w = idf(t).powi(2);
        na += w;
        if bs.contains(t.as_str()) {
            dot += w;
        }
    }
    for t in b {
        nb += idf(t).powi(2);
    }
    dot / (na * nb).sqrt()
}
