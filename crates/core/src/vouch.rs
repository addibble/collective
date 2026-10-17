//! Newcomers: pending tasks and vouching.
//!
//! Review is the scarcest thing the network spends, so a patch's review
//! task opens only when its author has **standing** (trust at or above
//! [`MIN_AUTHOR_TRUST`], the same bar as proposing an environment) or when
//! an operator with standing **vouches** for it. Until then the task is
//! *pending*: the patch is stored, searchable and adoptable, but nobody is
//! assigned to review it. Adoption never opens a task, since adoptions can
//! be faked.
//!
//! Duty is still owed at submission, so a newcomer pays for its patch with
//! review work, the work earns ledger credit, the credit raises its trust,
//! and its pending task opens by standing. That loop needs no voucher.
//!
//! A vouch is a loan of [`VOUCH_LOAN_REP`], written to the ledger when it is
//! made, so the voucher's rep really is lower while the loan is out. It
//! comes back when the task resolves without the patch being found bad,
//! and is lost when it is found bad. It earns nothing either way: a vouch
//! is a favour, not income, so it can't be farmed.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::environment::MIN_PROPOSER_TRUST;
use crate::proof::RunOutcome;
use crate::LedgerEntry;

/// Trust (`score::trust`) an author needs for its patches' review tasks to
/// open at once. Newcomers start at 0.25; genesis influence counts.
pub const MIN_AUTHOR_TRUST: f64 = MIN_PROPOSER_TRUST;

/// Rep a voucher lends for one task while the loan is out.
pub const VOUCH_LOAN_REP: f64 = 5.0;

/// Ledger reasons for a loan's lifecycle.
pub const VOUCH_LOAN: &str = "vouch_loan";
pub const VOUCH_RETURNED: &str = "vouch_returned";

/// Whether an author with this trust gets its review task opened. Fails
/// closed on NaN.
pub fn task_opens(author_trust: f64) -> bool {
    author_trust >= MIN_AUTHOR_TRUST
}

/// Whether an operator may vouch for a pending task. `trust_after_loans` is
/// its trust computed on its rep net of every active loan *including this
/// one*, so it must still have standing once the loan is out; and the rep
/// it can actually lend (ledger rep, not genesis influence, which can't be
/// spent) must cover the loan. Fails closed on NaN. The caller also refuses
/// a voucher from the author's own operator ([`may_vouch_for`]).
pub fn may_vouch(trust_after_loans: f64, available_rep: f64) -> bool {
    task_opens(trust_after_loans) && available_rep >= VOUCH_LOAN_REP
}

/// Whether `voucher` may vouch for a patch by `author`: never its own operator's.
pub fn may_vouch_for(voucher: &str, author: &str) -> bool {
    voucher != author
}

/// Operators who may not review a task, nor run its proofs: its author and,
/// once vouched for, its voucher, who has a stake in it coming out clean.
pub fn interested(author: &str, voucher: Option<&str>) -> HashSet<String> {
    let extra: Vec<String> = voucher.into_iter().map(String::from).collect();
    crate::review::excluded(author, &extra)
}

/// What happens to a loan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VouchEvent {
    /// The vouch is made: the loan leaves the voucher's rep.
    Made,
    /// The task resolved and nothing found the patch bad: the loan comes back.
    Returned,
    /// The patch was found bad (a challenge on it resolved `problem`, e.g. a
    /// review-quorum finding or a proven regression): the loan is lost.
    Lost,
}

impl VouchEvent {
    /// How the task's loan is recorded once settled.
    pub fn as_str(self) -> &'static str {
        match self {
            VouchEvent::Made => "made",
            VouchEvent::Returned => "returned",
            VouchEvent::Lost => "lost",
        }
    }
}

/// The ledger entries for a loan event. A lost loan writes nothing: the
/// debit made when it was lent simply stands.
pub fn vouch_entries(event: VouchEvent, voucher: &str) -> Vec<LedgerEntry> {
    match event {
        VouchEvent::Made => vec![LedgerEntry::new(voucher, -VOUCH_LOAN_REP, VOUCH_LOAN)],
        VouchEvent::Returned => vec![LedgerEntry::new(voucher, VOUCH_LOAN_REP, VOUCH_RETURNED)],
        VouchEvent::Lost => vec![],
    }
}

/// The ledger reference of a patch's loan entries.
pub fn vouch_ref(patch_id: &str) -> String {
    format!("vouch:{patch_id}")
}

/// Where a task's review stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskReview {
    /// Reviews are still coming in, or a challenge they raised is open.
    Underway,
    /// The full panel found nothing that needed a challenge.
    Clean,
    /// A challenge the reviewers raised resolved `clean`.
    ChallengeCleared,
}

/// Where a task's runs stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRuns {
    /// The project has no test suite: review-only.
    NotOwed,
    /// Runs are owed and fewer than a panel's worth are in.
    Pending,
    /// The runs are graded: the majority outcome, or `None` without one.
    Settled(Option<RunOutcome>),
}

/// What is known about a vouched task when deciding its loan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VouchFacts {
    /// A challenge on the patch resolved `problem`.
    pub found_bad: bool,
    /// A challenge on the patch is unresolved.
    pub challenge_open: bool,
    pub review: TaskReview,
    pub runs: TaskRuns,
}

/// Whether a vouched task's loan settles now, and how; `None` while it
/// must stay out. Found bad by any challenge: lost. Otherwise it waits for
/// every open challenge, for the review, and, when the panel called the
/// patch clean, for its runs, since a regression majority opens a
/// challenge that a proof may then settle against it. A challenge the
/// reviewers raised that resolved `clean` has been adjudicated: the loan
/// comes back without waiting for runs, which reviewers who reported the
/// finding never owed.
pub fn settle_vouch(f: &VouchFacts) -> Option<VouchEvent> {
    if f.found_bad {
        return Some(VouchEvent::Lost);
    }
    if f.challenge_open {
        return None;
    }
    match (f.review, f.runs) {
        (TaskReview::Underway, _) => None,
        (TaskReview::ChallengeCleared, _) => Some(VouchEvent::Returned),
        (TaskReview::Clean, TaskRuns::Pending) => None,
        (TaskReview::Clean, TaskRuns::Settled(Some(RunOutcome::Regressed))) => None,
        (TaskReview::Clean, _) => Some(VouchEvent::Returned),
    }
}
