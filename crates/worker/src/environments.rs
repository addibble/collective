//! Project environments over D1 (docs/ENVIRONMENTS.md): versioned records
//! of the system layer a project's proofs and review runs execute in. What
//! a record means (validity, canonical form, id) is in
//! collective_core::environment.
//!
//! Every job pins the version current when it was created, and the server
//! fills `spec.environment` from that pin whenever a spec leaves it, so a
//! submitter can't choose the environment and every runner of a job runs
//! the same one. A NULL pin is the stock image. A pin that can't be read
//! back makes the job unservable; it never falls back to stock.

use collective_core::environment::{
    may_propose, Environment, ProposalDecision, MAX_OPEN_PROPOSALS_PER_PROJECT,
    MAX_PROPOSALS_PER_OPERATOR_PER_DAY, MIN_PROPOSER_TRUST,
};
use collective_core::proof::{ProofKind, ProofSpec, RunOutcome};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::review;
use crate::store::{now, Agent, Store};

/// The environment a new job pins: the adopted version's id and record, or
/// `None` for the stock image.
pub type Current = Option<(String, Environment)>;

/// The id a job stores for its pin (NULL for stock).
pub fn pin_id(current: &Current) -> Option<&str> {
    current.as_ref().map(|(id, _)| id.as_str())
}

/// `stock` or `nix`, stored on review tasks for capability matching.
pub fn kind(current: &Current) -> &'static str {
    match current {
        Some((_, Environment::Nix(_))) => "nix",
        _ => "stock",
    }
}

/// What a caller says it can run, from `?capabilities=a,b` on the claim
/// routes or a `capabilities` array in `collective_review_next`. Only `nix`
/// means anything; other names are ignored. A caller that doesn't say is
/// offered only stock-image work, and so is one whose claim is malformed:
/// work pinned to a Nix environment (or to a record that can't be read
/// back) is never handed to a caller that hasn't said it has Nix.
#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    pub nix: bool,
}

impl Capabilities {
    fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Capabilities {
        Capabilities {
            nix: names.into_iter().any(|n| n.trim() == "nix"),
        }
    }

    /// From a JSON array of names, or a comma-separated string (as the
    /// query parameter arrives).
    pub fn from_value(v: Option<&Value>) -> Capabilities {
        match v {
            Some(Value::Array(a)) => Capabilities::from_names(a.iter().filter_map(Value::as_str)),
            Some(Value::String(s)) => Capabilities::from_names(s.split(',')),
            _ => Capabilities::default(),
        }
    }
}

/// Reads a stored record and checks it is what its id says: a row that
/// doesn't parse, doesn't validate or doesn't hash to its id is refused.
fn decode(id: &str, spec: &str) -> ApiResult<Environment> {
    let unreadable = |why: String| {
        ApiError::new(
            500,
            format!(
                "stored environment {id} is unreadable ({why}): work pinned to it can't be served, and new work can't pin it"
            ),
        )
    };
    let env: Environment = serde_json::from_str(spec).map_err(|e| unreadable(e.to_string()))?;
    env.validate().map_err(unreadable)?;
    if env.id() != id {
        return Err(unreadable("its content does not match its id".into()));
    }
    Ok(env)
}

#[derive(Deserialize)]
struct Pointer {
    environment_id: Option<String>,
    spec: Option<String>,
    state: Option<String>,
}

async fn pointer(store: &Store, project_id: &str) -> ApiResult<Option<Pointer>> {
    // The same record can appear in several versions; the newest one that
    // was adopted (or later quarantined) describes the current pin.
    store
        .db
        .first::<Pointer>(
            "SELECT p.environment_id, e.spec, e.state FROM projects p
             LEFT JOIN project_environments e ON e.project_id = p.id AND e.id = p.environment_id
               AND e.state IN ('adopted', 'quarantined')
             WHERE p.id = ? ORDER BY e.version DESC LIMIT 1",
            args![project_id],
        )
        .await
}

/// The environment new jobs of this project pin. Fails closed: a
/// quarantined environment stops new jobs rather than letting them run in
/// the stock image, and so does a record that can't be read back.
pub async fn current(store: &Store, project_id: &str) -> ApiResult<Current> {
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let Some(id) = p.environment_id else {
        return Ok(None);
    };
    match (p.state.as_deref(), p.spec) {
        (Some("adopted"), Some(spec)) => Ok(Some((id.clone(), decode(&id, &spec)?))),
        (Some("quarantined"), _) => status(
            409,
            format!("this project's environment {id} is quarantined: no new runs until an admin sets another (PUT /v1/admin/projects/:project/environment)"),
        ),
        _ => Err(ApiError::new(
            500,
            format!("this project points at environment {id}, which has no adopted version; no new runs until an admin sets one"),
        )),
    }
}

/// The record a job pinned. A version that was quarantined after the job
/// was created still applies to it; only new jobs stop pinning it.
pub async fn by_id(store: &Store, id: &str) -> ApiResult<Environment> {
    #[derive(Deserialize)]
    struct R {
        spec: String,
    }
    match store
        .db
        .first::<R>(
            "SELECT spec FROM project_environments WHERE id = ? LIMIT 1",
            args![id],
        )
        .await?
    {
        Some(r) => decode(id, &r.spec),
        None => Err(ApiError::new(
            500,
            format!("environment {id} is not on record; jobs pinned to it can't be served"),
        )),
    }
}

/// The record for a stored pin: stock when NULL.
pub async fn pinned(store: &Store, id: Option<&str>) -> ApiResult<Environment> {
    match id {
        Some(id) => by_id(store, id).await,
        None => Ok(Environment::Stock),
    }
}

/// Sets `spec.environment` from a job's pin, overwriting whatever was there.
pub async fn fill(store: &Store, spec: &mut ProofSpec, id: Option<&str>) -> ApiResult<()> {
    spec.environment = pinned(store, id).await?;
    Ok(())
}

/// Sets a project's environment directly (admin), for bootstrap until
/// operators with standing propose them. `null` returns to the stock
/// image. Either way a new adopted version is recorded, so the history
/// stays complete; setting what is already current changes nothing.
pub async fn set_admin(store: &Store, project_id: &str, body: Value) -> ApiResult<Value> {
    let env = if body.is_null() {
        Environment::Stock
    } else {
        serde_json::from_value::<Environment>(body).map_err(|e| {
            ApiError::new(
                400,
                format!("expected {{\"nix\": {{\"nixpkgs\": …, \"packages\": […]}}}} or null: {e}"),
            )
        })?
    };
    if let Err(e) = env.validate() {
        return bad(e);
    }
    let env = env.canonical();
    let id = env.id();
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let unchanged = match (&env, p.environment_id.as_deref()) {
        (Environment::Stock, None) => true,
        (Environment::Nix(_), Some(cur)) => cur == id && p.state.as_deref() == Some("adopted"),
        _ => false,
    };
    if !unchanged {
        #[derive(Deserialize)]
        struct V {
            n: f64,
        }
        let version = store
            .db
            .first::<V>(
                "SELECT COALESCE(MAX(version), 0) + 1 AS n FROM project_environments WHERE project_id = ?",
                args![project_id],
            )
            .await?
            .map_or(1.0, |v| v.n);
        let t = now();
        let points_at = matches!(env, Environment::Nix(_)).then(|| id.clone());
        store
            .db
            .batch(vec![
                store.db.stmt(
                    "INSERT INTO project_environments (id, project_id, version, spec, state, proposed_by, created_at, decided_at, reason)
                     VALUES (?, ?, ?, ?, 'adopted', NULL, ?, ?, 'set by an admin')",
                    args![&id, project_id, version, serde_json::to_string(&env)?, t, t],
                )?,
                store.db.stmt(
                    "UPDATE projects SET environment_id = ? WHERE id = ?",
                    args![points_at, project_id],
                )?,
            ])
            .await?;
    }
    let mut v = list(store, project_id).await?;
    v["changed"] = json!(!unchanged);
    Ok(v)
}

/// A project's environment history, newest first, and what new jobs pin.
/// Public: every job's environment can be read whole.
pub async fn list(store: &Store, project_id: &str) -> ApiResult<Value> {
    #[derive(Deserialize)]
    struct R {
        id: String,
        version: f64,
        spec: String,
        state: String,
        proposed_by: Option<String>,
        created_at: f64,
        decided_at: Option<f64>,
        reason: Option<String>,
        base_commit: Option<String>,
    }
    let rows: Vec<R> = store
        .db
        .all(
            "SELECT id, version, spec, state, proposed_by, created_at, decided_at, reason, base_commit
             FROM project_environments WHERE project_id = ? ORDER BY version DESC LIMIT 100",
            args![project_id],
        )
        .await?;
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let current = match &p.environment_id {
        None => json!({ "id": null, "environment": Environment::Stock }),
        Some(id) => json!({
            "id": id,
            "environment": p.spec.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
            "state": p.state,
        }),
    };
    let versions: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "version": r.version as u32,
                "id": r.id,
                "environment": serde_json::from_str::<Value>(&r.spec).unwrap_or(Value::Null),
                "state": r.state,
                "proposed_by": r.proposed_by,
                "created_at": r.created_at as i64,
                "decided_at": r.decided_at.map(|t| t as i64),
                "reason": r.reason,
                "base_commit": r.base_commit,
            })
        })
        .collect();
    Ok(json!({ "project_id": project_id, "current": current, "versions": versions }))
}

// -- proposals: operators with standing propose, runners prove ---------------

/// The proof job of a proposal: `<project id>:<version>`, the version's key.
pub fn job_id(project_id: &str, version: u32) -> String {
    format!("{project_id}:{version}")
}

/// A proposed version (or one that was proposed and since decided).
#[derive(Debug, Deserialize)]
pub struct Proposal {
    pub project_id: String,
    pub version: f64,
    pub id: String,
    pub spec: String,
    pub state: String,
    pub proposed_by: Option<String>,
    pub base_commit: Option<String>,
    pub run_spec: Option<String>,
    pub outcome: Option<String>,
}

impl Proposal {
    pub fn job_id(&self) -> String {
        job_id(&self.project_id, self.version as u32)
    }

    /// What its runners run: the project's suite as it was when the
    /// proposal was made, once, in the proposed environment. A record that
    /// can't be read back is never run as stock: it fails.
    pub fn proof_spec(&self) -> ApiResult<ProofSpec> {
        let env = decode(&self.id, &self.spec)?;
        let mut spec: ProofSpec = self
            .run_spec
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or_else(|| {
                ApiError::new(
                    500,
                    format!("proposal {} has no readable run spec", self.job_id()),
                )
            })?;
        spec.kind = ProofKind::Suite;
        spec.repeats = 1;
        spec.environment = env;
        spec.validate()
            .map_err(|e| ApiError::new(500, format!("proposal {}: {e}", self.job_id())))?;
        Ok(spec)
    }
}

/// The proposal behind a proof job.
pub async fn proposal(store: &Store, job: &str) -> ApiResult<Proposal> {
    let unknown = || ApiError::new(404, "unknown environment proposal");
    let (project_id, version) = job.rsplit_once(':').ok_or_else(unknown)?;
    let version: u32 = version.parse().map_err(|_| unknown())?;
    store
        .db
        .first::<Proposal>(
            "SELECT project_id, version, id, spec, state, proposed_by, base_commit, run_spec, outcome
             FROM project_environments WHERE project_id = ? AND version = ?",
            args![project_id, version],
        )
        .await?
        .ok_or_else(unknown)
}

/// The commit a project's suite is proven on: its default base if it has
/// one, else the base of its newest patch.
async fn proof_base(store: &Store, project_id: &str) -> ApiResult<Option<String>> {
    #[derive(Deserialize)]
    struct B {
        base: Option<String>,
    }
    Ok(store
        .db
        .first::<B>(
            "SELECT COALESCE(p.default_base,
               (SELECT base_commit FROM patches WHERE project_id = p.id ORDER BY created_at DESC LIMIT 1)) AS base
             FROM projects p WHERE p.id = ?",
            args![project_id],
        )
        .await?
        .and_then(|b| b.base))
}

/// Proposes an environment for a project (`POST /v1/projects/:p/environments`,
/// `collective_environment_propose`). Needs standing, because the proof
/// spends a quorum of other operators' compute: the version is stored as
/// `proposed`, and that row is the job runners claim
/// (`proofs::next_job`). It is adopted or refused by their reports.
pub async fn propose(
    store: &Store,
    agent: &Agent,
    project_id: &str,
    body: Value,
) -> ApiResult<Value> {
    let trust = store
        .trust_map(std::slice::from_ref(&agent.operator_id))
        .await?
        .get(&agent.operator_id)
        .copied()
        .unwrap_or(0.0);
    if !may_propose(trust) {
        return status(
            403,
            format!(
                "proposing an environment needs standing: your operator's trust is {trust:.2} and a proposal needs {MIN_PROPOSER_TRUST}. \
Trust is your operator's standing, from its rep: newcomers start at 0.25 and earn the rest through patches other operators adopt, \
review tasks done well (collective_review_next) and proof runs that agree with the other runners (`collective prove`). \
A proposal costs several runners a suite run, so it is open to operators who have shown they do work that holds up."
            ),
        );
    }
    let env = serde_json::from_value::<Environment>(body).map_err(|e| {
        ApiError::new(
            400,
            format!("expected {{\"nix\": {{\"nixpkgs\": <commit>, \"nixpkgs_hash\": …, \"packages\": […]}}}}: {e}"),
        )
    })?;
    if env == Environment::Stock {
        return bad("propose a Nix package set; a project without one runs in the stock image, and only an admin returns a project to it");
    }
    if let Err(e) = env.validate() {
        return bad(e);
    }
    let env = env.canonical();
    let id = env.id();
    let Some(run_spec) = review::project_run_spec(store, project_id).await? else {
        return bad("this project has no run spec: set a run spec first (PUT /v1/admin/projects/:project/run). An environment is proven by running the project's suite in it");
    };
    let Some(base) = proof_base(store, project_id).await? else {
        return bad("this project has no patches yet, so there is no base commit to prove an environment on");
    };
    let taken = store
        .count(
            "SELECT COUNT(*) AS n FROM project_environments e JOIN projects p ON p.id = e.project_id
             WHERE e.project_id = ? AND e.id = ?
               AND (e.state = 'proposed' OR (e.state = 'adopted' AND p.environment_id = e.id))",
            args![project_id, &id],
        )
        .await?;
    if taken > 0 {
        return status(
            409,
            format!("{id} is already this project's environment or an open proposal for it (GET /v1/projects/:project/environments)"),
        );
    }
    let open = store
        .count(
            "SELECT COUNT(*) AS n FROM project_environments WHERE project_id = ? AND state = 'proposed'",
            args![project_id],
        )
        .await?;
    if open >= MAX_OPEN_PROPOSALS_PER_PROJECT {
        return status(
            429,
            format!("this project already has {MAX_OPEN_PROPOSALS_PER_PROJECT} proposals waiting for runners; try again when one is decided"),
        );
    }
    let today = store
        .count(
            "SELECT COUNT(*) AS n FROM project_environments WHERE proposed_by = ? AND created_at > ?",
            args![&agent.operator_id, now() - 86_400_000],
        )
        .await?;
    if today >= MAX_PROPOSALS_PER_OPERATOR_PER_DAY {
        return status(
            429,
            format!("at most {MAX_PROPOSALS_PER_OPERATOR_PER_DAY} environment proposals per operator per day"),
        );
    }
    // The version is computed in the insert itself, so two proposals made at
    // once can't take the same number (the primary key refuses a repeat).
    store
        .db
        .run(
            "INSERT INTO project_environments (id, project_id, version, spec, state, proposed_by, created_at, base_commit, run_spec)
             SELECT ?, ?, COALESCE(MAX(version), 0) + 1, ?, 'proposed', ?, ?, ?, ? FROM project_environments WHERE project_id = ?",
            args![&id, project_id, serde_json::to_string(&env)?, &agent.operator_id, now(), &base, serde_json::to_string(&run_spec)?, project_id],
        )
        .await?;
    #[derive(Deserialize)]
    struct V {
        version: f64,
    }
    let version = store
        .db
        .first::<V>(
            "SELECT version FROM project_environments WHERE project_id = ? AND id = ? AND state = 'proposed' ORDER BY version DESC LIMIT 1",
            args![project_id, &id],
        )
        .await?
        .map_or(0, |v| v.version as u32);
    Ok(json!({
        "project_id": project_id,
        "version": version,
        "id": id,
        "environment": env,
        "state": "proposed",
        "base_commit": base,
        "job": job_id(project_id, version),
        "message": "Proposed. Runners will run the project's suite in it; it is adopted when a quorum of them pass it, and refused otherwise. You can't run it yourself. Follow it with GET /v1/projects/:project/environments.",
    }))
}

/// Decides a proposal once: adopts it (and points the project at it) or
/// refuses it, and grades the proposer. Returns whether this call decided
/// it.
pub async fn decide(
    store: &Store,
    job: &str,
    decision: ProposalDecision,
    reason: &str,
) -> ApiResult<bool> {
    let p = proposal(store, job).await?;
    let outcome = serde_json::to_value(decision.outcome())?;
    let decided = store
        .db
        .changes(
            "UPDATE project_environments SET state = ?, outcome = ?, reason = ?, decided_at = ?
             WHERE project_id = ? AND version = ? AND state = 'proposed'",
            args![
                decision.state(),
                outcome.as_str(),
                reason,
                now(),
                &p.project_id,
                p.version
            ],
        )
        .await?;
    if decided == 0 {
        return Ok(false);
    }
    if decision == ProposalDecision::Adopt {
        store
            .db
            .run(
                "UPDATE projects SET environment_id = ? WHERE id = ?",
                args![&p.id, &p.project_id],
            )
            .await?;
    }
    if let Some(e) = p
        .proposed_by
        .as_deref()
        .and_then(|op| decision.proposer_entry(op))
    {
        store.credit(&[e], &format!("environment:{job}")).await?;
    }
    Ok(true)
}

/// Refuses a proposal as inconclusive without grading anyone: its runners
/// split, or its proof couldn't be served.
pub async fn lapse(store: &Store, job: &str, reason: &str) -> ApiResult<()> {
    decide(
        store,
        job,
        ProposalDecision::Lapse,
        &format!("inconclusive: {reason}"),
    )
    .await
    .map(|_| ())
}

/// What a suite outcome says about the environment it ran in.
pub fn outcome_reason(o: RunOutcome) -> &'static str {
    match o {
        RunOutcome::Passed => "the project's suite passes in it",
        RunOutcome::BaseFails => "the project's suite fails in it",
        RunOutcome::Regressed | RunOutcome::Flaky => {
            "runs of the same tree disagree in it: the suite is not deterministic there"
        }
        RunOutcome::Inconclusive => {
            "the suite could not be run in it (setup, realisation, isolation or missing runs)"
        }
    }
}
