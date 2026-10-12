//! Convergence: fold the most-used compatible patches for one base commit
//! into a single "trunk" diff. Greedy in score order:
//!   - only patches above the score and adopter thresholds;
//!   - one patch per intent (alternatives compete, they don't stack);
//!   - skip a patch sharing a lineage with one already chosen;
//!   - skip a patch touching lines a chosen patch touches.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::diff::{combine, find_conflict, format_diff, FileDiff};

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub intent_id: String,
    pub parent_id: Option<String>,
    pub score: f64,
    pub adopters: u32,
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Copy)]
pub struct ConvergeOptions {
    pub min_score: f64,
    pub min_adopters: u32,
}

impl Default for ConvergeOptions {
    fn default() -> Self {
        Self {
            min_score: 1.0,
            min_adopters: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub patch_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trunk {
    pub patch_ids: Vec<String>,
    pub skipped: Vec<Skipped>,
    pub diff: String,
}

pub fn converge(candidates: &[Candidate], opts: ConvergeOptions) -> Trunk {
    let mut sorted: Vec<&Candidate> = candidates.iter().collect();
    sorted.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.adopters.cmp(&a.adopters))
            .then(a.id.cmp(&b.id))
    });
    let parent_of: HashMap<&str, Option<&str>> = candidates
        .iter()
        .map(|c| (c.id.as_str(), c.parent_id.as_deref()))
        .collect();
    let lineage = |id: &str| {
        let mut out = HashSet::new();
        let mut p = parent_of.get(id).copied().flatten();
        while let Some(x) = p {
            if !out.insert(x.to_string()) {
                break;
            }
            p = parent_of.get(x).copied().flatten();
        }
        out
    };

    let mut chosen: Vec<&Candidate> = Vec::new();
    let mut intents: HashSet<&str> = HashSet::new();
    let mut accepted: Vec<FileDiff> = Vec::new();
    let mut skipped = Vec::new();
    let mut skip = |c: &Candidate, reason: String| {
        skipped.push(Skipped {
            patch_id: c.id.clone(),
            reason,
        })
    };

    for c in sorted {
        if c.score < opts.min_score || c.adopters < opts.min_adopters {
            skip(
                c,
                format!(
                    "below threshold (score {}, adopters {})",
                    c.score, c.adopters
                ),
            );
            continue;
        }
        let mine = lineage(&c.id);
        if let Some(r) = chosen
            .iter()
            .find(|x| mine.contains(&x.id) || lineage(&x.id).contains(&c.id))
        {
            skip(c, format!("same lineage as {}, which ranks higher", r.id));
            continue;
        }
        if intents.contains(c.intent_id.as_str()) {
            skip(
                c,
                format!(
                    "intent {} already served by a higher-ranked patch",
                    c.intent_id
                ),
            );
            continue;
        }
        if let Some(conflict) = find_conflict(&accepted, &c.files) {
            skip(
                c,
                format!("conflicts in {}: {}", conflict.path, conflict.reason),
            );
            continue;
        }
        accepted = combine(&[&accepted, &c.files]);
        intents.insert(&c.intent_id);
        chosen.push(c);
    }

    Trunk {
        patch_ids: chosen.iter().map(|c| c.id.clone()).collect(),
        skipped,
        diff: if accepted.is_empty() {
            String::new()
        } else {
            format_diff(&accepted)
        },
    }
}
