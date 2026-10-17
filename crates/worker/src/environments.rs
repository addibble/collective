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
    contest_bond_entry, contest_cutoff, contest_entries, contest_quorum, contest_ref,
    contest_settled, may_contest, may_post_bond, may_propose, ContestClaim, ContestSide,
    Environment, ProposalDecision, CONTEST_BOND, CONTEST_BOND_REP, CONTEST_DAYS,
    MAX_OPEN_PROPOSALS_PER_PROJECT, MAX_PROPOSALS_PER_OPERATOR_PER_DAY, MIN_PROPOSER_TRUST,
};
use collective_core::proof::{ProofKind, ProofSpec, Quorum, RunOutcome};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::review;
use crate::store::{now, Agent, Store};

/// The environment a new job of a project pins.
#[derive(Debug, Clone)]
pub enum Current {
    /// The stock prover image (a NULL pin).
    Stock,
    /// The adopted version's id and record.
    Adopted(String, Environment),
    /// The current version is quarantined (ENVIRONMENTS.md, Quarantine):
    /// the project is review-only. New tasks still pin it, so their history
    /// says what the project's environment was, but have no run phase; new
    /// proofs and canary verifications are refused.
    Quarantined { id: String, reason: String },
}

impl Current {
    /// Refuses work that would run in the environment while it is
    /// quarantined: 409 with the admin's reason.
    pub fn refuse_if_quarantined(&self) -> ApiResult<()> {
        match self {
            Current::Quarantined { id, reason } => status(409, quarantined_message(id, reason)),
            _ => Ok(()),
        }
    }

    /// Whether new review tasks get a run phase (if the project has a suite).
    pub fn runs(&self) -> bool {
        !matches!(self, Current::Quarantined { .. })
    }

    /// The record to write into a new job's spec: `None` keeps stock.
    pub fn environment(&self) -> Option<&Environment> {
        match self {
            Current::Adopted(_, e) => Some(e),
            _ => None,
        }
    }
}

fn quarantined_message(id: &str, reason: &str) -> String {
    format!(
        "this project's environment {id} is quarantined ({reason}): the project is review-only, so nothing new runs in it. \
Patches are still accepted and reviewed without a run phase. Operators with standing can contest the quarantine \
(collective_environment_contest, POST /v1/projects/:project/environments/:version/contest), and an adopted proposal or an admin can replace or lift it"
    )
}

/// The id a job stores for its pin (NULL for stock).
pub fn pin_id(current: &Current) -> Option<&str> {
    match current {
        Current::Stock => None,
        Current::Adopted(id, _) | Current::Quarantined { id, .. } => Some(id),
    }
}

/// `stock` or `nix`, stored on review tasks for capability matching. A
/// quarantined pin keeps its kind: stock is never pinned by id, so any
/// pinned record is a Nix one, and a task pinned to it goes only to callers
/// with Nix, as every pinned job does.
pub fn kind(current: &Current) -> &'static str {
    match current {
        Current::Adopted(_, Environment::Nix(_)) | Current::Quarantined { .. } => "nix",
        _ => "stock",
    }
}

/// SQL: whether the pin `{pin}` (an environment id expression) is
/// quarantined in project `{project}`: the newest version of that record
/// there that was adopted or quarantined is quarantined, as `current`
/// reads it. Work pinned to such a record is not offered to anyone until
/// the quarantine is lifted, so nothing runs in it.
pub fn quarantined_sql(project: &str, pin: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM project_environments q WHERE q.project_id = {project} AND q.id = {pin} AND q.state = 'quarantined'
           AND q.version = (SELECT MAX(q2.version) FROM project_environments q2
                            WHERE q2.project_id = q.project_id AND q2.id = q.id AND q2.state IN ('adopted', 'quarantined')))"
    )
}

/// The reason a job's pin is quarantined in its project, if it is. A pin
/// of NULL (stock) is never quarantined.
pub async fn pin_quarantine(
    store: &Store,
    project_id: &str,
    pin: Option<&str>,
) -> ApiResult<Option<String>> {
    let Some(pin) = pin else {
        return Ok(None);
    };
    #[derive(Deserialize)]
    struct R {
        state: String,
        quarantine_reason: Option<String>,
    }
    let row = store
        .db
        .first::<R>(
            "SELECT state, quarantine_reason FROM project_environments
             WHERE project_id = ? AND id = ? AND state IN ('adopted', 'quarantined')
             ORDER BY version DESC LIMIT 1",
            args![project_id, pin],
        )
        .await?;
    Ok(match row {
        Some(r) if r.state == "quarantined" => Some(
            r.quarantine_reason
                .unwrap_or_else(|| "no reason recorded".into()),
        ),
        _ => None,
    })
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
    version: Option<f64>,
    spec: Option<String>,
    state: Option<String>,
    quarantine_reason: Option<String>,
}

async fn pointer(store: &Store, project_id: &str) -> ApiResult<Option<Pointer>> {
    // The same record can appear in several versions; the newest one that
    // was adopted (or later quarantined) describes the current pin.
    store
        .db
        .first::<Pointer>(
            "SELECT p.environment_id, e.version, e.spec, e.state, e.quarantine_reason FROM projects p
             LEFT JOIN project_environments e ON e.project_id = p.id AND e.id = p.environment_id
               AND e.state IN ('adopted', 'quarantined')
             WHERE p.id = ? ORDER BY e.version DESC LIMIT 1",
            args![project_id],
        )
        .await
}

/// The environment new jobs of this project pin. Fails closed: a
/// quarantined environment makes the project review-only rather than
/// letting anything run in the stock image, and a record that can't be
/// read back refuses new work.
pub async fn current(store: &Store, project_id: &str) -> ApiResult<Current> {
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let Some(id) = p.environment_id else {
        return Ok(Current::Stock);
    };
    match (p.state.as_deref(), p.spec) {
        (Some("adopted"), Some(spec)) => Ok(Current::Adopted(id.clone(), decode(&id, &spec)?)),
        (Some("quarantined"), _) => Ok(Current::Quarantined {
            id,
            reason: p
                .quarantine_reason
                .unwrap_or_else(|| "no reason recorded".into()),
        }),
        _ => Err(ApiError::new(
            500,
            format!("this project points at environment {id}, which has no adopted version; no new runs until an admin sets one"),
        )),
    }
}

/// The record a job pinned. A version quarantined after the job was
/// created still describes it (a run already claimed can be reported), but
/// jobs pinned to a quarantined record aren't offered (`quarantined_sql`)
/// and review tasks pinned to one have no run phase (`review::task_runs`).
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
/// Public: every job's environment can be read whole, and so can every
/// quarantine and the stakes on it.
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
        quarantined_at: Option<f64>,
        quarantine_reason: Option<String>,
        contest_settled: Option<String>,
        quarantine_lifted_at: Option<f64>,
        quarantine_lifted_by: Option<String>,
    }
    let rows: Vec<R> = store
        .db
        .all(
            "SELECT id, version, spec, state, proposed_by, created_at, decided_at, reason, base_commit,
                    quarantined_at, quarantine_reason, contest_settled, quarantine_lifted_at, quarantine_lifted_by
             FROM project_environments WHERE project_id = ? ORDER BY version DESC LIMIT 100",
            args![project_id],
        )
        .await?;
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let mut stakes = std::collections::HashMap::<u32, Vec<Value>>::new();
    for s in contest_stakes(store, project_id, None).await? {
        stakes.entry(s.version as u32).or_default().push(json!({
            "operator_id": s.operator_id,
            "side": s.side,
            "bond": s.bond,
            "trust": s.trust,
            "created_at": s.created_at as i64,
        }));
    }
    let current = match &p.environment_id {
        None => json!({ "id": null, "environment": Environment::Stock }),
        Some(id) => json!({
            "id": id,
            "version": p.version.map(|v| v as u32),
            "environment": p.spec.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
            "state": p.state,
            "quarantine_reason": if p.state.as_deref() == Some("quarantined") { p.quarantine_reason.clone() } else { None },
        }),
    };
    let versions: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let version = r.version as u32;
            let mut v = json!({
                "version": version,
                "id": r.id,
                "environment": serde_json::from_str::<Value>(&r.spec).unwrap_or(Value::Null),
                "state": r.state,
                "proposed_by": r.proposed_by,
                "created_at": r.created_at as i64,
                "decided_at": r.decided_at.map(|t| t as i64),
                "reason": r.reason,
                "base_commit": r.base_commit,
            });
            if r.quarantined_at.is_some() {
                v["quarantine"] = json!({
                    "quarantined_at": r.quarantined_at.map(|t| t as i64),
                    "reason": r.quarantine_reason,
                    "contest_settled": r.contest_settled,
                    "contest_closes_at": r.quarantined_at.map(|t| t as i64 + CONTEST_DAYS as i64 * 86_400_000),
                    "lifted_at": r.quarantine_lifted_at.map(|t| t as i64),
                    "lifted_by": r.quarantine_lifted_by,
                    "stakes": stakes.remove(&version).unwrap_or_default(),
                });
            }
            v
        })
        .collect();
    Ok(json!({ "project_id": project_id, "current": current, "versions": versions }))
}

// -- quarantine and contests -------------------------------------------------

/// A version as quarantine and contests see it.
#[derive(Deserialize)]
struct QuarantineRow {
    id: String,
    state: String,
    quarantined_at: Option<f64>,
    contest_settled: Option<String>,
}

async fn quarantine_row(
    store: &Store,
    project_id: &str,
    version: u32,
) -> ApiResult<Option<QuarantineRow>> {
    store
        .db
        .first::<QuarantineRow>(
            "SELECT id, state, quarantined_at, contest_settled FROM project_environments
             WHERE project_id = ? AND version = ?",
            args![project_id, version],
        )
        .await
}

#[derive(Deserialize)]
struct StakeRow {
    version: f64,
    operator_id: String,
    side: String,
    bond: f64,
    trust: f64,
    created_at: f64,
}

/// The stakes on a project's versions (one version, or all), oldest first.
async fn contest_stakes(
    store: &Store,
    project_id: &str,
    version: Option<u32>,
) -> ApiResult<Vec<StakeRow>> {
    match version {
        Some(v) => {
            store
                .db
                .all(
                    "SELECT version, operator_id, side, bond, trust, created_at FROM environment_contests
                     WHERE project_id = ? AND version = ? ORDER BY created_at, operator_id",
                    args![project_id, v],
                )
                .await
        }
        None => {
            store
                .db
                .all(
                    "SELECT version, operator_id, side, bond, trust, created_at FROM environment_contests
                     WHERE project_id = ? ORDER BY created_at, operator_id LIMIT 500",
                    args![project_id],
                )
                .await
        }
    }
}

/// The stakes as the core counts them. A side this build can't read is
/// left out of the quorum (it can't decide anything), but its bond still
/// comes back on a timeout or an admin's lift: see [`settlement_claims`].
fn claims(stakes: &[StakeRow]) -> Vec<ContestClaim> {
    stakes
        .iter()
        .filter_map(|s| {
            Some(ContestClaim {
                operator: s.operator_id.clone(),
                trust: s.trust,
                side: ContestSide::parse(&s.side)?,
                bond: s.bond,
            })
        })
        .collect()
}

/// Settles a quarantine's contest once: claims `contest_settled` (and, for
/// a lift, returns the version to `adopted`) and writes the ledger entries.
/// Returns whether this call settled it.
async fn settle_contest(
    store: &Store,
    project_id: &str,
    version: u32,
    quarantined_at: i64,
    winner: Option<ContestSide>,
    claims: &[ContestClaim],
) -> ApiResult<bool> {
    let t = now();
    let settled = contest_settled(winner);
    let claimed = match winner {
        Some(ContestSide::Lift) => {
            store
                .db
                .changes(
                    "UPDATE project_environments SET contest_settled = ?, state = 'adopted', quarantine_lifted_at = ?, quarantine_lifted_by = 'contest'
                     WHERE project_id = ? AND version = ? AND state = 'quarantined' AND contest_settled IS NULL AND quarantined_at = ?",
                    args![settled, t, project_id, version, quarantined_at],
                )
                .await?
        }
        _ => {
            store
                .db
                .changes(
                    "UPDATE project_environments SET contest_settled = ?
                     WHERE project_id = ? AND version = ? AND state = 'quarantined' AND contest_settled IS NULL AND quarantined_at = ?",
                    args![settled, project_id, version, quarantined_at],
                )
                .await?
        }
    };
    if claimed == 0 {
        return Ok(false);
    }
    store
        .credit(
            &contest_entries(winner, claims),
            &contest_ref(project_id, version, quarantined_at),
        )
        .await?;
    Ok(true)
}

/// Quarantines a project's current environment (admin,
/// `POST /v1/admin/projects/:project/environment/quarantine {reason}`). The
/// project becomes review-only and a contest opens for [`CONTEST_DAYS`].
/// The admin's quarantine is a claim, not a stake.
pub async fn quarantine(store: &Store, project_id: &str, body: Value) -> ApiResult<Value> {
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if reason.is_empty() || reason.chars().count() > 500 {
        return bad(
            "give a `reason` (1-500 characters): it is shown to everyone the quarantine refuses",
        );
    }
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let Some(id) = p.environment_id.as_deref() else {
        return status(
            409,
            "this project runs in the stock image: there is no environment to quarantine",
        );
    };
    let Some(version) = p.version else {
        return status(409, format!("{id} has no adopted version to quarantine"));
    };
    match p.state.as_deref() {
        Some("adopted") => {}
        Some("quarantined") => return status(409, format!("{id} is already quarantined")),
        _ => return status(409, format!("{id} has no adopted version to quarantine")),
    }
    let version = version as u32;
    let t = now();
    // A version quarantined before starts a new contest: the earlier one's
    // stakes are all settled (a lift settles them), and their ledger
    // entries stay under that quarantine's reference.
    store
        .db
        .batch(vec![
            store.db.stmt(
                "DELETE FROM environment_contests WHERE project_id = ? AND version = ?
                   AND EXISTS (SELECT 1 FROM project_environments WHERE project_id = ? AND version = ? AND state = 'adopted')",
                args![project_id, version, project_id, version],
            )?,
            store.db.stmt(
                "UPDATE project_environments SET state = 'quarantined', quarantined_at = ?, quarantine_reason = ?,
                   contest_settled = NULL, quarantine_lifted_at = NULL, quarantine_lifted_by = NULL
                 WHERE project_id = ? AND version = ? AND state = 'adopted'",
                args![t, reason, project_id, version],
            )?,
        ])
        .await?;
    let mut v = list(store, project_id).await?;
    v["quarantined"] =
        json!({ "version": version, "id": id, "reason": reason, "quarantined_at": t });
    Ok(v)
}

/// Lifts a quarantine directly (admin,
/// `POST /v1/admin/projects/:project/environment/lift`, optional
/// `{version}`, default the current one). Stakes on a contest nothing has
/// settled come back in full (`contest_settled = 'admin'`): no quorum
/// judged anyone.
pub async fn lift(store: &Store, project_id: &str, body: Value) -> ApiResult<Value> {
    let Some(p) = pointer(store, project_id).await? else {
        return status(404, "unknown project");
    };
    let version = match body.get("version").and_then(Value::as_f64) {
        Some(v) => v as u32,
        None => match (p.state.as_deref(), p.version) {
            (Some("quarantined"), Some(v)) => v as u32,
            _ => return status(409, "this project's current environment is not quarantined"),
        },
    };
    let Some(row) = quarantine_row(store, project_id, version).await? else {
        return status(404, "unknown environment version");
    };
    if row.state != "quarantined" {
        return status(409, format!("version {version} is not quarantined"));
    }
    let quarantined_at = row.quarantined_at.map_or(0, |t| t as i64);
    let t = now();
    // Claim the open contest first, so its bonds are returned once and a
    // quorum that settles concurrently doesn't also pay out.
    let claimed = store
        .db
        .changes(
            "UPDATE project_environments SET contest_settled = 'admin'
             WHERE project_id = ? AND version = ? AND state = 'quarantined' AND contest_settled IS NULL",
            args![project_id, version],
        )
        .await?;
    let lifted = store
        .db
        .changes(
            "UPDATE project_environments SET state = 'adopted', quarantine_lifted_at = ?, quarantine_lifted_by = 'admin'
             WHERE project_id = ? AND version = ? AND state = 'quarantined'",
            args![t, project_id, version],
        )
        .await?;
    if claimed > 0 {
        let stakes = contest_stakes(store, project_id, Some(version)).await?;
        store
            .credit(
                &contest_entries(None, &settlement_claims(&stakes)),
                &contest_ref(project_id, version, quarantined_at),
            )
            .await?;
    }
    if lifted == 0 {
        return status(409, format!("version {version} is not quarantined"));
    }
    let mut v = list(store, project_id).await?;
    v["lifted"] = json!({ "version": version, "id": row.id });
    Ok(v)
}

/// Stakes a bond on a quarantined version
/// (`POST /v1/projects/:project/environments/:version/contest {side}`,
/// `collective_environment_contest`): `lift` says the environment is fine,
/// `keep` that the quarantine is right. Needs standing; the bond comes out
/// of ledger rep in the same batch as the stake. Once a quorum of stakes
/// agrees the contest settles: `lift` returns the version to service.
pub async fn contest(
    store: &Store,
    agent: &Agent,
    project_id: &str,
    version: u32,
    body: Value,
) -> ApiResult<Value> {
    sweep_contests(store).await?;
    let side = body
        .get("side")
        .and_then(Value::as_str)
        .and_then(ContestSide::parse)
        .ok_or_else(|| ApiError::new(400, "`side` must be \"lift\" (the environment is fine) or \"keep\" (the quarantine is right)"))?;
    let Some(row) = quarantine_row(store, project_id, version).await? else {
        return status(404, "unknown environment version");
    };
    if row.state != "quarantined" {
        return status(
            409,
            format!(
                "version {version} is not quarantined (it is {}): there is nothing to contest",
                row.state
            ),
        );
    }
    if let Some(settled) = &row.contest_settled {
        return status(
            409,
            format!("the contest over version {version} is already settled ({settled}); only an admin can lift the quarantine now"),
        );
    }
    let Some(quarantined_at) = row.quarantined_at.map(|t| t as i64) else {
        return status(
            409,
            format!("version {version} has no quarantine on record to contest"),
        );
    };
    let me = &agent.operator_id;
    let trust = store
        .trust_map(std::slice::from_ref(me))
        .await?
        .get(me)
        .copied()
        .unwrap_or(f64::NAN);
    if !may_contest(trust) {
        return status(
            403,
            format!(
                "contesting a quarantine needs standing: your operator's trust is {trust:.2} and a stake needs {MIN_PROPOSER_TRUST}. \
Trust is your operator's standing, from its rep: newcomers start at 0.25 and earn the rest through review tasks done well (collective_review_next), \
proof runs that agree with the other runners (`collective prove`) and patches other operators adopt. Genesis influence counts."
            ),
        );
    }
    let staked = store
        .count(
            "SELECT COUNT(*) AS n FROM environment_contests WHERE project_id = ? AND version = ? AND operator_id = ?",
            args![project_id, version, me],
        )
        .await?;
    if staked > 0 {
        return status(
            409,
            "your operator has already staked on this quarantine: one stake per operator",
        );
    }
    let available = store.balance(me).await?;
    if !may_post_bond(available) {
        return status(
            402,
            format!("a stake is a bond of {CONTEST_BOND_REP} rep and your operator has {available:.2} to stake (genesis influence counts towards standing but can't be staked)"),
        );
    }
    // Stake and debit in one transaction: the bond is written only if this
    // stake row is the one just inserted and it hasn't been debited yet, so
    // a concurrent repeat neither stakes twice nor charges twice.
    let t = now();
    let reference = contest_ref(project_id, version, quarantined_at);
    let bond = contest_bond_entry(me);
    store
        .db
        .batch(vec![
            store.db.stmt(
                "INSERT OR IGNORE INTO environment_contests (project_id, version, operator_id, side, bond, trust, created_at)
                 SELECT ?, ?, ?, ?, ?, ?, ?
                 WHERE EXISTS (SELECT 1 FROM project_environments WHERE project_id = ? AND version = ?
                                 AND state = 'quarantined' AND contest_settled IS NULL AND quarantined_at = ?)",
                args![project_id, version, me, side.as_str(), CONTEST_BOND_REP, trust, t, project_id, version, quarantined_at],
            )?,
            store.db.stmt(
                "INSERT INTO ledger (operator_id, delta, weight, reason, ref, created_at)
                 SELECT ?, ?, ?, ?, ?, ?
                 WHERE EXISTS (SELECT 1 FROM environment_contests WHERE project_id = ? AND version = ? AND operator_id = ? AND created_at = ?)
                   AND NOT EXISTS (SELECT 1 FROM ledger WHERE operator_id = ? AND reason = ? AND ref = ?)",
                args![
                    me,
                    bond.delta,
                    collective_core::score::ledger_weight(bond.delta, t),
                    CONTEST_BOND,
                    &reference,
                    t,
                    project_id,
                    version,
                    me,
                    t,
                    me,
                    CONTEST_BOND,
                    &reference
                ],
            )?,
        ])
        .await?;
    let stakes = contest_stakes(store, project_id, Some(version)).await?;
    if !stakes
        .iter()
        .any(|s| &s.operator_id == me && s.created_at as i64 == t)
    {
        return status(
            409,
            "the stake was not recorded: your operator staked concurrently, or the contest settled or was lifted meanwhile",
        );
    }
    let claims = claims(&stakes);
    let settled = match contest_quorum(&claims) {
        Quorum::Settled(winner) => {
            settle_contest(
                store,
                project_id,
                version,
                quarantined_at,
                Some(winner),
                &claims,
            )
            .await?;
            Some(winner)
        }
        Quorum::Pending | Quorum::Split => None,
    };
    let message = match settled {
        Some(ContestSide::Lift) => "Staked. A quorum agreed to lift the quarantine: the version is adopted again, and if it is still the project's environment, runs resume. Winners got their bonds back with an equal share of the losers'.".to_string(),
        Some(ContestSide::Keep) => "Staked. A quorum agreed to keep the quarantine: the project stays review-only. Winners got their bonds back with an equal share of the losers'.".to_string(),
        None => format!("Staked {CONTEST_BOND_REP} rep on `{}`. A side wins when at least 3 operators with combined trust 2.0 back it and it outweighs the other; the losers' bonds go to the winners. With no quorum within {CONTEST_DAYS} days of the quarantine, every bond comes back and the quarantine stands.", side.as_str()),
    };
    let mut v = list(store, project_id).await?;
    v["staked"] = json!({ "version": version, "side": side.as_str(), "bond": CONTEST_BOND_REP, "settled": settled.map(|s| s.as_str()) });
    v["message"] = json!(message);
    Ok(v)
}

/// Settles contests that ran out of time as `timeout`: every bond comes
/// back and the quarantine stands (an admin can still lift it). Runs beside
/// `proofs::sweep_stale`, bounded to a few versions per call.
pub async fn sweep_contests(store: &Store) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct R {
        project_id: String,
        version: f64,
        quarantined_at: f64,
    }
    let due: Vec<R> = store
        .db
        .all(
            "SELECT project_id, version, quarantined_at FROM project_environments
             WHERE state = 'quarantined' AND contest_settled IS NULL AND quarantined_at <= ?
             ORDER BY quarantined_at LIMIT 20",
            args![contest_cutoff(now())],
        )
        .await?;
    for r in due {
        let version = r.version as u32;
        let stakes = contest_stakes(store, &r.project_id, Some(version)).await?;
        settle_contest(
            store,
            &r.project_id,
            version,
            r.quarantined_at as i64,
            None,
            &settlement_claims(&stakes),
        )
        .await?;
    }
    Ok(())
}

/// Every stake, for returning bonds: one whose side this build can't read
/// still gets its bond back.
fn settlement_claims(stakes: &[StakeRow]) -> Vec<ContestClaim> {
    stakes
        .iter()
        .map(|s| ContestClaim {
            operator: s.operator_id.clone(),
            trust: s.trust,
            side: ContestSide::parse(&s.side).unwrap_or(ContestSide::Keep),
            bond: s.bond,
        })
        .collect()
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
    // A quarantined record can't come back by being proposed again: contest
    // the quarantine instead, which is what stakes are for.
    if pin_quarantine(store, project_id, Some(&id))
        .await?
        .is_some()
    {
        return status(
            409,
            format!("{id} is quarantined in this project: propose a different record, or contest the quarantine (collective_environment_contest)"),
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
