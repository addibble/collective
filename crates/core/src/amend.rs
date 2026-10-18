//! Amendments: a reviewer's fix on top of the patch it is reviewing.
//!
//! A reviewer who sees how the patch under review should be faster or
//! more correct can attach the fix instead of only describing it. The fix
//! is submitted as the whole patch with the fix applied, a diff against
//! the same base commit, so it reads, compares and stores like any patch;
//! what the amendment changes about the patch is derived from the two
//! ([`check_amendment`]). The rest of the panel endorses or rejects it
//! ([`decide`]). An accepted amendment becomes a derived patch
//! (`parent_id` set) credited to the amender, with its own review task;
//! the original keeps its share through the lineage. Nothing is merged
//! into the original: patches are immutable.
//!
//! What an amendment may be is narrow on purpose:
//!   - **performance**: a pure win in memory or in processing, never a
//!     trade of one for the other and never a change in behavior;
//!   - **correctness**: a behavior fix backed by an external reference (a
//!     specification, standard or upstream document) that says what the
//!     correct behavior is.
//!
//! Neither may add functionality. The server checks what it can: the
//! amended patch changes the same files, creates and deletes nothing the
//! patch doesn't, leaves binary files alone, differs from the patch by a
//! bounded number of lines, and carries evidence in the right shape. The
//! panel judges the rest, and the derived patch is reviewed and run like
//! any other, so an accepted amendment is still only a claim until then.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::diff::{normalized_change, FileDiff, Hunk};
use crate::review::{check_task_size, PARAMS as REVIEW};
use crate::LedgerEntry;

/// Most lines an amendment may change about the patch: the symmetric
/// difference of their changed lines. A fix is small; anything larger is
/// a different patch, which the amender submits as one.
pub const MAX_AMENDMENT_LINES: u32 = 200;
/// Most amendments one reviewer may attach to one task.
pub const MAX_AMENDMENTS_PER_REVIEWER: u32 = 3;
pub const MAX_REASONING_CHARS: usize = 2000;
pub const MAX_QUOTE_CHARS: usize = 1000;
pub const MAX_NOTE_CHARS: usize = 1000;

/// Rep for the amender when the panel accepts its amendment, and when it
/// rejects it. Acceptance pays less than a confirmed finding: the derived
/// patch still has to earn its place.
pub const ACCEPTED_REP: f64 = 3.0;
pub const REJECTED_REP: f64 = -1.0;
pub const ACCEPTED: &str = "amendment accepted";
pub const REJECTED: &str = "amendment rejected";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A pure win in memory or processing: no trade-off, no behavior change.
    Performance,
    /// A behavior fix that an external reference says is right.
    Correctness,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Performance => "performance",
            Category::Correctness => "correctness",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        match s {
            "performance" => Some(Category::Performance),
            "correctness" => Some(Category::Correctness),
            _ => None,
        }
    }
}

/// What a performance amendment improves. It must improve one without
/// costing the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Memory,
    Cpu,
}

/// An external document that says what the correct behavior is: a
/// specification, standard, RFC, or the upstream project's own docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub url: String,
    /// Section, clause or heading within it.
    #[serde(default)]
    pub section: String,
    /// The passage that validates the behavior, quoted.
    pub quote: String,
}

/// Why the amendment is right, in the shape its category demands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "category", rename_all = "snake_case")]
pub enum Evidence {
    Performance {
        resource: Resource,
        /// Why this is a pure win: what work or memory it saves, and why
        /// nothing else gets worse.
        reasoning: String,
    },
    Correctness {
        reference: Reference,
        /// What the patch gets wrong and how the reference settles it.
        reasoning: String,
    },
}

impl Evidence {
    pub fn category(&self) -> Category {
        match self {
            Evidence::Performance { .. } => Category::Performance,
            Evidence::Correctness { .. } => Category::Correctness,
        }
    }

    pub fn reasoning(&self) -> &str {
        match self {
            Evidence::Performance { reasoning, .. } | Evidence::Correctness { reasoning, .. } => {
                reasoning
            }
        }
    }
}

/// Whether the evidence has the shape its category demands. Only the
/// shape: nothing here fetches the reference or measures the win; the
/// panel reads both.
pub fn check_evidence(e: &Evidence) -> Result<(), String> {
    let reasoning = e.reasoning().trim();
    if reasoning.len() < 20 {
        return Err(
            "evidence needs `reasoning`: at least a sentence on why the amendment is right".into(),
        );
    }
    if reasoning.chars().count() > MAX_REASONING_CHARS {
        return Err(format!(
            "`reasoning` is limited to {MAX_REASONING_CHARS} characters"
        ));
    }
    if let Evidence::Correctness { reference, .. } = e {
        let url = reference.url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() < 12 {
            return Err("a correctness amendment needs `reference.url`: an http(s) link to the specification, standard or upstream document that says what the correct behavior is".into());
        }
        if url.len() > 2000 || url.chars().any(char::is_whitespace) {
            return Err("`reference.url` must be a single URL of at most 2000 characters".into());
        }
        let quote = reference.quote.trim();
        if quote.is_empty() {
            return Err("a correctness amendment needs `reference.quote`: the passage of the reference that validates the behavior".into());
        }
        if quote.chars().count() > MAX_QUOTE_CHARS {
            return Err(format!(
                "`reference.quote` is limited to {MAX_QUOTE_CHARS} characters"
            ));
        }
        if reference.section.chars().count() > 200 {
            return Err("`reference.section` is limited to 200 characters".into());
        }
    }
    Ok(())
}

/// What an amendment changes about the patch in one file: the patch's
/// hunks it drops or rewrites, and the hunks of the amended patch that
/// take their place. Hunks that only moved keep their place in neither.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileDelta {
    pub path: String,
    pub replaced: Vec<Hunk>,
    pub amended: Vec<Hunk>,
}

/// An amendment the server accepted as well-formed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Checked {
    pub deltas: Vec<FileDelta>,
    /// Lines the amendment changes about the patch.
    pub lines: u32,
}

/// A hunk's edits with whitespace collapsed and position dropped, so a
/// hunk that only moved because an earlier one grew still matches.
fn hunk_key(h: &Hunk) -> String {
    h.lines
        .iter()
        .filter_map(|l| normalized_change(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn counts<'a>(items: impl Iterator<Item = &'a str>) -> BTreeMap<&'a str, i64> {
    let mut m = BTreeMap::new();
    for i in items {
        *m.entry(i).or_default() += 1;
    }
    m
}

/// Items of `a` not matched by `b`, as a multiset.
fn surplus<'a>(a: &BTreeMap<&'a str, i64>, b: &BTreeMap<&'a str, i64>) -> i64 {
    a.iter()
        .map(|(k, n)| (n - b.get(k).copied().unwrap_or(0)).max(0))
        .sum()
}

/// The hunks, keyed by `keys`, that have no counterpart in `other`: each
/// key in `other` matches that many hunks, in order.
fn unmatched(hunks: &[Hunk], keys: &[String], other: &BTreeMap<&str, i64>) -> Vec<Hunk> {
    let mut seen: BTreeMap<&str, i64> = BTreeMap::new();
    hunks
        .iter()
        .zip(keys)
        .filter(|(_, k)| {
            let n = seen.entry(k.as_str()).or_default();
            *n += 1;
            *n > other.get(k.as_str()).copied().unwrap_or(0)
        })
        .map(|(h, _)| h.clone())
        .collect()
}

/// Checks that `amended` is `patch` with a fix on top, and says what the
/// fix is. The amended patch must change exactly the files the patch
/// changes, creating and deleting what it creates and deletes, with any
/// binary change carried through unchanged; it must differ from the patch
/// somewhere; and the difference, counted in changed lines, must fit
/// [`MAX_AMENDMENT_LINES`]. It is also held to the task size cap.
pub fn check_amendment(patch: &[FileDiff], amended: &[FileDiff]) -> Result<Checked, String> {
    check_task_size(amended)?;
    for f in amended {
        if !patch.iter().any(|p| p.path() == f.path()) {
            return Err(format!(
                "the amendment changes {}, which the patch does not: an amendment fixes the patch's own changes and adds nothing beside them",
                f.path()
            ));
        }
    }
    let mut deltas = Vec::new();
    let mut lines = 0i64;
    for p in patch {
        let path = p.path();
        let Some(a) = amended.iter().find(|a| a.path() == path) else {
            return Err(format!(
                "the amendment leaves {path} as it is in the base: an amendment fixes the patch's changes, it does not drop them"
            ));
        };
        if p.old_path.is_some() != a.old_path.is_some() {
            return Err(format!(
                "{path}: the patch and the amendment disagree on whether the file is created"
            ));
        }
        if p.new_path.is_some() != a.new_path.is_some() {
            return Err(format!(
                "{path}: the patch and the amendment disagree on whether the file is deleted"
            ));
        }
        match (&p.binary, &a.binary) {
            (None, None) => {}
            (Some(pb), Some(ab)) if pb.new_id == ab.new_id && pb.old_id == ab.old_id => continue,
            _ => {
                return Err(format!(
                    "{path}: binary changes can't be amended; carry the patch's through unchanged"
                ))
            }
        }
        let pk: Vec<String> = p.hunks.iter().map(hunk_key).collect();
        let ak: Vec<String> = a.hunks.iter().map(hunk_key).collect();
        let pc = counts(pk.iter().map(String::as_str));
        let ac = counts(ak.iter().map(String::as_str));
        let replaced = unmatched(&p.hunks, &pk, &ac);
        let amended = unmatched(&a.hunks, &ak, &pc);
        if replaced.is_empty() && amended.is_empty() {
            continue;
        }
        // Lines, not hunks: a one-line fix inside a large hunk costs one
        // line each way, not the whole hunk twice.
        let pl: Vec<String> = p
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|l| normalized_change(l))
            .collect();
        let al: Vec<String> = a
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|l| normalized_change(l))
            .collect();
        let (plc, alc) = (
            counts(pl.iter().map(String::as_str)),
            counts(al.iter().map(String::as_str)),
        );
        let differing = surplus(&plc, &alc) + surplus(&alc, &plc);
        // The same lines in another order, or split across hunks, is a
        // change to every hunk involved.
        lines += if differing > 0 {
            differing
        } else {
            replaced
                .iter()
                .chain(&amended)
                .flat_map(|h| &h.lines)
                .filter(|l| normalized_change(l).is_some())
                .count() as i64
        };
        deltas.push(FileDelta {
            path: path.to_string(),
            replaced,
            amended,
        });
    }
    // Hunks that differ only in position or whitespace are a change to
    // the diff but not to the code; they don't amend anything.
    if deltas.is_empty() {
        return Err(
            "the amendment is the patch itself (position and whitespace aside): nothing changes"
                .into(),
        );
    }
    if lines > MAX_AMENDMENT_LINES as i64 {
        return Err(format!(
            "the amendment changes {lines} lines of the patch; an amendment is a fix of at most {MAX_AMENDMENT_LINES}. A larger change is a patch of its own: submit it with parent_id"
        ));
    }
    Ok(Checked {
        deltas,
        lines: lines as u32,
    })
}

// ---------------------------------------------------------------------------
// The panel's verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The fix is what it claims: a pure win, or a behavior the reference requires.
    Endorse,
    /// It isn't: a trade-off, a behavior change, new functionality, or
    /// evidence that doesn't say what it claims.
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub voter: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for the panel.
    Pending,
    /// Endorsed by a quorum: a derived patch was created.
    Accepted,
    /// Rejected by a quorum: the amender loses a little rep.
    Rejected,
    /// The panel finished without a quorum either way.
    Lapsed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Accepted => "accepted",
            State::Rejected => "rejected",
            State::Lapsed => "lapsed",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        match s {
            "pending" => Some(State::Pending),
            "accepted" => Some(State::Accepted),
            "rejected" => Some(State::Rejected),
            "lapsed" => Some(State::Lapsed),
            _ => None,
        }
    }
}

/// Whether `voter` may vote on an amendment by `amender`: it must be on
/// the panel (its review of the task submitted, so it has read the patch)
/// and not the amender.
pub fn may_vote(voter: &str, amender: &str, panel: &[String]) -> Result<(), String> {
    if voter == amender {
        return Err("you can't vote on your own amendment".into());
    }
    if !panel.iter().any(|p| p == voter) {
        return Err("only reviewers whose review of this task is in vote on its amendments".into());
    }
    Ok(())
}

/// What the votes say. Accepted once a quorum (the same as for a review
/// finding) endorses it and endorsements outnumber rejections; rejected
/// once a quorum rejects it and rejections outnumber endorsements;
/// otherwise pending until the panel is done, when it lapses. A tie
/// lapses: it is no verdict, so it costs the amender nothing. Each voter
/// counts once.
pub fn decide(votes: &[Vote], panel_done: bool) -> State {
    let mut seen: Vec<&str> = Vec::new();
    let (mut endorse, mut reject) = (0usize, 0usize);
    for v in votes {
        if seen.contains(&v.voter.as_str()) {
            continue;
        }
        seen.push(&v.voter);
        match v.verdict {
            Verdict::Endorse => endorse += 1,
            Verdict::Reject => reject += 1,
        }
    }
    let q = REVIEW.quorum;
    if endorse >= q && endorse > reject {
        State::Accepted
    } else if reject >= q && reject > endorse {
        State::Rejected
    } else if panel_done {
        State::Lapsed
    } else {
        State::Pending
    }
}

/// The ledger entries a verdict writes for the amender.
pub fn entries(state: State, amender: &str) -> Vec<LedgerEntry> {
    match state {
        State::Accepted => vec![LedgerEntry::new(amender, ACCEPTED_REP, ACCEPTED)],
        State::Rejected => vec![LedgerEntry::new(amender, REJECTED_REP, REJECTED)],
        State::Pending | State::Lapsed => vec![],
    }
}

/// The derived patch's title.
pub fn amended_title(title: &str, category: Category) -> String {
    let suffix = format!(" (amended: {})", category.as_str());
    let keep = 200usize.saturating_sub(suffix.len());
    let head: String = title.trim().chars().take(keep).collect();
    format!("{head}{suffix}")
}
