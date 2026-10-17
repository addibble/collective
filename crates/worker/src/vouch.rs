//! Newcomers: pending tasks and vouching, over D1. The rules (who has
//! standing, who may vouch, when a loan comes back) are in
//! collective_core::vouch; this module looks things up and writes them.
//!
//! A real patch's review task is created `pending` when its author lacks
//! standing. It opens when the author earns standing (the sweep in
//! `review::next_task` calls [`open_by_standing`]) or when an operator with
//! standing vouches for it ([`vouch`]), lending rep that comes back when
//! the task resolves without the patch being found bad ([`settle`]).

use collective_core::review::task_id;
use collective_core::vouch::{
    may_vouch, may_vouch_for, settle_vouch, task_opens, vouch_entries, vouch_ref, TaskReview,
    TaskRuns, VouchEvent, VouchFacts, MIN_AUTHOR_TRUST, VOUCH_LOAN, VOUCH_LOAN_REP,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::db::{placeholders, MAX_PARAMS};
use crate::error::{status, ApiResult};
use crate::review::{load_shard, parse_outcome, project_run_spec, ShardRow};
use crate::store::{now, random_hex, Agent, Store};

/// Most distinct pending authors one sweep looks at.
pub const SWEEP_AUTHORS: usize = 50;

/// What an agent is told about a pending task, wherever the patch is shown.
pub const PENDING_NOTE: &str = "Review pending: the patch is stored and searchable now, but its review task opens only when the author's operator has standing \
(trust 0.5; newcomers start at 0.25 and earn it with review tasks done well, collective_review_next, and proof runs that agree with other runners, `collective prove`), \
or when an operator with standing vouches for it (collective_vouch, POST /v1/patches/:id/vouch), lending 5 rep that it loses if the patch is found bad.";

/// The state a new real task starts in: open for an author with standing,
/// pending otherwise, including when the author's trust couldn't be read.
pub fn initial_state(author_trust: Option<f64>) -> &'static str {
    match author_trust {
        Some(t) if task_opens(t) => "open",
        _ => "pending",
    }
}

/// Adds the review state to a patch view, with the explanation when pending.
pub fn annotate(view: &mut Value, state: &str) {
    view["review"] = json!(state);
    if state == "pending" {
        view["review_note"] = json!(PENDING_NOTE);
    }
}

/// The state of a patch's review task, if it has one.
pub async fn review_state(store: &Store, patch_id: &str) -> ApiResult<Option<String>> {
    #[derive(Deserialize)]
    struct S {
        state: String,
    }
    Ok(store
        .db
        .first::<S>(
            "SELECT state FROM shards WHERE id = ?",
            args![task_id(patch_id)],
        )
        .await?
        .map(|s| s.state))
}

/// The operator that vouched for a patch's task, if any: an interested
/// party, so it doesn't run the patch's proofs either.
pub async fn voucher(store: &Store, patch_id: &str) -> ApiResult<Option<String>> {
    #[derive(Deserialize)]
    struct V {
        vouched_by: Option<String>,
    }
    Ok(store
        .db
        .first::<V>(
            "SELECT vouched_by FROM shards WHERE id = ?",
            args![task_id(patch_id)],
        )
        .await?
        .and_then(|v| v.vouched_by))
}

fn vouch_view(shard: &ShardRow) -> Value {
    json!({
        "patch_id": shard.patch_id,
        "task_id": shard.id,
        "review": shard.state,
        "vouched_by": shard.vouched_by,
        "vouched_at": shard.vouched_at.map(|t| t as i64),
        "loan": VOUCH_LOAN_REP,
        "message": format!("You lent {VOUCH_LOAN_REP} rep (ledger `{VOUCH_LOAN}`) and the task is open for review. It comes back when the task resolves \
    without the patch being found bad, and is lost if a challenge on it resolves `problem`. Vouching earns nothing. \
    You won't be assigned this task, nor its proofs."),
    })
}

/// Vouches for a patch's pending task (`POST /v1/patches/:id/vouch`,
/// `collective_vouch`): lends [`VOUCH_LOAN_REP`] and opens the task.
/// Idempotent for the same voucher.
pub async fn vouch(store: &Store, agent: &Agent, patch_id: &str) -> ApiResult<Value> {
    let patch = store.require_patch(patch_id).await?;
    let me = &agent.operator_id;
    let shard_id = task_id(&patch.id);
    let shard = match store
        .db
        .first::<ShardRow>("SELECT * FROM shards WHERE id = ?", args![&shard_id])
        .await?
    {
        Some(s) => s,
        None => return status(404, "this patch has no review task"),
    };
    if shard.vouched_by.as_deref() == Some(me.as_str()) {
        return Ok(vouch_view(&shard));
    }
    if shard.state != "pending" {
        return status(
            409,
            format!(
                "this patch's review task is not pending (it is {}): only a task waiting for its author's standing can be vouched for",
                shard.state
            ),
        );
    }
    if !may_vouch_for(me, &patch.author_op) {
        return status(
            403,
            "you can't vouch for your own operator's patch: its task opens by itself once your operator has standing. \
Earn it with review tasks done well (collective_review_next) and proof runs that agree with the other runners (`collective prove`)",
        );
    }
    let trust_after = store
        .trust_map_adjusted(std::slice::from_ref(me), -VOUCH_LOAN_REP)
        .await?
        .get(me)
        .copied()
        .unwrap_or(f64::NAN);
    let available = store.balance(me).await?;
    if !may_vouch(trust_after, available) {
        return status(
            403,
            format!(
                "vouching needs standing and rep to lend: with this loan out your operator's trust would be {trust_after:.2} (vouching needs {MIN_AUTHOR_TRUST}), \
and it has {available:.2} rep to lend (a vouch lends {VOUCH_LOAN_REP}; genesis influence counts towards trust but can't be lent). \
Trust is your operator's standing, from its rep: newcomers start at 0.25 and earn the rest through review tasks done well (collective_review_next), \
proof runs that agree with the other runners (`collective prove`) and patches other operators adopt. A vouch spends other operators' review work, \
so it is open to operators who have shown they do work that holds up."
            ),
        );
    }
    // Claim the task and lend in one transaction: the loan is written only
    // if this voucher holds the task and hasn't been debited for it yet, so
    // concurrent vouches neither open a task twice nor charge twice.
    let t = now();
    let reference = vouch_ref(&patch.id);
    let loan = &vouch_entries(VouchEvent::Made, me)[0];
    store
        .db
        .batch(vec![
            store.db.stmt(
                "UPDATE shards SET state = 'open', vouched_by = ?, vouched_at = ?
                 WHERE id = ? AND state = 'pending' AND vouched_by IS NULL AND canary = 0",
                args![me, t, &shard_id],
            )?,
            store.db.stmt(
                "INSERT INTO ledger (operator_id, delta, weight, reason, ref, created_at)
                 SELECT ?, ?, ?, ?, ?, ?
                 WHERE EXISTS (SELECT 1 FROM shards WHERE id = ? AND vouched_by = ?)
                   AND NOT EXISTS (SELECT 1 FROM ledger WHERE operator_id = ? AND reason = ? AND ref = ?)",
                args![
                    me,
                    loan.delta,
                    collective_core::score::ledger_weight(loan.delta, t),
                    &loan.reason,
                    &reference,
                    t,
                    &shard_id,
                    me,
                    me,
                    &loan.reason,
                    &reference
                ],
            )?,
        ])
        .await?;
    let shard = load_shard(store, &shard_id).await?;
    if shard.vouched_by.as_deref() != Some(me.as_str()) {
        return status(
            409,
            "another operator vouched for this task first, or it opened by its author's standing",
        );
    }
    Ok(vouch_view(&shard))
}

/// Opens the pending tasks of authors who now have standing. Called before
/// a reviewer is assigned work, so standing earned by reviewing opens an
/// author's own tasks without anyone asking.
///
/// Cost, bounded per call: one or two index range reads of at most
/// [`SWEEP_AUTHORS`] distinct authors (`shards_pending_author`), one trust
/// query per chunk of 30 of them (each a few indexed subqueries:
/// `patches_author`, `ledger_operator`, `genesis_operator`), and one update
/// for those who pass. The range starts at a random operator id and wraps,
/// so when more authors are pending than one sweep reads, every one of them
/// is reached in turn rather than the same 50 forever.
pub async fn open_by_standing(store: &Store) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct A {
        author_op: String,
    }
    let pivot = random_hex(10);
    let page = |cmp: &'static str, limit: usize| {
        let pivot = pivot.clone();
        async move {
            store
                .db
                .all::<A>(
                    &format!(
                        "SELECT DISTINCT author_op FROM shards WHERE state = 'pending' AND author_op {cmp} ?
                         ORDER BY author_op LIMIT ?"
                    ),
                    args![pivot, limit],
                )
                .await
        }
    };
    let mut authors: Vec<String> = page(">=", SWEEP_AUTHORS)
        .await?
        .into_iter()
        .map(|a| a.author_op)
        .collect();
    if authors.len() < SWEEP_AUTHORS {
        authors.extend(
            page("<", SWEEP_AUTHORS - authors.len())
                .await?
                .into_iter()
                .map(|a| a.author_op),
        );
    }
    if authors.is_empty() {
        return Ok(());
    }
    let trust = store.trust_map(&authors).await?;
    let ready: Vec<&String> = authors
        .iter()
        .filter(|a| trust.get(*a).is_some_and(|t| task_opens(*t)))
        .collect();
    for chunk in ready.chunks(MAX_PARAMS) {
        store
            .db
            .run(
                &format!(
                    "UPDATE shards SET state = 'open' WHERE state = 'pending' AND canary = 0 AND author_op IN ({})",
                    placeholders(chunk.len())
                ),
                chunk.iter().map(|a| (*a).into()).collect(),
            )
            .await?;
    }
    Ok(())
}

/// Settles a vouched task's loan if the task has resolved: returns it, or
/// records it lost (the debit stands). Called wherever a task can resolve:
/// when its panel calls it clean, when its runs are graded, and when a
/// challenge on its patch resolves. The decision is the core's
/// (`settle_vouch`); this only gathers the facts and writes the entries,
/// once.
pub async fn settle(store: &Store, shard_id: &str) -> ApiResult<()> {
    let shard = match store
        .db
        .first::<ShardRow>("SELECT * FROM shards WHERE id = ?", args![shard_id])
        .await?
    {
        Some(s) => s,
        None => return Ok(()),
    };
    let (Some(voucher), None) = (shard.vouched_by.as_deref(), shard.vouch_settled.as_deref())
    else {
        return Ok(());
    };
    let patch = store.require_patch_any(&shard.patch_id).await?;
    let (challenge_open, found_bad) = crate::challenges::patch_verdicts(store, &patch.id).await?;
    let review = match shard.state.as_str() {
        "clean" => TaskReview::Clean,
        // Only a resolved challenge the reviewers raised grades a task;
        // a `problem` verdict shows up as `found_bad`.
        "graded" => TaskReview::ChallengeCleared,
        _ => TaskReview::Underway,
    };
    let runs = if project_run_spec(store, &patch.project_id).await?.is_none() {
        TaskRuns::NotOwed
    } else {
        match shard.run_majority.as_deref() {
            None => TaskRuns::Pending,
            Some("none") => TaskRuns::Settled(None),
            // An outcome this build can't read settles nothing.
            Some(o) => parse_outcome(o).map_or(TaskRuns::Pending, |o| TaskRuns::Settled(Some(o))),
        }
    };
    let Some(event) = settle_vouch(&VouchFacts {
        found_bad,
        challenge_open,
        review,
        runs,
    }) else {
        return Ok(());
    };
    let claimed = store
        .db
        .changes(
            "UPDATE shards SET vouch_settled = ? WHERE id = ? AND vouched_by = ? AND vouch_settled IS NULL",
            args![event.as_str(), shard_id, voucher],
        )
        .await?;
    if claimed > 0 {
        store
            .credit(&vouch_entries(event, voucher), &vouch_ref(&patch.id))
            .await?;
    }
    Ok(())
}
