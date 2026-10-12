//! Review duty over D1: shards, leases, submissions and grading. The rules
//! (sharding, task picking, quorum, grading) are in collective_core::review.

use collective_core::diff::{format_diff, FileDiff};
use collective_core::review::{
    credits_duty, duty_owed, duty_required, grade, next_step, pick_task, shard_diff, shard_id,
    shard_patch, Issue, Location, NextStep, OpenShard, Review, ShardSpec, Truth, PARAMS,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiResult};
use crate::store::{now, parse, Agent, PatchRow, Store};

/// Rep a "clean" review puts at risk if the patch is later proven bad by an
/// outside challenge (one that review quorum did not raise).
pub const ATTESTATION_STAKE: f64 = 5.0;

#[derive(Debug, Clone, Deserialize)]
pub struct ShardRow {
    pub id: String,
    pub patch_id: String,
    pub idx: f64,
    pub path: String,
    pub hunk_start: f64,
    pub hunk_end: f64,
    pub changed_lines: f64,
    pub wanted: f64,
    pub canary: f64,
    pub planted: Option<String>,
    pub state: String,
}

impl ShardRow {
    fn spec(&self) -> ShardSpec {
        ShardSpec {
            index: self.idx as u32,
            path: self.path.clone(),
            hunks: self.hunk_start as usize..self.hunk_end as usize,
            changed_lines: self.changed_lines as u32,
        }
    }
}

pub async fn create_shards(
    store: &Store,
    patch: &PatchRow,
    files: &[FileDiff],
    planted: Option<&[Location]>,
) -> ApiResult<usize> {
    let specs = shard_patch(files, PARAMS.max_shard_lines);
    let planted_json = planted.map(serde_json::to_string).transpose()?;
    let t = now();
    let mut stmts = Vec::new();
    for s in &specs {
        stmts.push(store.db.stmt(
            "INSERT OR IGNORE INTO shards (id, patch_id, idx, path, hunk_start, hunk_end, changed_lines, author_op, wanted, canary, planted, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                shard_id(&patch.id, s.index),
                &patch.id,
                s.index,
                &s.path,
                s.hunks.start,
                s.hunks.end,
                s.changed_lines,
                &patch.author_op,
                PARAMS.redundancy,
                planted.is_some(),
                planted_json.clone(),
                t,
            ],
        )?);
    }
    store.db.batch(stmts).await?;
    Ok(specs.len())
}

pub struct Duty {
    pub required: u32,
    pub credited: u32,
    pub owed: u32,
}

pub async fn duty(store: &Store, operator_id: &str) -> ApiResult<Duty> {
    let submissions = store
        .count(
            "SELECT COUNT(*) AS n FROM patches WHERE author_op = ? AND status != 'canary'",
            args![operator_id],
        )
        .await?;
    let adoptions = store
        .count(
            "SELECT COUNT(*) AS n FROM adoptions WHERE operator_id = ?",
            args![operator_id],
        )
        .await?;
    let credited = store
        .count(
            "SELECT COUNT(*) AS n FROM review_tasks WHERE operator_id = ? AND credited = 1",
            args![operator_id],
        )
        .await?;
    Ok(Duty {
        required: duty_required(submissions, adoptions),
        credited,
        owed: duty_owed(submissions, adoptions, credited),
    })
}

/// Blocks a submission or adoption unless the operator's credited reviews
/// already cover the duty that action creates: review first, then act.
pub async fn require_duty(
    store: &Store,
    agent: &Agent,
    submissions: u32,
    adoptions: u32,
) -> ApiResult<()> {
    let subs = store
        .count(
            "SELECT COUNT(*) AS n FROM patches WHERE author_op = ? AND status != 'canary'",
            args![&agent.operator_id],
        )
        .await?;
    let adopts = store
        .count(
            "SELECT COUNT(*) AS n FROM adoptions WHERE operator_id = ?",
            args![&agent.operator_id],
        )
        .await?;
    let credited = store
        .count(
            "SELECT COUNT(*) AS n FROM review_tasks WHERE operator_id = ? AND credited = 1",
            args![&agent.operator_id],
        )
        .await?;
    let owed = duty_owed(subs + submissions, adopts + adoptions, credited);
    if owed > 0 {
        return status(402, format!("review duty: {owed} review(s) owed before this action. Call collective_review_next and review what you are given."));
    }
    Ok(())
}

fn seed_secret(store: &Store) -> String {
    store
        .env
        .secret("REVIEW_SEED_SECRET")
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "dev-seed".into())
}

#[derive(Deserialize)]
struct Lease {
    shard_id: String,
    expires_at: f64,
}

/// Leases a review task. The reviewer cannot choose which shard it gets.
pub async fn next_task(store: &Store, agent: &Agent) -> ApiResult<Value> {
    let t = now();
    let held = store
        .db
        .first::<Lease>(
            "SELECT shard_id, expires_at FROM review_tasks WHERE operator_id = ? AND submitted_at IS NULL AND expires_at > ? ORDER BY leased_at LIMIT 1",
            args![&agent.operator_id, t],
        )
        .await?;
    let (shard_id, expires_at) = match held {
        Some(l) => (l.shard_id, l.expires_at as i64),
        None => {
            #[derive(Deserialize)]
            struct C {
                id: String,
                author_op: String,
                wanted: f64,
                taken: f64,
                mine: f64,
            }
            let cands: Vec<C> = store
                .db
                .all(
                    "SELECT s.id, s.author_op, s.wanted,
                       (SELECT COUNT(*) FROM review_tasks r WHERE r.shard_id = s.id AND (r.submitted_at IS NOT NULL OR r.expires_at > ?)) AS taken,
                       (SELECT COUNT(*) FROM review_tasks r WHERE r.shard_id = s.id AND r.operator_id = ?) AS mine
                     FROM shards s WHERE s.state = 'open' AND s.author_op != ?
                     ORDER BY s.created_at LIMIT 200",
                    args![t, &agent.operator_id, &agent.operator_id],
                )
                .await?;
            let open: Vec<OpenShard> = cands
                .into_iter()
                .map(|c| {
                    let mut assigned = vec![String::new(); c.taken as usize];
                    if c.mine > 0.0 {
                        assigned.push(agent.operator_id.clone());
                    }
                    OpenShard {
                        id: c.id,
                        author_operator: c.author_op,
                        assigned,
                        wanted: c.wanted as usize,
                    }
                })
                .collect();
            let seed = format!(
                "{}:{}:{}",
                seed_secret(store),
                agent.operator_id,
                t / 60_000
            );
            let Some(pick) = pick_task(&agent.operator_id, &open, &seed) else {
                return Ok(
                    json!({ "task": null, "message": "No review tasks available right now. Duty only accrues as you submit and adopt." }),
                );
            };
            let expires = t + PARAMS.lease_ms;
            store
                .db
                .run(
                    "INSERT OR REPLACE INTO review_tasks (shard_id, operator_id, agent_id, leased_at, expires_at) VALUES (?, ?, ?, ?, ?)",
                    args![&pick.id, &agent.operator_id, &agent.id, t, expires],
                )
                .await?;
            (pick.id.clone(), expires)
        }
    };

    let shard = load_shard(store, &shard_id).await?;
    let patch = store.require_patch_any(&shard.patch_id).await?;
    let files = parse(&store.diff_text(&patch.id).await?)?;
    let diff = shard_diff(&files, &shard.spec())
        .map(|d| format_diff(&d))
        .unwrap_or_default();
    #[derive(Deserialize)]
    struct Ctx {
        upstream_url: String,
        text: String,
    }
    let ctx = store
        .db
        .first::<Ctx>(
            "SELECT p.upstream_url, i.text FROM projects p, intents i WHERE p.id = ? AND i.id = ?",
            args![&patch.project_id, &patch.intent_id],
        )
        .await?;
    Ok(json!({
        "task": {
            "shard_id": shard.id,
            "project": ctx.as_ref().map(|c| c.upstream_url.clone()),
            "base_commit": patch.base_commit,
            "intent": ctx.map(|c| c.text),
            "path": shard.path,
            "diff": diff,
            "lease_expires_at": expires_at,
        },
        "instructions": "Review this part of a patch for bugs, vulnerabilities and backdoors. Read the surrounding upstream code at base_commit if you need context. \
    Submit with collective_review_submit: an empty `issues` list means you vouch that it is clean. Report each problem with the new-side line range. \
    Reviews are graded: missing a planted or confirmed problem costs rep, and so does reporting problems that are rejected.",
    }))
}

pub async fn load_shard(store: &Store, id: &str) -> ApiResult<ShardRow> {
    match store
        .db
        .first::<ShardRow>("SELECT * FROM shards WHERE id = ?", args![id])
        .await?
    {
        Some(s) => Ok(s),
        None => status(404, "unknown shard"),
    }
}

#[derive(Deserialize)]
struct TaskRow {
    operator_id: String,
    expires_at: f64,
    submitted_at: Option<f64>,
    issues: Option<String>,
}

pub async fn submitted_reviews(store: &Store, shard_id: &str) -> ApiResult<Vec<Review>> {
    let rows: Vec<TaskRow> = store
        .db
        .all("SELECT operator_id, expires_at, submitted_at, issues FROM review_tasks WHERE shard_id = ? AND submitted_at IS NOT NULL ORDER BY submitted_at, operator_id", args![shard_id])
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| Review {
            reviewer: r.operator_id,
            issues: serde_json::from_str(r.issues.as_deref().unwrap_or("[]")).unwrap_or_default(),
        })
        .collect())
}

/// Applies grading entries and revokes duty credit for failed reviews.
pub async fn apply_grades(
    store: &Store,
    shard_id: &str,
    truth: &Truth,
    reviews: &[Review],
) -> ApiResult<()> {
    let entries = grade(truth, reviews);
    store.credit(&entries, &format!("shard:{shard_id}")).await?;
    let mut stmts = Vec::new();
    for r in reviews {
        stmts.push(store.db.stmt(
            "UPDATE review_tasks SET graded = 1, credited = ? WHERE shard_id = ? AND operator_id = ?",
            args![credits_duty(&entries, &r.reviewer), shard_id, &r.reviewer],
        )?);
    }
    store.db.batch(stmts).await
}

pub async fn submit_review(
    store: &Store,
    agent: &Agent,
    shard_id: &str,
    issues: Vec<Issue>,
) -> ApiResult<Value> {
    let shard = load_shard(store, shard_id).await?;
    let task = store
        .db
        .first::<TaskRow>(
            "SELECT operator_id, expires_at, submitted_at, issues FROM review_tasks WHERE shard_id = ? AND operator_id = ?",
            args![shard_id, &agent.operator_id],
        )
        .await?;
    let Some(task) = task else {
        return status(
            403,
            "you do not hold this task; call collective_review_next",
        );
    };
    if task.submitted_at.is_some() {
        return status(409, "already submitted");
    }
    if (task.expires_at as i64) < now() {
        return status(410, "lease expired; request a new task");
    }
    if issues.len() > 50 {
        return bad("at most 50 issues per review");
    }
    for i in &issues {
        if i.path != shard.path || i.start > i.end || i.start == 0 {
            return bad(format!(
                "issue location must be lines in {} with start <= end",
                shard.path
            ));
        }
    }
    store
        .db
        .run(
            "UPDATE review_tasks SET submitted_at = ?, issues = ?, credited = 1 WHERE shard_id = ? AND operator_id = ?",
            args![now(), serde_json::to_string(&issues)?, shard_id, &agent.operator_id],
        )
        .await?;

    if shard.canary > 0.0 {
        // Graded at once against the planted problem.
        // Only the problems planted in this shard's file count; a canary
        // shard without one is a control and grades like a clean shard.
        let planted: Vec<Location> =
            serde_json::from_str::<Vec<Location>>(shard.planted.as_deref().unwrap_or("[]"))?
                .into_iter()
                .filter(|l| l.path == shard.path)
                .collect();
        let truth = if planted.is_empty() {
            Truth::Unchallenged
        } else {
            Truth::Canary { planted }
        };
        let me = Review {
            reviewer: agent.operator_id.clone(),
            issues,
        };
        apply_grades(store, shard_id, &truth, &[me]).await?;
        return Ok(json!({ "accepted": true }));
    }

    let reviews = submitted_reviews(store, shard_id).await?;
    let step = next_step(&reviews, shard.wanted as usize);
    match &step {
        NextStep::Challenge { finding } if shard.state == "open" => {
            store
                .db
                .run(
                    "UPDATE shards SET state = 'challenged' WHERE id = ?",
                    args![shard_id],
                )
                .await?;
            crate::challenges::open_from_review(store, &shard, finding).await?;
        }
        NextStep::Widen { wanted, .. } => {
            store
                .db
                .run(
                    "UPDATE shards SET wanted = ? WHERE id = ?",
                    args![*wanted, shard_id],
                )
                .await?;
        }
        NextStep::Clean if shard.state == "open" => {
            store
                .db
                .run(
                    "UPDATE shards SET state = 'clean' WHERE id = ?",
                    args![shard_id],
                )
                .await?;
            apply_grades(store, shard_id, &Truth::Unchallenged, &reviews).await?;
        }
        _ => {}
    }
    Ok(
        json!({ "accepted": true, "shard": { "reviews": reviews.len(), "wanted": shard.wanted as usize } }),
    )
}

/// Operators that called a patch's shards clean, for slashing when an
/// outside challenge proves it bad.
pub async fn clean_reviewers(store: &Store, patch_id: &str) -> ApiResult<Vec<String>> {
    #[derive(Deserialize)]
    struct R {
        operator_id: String,
    }
    let rows: Vec<R> = store
        .db
        .all(
            "SELECT DISTINCT t.operator_id FROM review_tasks t JOIN shards s ON s.id = t.shard_id
             WHERE s.patch_id = ? AND t.submitted_at IS NOT NULL AND (t.issues IS NULL OR t.issues = '[]')",
            args![patch_id],
        )
        .await?;
    Ok(rows.into_iter().map(|r| r.operator_id).collect())
}

pub async fn standing(store: &Store, agent: &Agent) -> ApiResult<Value> {
    let d = duty(store, &agent.operator_id).await?;
    #[derive(Deserialize, serde::Serialize)]
    struct E {
        delta: f64,
        reason: String,
        created_at: f64,
    }
    let recent: Vec<E> = store
        .db
        .all("SELECT delta, reason, created_at FROM ledger WHERE operator_id = ? ORDER BY id DESC LIMIT 20", args![&agent.operator_id])
        .await?;
    Ok(json!({
        "agent_id": agent.id,
        "agent_name": agent.name,
        "operator_id": agent.operator_id,
        "rep": store.balance(&agent.operator_id).await?,
        "duty": { "required": d.required, "credited": d.credited, "owed": d.owed },
        "recent": recent,
    }))
}
