//! Proofs over D1. Nobody runs a proof centrally: a proof, or the
//! verification of a contributed canary, is a job that disinterested
//! operators' agents claim, run in their own sandbox (crates/prover, through
//! `collective prove` or a runner Worker) and report on. Each report is
//! assessed here, never taken at the runner's word, and a verdict stands
//! once enough independent, trusted runners agree. The rules are in
//! collective_core::proof.

use std::collections::HashSet;

use collective_core::challenge::{Challenge, Intent, Phase, ResolvedVia, Verdict};
use collective_core::proof::{
    assess, grade_proof_runs, proof_quorum, run_lease_secs, ProofClaim, ProofKind, ProofReport,
    ProofSpec, Quorum, Verdict as ProofVerdict, MAX_RUNNERS,
};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::*;

use crate::args;
use crate::challenges;
use crate::contributions;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::store::{new_id, now, Agent, Store};

/// Proofs an operator may submit per challenge: each one costs several
/// runners a sandbox run.
pub const MAX_PER_OPERATOR: u32 = 3;
/// Proofs an operator may submit per day across all challenges, so cheap
/// bare reports can't multiply the runners' work.
pub const MAX_PER_OPERATOR_PER_DAY: u32 = 10;
/// Largest upstream source tarball a proof will fetch (Worker memory is limited).
pub const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
/// Largest run report accepted: the prover caps output tails at 8 KB per run.
const MAX_REPORT_BYTES: usize = 256 * 1024;
/// How long a job waits for a quorum of runners before it is closed
/// without a verdict.
pub const JOB_TTL_MS: i64 = 3 * 86_400_000;

fn verdict_str(v: ProofVerdict) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_verdict(s: &str) -> Option<ProofVerdict> {
    serde_json::from_value(Value::String(s.into())).ok()
}

#[derive(Debug, Deserialize)]
struct ProofRow {
    id: String,
    challenge_id: String,
    patch_id: String,
    spec: String,
    status: String,
    verdict: Option<String>,
    reason: Option<String>,
    report: Option<String>,
    created_at: f64,
    finished_at: Option<f64>,
}

impl ProofRow {
    fn view(&self, with_report: bool) -> Value {
        let mut v = json!({
            "id": self.id,
            "challenge_id": self.challenge_id,
            "patch_id": self.patch_id,
            "spec": serde_json::from_str::<Value>(&self.spec).unwrap_or(Value::Null),
            "status": self.status,
            "verdict": self.verdict,
            "reason": self.reason,
            "created_at": self.created_at as i64,
            "finished_at": self.finished_at.map(|t| t as i64),
        });
        if with_report {
            v["report"] = self
                .report
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or(Value::Null);
        }
        v
    }
}

async fn load(store: &Store, id: &str) -> ApiResult<ProofRow> {
    match store
        .db
        .first::<ProofRow>("SELECT * FROM proofs WHERE id = ?", args![id])
        .await?
    {
        Some(p) => Ok(p),
        None => status(404, "unknown proof"),
    }
}

pub async fn get(store: &Store, id: &str) -> ApiResult<Value> {
    let mut v = load(store, id).await?.view(true);
    let runs = run_rows(store, Kind::Proof, id).await?;
    v["runs"] = runs
        .iter()
        .map(|r| json!({ "operator_id": r.operator_id, "verdict": r.verdict, "trust": r.trust, "submitted_at": r.submitted_at.map(|t| t as i64) }))
        .collect();
    Ok(v)
}

pub async fn list_for_challenge(store: &Store, challenge_id: &str) -> ApiResult<Vec<Value>> {
    let rows: Vec<ProofRow> = store
        .db
        .all(
            "SELECT * FROM proofs WHERE challenge_id = ? ORDER BY created_at",
            args![challenge_id],
        )
        .await?;
    Ok(rows.iter().map(|r| r.view(false)).collect())
}

/// Queues a proof for runners. Only the finder or operators who staked on
/// "problem" may submit: every proof costs several runners a sandbox run.
pub async fn submit(
    store: &Store,
    agent: &Agent,
    challenge_id: &str,
    spec: ProofSpec,
) -> ApiResult<Value> {
    spec.validate().map_err(|e| ApiError::new(400, e))?;
    if spec.kind == ProofKind::Suite {
        return bad("a suite run is part of a review task (collective run), not a challenge proof");
    }
    let c: Challenge = challenges::load_state(store, challenge_id).await?;
    if c.phase == Phase::Resolved {
        return status(409, "challenge already resolved");
    }
    let backer = c.flagged_by == agent.operator_id
        || c.contributions
            .iter()
            .any(|x| x.operator_id == agent.operator_id && x.side == Verdict::Problem);
    if !backer {
        return status(
            403,
            "only the finder or operators who staked on \"problem\" can submit proofs",
        );
    }
    let mine = store
        .count(
            "SELECT COUNT(*) AS n FROM proofs WHERE challenge_id = ? AND submitted_by = ?",
            args![challenge_id, &agent.operator_id],
        )
        .await?;
    if mine >= MAX_PER_OPERATOR {
        return status(
            429,
            format!("at most {MAX_PER_OPERATOR} proofs per operator per challenge"),
        );
    }
    let today = store
        .count(
            "SELECT COUNT(*) AS n FROM proofs WHERE submitted_by = ? AND created_at > ?",
            args![&agent.operator_id, now() - 86_400_000],
        )
        .await?;
    if today >= MAX_PER_OPERATOR_PER_DAY {
        return status(
            429,
            format!("at most {MAX_PER_OPERATOR_PER_DAY} proofs per operator per day"),
        );
    }
    let id = new_id();
    store
        .db
        .run(
            "INSERT INTO proofs (id, challenge_id, patch_id, submitted_by, spec, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            args![&id, challenge_id, &c.patch_id, &agent.operator_id, serde_json::to_string(&spec)?, now()],
        )
        .await?;
    Ok(load(store, &id).await?.view(false))
}

/// The upstream source at the base commit, cached in R2. GitHub projects
/// are fetched automatically; others must be uploaded by an admin.
pub async fn source_tarball(
    store: &Store,
    project_id: &str,
    upstream: &str,
    base: &str,
) -> ApiResult<Vec<u8>> {
    let key = format!("sources/{project_id}/{base}.tar.gz");
    if let Some(obj) = store.blobs.get(&key).execute().await? {
        return Ok(obj
            .body()
            .ok_or_else(|| ApiError::new(500, "empty source blob"))?
            .bytes()
            .await?);
    }
    let Some(repo) = upstream.strip_prefix("github.com/") else {
        return bad(format!("no cached source for {upstream} at {base}; only GitHub sources are fetched automatically"));
    };
    if base.len() != 40 {
        return bad("proofs need the patch's base_commit as a full 40-character SHA");
    }
    let url = Url::parse(&format!("https://codeload.github.com/{repo}/tar.gz/{base}"))
        .map_err(|e| ApiError::new(400, e.to_string()))?;
    let mut resp = Fetch::Url(url).send().await?;
    if resp.status_code() != 200 {
        return bad(format!(
            "could not fetch {upstream} at {base}: HTTP {}",
            resp.status_code()
        ));
    }
    let declared = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_SOURCE_BYTES) {
        return bad(format!(
            "{upstream} at {base} is larger than the {} MB proof limit",
            MAX_SOURCE_BYTES >> 20
        ));
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return bad(format!(
            "{upstream} at {base} is larger than the {} MB proof limit",
            MAX_SOURCE_BYTES >> 20
        ));
    }
    store.blobs.put(&key, bytes.clone()).execute().await?;
    Ok(bytes)
}

pub async fn put_source(
    store: &Store,
    project: &str,
    base: &str,
    bytes: Vec<u8>,
) -> ApiResult<Value> {
    let p = store.require_project(project, false).await?;
    if !(7..=64).contains(&base.len()) || !base.chars().all(|c| c.is_ascii_hexdigit()) {
        return bad("base must be a commit SHA");
    }
    let size = bytes.len();
    store
        .blobs
        .put(
            format!("sources/{}/{}.tar.gz", p.id, base.to_lowercase()),
            bytes,
        )
        .execute()
        .await?;
    Ok(json!({ "project_id": p.id, "base_commit": base, "bytes": size }))
}

// -- jobs: what runners claim, run and report on -------------------------------

/// What a run verifies: a challenge proof or a contributed canary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Proof,
    Canary,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Proof => "proof",
            Kind::Canary => "canary",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        match s {
            "proof" => Some(Kind::Proof),
            "canary" => Some(Kind::Canary),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RunRow {
    id: String,
    job_kind: String,
    job_id: String,
    operator_id: String,
    expires_at: f64,
    verdict: Option<String>,
    reason: Option<String>,
    report: Option<String>,
    trust: Option<f64>,
    submitted_at: Option<f64>,
}

async fn run_rows(store: &Store, kind: Kind, job_id: &str) -> ApiResult<Vec<RunRow>> {
    store
        .db
        .all(
            "SELECT * FROM proof_runs WHERE job_kind = ? AND job_id = ? AND submitted_at IS NOT NULL ORDER BY submitted_at, operator_id",
            args![kind.as_str(), job_id],
        )
        .await
}

fn claims(rows: &[RunRow]) -> Vec<ProofClaim> {
    rows.iter()
        .filter_map(|r| {
            Some(ProofClaim {
                runner: r.operator_id.clone(),
                trust: r.trust.unwrap_or(0.0),
                verdict: parse_verdict(r.verdict.as_deref()?)?,
            })
        })
        .collect()
}

struct Job {
    kind: Kind,
    id: String,
    patch_id: String,
    spec: ProofSpec,
    /// Still waiting for a quorum.
    open: bool,
    /// The verdict it settled on, if it did.
    settled: Option<ProofVerdict>,
}

async fn load_job(store: &Store, kind: Kind, id: &str) -> ApiResult<Job> {
    let (patch_id, spec, open, verdict) = match kind {
        Kind::Proof => {
            let p = load(store, id).await?;
            (p.patch_id, p.spec, p.status == "queued", p.verdict)
        }
        Kind::Canary => {
            let c = contributions::job(store, id).await?;
            (c.patch_id, c.spec, c.state == "verifying", c.verdict)
        }
    };
    Ok(Job {
        kind,
        id: id.into(),
        patch_id,
        spec: serde_json::from_str(&spec)?,
        open,
        // A split or abandoned job is recorded as inconclusive: no verdict.
        settled: verdict
            .as_deref()
            .and_then(parse_verdict)
            .filter(|v| *v != ProofVerdict::Inconclusive),
    })
}

/// Operators with a stake in a job's outcome, who may not run it: the
/// patch's author, whoever submitted the proof, and everyone who staked on
/// the challenge. `None` if the job is no longer worth running.
async fn interested(store: &Store, kind: Kind, id: &str) -> ApiResult<Option<HashSet<String>>> {
    match kind {
        Kind::Proof => {
            #[derive(Deserialize)]
            struct P {
                challenge_id: String,
                patch_id: String,
                submitted_by: String,
            }
            let Some(p) = store
                .db
                .first::<P>(
                    "SELECT challenge_id, patch_id, submitted_by FROM proofs WHERE id = ?",
                    args![id],
                )
                .await?
            else {
                return Ok(None);
            };
            let c = challenges::load_state(store, &p.challenge_id).await?;
            if c.phase == Phase::Resolved {
                close(
                    store,
                    kind,
                    id,
                    "the challenge was resolved before the proof settled",
                )
                .await?;
                return Ok(None);
            }
            let author = store.require_patch_any(&p.patch_id).await?.author_op;
            let mut backers: Vec<String> =
                c.contributions.into_iter().map(|x| x.operator_id).collect();
            backers.extend([p.submitted_by, c.flagged_by]);
            Ok(Some(collective_core::review::excluded(&author, &backers)))
        }
        Kind::Canary => {
            let c = contributions::job(store, id).await?;
            Ok(Some(collective_core::review::excluded(&c.operator_id, &[])))
        }
    }
}

/// Closes an open job without a verdict.
async fn close(store: &Store, kind: Kind, id: &str, reason: &str) -> ApiResult<()> {
    match kind {
        Kind::Proof => store
            .db
            .run(
                "UPDATE proofs SET status = 'done', verdict = 'inconclusive', reason = ?, finished_at = ? WHERE id = ? AND status = 'queued'",
                args![reason, now(), id],
            )
            .await,
        Kind::Canary => contributions::verification_error(store, id, reason).await,
    }
}

/// Closes jobs no quorum of runners got to in time.
pub async fn sweep_stale(store: &Store) -> ApiResult<()> {
    let (t, cutoff) = (now(), now() - JOB_TTL_MS);
    let why = "no quorum of runners reported in time";
    store
        .db
        .batch(vec![
            store.db.stmt(
                "UPDATE proofs SET status = 'done', verdict = 'inconclusive', reason = ?, finished_at = ? WHERE status = 'queued' AND created_at < ?",
                args![why, t, cutoff],
            )?,
            store.db.stmt(
                "UPDATE shards SET state = 'retired' WHERE state = 'verifying' AND id IN (SELECT task_id FROM canary_contributions WHERE state = 'verifying' AND created_at < ?)",
                args![cutoff],
            )?,
            store.db.stmt(
                "UPDATE canary_contributions SET state = 'error', reason = ?, finished_at = ? WHERE state = 'verifying' AND created_at < ?",
                args![why, t, cutoff],
            )?,
        ])
        .await
}

/// What a runner needs to run a job it holds.
async fn run_view(store: &Store, run: &RunRow) -> ApiResult<Value> {
    let kind = Kind::parse(&run.job_kind).ok_or_else(|| ApiError::new(500, "bad job kind"))?;
    let job = load_job(store, kind, &run.job_id).await?;
    let patch = store.require_patch_any(&job.patch_id).await?;
    let project = store.require_project(&patch.project_id, false).await?;
    Ok(json!({ "job": {
        "run_id": run.id,
        "project": project.upstream_url,
        "base_commit": patch.base_commit,
        "spec": job.spec,
        "diff": store.diff_text(&patch.id).await?,
        "source": format!("/v1/proof-runs/{}/source", run.id),
        "expires_at": run.expires_at as i64,
    } }))
}

/// Claims the next job this operator may run, or returns the one this
/// agent already holds. The oldest job comes first: a runner can't choose.
pub async fn next_job(store: &Store, agent: &Agent) -> ApiResult<Value> {
    sweep_stale(store).await?;
    let t = now();
    if let Some(held) = store
        .db
        .first::<RunRow>(
            "SELECT * FROM proof_runs WHERE agent_id = ? AND submitted_at IS NULL AND expires_at > ? ORDER BY claimed_at LIMIT 1",
            args![&agent.id, t],
        )
        .await?
    {
        return run_view(store, &held).await;
    }
    #[derive(Deserialize)]
    struct C {
        id: String,
        spec: String,
        created_at: f64,
        mine: f64,
        active: f64,
    }
    // `active`: conclusive runs in, plus runs still being worked on.
    let counts = |kind: Kind| {
        format!(
            "(SELECT COUNT(*) FROM proof_runs r WHERE r.job_kind = '{k}' AND r.job_id = j.id AND r.operator_id = ?) AS mine,
             (SELECT COUNT(*) FROM proof_runs r WHERE r.job_kind = '{k}' AND r.job_id = j.id
                AND ((r.submitted_at IS NOT NULL AND r.verdict != 'inconclusive') OR (r.submitted_at IS NULL AND r.expires_at > ?))) AS active",
            k = kind.as_str()
        )
    };
    let mut cands: Vec<(Kind, C)> = Vec::new();
    for (kind, table, open) in [
        (Kind::Proof, "proofs", "j.status = 'queued'"),
        (
            Kind::Canary,
            "canary_contributions",
            "j.state = 'verifying'",
        ),
    ] {
        let rows: Vec<C> = store
            .db
            .all(
                &format!(
                    "SELECT j.id, j.spec, j.created_at, {} FROM {table} j WHERE {open} ORDER BY j.created_at LIMIT 50",
                    counts(kind)
                ),
                args![&agent.operator_id, t],
            )
            .await?;
        cands.extend(rows.into_iter().map(|c| (kind, c)));
    }
    cands.sort_by(|a, b| a.1.created_at.total_cmp(&b.1.created_at));
    for (kind, c) in cands {
        if c.mine > 0.0 || c.active as usize >= MAX_RUNNERS {
            continue;
        }
        match interested(store, kind, &c.id).await? {
            Some(ops) if !ops.contains(&agent.operator_id) => {}
            _ => continue,
        }
        let spec: ProofSpec = serde_json::from_str(&c.spec)?;
        let id = new_id();
        let claimed = store
            .db
            .changes(
                "INSERT OR IGNORE INTO proof_runs (id, job_kind, job_id, operator_id, agent_id, claimed_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                args![&id, kind.as_str(), &c.id, &agent.operator_id, &agent.id, t, t + run_lease_secs(&spec) as i64 * 1000],
            )
            .await?;
        if claimed == 0 {
            continue;
        }
        let run = held_run(store, agent, &id).await?;
        return run_view(store, &run).await;
    }
    Ok(json!({ "job": null, "message": "No proofs need running right now." }))
}

/// A run this agent's operator claimed and hasn't reported yet.
async fn held_run(store: &Store, agent: &Agent, run_id: &str) -> ApiResult<RunRow> {
    let run = store
        .db
        .first::<RunRow>("SELECT * FROM proof_runs WHERE id = ?", args![run_id])
        .await?;
    match run {
        Some(r) if r.operator_id == agent.operator_id => {
            if r.submitted_at.is_some() {
                return status(409, "this run was already reported");
            }
            if (r.expires_at as i64) < now() {
                return status(410, "run deadline passed");
            }
            Ok(r)
        }
        _ => status(403, "you do not hold this run"),
    }
}

pub async fn get_run(store: &Store, agent: &Agent, run_id: &str) -> ApiResult<Value> {
    run_view(store, &held_run(store, agent, run_id).await?).await
}

/// The source tarball for a run the caller holds.
pub async fn run_source(store: &Store, agent: &Agent, run_id: &str) -> ApiResult<Vec<u8>> {
    let run = held_run(store, agent, run_id).await?;
    let kind = Kind::parse(&run.job_kind).ok_or_else(|| ApiError::new(500, "bad job kind"))?;
    let job = load_job(store, kind, &run.job_id).await?;
    let patch = store.require_patch_any(&job.patch_id).await?;
    let project = store.require_project(&patch.project_id, false).await?;
    source_tarball(
        store,
        &project.id,
        &project.upstream_url,
        &patch.base_commit,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct RunSubmission {
    pub report: ProofReport,
}

/// A runner's sandbox report. The verdict is computed here from the
/// report, and counts only as one claim towards the job's quorum.
pub async fn submit_run(
    store: &Store,
    agent: &Agent,
    run_id: &str,
    sub: RunSubmission,
) -> ApiResult<Value> {
    let run = held_run(store, agent, run_id).await?;
    let kind = Kind::parse(&run.job_kind).ok_or_else(|| ApiError::new(500, "bad job kind"))?;
    let job = load_job(store, kind, &run.job_id).await?;
    let report_json = serde_json::to_string(&sub.report)?;
    if report_json.len() > MAX_REPORT_BYTES {
        return status(413, "run report too large");
    }
    let a = assess(&job.spec, &sub.report);
    let trust = store
        .trust_map(std::slice::from_ref(&agent.operator_id))
        .await?
        .get(&agent.operator_id)
        .copied()
        .unwrap_or(0.0);
    let recorded = store
        .db
        .changes(
            "UPDATE proof_runs SET verdict = ?, reason = ?, report = ?, trust = ?, cpu_ms = ?, submitted_at = ? WHERE id = ? AND submitted_at IS NULL",
            args![verdict_str(a.verdict), &a.reason, report_json, trust, sub.report.cpu_ms.map(|c| c as f64), now(), run_id],
        )
        .await?;
    if recorded == 0 {
        return status(409, "this run was already reported");
    }
    let reference = format!("{}:{}", kind.as_str(), job.id);
    let mut settled = job.settled;
    if job.open {
        let rows = run_rows(store, kind, &job.id).await?;
        let claims = claims(&rows);
        match proof_quorum(&claims) {
            Quorum::Settled(v) => {
                if settle(store, &job, v, &rows).await? {
                    store
                        .credit(&grade_proof_runs(v, &claims), &reference)
                        .await?;
                }
                settled = Some(v);
            }
            Quorum::Split => {
                close(
                    store,
                    kind,
                    &job.id,
                    "the runners disagreed: no verdict reached a quorum",
                )
                .await?
            }
            Quorum::Pending => {}
        }
    } else if let Some(v) = job.settled {
        // A late run is graded against the verdict the others settled.
        let me = ProofClaim {
            runner: agent.operator_id.clone(),
            trust,
            verdict: a.verdict,
        };
        store
            .credit(&grade_proof_runs(v, &[me]), &reference)
            .await?;
    }
    Ok(json!({ "accepted": true, "verdict": a.verdict, "reason": a.reason, "settled": settled }))
}

/// Applies a verdict the runners settled, once: a proven regression
/// resolves its challenge, a demonstration is evidence for the jury or
/// council, and a canary goes live or fails. Returns whether this call
/// settled it.
async fn settle(store: &Store, job: &Job, v: ProofVerdict, rows: &[RunRow]) -> ApiResult<bool> {
    let agreeing: Vec<&RunRow> = rows
        .iter()
        .filter(|r| r.verdict.as_deref().and_then(parse_verdict) == Some(v))
        .collect();
    let reason = format!(
        "{} runners with combined trust {:.2} agree: {}",
        agreeing.len(),
        agreeing.iter().filter_map(|r| r.trust).sum::<f64>(),
        agreeing
            .first()
            .and_then(|r| r.reason.as_deref())
            .unwrap_or_default()
    );
    // One agreeing runner's report stands as the record; the rest are in proof_runs.
    let report = agreeing.first().and_then(|r| r.report.clone());
    match job.kind {
        Kind::Canary => {
            contributions::complete_verification(store, &job.id, v, &reason, report).await
        }
        Kind::Proof => {
            let claimed = store
                .db
                .changes(
                    "UPDATE proofs SET status = 'done', verdict = ?, reason = ?, report = ?, finished_at = ? WHERE id = ? AND status = 'queued'",
                    args![verdict_str(v), reason, report, now(), &job.id],
                )
                .await?;
            if claimed == 0 {
                return Ok(false);
            }
            if v == ProofVerdict::Proven {
                let row = load(store, &job.id).await?;
                let c = challenges::load_state(store, &row.challenge_id).await?;
                if c.phase != Phase::Resolved {
                    let r = challenges::Resolve {
                        verdict: Verdict::Problem,
                        via: ResolvedVia::Proof,
                        severity: c.severity,
                        intent: Intent::Unknown,
                    };
                    challenges::resolve(store, &row.challenge_id, r).await?;
                }
            }
            Ok(true)
        }
    }
}
