//! Review duty: participation is paid for with useful compute.
//!
//! Every operator that submits or adopts patches owes reviews. Patches are
//! split into small shards, each reviewed independently by several agents
//! that cannot choose what they get. Reviews are graded against:
//!   - canaries: planted, inert, known-bad shards (Truebit's forced errors),
//!     which give an objective score for every reviewer;
//!   - resolved challenges: when a finding is confirmed or rejected, the
//!     reviewers who saw (or missed) it are paid (or slashed).
//!
//! Two or more independent reviewers reporting the same lines open a
//! challenge on the patch (see `challenge`). A lone serious report widens
//! the review instead (more reviewers, as a Kleros appeal widens a jury).
//! Authors are only penalized through a resolved challenge, never by raw
//! review votes, so colluding reviewers cannot punish a rival directly.

use std::collections::HashSet;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::challenge::Severity;
use crate::diff::{FileDiff, Hunk};
use crate::hash::{sha256_hex, SeededRng};
use crate::LedgerEntry;

pub struct ReviewParams {
    /// Independent reviewers per shard.
    pub redundancy: usize,
    /// Changed lines per shard, so a review is a bounded unit of work.
    pub max_shard_lines: u32,
    /// Share of review assignments that are canaries, in percent, whenever
    /// canaries are available (see [`pick_task`]). Above about 6% a
    /// reviewer who calls everything clean loses rep on average.
    pub canary_percent: u32,
    /// Reviews owed per adoption reported. A submission owes the reviews its
    /// shards will take (see [`duty_required`]).
    pub per_adoption: u32,
    /// Reviews a new operator may skip: enough for one single-shard patch.
    pub free_reviews: u32,
    /// Distinct reviewers needed on overlapping lines to open a challenge.
    pub quorum: usize,
    /// Lines of slack when matching reported locations.
    pub slack: u32,
    /// How long a reviewer holds a task before it is offered to someone else.
    pub lease_ms: i64,
    // Grading, in rep.
    pub completion: f64,
    pub canary_catch: f64,
    pub canary_miss: f64,
    pub confirmed_hit: f64,
    pub confirmed_miss: f64,
    pub false_positive: f64,
}

pub const PARAMS: ReviewParams = ReviewParams {
    redundancy: 3,
    max_shard_lines: 200,
    canary_percent: 10,
    per_adoption: 1,
    free_reviews: 4,
    quorum: 2,
    slack: 2,
    lease_ms: 30 * 60_000,
    completion: 1.0,
    canary_catch: 25.0,
    canary_miss: -15.0,
    confirmed_hit: 10.0,
    confirmed_miss: -10.0,
    false_positive: -3.0,
};

// ---------------------------------------------------------------------------
// Duty
// ---------------------------------------------------------------------------

/// Reviews a submission of `shards` shards costs the network: each shard
/// gets `redundancy` reviewers, and canaries take `canary_percent` of all
/// assignments on top. Charging per shard keeps large patches from
/// starving the queue.
pub fn shard_cost(shards: u32) -> u32 {
    let p = &PARAMS;
    let real = shards as u64 * p.redundancy as u64 * 100;
    let share = (100 - p.canary_percent) as u64;
    real.div_ceil(share) as u32
}

/// Reviews required for this much activity: the shards an operator has
/// submitted and the adoptions it has reported.
pub fn duty_required(shards: u32, adoptions: u32) -> u32 {
    (shard_cost(shards) + adoptions * PARAMS.per_adoption).saturating_sub(PARAMS.free_reviews)
}

/// Reviews still owed before the operator may submit or adopt again.
/// `credited` counts completed reviews that were not graded as failures.
pub fn duty_owed(shards: u32, adoptions: u32, credited: u32) -> u32 {
    duty_required(shards, adoptions).saturating_sub(credited)
}

// ---------------------------------------------------------------------------
// Sharding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShardSpec {
    pub index: u32,
    pub path: String,
    /// Hunk indices within the file's diff.
    pub hunks: Range<usize>,
    pub changed_lines: u32,
}

pub fn shard_id(patch_id: &str, index: u32) -> String {
    sha256_hex(format!("shard:{patch_id}:{index}"))[..24].to_string()
}

fn changed(h: &Hunk) -> u32 {
    h.lines
        .iter()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .count() as u32
}

/// Splits a patch into review shards: consecutive hunks of one file, up to
/// `max_lines` changed lines each. A single larger hunk is its own shard.
pub fn shard_patch(files: &[FileDiff], max_lines: u32) -> Vec<ShardSpec> {
    let mut out = Vec::new();
    for f in files {
        let mut start = 0;
        let mut lines = 0;
        for (i, h) in f.hunks.iter().enumerate() {
            let n = changed(h);
            if i > start && lines + n > max_lines {
                out.push(ShardSpec {
                    index: out.len() as u32,
                    path: f.path().into(),
                    hunks: start..i,
                    changed_lines: lines,
                });
                start = i;
                lines = 0;
            }
            lines += n;
        }
        if start < f.hunks.len() {
            out.push(ShardSpec {
                index: out.len() as u32,
                path: f.path().into(),
                hunks: start..f.hunks.len(),
                changed_lines: lines,
            });
        }
    }
    out
}

/// The diff a reviewer sees for one shard.
pub fn shard_diff(files: &[FileDiff], spec: &ShardSpec) -> Option<Vec<FileDiff>> {
    let f = files.iter().find(|f| f.path() == spec.path)?;
    let mut sub = f.clone();
    sub.hunks = f.hunks.get(spec.hunks.clone())?.to_vec();
    Some(vec![sub])
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct OpenShard {
    pub id: String,
    pub author_operator: String,
    /// Operators currently holding or having completed this shard.
    pub assigned: Vec<String>,
    pub wanted: usize,
    pub canary: bool,
}

/// Picks a task for `operator` among the shards it may review. The seed
/// must be unpredictable to the reviewer (e.g. a server secret, the
/// operator and the current minute) so nobody can steer itself onto a
/// friend's or rival's patch.
///
/// A canary comes up with probability at least `canary_percent` whenever
/// one is eligible, however many real shards are waiting, so that calling
/// everything clean never pays; otherwise the pick is uniform.
pub fn pick_task<'a>(operator: &str, open: &'a [OpenShard], seed: &str) -> Option<&'a OpenShard> {
    let (canaries, real): (Vec<&OpenShard>, Vec<&OpenShard>) = open
        .iter()
        .filter(|s| {
            s.author_operator != operator
                && !s.assigned.iter().any(|a| a == operator)
                && s.assigned.len() < s.wanted
        })
        .partition(|s| s.canary);
    let mut rng = SeededRng::new(seed);
    let total = (canaries.len() + real.len()) as f64;
    let p_canary = (canaries.len() as f64 / total).max(PARAMS.canary_percent as f64 / 100.0);
    let pool = if real.is_empty() || (!canaries.is_empty() && rng.next_f64() < p_canary) {
        canaries
    } else {
        real
    };
    let i = rng.weighted_index(&vec![1.0; pool.len()])?;
    Some(pool[i])
}

// ---------------------------------------------------------------------------
// Reviews and findings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Bug,
    Vulnerability,
    Backdoor,
    Other,
}

/// A reported problem. Lines are new-side line numbers in the patched file
/// (old-side for pure deletions), inclusive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub kind: IssueKind,
    pub severity: Severity,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

impl Issue {
    pub fn location(&self) -> Location {
        Location {
            path: self.path.clone(),
            start: self.start,
            end: self.end,
        }
    }
}

impl Location {
    pub fn overlaps(&self, other: &Location, slack: u32) -> bool {
        self.path == other.path
            && self.start.saturating_sub(slack) <= other.end + slack
            && other.start.saturating_sub(slack) <= self.end + slack
    }
}

/// One reviewer's verdict on a shard. No issues means "clean".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub location: Location,
    pub reporters: Vec<String>,
    pub severity: Severity,
    pub kinds: Vec<IssueKind>,
}

/// Clusters overlapping issues across reviewers.
pub fn findings(reviews: &[Review]) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for r in reviews {
        for issue in &r.issues {
            let loc = issue.location();
            match out
                .iter_mut()
                .find(|f| f.location.overlaps(&loc, PARAMS.slack))
            {
                Some(f) => {
                    f.location.start = f.location.start.min(loc.start);
                    f.location.end = f.location.end.max(loc.end);
                    if !f.reporters.contains(&r.reviewer) {
                        f.reporters.push(r.reviewer.clone());
                    }
                    f.severity = f.severity.max(issue.severity);
                    if !f.kinds.contains(&issue.kind) {
                        f.kinds.push(issue.kind);
                    }
                }
                None => out.push(Finding {
                    location: loc,
                    reporters: vec![r.reviewer.clone()],
                    severity: issue.severity,
                    kinds: vec![issue.kind],
                }),
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum NextStep {
    /// Waiting for more reviews.
    Pending,
    /// Independent reviewers agree on a problem: open a challenge (quarantine).
    Challenge { finding: Finding },
    /// A lone serious report: widen the review before deciding.
    Widen { wanted: usize, finding: Finding },
    /// Nothing serious found by the full panel.
    Clean,
}

/// What to do with a shard once reviews come in.
pub fn next_step(reviews: &[Review], wanted: usize) -> NextStep {
    let fs = findings(reviews);
    if let Some(f) = fs
        .iter()
        .filter(|f| f.reporters.len() >= PARAMS.quorum)
        .max_by_key(|f| f.severity)
    {
        return NextStep::Challenge { finding: f.clone() };
    }
    if reviews.len() < wanted {
        return NextStep::Pending;
    }
    let serious =
        |f: &&Finding| f.severity >= Severity::High || f.kinds.contains(&IssueKind::Backdoor);
    if let Some(f) = fs.iter().find(serious) {
        if wanted < 2 * PARAMS.redundancy + 1 {
            return NextStep::Widen {
                wanted: 2 * wanted + 1,
                finding: f.clone(),
            };
        }
    }
    NextStep::Clean
}

// ---------------------------------------------------------------------------
// Grading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Truth {
    /// A planted, known-bad shard.
    Canary { planted: Vec<Location> },
    /// Findings on a real shard after their challenges resolved.
    Resolved {
        confirmed: Vec<Location>,
        rejected: Vec<Location>,
    },
    /// The panel found nothing and nothing was challenged within the window.
    Unchallenged,
}

/// Grades every review of one shard. A reviewer who misses a canary or a
/// confirmed problem loses rep and gets no completion credit, so a lazy
/// "looks fine" never pays.
pub fn grade(truth: &Truth, reviews: &[Review]) -> Vec<LedgerEntry> {
    let p = &PARAMS;
    let mut out = Vec::new();
    for r in reviews {
        let hit = |loc: &Location| r.issues.iter().any(|i| i.location().overlaps(loc, p.slack));
        let mut failed = false;
        match truth {
            Truth::Canary { planted } => {
                if planted.iter().all(hit) {
                    out.push(LedgerEntry::new(
                        &r.reviewer,
                        p.canary_catch,
                        "caught a canary",
                    ));
                } else {
                    out.push(LedgerEntry::new(
                        &r.reviewer,
                        p.canary_miss,
                        "missed a canary",
                    ));
                    failed = true;
                }
            }
            Truth::Resolved {
                confirmed,
                rejected,
            } => {
                for loc in confirmed {
                    if hit(loc) {
                        out.push(LedgerEntry::new(
                            &r.reviewer,
                            p.confirmed_hit,
                            "found a confirmed problem",
                        ));
                    } else {
                        out.push(LedgerEntry::new(
                            &r.reviewer,
                            p.confirmed_miss,
                            "missed a confirmed problem",
                        ));
                        failed = true;
                    }
                }
                for _ in rejected.iter().filter(|l| hit(l)) {
                    out.push(LedgerEntry::new(
                        &r.reviewer,
                        p.false_positive,
                        "reported a rejected problem",
                    ));
                }
            }
            Truth::Unchallenged => {}
        }
        if !failed {
            out.push(LedgerEntry::new(
                &r.reviewer,
                p.completion,
                "review completed",
            ));
        }
    }
    out
}

/// Whether a graded review counts towards the reviewer's duty.
pub fn credits_duty(entries: &[LedgerEntry], reviewer: &str) -> bool {
    entries
        .iter()
        .any(|e| e.operator_id == reviewer && e.reason == "review completed")
}

/// Operators that should not review a shard: its author, plus any the caller adds.
pub fn excluded(author_operator: &str, extra: &[String]) -> HashSet<String> {
    let mut s: HashSet<String> = extra.iter().cloned().collect();
    s.insert(author_operator.into());
    s
}
