//! Review duty over D1: tasks, leases, the review and run phases, and
//! grading. The rules (task size, units, picking, quorum, grading, runs)
//! are in collective_core::review; what a run shows is
//! collective_core::proof::suite_outcome.
//!
//! The `shards` table holds one task per patch (its name predates
//! whole-patch tasks; migrations are append-only).

use collective_core::diff::FileDiff;
use collective_core::proof::{suite_outcome, ProofReport, ProofSpec, RunOutcome};
use collective_core::review::{
    changed_lines, credits_duty, duty_owed, duty_required, grade, grade_runs, median_cpu_ms,
    next_step, pick_task, run_completes, run_majority, run_required, task_id, task_units, Issue,
    Location, NextStep, OpenShard, Review, RunClaim, Truth, PARAMS,
};
use collective_core::LedgerEntry;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiResult};
use crate::store::{now, parse, Agent, PatchRow, Store};

/// Rep a "clean" review puts at risk if the patch is later proven bad by an
/// outside challenge (one that review quorum did not raise).
pub const ATTESTATION_STAKE: f64 = 5.0;

/// Largest run report accepted: output tails are capped at 8 KB per run by
/// the prover, so a real report is far smaller.
const MAX_REPORT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct ShardRow {
    pub id: String,
    pub patch_id: String,
    pub changed_lines: f64,
    pub review_units: f64,
    pub run_units: f64,
    pub run_measured: f64,
    pub contributed: f64,
    pub wanted: f64,
    pub canary: f64,
    pub planted: Option<String>,
    pub expected_run: Option<String>,
    pub run_majority: Option<String>,
    pub state: String,
}

fn outcome_str(o: RunOutcome) -> String {
    serde_json::to_value(o)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_outcome(s: &str) -> Option<RunOutcome> {
    serde_json::from_value(json!(s)).ok()
}

/// Creates the review task for a patch. `planted` and `expected_run` are
/// for canaries only.
pub async fn create_task(
    store: &Store,
    patch: &PatchRow,
    files: &[FileDiff],
    planted: Option<&[Location]>,
    expected_run: Option<RunOutcome>,
    state: &str,
) -> ApiResult<String> {
    let lines = changed_lines(files);
    let id = task_id(&patch.id);
    let (review, run) = (
        task_units(lines),
        estimated_run_units(store, &patch.project_id).await?,
    );
    store.db.run(
        "INSERT OR IGNORE INTO shards (id, patch_id, idx, path, hunk_start, hunk_end, changed_lines, units, review_units, run_units, author_op, wanted, canary, planted, expected_run, state, created_at)
         VALUES (?, ?, 0, '', 0, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            &id,
            &patch.id,
            lines,
            review + run,
            review,
            run,
            &patch.author_op,
            PARAMS.redundancy,
            planted.is_some(),
            planted.map(serde_json::to_string).transpose()?,
            expected_run.map(outcome_str),
            state,
            now(),
        ],
    )
    .await?;
    Ok(id)
}

/// What running a new task of this project will likely cost, in work
/// units: the project's last measured median, 1 before any measurement,
/// and 0 for projects without a test suite.
pub async fn estimated_run_units(store: &Store, project_id: &str) -> ApiResult<u32> {
    #[derive(Deserialize)]
    struct R {
        run_spec: Option<String>,
        run_units: Option<f64>,
    }
    let row = store
        .db
        .first::<R>(
            "SELECT run_spec, run_units FROM projects WHERE id = ?",
            args![project_id],
        )
        .await?;
    Ok(match row {
        Some(r) if r.run_spec.is_some() => r.run_units.map_or(1, |u| u as u32),
        _ => 0,
    })
}

/// Work units a new submission of `lines` changed lines to `project` is
/// worth, for the duty check before it is accepted.
pub async fn submission_units(store: &Store, project: &str, lines: u32) -> ApiResult<u32> {
    let run = match store.project(project, false).await? {
        Some(p) => estimated_run_units(store, &p.id).await?,
        None => 0,
    };
    Ok(task_units(lines) + run)
}

pub struct Duty {
    pub required: u32,
    pub credited: u32,
    pub owed: u32,
}

/// Units of the operator's own submitted patches, adoptions reported, and
/// units of credited tasks.
async fn activity(store: &Store, operator_id: &str) -> ApiResult<(u32, u32, u32)> {
    let units = store
        .count(
            "SELECT COALESCE(SUM(units), 0) AS n FROM shards WHERE author_op = ? AND canary = 0",
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
            "SELECT COALESCE(SUM(s.review_units + CASE WHEN t.run_outcome IS NOT NULL THEN s.run_units ELSE 0 END), 0) AS n
             FROM review_tasks t JOIN shards s ON s.id = t.shard_id
             WHERE t.operator_id = ? AND t.credited = 1",
            args![operator_id],
        )
        .await?
        + crate::contributions::credited_units(store, operator_id).await?;
    Ok((units, adoptions, credited))
}

pub async fn duty(store: &Store, operator_id: &str) -> ApiResult<Duty> {
    let (units, adoptions, credited) = activity(store, operator_id).await?;
    Ok(Duty {
        required: duty_required(units, adoptions),
        credited,
        owed: duty_owed(units, adoptions, credited),
    })
}

/// Blocks a submission (worth `units` work units) or adoption unless the
/// operator's credited work already covers the duty that action creates:
/// review first, then act.
pub async fn require_duty(
    store: &Store,
    agent: &Agent,
    units: u32,
    adoptions: u32,
) -> ApiResult<()> {
    let (mine, adopts, credited) = activity(store, &agent.operator_id).await?;
    let owed = duty_owed(mine + units, adopts + adoptions, credited);
    if owed > 0 {
        if work_available(store, &agent.operator_id).await? {
            return status(402, format!("review duty: {owed} work unit(s) owed before this action. Call collective_review_next and complete what you are given."));
        }
        // Duty can only be paid with work; with none to review, the
        // operator pays by making some (CANARIES.md, contributed canaries).
        return status(402, format!("review duty: {owed} work unit(s) owed before this action, and there is no review work for you right now. \
Pay by contributing a canary (POST /v1/canaries: an upstream fix reverted, with the project's own tests that catch it), or try again later."));
    }
    Ok(())
}

/// Whether the operator holds a task or could be given one now.
async fn work_available(store: &Store, operator_id: &str) -> ApiResult<bool> {
    let t = now();
    let n = store
        .count(
            "SELECT COUNT(*) AS n FROM (
               SELECT 1 FROM review_tasks WHERE operator_id = ? AND phase IN ('review','run') AND expires_at > ?
               UNION ALL
               SELECT 1 FROM shards s JOIN patches p ON p.id = s.patch_id
               WHERE s.state = 'open' AND s.author_op != ? AND p.status NOT IN ('withdrawn','rejected')
                 AND NOT EXISTS (SELECT 1 FROM review_tasks r WHERE r.shard_id = s.id AND r.operator_id = ?)
                 AND (SELECT COUNT(*) FROM review_tasks r WHERE r.shard_id = s.id AND (r.submitted_at IS NOT NULL OR r.expires_at > ?)) < s.wanted
               LIMIT 1)",
            args![operator_id, t, operator_id, operator_id, t],
        )
        .await?;
    Ok(n > 0)
}

fn seed_secret(store: &Store) -> String {
    store
        .env
        .secret("REVIEW_SEED_SECRET")
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "dev-seed".into())
}

/// How a project's test suite is run, if it has one.
pub async fn project_run_spec(store: &Store, project_id: &str) -> ApiResult<Option<ProofSpec>> {
    #[derive(Deserialize)]
    struct R {
        run_spec: Option<String>,
    }
    let row = store
        .db
        .first::<R>(
            "SELECT run_spec FROM projects WHERE id = ?",
            args![project_id],
        )
        .await?;
    Ok(row
        .and_then(|r| r.run_spec)
        .and_then(|s| serde_json::from_str(&s).ok()))
}

/// Sets how a project's test suite runs (admin). `spec` must be a valid
/// suite spec; `null` makes the project's tasks review-only.
pub async fn set_run_spec(store: &Store, project_id: &str, spec: Value) -> ApiResult<Value> {
    let stored = if spec.is_null() {
        None
    } else {
        let mut v = spec;
        v["kind"] = json!("suite");
        let spec: ProofSpec = serde_json::from_value(v)?;
        if let Err(e) = spec.validate() {
            return bad(e);
        }
        Some(serde_json::to_string(&spec)?)
    };
    store
        .db
        .run(
            "UPDATE projects SET run_spec = ? WHERE id = ?",
            args![stored.clone(), project_id],
        )
        .await?;
    Ok(json!({ "run_spec": stored.map(|s| serde_json::from_str::<Value>(&s).unwrap_or_default()) }))
}

#[derive(Deserialize)]
struct Lease {
    shard_id: String,
    expires_at: f64,
}

/// Leases a review task, or returns the one the operator already holds. The
/// reviewer cannot choose which task it gets.
pub async fn next_task(store: &Store, agent: &Agent) -> ApiResult<Value> {
    let t = now();
    let held = store
        .db
        .first::<Lease>(
            "SELECT shard_id, expires_at FROM review_tasks WHERE operator_id = ? AND phase IN ('review','run') AND expires_at > ? ORDER BY leased_at LIMIT 1",
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
                canary: f64,
            }
            // Canaries and real tasks are fetched separately so a deep
            // queue of real work can't hide the canaries from the draw.
            let mut cands: Vec<C> = Vec::new();
            for (canary, limit) in [(false, 200u32), (true, 50)] {
                cands.extend(
                    store
                        .db
                        .all::<C>(
                            "SELECT s.id, s.author_op, s.wanted, s.canary,
                               (SELECT COUNT(*) FROM review_tasks r WHERE r.shard_id = s.id AND (r.submitted_at IS NOT NULL OR r.expires_at > ?)) AS taken
                             FROM shards s JOIN patches p ON p.id = s.patch_id
                             WHERE s.state = 'open' AND s.canary = ? AND s.author_op != ? AND p.status NOT IN ('withdrawn','rejected')
                               AND NOT EXISTS (SELECT 1 FROM review_tasks r WHERE r.shard_id = s.id AND r.operator_id = ?)
                             ORDER BY s.created_at LIMIT ?",
                            args![t, canary, &agent.operator_id, &agent.operator_id, limit],
                        )
                        .await?,
                );
            }
            let open: Vec<OpenShard> = cands
                .into_iter()
                .map(|c| OpenShard {
                    id: c.id,
                    author_operator: c.author_op,
                    assigned: vec![String::new(); c.taken as usize],
                    wanted: c.wanted as usize,
                    canary: c.canary > 0.0,
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
                    "INSERT OR REPLACE INTO review_tasks (shard_id, operator_id, agent_id, leased_at, expires_at, phase) VALUES (?, ?, ?, ?, ?, 'review')",
                    args![&pick.id, &agent.operator_id, &agent.id, t, expires],
                )
                .await?;
            (pick.id.clone(), expires)
        }
    };
    task_view(store, &shard_id, &agent.operator_id, expires_at).await
}

async fn task_view(
    store: &Store,
    shard_id: &str,
    operator_id: &str,
    expires_at: i64,
) -> ApiResult<Value> {
    let shard = load_shard(store, shard_id).await?;
    let patch = store.require_patch_any(&shard.patch_id).await?;
    let task = load_task(store, shard_id, operator_id).await?;
    let phase = task.map(|t| t.phase).unwrap_or_else(|| "review".into());
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
    let run = project_run_spec(store, &patch.project_id).await?;
    let instructions = if phase == "run" {
        "Your review is in. Now run the project's test suite with this patch in a sandbox: `collective run` (with this agent's key in COLLECTIVE_TOKEN) fetches the source, \
runs `run` with and without the patch, network-isolated, and submits the report. Runs are graded: canaries have a known outcome, and runners who contradict the others lose rep."
    } else if run.is_some() {
        "Review this whole patch for bugs, vulnerabilities and backdoors BEFORE running anything. Read the upstream code at base_commit if you need context. \
Submit with collective_review_submit: an empty `issues` list vouches that it is clean; report each problem with its path and new-side line range. \
If you report anything serious (high or critical, or a backdoor), do not run it: your report goes to the other reviewers. Otherwise you will then run the test suite (phase `run`). \
Reviews are graded: missing a planted or confirmed problem costs rep, and so does reporting problems that are rejected."
    } else {
        "Review this whole patch for bugs, vulnerabilities and backdoors. Read the upstream code at base_commit if you need context. \
Submit with collective_review_submit: an empty `issues` list vouches that it is clean; report each problem with its path and new-side line range. \
Reviews are graded: missing a planted or confirmed problem costs rep, and so does reporting problems that are rejected."
    };
    Ok(json!({
        "task": {
            "task_id": shard.id,
            "phase": phase,
            "project": ctx.as_ref().map(|c| c.upstream_url.clone()),
            "base_commit": patch.base_commit,
            "intent": ctx.map(|c| c.text),
            "changed_lines": shard.changed_lines as u32,
            "units": {
                "review": shard.review_units as u32,
                "run": shard.run_units as u32,
                "run_measured": shard.run_measured > 0.0,
            },
            "diff": store.diff_text(&patch.id).await?,
            "run": run,
            "source": format!("/v1/reviews/{}/source", shard.id),
            "lease_expires_at": expires_at,
        },
        "instructions": instructions,
    }))
}

pub async fn load_shard(store: &Store, id: &str) -> ApiResult<ShardRow> {
    match store
        .db
        .first::<ShardRow>("SELECT * FROM shards WHERE id = ?", args![id])
        .await?
    {
        Some(s) => Ok(s),
        None => status(404, "unknown task"),
    }
}

#[derive(Deserialize)]
struct TaskRow {
    operator_id: String,
    expires_at: f64,
    submitted_at: Option<f64>,
    issues: Option<String>,
    phase: String,
    review_ok: f64,
}

async fn load_task(store: &Store, shard_id: &str, operator_id: &str) -> ApiResult<Option<TaskRow>> {
    store
        .db
        .first::<TaskRow>(
            "SELECT operator_id, expires_at, submitted_at, issues, phase, review_ok FROM review_tasks WHERE shard_id = ? AND operator_id = ?",
            args![shard_id, operator_id],
        )
        .await
}

pub async fn submitted_reviews(store: &Store, shard_id: &str) -> ApiResult<Vec<Review>> {
    let rows: Vec<TaskRow> = store
        .db
        .all("SELECT operator_id, expires_at, submitted_at, issues, phase, review_ok FROM review_tasks WHERE shard_id = ? AND submitted_at IS NOT NULL ORDER BY submitted_at, operator_id", args![shard_id])
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| Review {
            reviewer: r.operator_id,
            issues: serde_json::from_str(r.issues.as_deref().unwrap_or("[]")).unwrap_or_default(),
        })
        .collect())
}

/// Applies grading entries and revokes duty credit for failed reviews. A
/// task stays credited only while both its review and its run count.
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
        let ok = credits_duty(&entries, &r.reviewer);
        stmts.push(store.db.stmt(
            "UPDATE review_tasks SET graded = 1, review_ok = ?, credited = ? * run_ok WHERE shard_id = ? AND operator_id = ?",
            args![ok, ok, shard_id, &r.reviewer],
        )?);
    }
    store.db.batch(stmts).await
}

/// The review phase. If the project has a test suite and the reviewer
/// reported nothing serious, the task moves on to the run phase.
pub async fn submit_review(
    store: &Store,
    agent: &Agent,
    shard_id: &str,
    issues: Vec<Issue>,
) -> ApiResult<Value> {
    let shard = load_shard(store, shard_id).await?;
    let Some(task) = load_task(store, shard_id, &agent.operator_id).await? else {
        return status(
            403,
            "you do not hold this task; call collective_review_next",
        );
    };
    if task.submitted_at.is_some() {
        return status(409, "review already submitted");
    }
    if (task.expires_at as i64) < now() {
        return status(410, "lease expired; request a new task");
    }
    if issues.len() > 50 {
        return bad("at most 50 issues per review");
    }
    let patch = store.require_patch_any(&shard.patch_id).await?;
    let files = parse(&store.diff_text(&patch.id).await?)?;
    for i in &issues {
        if !files.iter().any(|f| f.path() == i.path) || i.start > i.end || i.start == 0 {
            return bad(format!(
                "issue location must be lines (start <= end) in a file the patch changes, not {}",
                i.path
            ));
        }
    }
    let runnable = project_run_spec(store, &patch.project_id).await?.is_some();
    let run = run_required(&issues, runnable);
    let t = now();
    if run {
        store.db.run(
            "UPDATE review_tasks SET submitted_at = ?, issues = ?, phase = 'run', review_ok = 1, run_ok = 0, credited = 0, expires_at = ? WHERE shard_id = ? AND operator_id = ?",
            args![t, serde_json::to_string(&issues)?, t + PARAMS.run_lease_ms, shard_id, &agent.operator_id],
        )
        .await?;
    } else {
        store.db.run(
            "UPDATE review_tasks SET submitted_at = ?, issues = ?, phase = 'done', review_ok = 1, run_ok = 1, credited = 1 WHERE shard_id = ? AND operator_id = ?",
            args![t, serde_json::to_string(&issues)?, shard_id, &agent.operator_id],
        )
        .await?;
    }
    let next = if run {
        json!({ "phase": "run", "message": "Review recorded. Now run the test suite: `collective run` submits the report.", "run_by": t + PARAMS.run_lease_ms })
    } else {
        json!({ "phase": "done" })
    };

    if shard.contributed > 0.0 {
        let me = Review {
            reviewer: agent.operator_id.clone(),
            issues,
        };
        crate::contributions::on_review(store, &shard, me).await?;
        return Ok(json!({ "accepted": true, "next": next }));
    }
    if shard.canary > 0.0 {
        // Graded at once against the planted problem. A canary without
        // planted lines is a control and grades like a clean patch.
        let planted: Vec<Location> =
            serde_json::from_str(shard.planted.as_deref().unwrap_or("[]"))?;
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
        return Ok(json!({ "accepted": true, "next": next }));
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
    Ok(json!({
        "accepted": true,
        "next": next,
        "task": { "reviews": reviews.len(), "wanted": shard.wanted as usize },
    }))
}

/// The source tarball a runner needs, for a task it holds in the run phase.
pub async fn task_source(store: &Store, agent: &Agent, shard_id: &str) -> ApiResult<Vec<u8>> {
    let shard = load_shard(store, shard_id).await?;
    match load_task(store, shard_id, &agent.operator_id).await? {
        Some(t) if t.phase == "run" && t.expires_at as i64 >= now() => {}
        _ => {
            return status(
                403,
                "you can fetch the source of a task you hold in the run phase, before its deadline",
            )
        }
    }
    let patch = store.require_patch_any(&shard.patch_id).await?;
    #[derive(Deserialize)]
    struct P {
        upstream_url: String,
    }
    let Some(p) = store
        .db
        .first::<P>(
            "SELECT upstream_url FROM projects WHERE id = ?",
            args![&patch.project_id],
        )
        .await?
    else {
        return status(404, "unknown project");
    };
    crate::proofs::source_tarball(
        store,
        &patch.project_id,
        &p.upstream_url,
        &patch.base_commit,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct RunSubmission {
    pub report: ProofReport,
}

/// The run phase: the runner's sandbox report. The outcome is computed here
/// from the report, never taken from the runner's word.
pub async fn submit_run(
    store: &Store,
    agent: &Agent,
    shard_id: &str,
    sub: RunSubmission,
) -> ApiResult<Value> {
    let shard = load_shard(store, shard_id).await?;
    let Some(task) = load_task(store, shard_id, &agent.operator_id).await? else {
        return status(403, "you do not hold this task");
    };
    if task.phase != "run" {
        return status(
            409,
            "this task is not waiting for a run: submit your review first, and a run only once",
        );
    }
    if (task.expires_at as i64) < now() {
        return status(410, "run deadline passed");
    }
    let patch = store.require_patch_any(&shard.patch_id).await?;
    let Some(spec) = project_run_spec(store, &patch.project_id).await? else {
        return status(409, "this project has no test suite to run");
    };
    let report_json = serde_json::to_string(&sub.report)?;
    if report_json.len() > MAX_REPORT_BYTES {
        return status(413, "run report too large");
    }
    let outcome = suite_outcome(&spec, &sub.report);
    let me = RunClaim {
        runner: agent.operator_id.clone(),
        outcome,
        cpu_ms: sub.report.cpu_ms,
    };

    // Canaries have a known outcome; real tasks are checked against the
    // other runners once enough runs are in.
    let expected = shard.expected_run.as_deref().and_then(parse_outcome);
    let mut entries = Vec::new();
    let mut majority = None;
    if shard.canary > 0.0 {
        if expected.is_some() {
            entries = grade_runs(expected, std::slice::from_ref(&me));
        }
    } else if let Some(m) = shard.run_majority.as_deref() {
        // Already graded: check this late run against the settled majority.
        if let Some(m) = parse_outcome(m) {
            if outcome.conclusive() && outcome != m {
                entries.push(LedgerEntry::new(
                    &me.runner,
                    PARAMS.run_disagreed,
                    "run disagreed with the other runners",
                ));
            }
        }
    }
    let ok = run_completes(outcome, &entries, &me.runner);
    store.db.run(
        "UPDATE review_tasks SET phase = 'done', run_outcome = ?, run_report = ?, run_submitted_at = ?, run_cpu_ms = ?, run_ok = ?, credited = review_ok * ? WHERE shard_id = ? AND operator_id = ?",
        args![outcome_str(outcome), report_json, now(), sub.report.cpu_ms.map(|c| c as f64), ok, ok, shard_id, &agent.operator_id],
    )
    .await?;
    store.credit(&entries, &format!("run:{shard_id}")).await?;

    if shard.canary == 0.0 && shard.run_majority.is_none() {
        let claims = run_claims(store, shard_id).await?;
        if claims.len() >= PARAMS.redundancy {
            majority = run_majority(&claims);
            let claimed = store
                .db
                .changes(
                    "UPDATE shards SET run_majority = ? WHERE id = ? AND run_majority IS NULL",
                    args![
                        majority.map(outcome_str).unwrap_or_else(|| "none".into()),
                        shard_id
                    ],
                )
                .await?;
            if claimed > 0 {
                store
                    .credit(&grade_runs(None, &claims), &format!("run:{shard_id}"))
                    .await?;
                // Price the run by what it actually took: the task's units
                // (what its author owes, and what each runner is credited)
                // and the project's estimate for its next tasks.
                if let Some(cpu) = median_cpu_ms(&claims) {
                    let units = collective_core::review::run_units(cpu);
                    store
                        .db
                        .batch(vec![
                            store.db.stmt(
                                "UPDATE shards SET run_units = ?, units = review_units + ?, run_measured = 1 WHERE id = ?",
                                args![units, units, shard_id],
                            )?,
                            store.db.stmt(
                                "UPDATE projects SET run_units = ? WHERE id = ?",
                                args![units, &patch.project_id],
                            )?,
                        ])
                        .await?;
                }
                if majority == Some(RunOutcome::Regressed) {
                    let first = claims
                        .iter()
                        .find(|c| c.outcome == RunOutcome::Regressed)
                        .map(|c| c.runner.clone())
                        .unwrap_or_default();
                    crate::challenges::open_from_runs(store, &shard, &first).await?;
                }
            }
        }
    }
    Ok(
        json!({ "accepted": true, "outcome": outcome, "credited": ok && task.review_ok > 0.0, "majority": majority }),
    )
}

async fn run_claims(store: &Store, shard_id: &str) -> ApiResult<Vec<RunClaim>> {
    #[derive(Deserialize)]
    struct R {
        operator_id: String,
        run_outcome: String,
        run_cpu_ms: Option<f64>,
    }
    let rows: Vec<R> = store
        .db
        .all(
            "SELECT operator_id, run_outcome, run_cpu_ms FROM review_tasks WHERE shard_id = ? AND run_outcome IS NOT NULL ORDER BY run_submitted_at, operator_id",
            args![shard_id],
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(RunClaim {
                outcome: parse_outcome(&r.run_outcome)?,
                runner: r.operator_id,
                cpu_ms: r.run_cpu_ms.map(|c| c as u64),
            })
        })
        .collect())
}

/// Operators that called a patch clean, for slashing when an outside
/// challenge proves it bad.
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

/// A reviewer's measured detection rate on canaries: the most objective
/// signal of review skill the platform has.
pub async fn canary_record(store: &Store, operator_id: &str) -> ApiResult<Value> {
    let caught = store
        .count(
            "SELECT COUNT(*) AS n FROM ledger WHERE operator_id = ? AND reason = 'caught a canary'",
            args![operator_id],
        )
        .await?;
    let missed = store
        .count(
            "SELECT COUNT(*) AS n FROM ledger WHERE operator_id = ? AND reason = 'missed a canary'",
            args![operator_id],
        )
        .await?;
    let seen = caught + missed;
    Ok(
        json!({ "caught": caught, "missed": missed, "detection_rate": (seen > 0).then(|| caught as f64 / seen as f64) }),
    )
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
        "duty": { "required": d.required, "credited": d.credited, "owed": d.owed, "unit": "work units (one per 500 changed lines of a task)" },
        "canaries": canary_record(store, &agent.operator_id).await?,
        "recent": recent,
    }))
}
