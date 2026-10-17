//! Proofs over D1. Nobody runs a proof centrally: a proof, the
//! verification of a contributed canary, or the proof of a proposed
//! environment (environments.rs) is a job that disinterested
//! operators' agents claim, run in their own sandbox (crates/prover, through
//! `collective prove` or a runner Worker) and report on. Each report is
//! assessed here, never taken at the runner's word, and a verdict stands
//! once enough independent, trusted runners agree. The rules are in
//! collective_core::proof.

use std::collections::HashSet;

use collective_core::challenge::{Challenge, Intent, Phase, ResolvedVia, Verdict};
use collective_core::environment::decide_proposal;
use collective_core::proof::{
    assess, grade_outcome_runs, grade_proof_runs, outcome_quorum, proof_quorum, run_lease_secs,
    suite_outcome, OutcomeClaim, ProofClaim, ProofKind, ProofReport, ProofSpec, Quorum, RunOutcome,
    Verdict as ProofVerdict, MAX_RUNNERS,
};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::*;

use crate::args;
use crate::challenges;
use crate::contributions;
use crate::environments::{self, Capabilities};
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

fn outcome_str(o: RunOutcome) -> String {
    serde_json::to_value(o)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_outcome(s: &str) -> Option<RunOutcome> {
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
    environment_id: Option<String>,
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
/// The proof runs in the project's current environment, pinned here; any
/// environment the submitter sent is ignored.
pub async fn submit(
    store: &Store,
    agent: &Agent,
    challenge_id: &str,
    mut spec: ProofSpec,
) -> ApiResult<Value> {
    spec.environment = Default::default();
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
    let project_id = store.require_patch_any(&c.patch_id).await?.project_id;
    // Nothing runs in a quarantined environment: no new proofs while the
    // project is review-only.
    let environment = environments::current(store, &project_id).await?;
    environment.refuse_if_quarantined()?;
    if let Some(e) = environment.environment() {
        spec.environment = e.clone();
    }
    let id = new_id();
    store
        .db
        .run(
            "INSERT INTO proofs (id, challenge_id, patch_id, submitted_by, spec, created_at, environment_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
            args![&id, challenge_id, &c.patch_id, &agent.operator_id, serde_json::to_string(&spec)?, now(), environments::pin_id(&environment)],
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

/// What a run verifies: a challenge proof, a contributed canary, or a
/// proposed environment (keyed `<project id>:<version>`; its report is a
/// suite run, read with `suite_outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Proof,
    Canary,
    Environment,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Proof => "proof",
            Kind::Canary => "canary",
            Kind::Environment => "environment",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        match s {
            "proof" => Some(Kind::Proof),
            "canary" => Some(Kind::Canary),
            "environment" => Some(Kind::Environment),
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

fn outcome_claims(rows: &[RunRow]) -> Vec<OutcomeClaim> {
    rows.iter()
        .filter_map(|r| {
            Some(OutcomeClaim {
                runner: r.operator_id.clone(),
                trust: r.trust.unwrap_or(0.0),
                outcome: parse_outcome(r.verdict.as_deref()?)?,
            })
        })
        .collect()
}

struct Job {
    kind: Kind,
    id: String,
    project_id: String,
    base_commit: String,
    /// The patch it runs; none for an environment's proof, which runs the
    /// base twice.
    patch_id: Option<String>,
    spec: ProofSpec,
    /// Still waiting for a quorum.
    open: bool,
    /// The verdict (or, for an environment, the suite outcome) it settled
    /// on, if it did.
    settled: Option<String>,
}

async fn load_job(store: &Store, kind: Kind, id: &str) -> ApiResult<Job> {
    if kind == Kind::Environment {
        let p = environments::proposal(store, id).await?;
        return Ok(Job {
            kind,
            id: id.into(),
            spec: p.proof_spec()?,
            base_commit: p
                .base_commit
                .clone()
                .ok_or_else(|| ApiError::new(500, "proposal has no base commit"))?,
            project_id: p.project_id,
            patch_id: None,
            open: p.state == "proposed",
            // A lapsed proposal is recorded as inconclusive: no verdict.
            settled: p.outcome.filter(|o| o != "inconclusive"),
        });
    }
    let (patch_id, spec, open, verdict, environment_id) = match kind {
        Kind::Proof => {
            let p = load(store, id).await?;
            (
                p.patch_id,
                p.spec,
                p.status == "queued",
                p.verdict,
                p.environment_id,
            )
        }
        Kind::Canary => {
            let c = contributions::job(store, id).await?;
            (
                c.patch_id,
                c.spec,
                c.state == "verifying",
                c.verdict,
                c.environment_id,
            )
        }
        Kind::Environment => unreachable!("handled above"),
    };
    let mut spec: ProofSpec = serde_json::from_str(&spec)?;
    environments::fill(store, &mut spec, environment_id.as_deref()).await?;
    let patch = store.require_patch_any(&patch_id).await?;
    Ok(Job {
        kind,
        id: id.into(),
        project_id: patch.project_id,
        base_commit: patch.base_commit,
        patch_id: Some(patch_id),
        spec,
        open,
        // A split or abandoned job is recorded as inconclusive: no verdict.
        settled: verdict.filter(|v| v != "inconclusive"),
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
            // Whoever vouched for the patch's review has a stake in it too.
            backers.extend(crate::vouch::voucher(store, &p.patch_id).await?);
            Ok(Some(collective_core::review::excluded(&author, &backers)))
        }
        Kind::Canary => {
            let c = contributions::job(store, id).await?;
            Ok(Some(collective_core::review::excluded(&c.operator_id, &[])))
        }
        // The proposer can't prove its own environment.
        Kind::Environment => {
            let p = environments::proposal(store, id).await?;
            if p.state != "proposed" {
                return Ok(None);
            }
            Ok(Some(collective_core::review::excluded(
                p.proposed_by.as_deref().unwrap_or_default(),
                &[],
            )))
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
        Kind::Environment => environments::lapse(store, id, reason).await,
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
            // A lapsed proposal is refused as inconclusive; nobody is graded.
            store.db.stmt(
                "UPDATE project_environments SET state = 'refused', outcome = 'inconclusive', reason = ?, decided_at = ? WHERE state = 'proposed' AND created_at < ?",
                args![format!("inconclusive: {why}"), t, cutoff],
            )?,
        ])
        .await
}

/// What a runner needs to run a job it holds.
async fn run_view(store: &Store, run: &RunRow) -> ApiResult<Value> {
    let kind = Kind::parse(&run.job_kind).ok_or_else(|| ApiError::new(500, "bad job kind"))?;
    let job = load_job(store, kind, &run.job_id).await?;
    let project = store.require_project(&job.project_id, false).await?;
    let diff = match &job.patch_id {
        Some(p) => store.diff_text(p).await?,
        None => String::new(),
    };
    Ok(json!({ "job": {
        "run_id": run.id,
        "job_kind": kind.as_str(),
        "project": project.upstream_url,
        "base_commit": job.base_commit,
        "spec": job.spec,
        "diff": diff,
        "source": format!("/v1/proof-runs/{}/source", run.id),
        "expires_at": run.expires_at as i64,
    } }))
}

/// Claims the next job this operator may run, or returns the one this
/// agent already holds. The oldest job comes first: a runner can't choose.
/// Jobs in a Nix environment go only to callers with the `nix` capability.
pub async fn next_job(store: &Store, agent: &Agent, caps: Capabilities) -> ApiResult<Value> {
    sweep_stale(store).await?;
    environments::sweep_contests(store).await?;
    let t = now();
    // A held run is returned whatever the caller says it can do now: a
    // claim isn't dropped by changing flags.
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
    let offer = if caps.nix { Needs::Any } else { Needs::Stock };
    for (kind, c) in job_candidates(store, agent, t, offer).await? {
        if !may_run(store, agent, kind, &c).await? {
            continue;
        }
        // The lease covers realising the pinned (or proposed) environment.
        // One that can't be read back is never served as stock: the job is
        // closed.
        let spec = if kind == Kind::Environment {
            load_job(store, kind, &c.id).await.map(|j| j.spec)
        } else {
            let mut spec: ProofSpec = serde_json::from_str(&c.spec)?;
            environments::fill(store, &mut spec, c.environment_id.as_deref())
                .await
                .map(|_| spec)
        };
        let spec = match spec {
            Ok(spec) => spec,
            Err(e) => {
                close(store, kind, &c.id, &e.message).await?;
                continue;
            }
        };
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
    if !caps.nix {
        for (kind, c) in job_candidates(store, agent, t, Needs::Nix).await? {
            if may_run(store, agent, kind, &c).await? {
                return Ok(json!({ "job": null, "message": NEEDS_NIX_JOBS }));
            }
        }
    }
    Ok(json!({ "job": null, "message": "No proofs need running right now." }))
}

/// What a runner without the `nix` capability is told when the only jobs
/// it could have taken need Nix.
const NEEDS_NIX_JOBS: &str = "No proofs you can run without Nix need running right now, but there are jobs in projects with a Nix environment. \
Install Nix (single-user is enough; see docs/ENVIRONMENTS.md) and claim with `POST /v1/proofs/next?capabilities=nix`, \
which `collective prove` sends when `nix --version` works.";

/// Which jobs a candidate query selects, by the environment they run in.
#[derive(Clone, Copy, PartialEq)]
enum Needs {
    Any,
    /// The stock image only: what a caller without `nix` is offered.
    Stock,
    /// Nix environments only.
    Nix,
}

#[derive(Deserialize)]
struct JobCandidate {
    id: String,
    spec: String,
    environment_id: Option<String>,
    created_at: f64,
    mine: f64,
    active: f64,
}

/// Open jobs of every kind, oldest first, filtered by the environment
/// they need in the query itself, so a queue of Nix jobs can't crowd stock
/// ones out of a stock-only caller's candidates. A proof or canary job
/// needs Nix whenever it pinned an environment (stock is never pinned:
/// it's a NULL pin), whether or not the pinned record can be read back;
/// a proposal's job always does.
async fn job_candidates(
    store: &Store,
    agent: &Agent,
    t: i64,
    needs: Needs,
) -> ApiResult<Vec<(Kind, JobCandidate)>> {
    // `active`: counted runs in (conclusive ones; every one for an
    // environment, where inconclusive is a vote), plus runs still being
    // worked on.
    let counts = |kind: Kind, key: &str| {
        let counted = if kind == Kind::Environment {
            "r.submitted_at IS NOT NULL"
        } else {
            "r.submitted_at IS NOT NULL AND r.verdict != 'inconclusive'"
        };
        format!(
            "(SELECT COUNT(*) FROM proof_runs r WHERE r.job_kind = '{k}' AND r.job_id = {key} AND r.operator_id = ?) AS mine,
             (SELECT COUNT(*) FROM proof_runs r WHERE r.job_kind = '{k}' AND r.job_id = {key}
                AND (({counted}) OR (r.submitted_at IS NULL AND r.expires_at > ?))) AS active",
            k = kind.as_str()
        )
    };
    let mut cands: Vec<(Kind, JobCandidate)> = Vec::new();
    for (kind, key, table, open) in [
        (Kind::Proof, "j.id", "proofs", "j.status = 'queued'"),
        (
            Kind::Canary,
            "j.id",
            "canary_contributions",
            "j.state = 'verifying'",
        ),
        // A proposal's job is its version; its spec is built in `load_job`.
        (
            Kind::Environment,
            "(j.project_id || ':' || j.version)",
            "project_environments",
            "j.state = 'proposed'",
        ),
    ] {
        let (pin, env) = if kind == Kind::Environment {
            if needs == Needs::Stock {
                continue;
            }
            ("NULL AS environment_id", "1".to_string())
        } else {
            // Nothing runs in a quarantined environment: a job pinned to
            // one waits, unoffered, until the quarantine is lifted (or
            // goes stale like any other job).
            let quarantined = environments::quarantined_sql(
                "(SELECT pa.project_id FROM patches pa WHERE pa.id = j.patch_id)",
                "j.environment_id",
            );
            (
                "j.environment_id",
                format!(
                    "{} AND NOT {quarantined}",
                    match needs {
                        Needs::Any => "1",
                        Needs::Stock => "j.environment_id IS NULL",
                        Needs::Nix => "j.environment_id IS NOT NULL",
                    }
                ),
            )
        };
        let rows: Vec<JobCandidate> = store
            .db
            .all(
                &format!(
                    "SELECT {key} AS id, j.spec, {pin}, j.created_at, {} FROM {table} j WHERE {open} AND {env} ORDER BY j.created_at LIMIT 50",
                    counts(kind, key)
                ),
                args![&agent.operator_id, t],
            )
            .await?;
        cands.extend(rows.into_iter().map(|c| (kind, c)));
    }
    cands.sort_by(|a, b| a.1.created_at.total_cmp(&b.1.created_at));
    Ok(cands)
}

/// Whether this agent's operator may take a run of the job: it hasn't run
/// it, the job isn't full, and the operator has no stake in it.
async fn may_run(store: &Store, agent: &Agent, kind: Kind, c: &JobCandidate) -> ApiResult<bool> {
    if c.mine > 0.0 || c.active as usize >= MAX_RUNNERS {
        return Ok(false);
    }
    Ok(matches!(
        interested(store, kind, &c.id).await?,
        Some(ops) if !ops.contains(&agent.operator_id)
    ))
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
    let project = store.require_project(&job.project_id, false).await?;
    source_tarball(store, &project.id, &project.upstream_url, &job.base_commit).await
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
    let trust = store
        .trust_map(std::slice::from_ref(&agent.operator_id))
        .await?
        .get(&agent.operator_id)
        .copied()
        .unwrap_or(0.0);
    let record = |verdict: String, reason: String| {
        store.db.changes(
            "UPDATE proof_runs SET verdict = ?, reason = ?, report = ?, trust = ?, cpu_ms = ?, submitted_at = ? WHERE id = ? AND submitted_at IS NULL",
            args![verdict, reason, report_json.clone(), trust, sub.report.cpu_ms.map(|c| c as f64), now(), run_id],
        )
    };
    if kind == Kind::Environment {
        let o = suite_outcome(&job.spec, &sub.report);
        let reason = environments::outcome_reason(o);
        if record(outcome_str(o), reason.into()).await? == 0 {
            return status(409, "this run was already reported");
        }
        let settled = environment_run(store, &job, agent).await?;
        return Ok(json!({ "accepted": true, "verdict": o, "reason": reason, "settled": settled }));
    }
    let a = assess(&job.spec, &sub.report);
    if record(verdict_str(a.verdict), a.reason.clone()).await? == 0 {
        return status(409, "this run was already reported");
    }
    let reference = format!("{}:{}", kind.as_str(), job.id);
    let mut settled = job.settled.as_deref().and_then(parse_verdict);
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
    } else if let Some(v) = settled {
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

/// After a run of an environment's proof is recorded: decides the proposal
/// once a quorum of runners agree (adopt on `passed`, refuse on anything
/// else), lapses it when they split, and grades the runners as for a
/// proof. A late run is graded against the outcome the others settled.
/// Returns the settled outcome, if any.
async fn environment_run(store: &Store, job: &Job, agent: &Agent) -> ApiResult<Option<RunOutcome>> {
    let reference = format!("{}:{}", Kind::Environment.as_str(), job.id);
    let rows = run_rows(store, Kind::Environment, &job.id).await?;
    let claims = outcome_claims(&rows);
    let settled = job.settled.as_deref().and_then(parse_outcome);
    if !job.open {
        if let Some(v) = settled {
            let me: Vec<OutcomeClaim> = claims
                .into_iter()
                .filter(|c| c.runner == agent.operator_id)
                .collect();
            store
                .credit(&grade_outcome_runs(v, &me), &reference)
                .await?;
        }
        return Ok(settled);
    }
    let q = outcome_quorum(&claims);
    let Some(decision) = decide_proposal(q) else {
        return Ok(None);
    };
    let Quorum::Settled(v) = q else {
        close(
            store,
            Kind::Environment,
            &job.id,
            "the runners disagreed: no outcome reached a quorum",
        )
        .await?;
        return Ok(None);
    };
    let (agreement, _) = agreement(&rows, &outcome_str(v));
    let reason = format!("{}: {agreement}", outcome_str(v));
    if environments::decide(store, &job.id, decision, &reason).await? {
        store
            .credit(&grade_outcome_runs(v, &claims), &reference)
            .await?;
    }
    Ok(Some(v))
}

/// Who agrees on a settled verdict, as a reason, and one agreeing
/// runner's report to stand as the record (the rest are in proof_runs).
fn agreement(rows: &[RunRow], v: &str) -> (String, Option<String>) {
    let agreeing: Vec<&RunRow> = rows
        .iter()
        .filter(|r| r.verdict.as_deref() == Some(v))
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
    (reason, agreeing.first().and_then(|r| r.report.clone()))
}

/// Applies a verdict the runners settled, once: a proven regression
/// resolves its challenge, a demonstration is evidence for the jury or
/// council, and a canary goes live or fails. Returns whether this call
/// settled it.
async fn settle(store: &Store, job: &Job, v: ProofVerdict, rows: &[RunRow]) -> ApiResult<bool> {
    let (reason, report) = agreement(rows, &verdict_str(v));
    match job.kind {
        Kind::Environment => unreachable!("environments settle in environment_run"),
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
