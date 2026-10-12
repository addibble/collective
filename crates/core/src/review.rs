//! Review duty: participation is paid for with useful compute.
//!
//! Every operator that submits or adopts patches owes review work. Each
//! patch is one review task, reviewed whole and independently by several
//! agents that cannot choose what they get. A task has two phases, in
//! order: **review** the code, then **run** the project's test suite with
//! the patch in a sandbox. Reading first is the point: nobody should run
//! untrusted code they haven't looked at, and a reviewer who thinks the
//! code is dangerous says where instead of running it. Patches are capped
//! at [`ReviewParams::max_task_lines`] so one task stays one sitting;
//! larger changes must be split. Reviews are graded against:
//!   - canaries: planted, inert, known-bad shards (Truebit's forced errors),
//!     which give an objective score for every reviewer;
//!   - resolved challenges: when a finding is confirmed or rejected, the
//!     reviewers who saw (or missed) it are paid (or slashed).
//!
//! Runs are graded against canaries whose outcome is known, and against
//! the other runners of the same patch (see [`grade_runs`]).
//!
//! Two or more independent reviewers reporting the same lines open a
//! challenge on the patch (see `challenge`). A lone serious report widens
//! the review instead (more reviewers, as a Kleros appeal widens a jury).
//! Authors are only penalized through a resolved challenge, never by raw
//! review votes, so colluding reviewers cannot punish a rival directly.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::challenge::Severity;
use crate::diff::{FileDiff, Hunk};
use crate::hash::{sha256_hex, SeededRng};
use crate::proof::RunOutcome;
use crate::LedgerEntry;

pub struct ReviewParams {
    /// Independent reviewers (and runners) per task.
    pub redundancy: usize,
    /// Most changed lines a patch may have: one task is one review and one
    /// sandbox run, done in one sitting. Larger changes must be split.
    pub max_task_lines: u32,
    /// Changed lines per work unit. A task is worth ⌈lines / this⌉ units
    /// (1 to 4), since reading and checking scale with size while sandbox
    /// setup is a fixed cost.
    pub lines_per_unit: u32,
    /// Share of review assignments that are canaries, in percent, whenever
    /// canaries are available (see [`pick_task`]). Above about 6% a
    /// reviewer who calls everything clean loses rep on average.
    pub canary_percent: u32,
    /// Units owed per adoption reported. A submission owes the units its
    /// review will take (see [`duty_required`]).
    pub per_adoption: u32,
    /// Units a new operator may skip: enough for one first small patch.
    pub free_units: u32,
    /// Distinct reviewers needed on overlapping lines to open a challenge.
    pub quorum: usize,
    /// Lines of slack when matching reported locations.
    pub slack: u32,
    /// How long a reviewer holds a task before it is offered to someone else.
    pub lease_ms: i64,
    /// Extra time to submit the run once the review is in (dependency
    /// fetch, build and the suite).
    pub run_lease_ms: i64,
    // Grading, in rep.
    pub completion: f64,
    pub canary_catch: f64,
    pub canary_miss: f64,
    pub confirmed_hit: f64,
    pub confirmed_miss: f64,
    pub false_positive: f64,
    pub run_canary_correct: f64,
    pub run_canary_misreported: f64,
    pub run_disagreed: f64,
}

pub const PARAMS: ReviewParams = ReviewParams {
    redundancy: 3,
    max_task_lines: 2000,
    lines_per_unit: 500,
    canary_percent: 10,
    per_adoption: 1,
    free_units: 4,
    quorum: 2,
    slack: 2,
    lease_ms: 30 * 60_000,
    run_lease_ms: 2 * 60 * 60_000,
    completion: 1.0,
    canary_catch: 25.0,
    canary_miss: -15.0,
    confirmed_hit: 10.0,
    confirmed_miss: -10.0,
    false_positive: -3.0,
    run_canary_correct: 2.0,
    run_canary_misreported: -15.0,
    run_disagreed: -5.0,
};

// ---------------------------------------------------------------------------
// Duty
// ---------------------------------------------------------------------------

/// Work units a submission of `units` units costs the network: each task
/// gets `redundancy` reviewers, and canaries take `canary_percent` of all
/// assignments on top.
pub fn review_cost(units: u32) -> u32 {
    let p = &PARAMS;
    let real = units as u64 * p.redundancy as u64 * 100;
    let share = (100 - p.canary_percent) as u64;
    real.div_ceil(share) as u32
}

/// Work units required for this much activity: the units of the patches
/// an operator has submitted and the adoptions it has reported.
pub fn duty_required(units: u32, adoptions: u32) -> u32 {
    (review_cost(units) + adoptions * PARAMS.per_adoption).saturating_sub(PARAMS.free_units)
}

/// Work units still owed before the operator may submit or adopt again.
/// `credited` sums the units of completed tasks that were not graded as failures.
pub fn duty_owed(units: u32, adoptions: u32, credited: u32) -> u32 {
    duty_required(units, adoptions).saturating_sub(credited)
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

pub fn task_id(patch_id: &str) -> String {
    sha256_hex(format!("task:{patch_id}"))[..24].to_string()
}

fn changed(h: &Hunk) -> u32 {
    h.lines
        .iter()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .count() as u32
}

/// Changed lines in a patch: what a reviewer has to read.
pub fn changed_lines(files: &[FileDiff]) -> u32 {
    files.iter().flat_map(|f| &f.hunks).map(changed).sum()
}

/// A patch's changed lines, or why it is too large to be one task.
pub fn check_task_size(files: &[FileDiff]) -> Result<u32, String> {
    let n = changed_lines(files);
    if n > PARAMS.max_task_lines {
        return Err(format!(
            "this patch changes {n} lines; one review task covers at most {}. Split it into smaller patches that each make sense on their own",
            PARAMS.max_task_lines
        ));
    }
    Ok(n)
}

/// Work units one task of `lines` changed lines is worth.
pub fn task_units(lines: u32) -> u32 {
    lines.div_ceil(PARAMS.lines_per_unit).max(1)
}

/// Whether a reviewer must also run the patch: when the project has a
/// test suite to run, unless the reviewer reported something serious. A
/// reviewer who believes the code is dangerous says where instead of
/// running it, and that report goes to the quorum like any other.
pub fn run_required(issues: &[Issue], project_runnable: bool) -> bool {
    project_runnable && !issues.iter().any(Issue::serious)
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
    /// Serious enough to widen review, or to decline running the code.
    pub fn serious(&self) -> bool {
        self.severity >= Severity::High || self.kind == IssueKind::Backdoor
    }

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

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// One runner's reported outcome for a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunClaim {
    pub runner: String,
    pub outcome: RunOutcome,
}

/// The outcome most runners agree on: at least `quorum` conclusive claims,
/// and more than any other outcome. `None` while there's no such majority.
pub fn run_majority(claims: &[RunClaim]) -> Option<RunOutcome> {
    let mut counts: Vec<(RunOutcome, usize)> = Vec::new();
    for c in claims.iter().filter(|c| c.outcome.conclusive()) {
        match counts.iter_mut().find(|(o, _)| *o == c.outcome) {
            Some((_, n)) => *n += 1,
            None => counts.push((c.outcome, 1)),
        }
    }
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    match counts.as_slice() {
        [(o, n), rest @ ..] if *n >= PARAMS.quorum && rest.first().is_none_or(|r| r.1 < *n) => {
            Some(*o)
        }
        _ => None,
    }
}

/// Grades runs. On a canary the outcome is known (`expected`), so a runner
/// who reports anything else either didn't run it or lied: that costs as
/// much as missing the canary in review, and the task's credit. On a real
/// patch, a runner who contradicts the majority of the others loses a
/// little. Inconclusive runs earn nothing and cost nothing; they just don't
/// complete the task.
pub fn grade_runs(expected: Option<RunOutcome>, claims: &[RunClaim]) -> Vec<LedgerEntry> {
    let p = &PARAMS;
    let mut out = Vec::new();
    let truth = expected.or_else(|| run_majority(claims));
    for c in claims.iter().filter(|c| c.outcome.conclusive()) {
        match (expected, truth) {
            (Some(e), _) if c.outcome == e => out.push(LedgerEntry::new(
                &c.runner,
                p.run_canary_correct,
                "ran a canary correctly",
            )),
            (Some(_), _) => out.push(LedgerEntry::new(
                &c.runner,
                p.run_canary_misreported,
                "misreported a canary run",
            )),
            (None, Some(m)) if c.outcome != m => out.push(LedgerEntry::new(
                &c.runner,
                p.run_disagreed,
                "run disagreed with the other runners",
            )),
            _ => {}
        }
    }
    out
}

/// Whether a submitted run completes its task: it was conclusive and not
/// graded as misreported.
pub fn run_completes(outcome: RunOutcome, entries: &[LedgerEntry], runner: &str) -> bool {
    outcome.conclusive()
        && !entries
            .iter()
            .any(|e| e.operator_id == runner && e.reason == "misreported a canary run")
}
