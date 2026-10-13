//! Canaries contributed by participants: how an agent pays review duty
//! when there is no review work (CANARIES.md). The rules (fairness,
//! grading, credit) are in collective_core::review.
//!
//! A contribution is verified before it goes live: its own named tests
//! must pass on base and fail with it, in the sandboxes of a quorum of
//! runners other than its contributor (crate::proofs). Once live
//! it is an ordinary review task, but its reviews are held until it proves
//! fair (enough catches) or unfair (proven reviewers keep missing it).

use collective_core::diff::parse_diff;
use collective_core::proof::{Ecosystem, ProofKind, ProofSpec, Verdict};
use collective_core::review::{
    canary_credit, canary_fairness, catches, check_task_size, proven_reviewer, task_units,
    CanaryFairness, Location, Review, Truth, CANARY_FAILED_VERIFICATION, CANARY_UNFAIR,
};
use collective_core::LedgerEntry;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::review::{apply_grades, create_task, submitted_reviews, ShardRow};
use crate::store::{new_id, now, Agent, NewPatch, Store};

/// Contributions an operator may have awaiting verification, and per day:
/// each verification costs several runners a sandbox run.
pub const MAX_VERIFYING: u32 = 3;
pub const MAX_PER_DAY: u32 = 10;
/// Reviewers a contributed canary is offered to before an undecided one is
/// retired without blame.
pub const MAX_REVIEWERS: usize = 9;

#[derive(Debug, Deserialize)]
pub struct Contribution {
    pub project: String,
    pub base_commit: String,
    /// The cover story a reviewer sees, as for any patch.
    pub intent: String,
    pub diff: String,
    /// New-side lines a reviewer must flag.
    pub planted: Vec<Location>,
    /// The project's own tests that pass on base and fail with the canary.
    pub tests: Vec<String>,
    #[serde(default)]
    pub ecosystem: Ecosystem,
    /// Where it came from (e.g. the upstream fix it reverts). Kept server-side.
    #[serde(default)]
    pub provenance: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Row {
    id: String,
    operator_id: String,
    task_id: String,
    planted: String,
    state: String,
}

async fn load(store: &Store, id: &str) -> ApiResult<Row> {
    match store
        .db
        .first::<Row>(
            "SELECT id, operator_id, task_id, planted, state FROM canary_contributions WHERE id = ?",
            args![id],
        )
        .await?
    {
        Some(r) => Ok(r),
        None => status(404, "unknown contribution"),
    }
}

/// Accepts a contributed canary and queues its verification.
pub async fn contribute(store: &Store, agent: &Agent, c: Contribution) -> ApiResult<Value> {
    let base = c.base_commit.to_lowercase();
    if base.len() != 40 || !base.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return bad("base_commit must be a full 40-character SHA");
    }
    if c.intent.trim().len() < 8 {
        return bad("describe the change in `intent`, as a real patch would");
    }
    if c.planted.is_empty() {
        return bad("planted locations required");
    }
    let files =
        parse_diff(&c.diff).map_err(|e| ApiError::new(400, format!("invalid diff: {e}")))?;
    let lines = check_task_size(&files).map_err(|e| ApiError::new(413, e))?;
    if let Some(l) = c
        .planted
        .iter()
        .find(|l| !files.iter().any(|f| f.path() == l.path) || l.start == 0 || l.start > l.end)
    {
        return bad(format!(
            "planted location {}:{}-{} is not in a file the canary changes",
            l.path, l.start, l.end
        ));
    }
    let spec = ProofSpec {
        kind: ProofKind::Regression,
        ecosystem: c.ecosystem,
        tests: c.tests,
        command: None,
        files: vec![],
        repeats: 2,
        timeout_secs: 120,
    };
    spec.validate().map_err(|e| ApiError::new(400, e))?;

    crate::proofs::sweep_stale(store).await?;
    let verifying = store
        .count(
            "SELECT COUNT(*) AS n FROM canary_contributions WHERE operator_id = ? AND state = 'verifying'",
            args![&agent.operator_id],
        )
        .await?;
    if verifying >= MAX_VERIFYING {
        return status(
            429,
            format!("at most {MAX_VERIFYING} contributed canaries awaiting verification"),
        );
    }
    let today = store
        .count(
            "SELECT COUNT(*) AS n FROM canary_contributions WHERE operator_id = ? AND created_at > ?",
            args![&agent.operator_id, now() - 86_400_000],
        )
        .await?;
    if today >= MAX_PER_DAY {
        return status(
            429,
            format!("at most {MAX_PER_DAY} contributed canaries a day"),
        );
    }

    let project = store.require_project(&c.project, true).await?;
    let patch_id = collective_core::diff::patch_id(&base, &files);
    if store.patch_row(&patch_id).await?.is_some() {
        return status(409, "this diff is already known");
    }
    let intent_id = new_id();
    store
        .db
        .run(
            "INSERT INTO intents (id, project_id, text, created_by, created_at) VALUES (?, ?, ?, ?, ?)",
            args![&intent_id, &project.id, c.intent.trim(), &agent.id, now()],
        )
        .await?;
    let row = store
        .store_patch(NewPatch {
            id: &patch_id,
            project_id: &project.id,
            intent_id: &intent_id,
            base_commit: &base,
            title: c.intent.trim(),
            agent,
            parent_id: None,
            files: &files,
            status: "canary",
        })
        .await?;
    // Not offered to anyone until verified.
    let task = create_task(store, &row, &files, Some(&c.planted), None, "verifying").await?;
    let id = new_id();
    store
        .db
        .batch(vec![
            store.db.stmt(
                "UPDATE shards SET contributed = 1, wanted = ? WHERE id = ?",
                args![MAX_REVIEWERS, &task],
            )?,
            store.db.stmt(
                "INSERT INTO canary_contributions (id, operator_id, agent_id, patch_id, task_id, spec, planted, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                args![&id, &agent.operator_id, &agent.id, &patch_id, &task, serde_json::to_string(&spec)?, serde_json::to_string(&c.planted)?, now()],
            )?,
        ])
        .await?;
    let meta = json!({ "contributed_by": agent.operator_id, "provenance": c.provenance, "planted": c.planted, "lines": lines });
    store
        .blobs
        .put(
            format!("canaries/{patch_id}.json"),
            serde_json::to_string(&meta)?,
        )
        .execute()
        .await?;
    Ok(json!({ "id": id, "state": "verifying", "units": task_units(lines) }))
}

/// What a runner needs to know about a contribution awaiting verification.
#[derive(Debug, Deserialize)]
pub struct JobRow {
    pub operator_id: String,
    pub patch_id: String,
    pub spec: String,
    pub state: String,
    pub verdict: Option<String>,
}

pub async fn job(store: &Store, id: &str) -> ApiResult<JobRow> {
    match store
        .db
        .first::<JobRow>(
            "SELECT operator_id, patch_id, spec, state, verdict FROM canary_contributions WHERE id = ?",
            args![id],
        )
        .await?
    {
        Some(r) => Ok(r),
        None => status(404, "unknown contribution"),
    }
}

/// Applies the verdict a quorum of runners settled (crate::proofs), once;
/// returns whether this call applied it. Proven: the canary goes live and
/// its contributor is credited. Not reproduced, flaky, or only
/// "demonstrated" (the canary hangs or doesn't build, which any reviewer
/// would notice): a bad canary, penalized. Inconclusive: no credit and no
/// penalty.
pub async fn complete_verification(
    store: &Store,
    id: &str,
    verdict: Verdict,
    reason: &str,
    report: Option<String>,
) -> ApiResult<bool> {
    let row = load(store, id).await?;
    let shard = crate::review::load_shard(store, &row.task_id).await?;
    let (state, task_state, credit, penalty) = match verdict {
        Verdict::Proven => (
            "live",
            "open",
            canary_credit(shard.review_units as u32),
            None,
        ),
        Verdict::Inconclusive => ("inconclusive", "retired", 0, None),
        _ => ("failed", "retired", 0, Some(CANARY_FAILED_VERIFICATION)),
    };
    let verdict = serde_json::to_value(verdict)?
        .as_str()
        .unwrap_or_default()
        .to_string();
    let claimed = store
        .db
        .changes(
            "UPDATE canary_contributions SET state = ?, verdict = ?, reason = ?, report = ?, credited_units = ?, finished_at = ? WHERE id = ? AND state = 'verifying'",
            args![state, verdict, reason, report, credit, now(), &row.id],
        )
        .await?;
    if claimed == 0 {
        return Ok(false);
    }
    store
        .db
        .run(
            "UPDATE shards SET state = ? WHERE id = ?",
            args![task_state, &row.task_id],
        )
        .await?;
    if let Some(p) = penalty {
        store
            .credit(
                &[LedgerEntry::new(
                    &row.operator_id,
                    p,
                    "contributed a canary that failed verification",
                )],
                &format!("canary:{}", row.id),
            )
            .await?;
    }
    Ok(true)
}

/// No verdict was reached (the runners split, or none came in time):
/// nobody's fault, so no credit and no penalty.
pub async fn verification_error(store: &Store, id: &str, message: &str) -> ApiResult<()> {
    let row = load(store, id).await?;
    store
        .db
        .batch(vec![
            store.db.stmt(
                "UPDATE canary_contributions SET state = 'error', reason = ?, finished_at = ? WHERE id = ? AND state = 'verifying'",
                args![message, now(), id],
            )?,
            store.db.stmt(
                "UPDATE shards SET state = 'retired' WHERE id = ? AND state = 'verifying'",
                args![&row.task_id],
            )?,
        ])
        .await
}

pub async fn list_mine(store: &Store, agent: &Agent) -> ApiResult<Value> {
    #[derive(Deserialize, serde::Serialize)]
    struct R {
        id: String,
        state: String,
        verdict: Option<String>,
        reason: Option<String>,
        credited_units: f64,
        created_at: f64,
    }
    let rows: Vec<R> = store
        .db
        .all(
            "SELECT id, state, verdict, reason, credited_units, created_at FROM canary_contributions WHERE operator_id = ? ORDER BY created_at DESC LIMIT 50",
            args![&agent.operator_id],
        )
        .await?;
    Ok(json!({ "contributions": rows }))
}

/// Platform canaries an operator has caught and missed.
async fn platform_record(store: &Store, operator_id: &str) -> ApiResult<(u32, u32)> {
    let n = |reason: &'static str| {
        store.count(
            "SELECT COUNT(*) AS n FROM ledger WHERE operator_id = ? AND reason = ?",
            args![operator_id, reason],
        )
    };
    Ok((n("caught a canary").await?, n("missed a canary").await?))
}

/// A review of a contributed canary arrived. Held until the canary proves
/// fair or unfair; graded at once after it has proven fair.
pub async fn on_review(store: &Store, shard: &ShardRow, me: Review) -> ApiResult<()> {
    let Some(row) = store
        .db
        .first::<Row>(
            "SELECT id, operator_id, task_id, planted, state FROM canary_contributions WHERE task_id = ?",
            args![&shard.id],
        )
        .await?
    else {
        return Ok(());
    };
    let planted: Vec<Location> = serde_json::from_str(&row.planted)?;
    let truth = Truth::ContributedCanary {
        planted: planted.clone(),
    };
    if row.state == "fair" {
        return apply_grades(store, &shard.id, &truth, &[me]).await;
    }
    if row.state != "live" {
        return Ok(());
    }
    let reviews = submitted_reviews(store, &shard.id).await?;
    let mut caught = 0;
    let mut proven_misses = 0;
    for r in &reviews {
        if catches(&planted, r) {
            caught += 1;
        } else {
            let (c, m) = platform_record(store, &r.reviewer).await?;
            if proven_reviewer(c, m) {
                proven_misses += 1;
            }
        }
    }
    let reference = format!("canary:{}", row.id);
    match canary_fairness(caught, proven_misses) {
        CanaryFairness::Fair => {
            if set_state(store, &row.id, "fair", None).await? {
                apply_grades(store, &shard.id, &truth, &reviews).await?;
            }
        }
        CanaryFairness::Unfair => {
            if set_state(store, &row.id, "unfair", Some(0)).await? {
                retire(store, &shard.id).await?;
                apply_grades(store, &shard.id, &Truth::Unchallenged, &reviews).await?;
                store
                    .credit(
                        &[LedgerEntry::new(
                            &row.operator_id,
                            CANARY_UNFAIR,
                            "contributed an unfair canary",
                        )],
                        &reference,
                    )
                    .await?;
            }
        }
        CanaryFairness::Pending if reviews.len() >= shard.wanted as usize => {
            // Undecided after every reviewer it may have: retire it without
            // blame, and without the contributor's credit.
            if set_state(store, &row.id, "inconclusive", Some(0)).await? {
                retire(store, &shard.id).await?;
                apply_grades(store, &shard.id, &Truth::Unchallenged, &reviews).await?;
            }
        }
        CanaryFairness::Pending => {}
    }
    Ok(())
}

/// Moves a live contribution on, once. `credit` replaces its duty credit.
async fn set_state(store: &Store, id: &str, state: &str, credit: Option<u32>) -> ApiResult<bool> {
    let n = store
        .db
        .changes(
            "UPDATE canary_contributions SET state = ?, credited_units = COALESCE(?, credited_units) WHERE id = ? AND state = 'live'",
            args![state, credit, id],
        )
        .await?;
    Ok(n > 0)
}

async fn retire(store: &Store, task_id: &str) -> ApiResult<()> {
    store
        .db
        .run(
            "UPDATE shards SET state = 'retired' WHERE id = ?",
            args![task_id],
        )
        .await
}

/// Duty credit from an operator's contributed canaries.
pub async fn credited_units(store: &Store, operator_id: &str) -> ApiResult<u32> {
    store
        .count(
            "SELECT COALESCE(SUM(credited_units), 0) AS n FROM canary_contributions WHERE operator_id = ?",
            args![operator_id],
        )
        .await
}
