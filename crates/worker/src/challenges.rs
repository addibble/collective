//! Challenges over D1: open (bonded flag, proof-of-work report, or review
//! quorum), stake, resolve, settle. Rules live in collective_core::challenge.

use collective_core::challenge::{
    settle, Challenge, Exposure, Intent, OpenChallenge, Phase, Resolution, ResolvedVia,
    SettlementContext, Severity, Verdict,
};
use collective_core::review::{Finding, Location, Truth};
use collective_core::work::{verify_work, REPORT_BITS};
use collective_core::LedgerEntry;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiResult};
use crate::review::{
    apply_grades, clean_reviewers, load_shard, submitted_reviews, ShardRow, ATTESTATION_STAKE,
};
use crate::store::{new_id, now, Agent, Store, POOL};

#[derive(Deserialize)]
struct Row {
    state: String,
    shard_id: Option<String>,
    location: Option<String>,
    settled: f64,
}

async fn load(store: &Store, id: &str) -> ApiResult<(Challenge, Row)> {
    let Some(row) = store
        .db
        .first::<Row>(
            "SELECT state, shard_id, location, settled FROM challenges WHERE id = ?",
            args![id],
        )
        .await?
    else {
        return status(404, "unknown challenge");
    };
    Ok((serde_json::from_str(&row.state)?, row))
}

fn phase_str(p: Phase) -> &'static str {
    match p {
        Phase::Staking => "staking",
        Phase::Jury => "jury",
        Phase::Resolved => "resolved",
    }
}

async fn save(store: &Store, c: &Challenge) -> ApiResult<()> {
    store
        .db
        .run(
            "UPDATE challenges SET state = ?, phase = ? WHERE id = ?",
            args![serde_json::to_string(c)?, phase_str(c.phase), &c.id],
        )
        .await?;
    sync_quarantine(store, &c.patch_id).await
}

/// Whether any challenge on a patch is unresolved, and whether any
/// resolved `problem`.
pub async fn patch_verdicts(store: &Store, patch_id: &str) -> ApiResult<(bool, bool)> {
    #[derive(Deserialize)]
    struct S {
        state: String,
    }
    let rows: Vec<S> = store
        .db
        .all(
            "SELECT state FROM challenges WHERE patch_id = ?",
            args![patch_id],
        )
        .await?;
    let (mut open, mut problem) = (false, false);
    for r in rows {
        // A challenge whose state can't be read holds a loan up rather
        // than letting it go either way.
        let Ok(c) = serde_json::from_str::<Challenge>(&r.state) else {
            open = true;
            continue;
        };
        match &c.resolution {
            Some(res) if c.phase == Phase::Resolved => {
                problem |= res.verdict == Verdict::Problem;
            }
            _ => open = true,
        }
    }
    Ok((open, problem))
}

/// A patch is quarantined while any of its challenges says "problem".
async fn sync_quarantine(store: &Store, patch_id: &str) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct S {
        state: String,
    }
    let rows: Vec<S> = store
        .db
        .all(
            "SELECT state FROM challenges WHERE patch_id = ?",
            args![patch_id],
        )
        .await?;
    let challenges: Vec<Challenge> = rows
        .iter()
        .filter_map(|r| serde_json::from_str(&r.state).ok())
        .collect();
    let rejected = challenges.iter().any(|c| {
        c.resolution
            .as_ref()
            .is_some_and(|r| r.verdict == Verdict::Problem)
    });
    let quarantined = challenges.iter().any(Challenge::quarantined);
    let status = if rejected {
        "rejected"
    } else if quarantined {
        "quarantined"
    } else {
        "open"
    };
    store
        .db
        .run("UPDATE patches SET status = ? WHERE id = ? AND status IN ('open','converged','quarantined','rejected')", args![status, patch_id])
        .await?;
    // A patch under challenge loses its branch until it is cleared.
    if let Some(p) = store.patch_row(patch_id).await? {
        crate::git::notify(&store.env, &p.project_id, crate::git::Job::Patch(p.id)).await;
    }
    Ok(())
}

async fn insert(
    store: &Store,
    c: &Challenge,
    shard_id: Option<&str>,
    location: Option<&Location>,
) -> ApiResult<()> {
    store
        .db
        .run(
            "INSERT INTO challenges (id, patch_id, shard_id, location, state, phase, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            args![&c.id, &c.patch_id, shard_id, location.map(serde_json::to_string).transpose()?, serde_json::to_string(c)?, phase_str(c.phase), now()],
        )
        .await?;
    sync_quarantine(store, &c.patch_id).await
}

async fn debit(
    store: &Store,
    operator_id: &str,
    amount: f64,
    reason: &str,
    reference: &str,
) -> ApiResult<()> {
    store
        .credit(&[LedgerEntry::new(operator_id, -amount, reason)], reference)
        .await
}

pub fn view(c: &Challenge) -> Value {
    json!({
        "id": c.id,
        "patch_id": c.patch_id,
        "flagged_by": c.flagged_by,
        "severity": c.severity,
        "phase": c.phase,
        "tentative": c.tentative,
        "quarantined": c.quarantined(),
        "round": c.round,
        "round_ends_at": c.round_ends_at,
        "bond_to_flip": (c.phase == Phase::Staking).then(|| c.bond_to_flip()),
        "pending": c.pending().max(0.0),
        "resolution": c.resolution,
    })
}

#[derive(Debug, Deserialize)]
pub struct Flag {
    pub severity: Severity,
    /// sha256(evidence ‖ salt); reveal the evidence to jurors later.
    pub evidence_commitment: String,
    #[serde(default)]
    pub bond: f64,
    /// Hashcash nonce over "<operator>:<patch>:<commitment>", required when bond is 0.
    pub work_nonce: Option<String>,
}

pub async fn flag(store: &Store, agent: &Agent, patch_id: &str, f: Flag) -> ApiResult<Value> {
    let patch = store.require_patch(patch_id).await?;
    if patch.author_op == agent.operator_id {
        return bad("you cannot challenge your own operator's patch; withdraw it instead");
    }
    if f.evidence_commitment.len() != 64
        || !f.evidence_commitment.chars().all(|c| c.is_ascii_hexdigit())
    {
        return bad("evidence_commitment must be a sha256 hex digest");
    }
    let id = new_id();
    if f.bond > 0.0 {
        let balance = store.balance(&agent.operator_id).await?;
        if balance < f.bond {
            return status(402, format!("bond {} exceeds your rep ({balance}); file a bare report with proof of work instead", f.bond));
        }
    } else {
        let resource = format!(
            "{}:{}:{}",
            agent.operator_id, patch_id, f.evidence_commitment
        );
        if !f
            .work_nonce
            .as_deref()
            .is_some_and(|n| verify_work(&resource, n, REPORT_BITS))
        {
            return status(402, format!("a report without a bond needs proof of work: a nonce such that sha256(\"{resource}:<nonce>\") has {REPORT_BITS} leading zero bits"));
        }
    }
    let c = Challenge::open(OpenChallenge {
        id: &id,
        patch_id,
        flagged_by: &agent.operator_id,
        severity: f.severity,
        evidence_commitment: &f.evidence_commitment,
        bond: f.bond,
        now: now(),
    })?;
    if f.bond > 0.0 {
        debit(
            store,
            &agent.operator_id,
            f.bond,
            "challenge bond",
            &format!("challenge:{id}"),
        )
        .await?;
    }
    insert(store, &c, None, None).await?;
    Ok(view(&c))
}

/// Review quorum found the same problem: the platform pool posts the bond
/// and the first reporter is credited as finder.
pub async fn open_from_review(store: &Store, shard: &ShardRow, finding: &Finding) -> ApiResult<()> {
    let id = new_id();
    let commitment = collective_core::hash::sha256_hex(serde_json::to_string(finding)?);
    let mut c = Challenge::open(OpenChallenge {
        id: &id,
        patch_id: &shard.patch_id,
        flagged_by: &finding.reporters[0],
        severity: finding.severity,
        evidence_commitment: &commitment,
        bond: 0.0,
        now: now(),
    })?;
    let bond = c.bond_to_flip();
    c.contribute(POOL, Verdict::Problem, bond, now())?;
    debit(
        store,
        POOL,
        bond,
        "bond for review-quorum challenge",
        &format!("challenge:{id}"),
    )
    .await?;
    insert(store, &c, Some(&shard.id), Some(&finding.location)).await
}

/// Most runners found the project's own test suite passes on base and
/// fails with the patch: the platform pool posts the bond, the first such
/// runner is the finder. There are no lines to grade reviewers on, so a
/// "problem" verdict treats the patch's clean reviewers as attesters, as
/// for an outside flag. A regression proof naming the failing tests
/// settles it (PROOFS.md).
pub async fn open_from_runs(store: &Store, shard: &ShardRow, finder: &str) -> ApiResult<()> {
    let id = new_id();
    let commitment = collective_core::hash::sha256_hex(format!("runs:{}", shard.id));
    let mut c = Challenge::open(OpenChallenge {
        id: &id,
        patch_id: &shard.patch_id,
        flagged_by: finder,
        severity: Severity::Medium,
        evidence_commitment: &commitment,
        bond: 0.0,
        now: now(),
    })?;
    let bond = c.bond_to_flip();
    c.contribute(POOL, Verdict::Problem, bond, now())?;
    debit(
        store,
        POOL,
        bond,
        "bond for run-majority challenge",
        &format!("challenge:{id}"),
    )
    .await?;
    insert(store, &c, None, None).await
}

#[derive(Debug, Deserialize)]
pub struct Stake {
    pub side: Verdict,
    pub amount: f64,
}

pub async fn stake(store: &Store, agent: &Agent, id: &str, s: Stake) -> ApiResult<Value> {
    let (mut c, row) = load(store, id).await?;
    if advance_and_settle(store, &mut c, &row).await? {
        return status(409, "challenge already resolved");
    }
    let balance = store.balance(&agent.operator_id).await?;
    let amount = s.amount.min(balance);
    if amount <= 0.0 {
        return status(402, "no rep to stake");
    }
    let accepted = c.contribute(&agent.operator_id, s.side, amount, now())?;
    debit(
        store,
        &agent.operator_id,
        accepted,
        "challenge stake",
        &format!("challenge:{id}"),
    )
    .await?;
    save(store, &c).await?;
    Ok(json!({ "accepted": accepted, "challenge": view(&c) }))
}

pub async fn get(store: &Store, id: &str) -> ApiResult<Value> {
    let (mut c, row) = load(store, id).await?;
    advance_and_settle(store, &mut c, &row).await?;
    let mut v = view(&c);
    v["proofs"] = json!(crate::proofs::list_for_challenge(store, id).await?);
    Ok(v)
}

pub async fn load_state(store: &Store, id: &str) -> ApiResult<Challenge> {
    Ok(load(store, id).await?.0)
}

#[derive(Debug, Deserialize)]
pub struct Resolve {
    pub verdict: Verdict,
    pub via: ResolvedVia,
    pub severity: Severity,
    pub intent: Intent,
}

/// Resolution by a verified proof, a jury or the council (admin-only for now).
pub async fn resolve(store: &Store, id: &str, r: Resolve) -> ApiResult<Value> {
    let (mut c, row) = load(store, id).await?;
    if advance_and_settle(store, &mut c, &row).await? {
        return status(409, "challenge already resolved");
    }
    c.resolve(Resolution {
        verdict: r.verdict,
        via: r.via,
        severity: r.severity,
        intent: r.intent,
        at: now(),
    })?;
    save(store, &c).await?;
    let settlement = finalize(store, &c, &row).await?;
    Ok(json!({ "challenge": view(&c), "settlement": settlement }))
}

/// Applies a timeout if due; settles once resolved. Returns whether resolved.
async fn advance_and_settle(store: &Store, c: &mut Challenge, row: &Row) -> ApiResult<bool> {
    let before = c.phase;
    c.advance(now());
    if c.phase != before {
        save(store, c).await?;
    }
    if c.phase == Phase::Resolved {
        if row.settled == 0.0 {
            finalize(store, c, row).await?;
        }
        return Ok(true);
    }
    Ok(false)
}

async fn finalize(store: &Store, c: &Challenge, row: &Row) -> ApiResult<Value> {
    // Claim the settlement first so concurrent requests cannot pay twice.
    if store
        .db
        .changes(
            "UPDATE challenges SET settled = 1 WHERE id = ? AND settled = 0",
            args![&c.id],
        )
        .await?
        == 0
    {
        return Ok(json!({ "already_settled": true }));
    }
    let patch = store.require_patch_any(&c.patch_id).await?;
    let res = c.resolution.as_ref().expect("resolved");
    let location: Option<Location> = row
        .location
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;

    // Reviewers are graded on the shard the finding came from; for outside
    // challenges, every reviewer who called the patch clean is an attester.
    let mut attestations = Vec::new();
    match (&row.shard_id, &location) {
        (Some(shard_id), Some(loc)) => {
            let shard = load_shard(store, shard_id).await?;
            let reviews = submitted_reviews(store, &shard.id).await?;
            let truth = if res.verdict == Verdict::Problem {
                Truth::Resolved {
                    confirmed: vec![loc.clone()],
                    rejected: vec![],
                }
            } else {
                Truth::Resolved {
                    confirmed: vec![],
                    rejected: vec![loc.clone()],
                }
            };
            apply_grades(store, &shard.id, &truth, &reviews).await?;
            store
                .db
                .run(
                    "UPDATE shards SET state = 'graded' WHERE id = ?",
                    args![&shard.id],
                )
                .await?;
        }
        _ if res.verdict == Verdict::Problem => {
            for op in clean_reviewers(store, &c.patch_id).await? {
                attestations.push(Exposure {
                    operator_id: op,
                    amount: ATTESTATION_STAKE,
                });
            }
        }
        _ => {}
    }

    let ctx = SettlementContext {
        author_operator: patch.author_op.clone(),
        author_balance: store.balance(&patch.author_op).await?,
        adopters: store
            .positive_adopters(&c.patch_id)
            .await?
            .into_iter()
            .map(|(operator_id, amount)| Exposure {
                operator_id,
                amount,
            })
            .collect(),
        attestations,
        pool: store.balance(POOL).await?,
        found_by_assigned_review: row.shard_id.is_some(),
    };
    let s = settle(c, &ctx)?;
    let reference = format!("challenge:{}", c.id);
    store.credit(&s.entries, &reference).await?;
    store
        .credit(
            &[LedgerEntry::new(POOL, s.pool_delta, "challenge settlement")],
            &reference,
        )
        .await?;

    for op in &s.bans {
        ban(
            store,
            op,
            &format!("authored a malicious patch ({})", c.patch_id),
        )
        .await?;
    }
    store.rescore(&c.patch_id).await?;
    // A vouch for the patch's task is lost on a `problem` verdict, and may
    // come back now that this challenge no longer holds the task up.
    crate::vouch::settle(store, &collective_core::review::task_id(&c.patch_id)).await?;
    Ok(json!({ "bounty": s.bounty, "bans": s.bans, "entries": s.entries }))
}

/// Bans an operator: revokes every agent key, quarantines its other patches
/// and sends their shards back for review, pending ones included: a proven
/// backdoor author's other work is worth the review it was waiting for.
pub async fn ban(store: &Store, operator_id: &str, reason: &str) -> ApiResult<()> {
    let t = now();
    #[derive(Deserialize)]
    struct P {
        id: String,
        project_id: String,
    }
    let quarantined: Vec<P> = store
        .db
        .all(
            "SELECT id, project_id FROM patches WHERE author_op = ? AND status IN ('open','converged')",
            args![operator_id],
        )
        .await?;
    store
        .db
        .batch(vec![
            store.db.stmt("UPDATE operators SET banned_at = ?, ban_reason = ? WHERE id = ? AND banned_at IS NULL", args![t, reason, operator_id])?,
            store.db.stmt("UPDATE agents SET revoked_at = ? WHERE operator_id = ? AND revoked_at IS NULL", args![t, operator_id])?,
            store.db.stmt("UPDATE patches SET status = 'quarantined' WHERE author_op = ? AND status IN ('open','converged')", args![operator_id])?,
            store.db.stmt(
                "UPDATE shards SET state = 'open', wanted = wanted + ? WHERE author_op = ? AND canary = 0 AND state IN ('clean','graded','pending')",
                args![collective_core::review::PARAMS.redundancy, operator_id],
            )?,
        ])
        .await?;
    for p in quarantined {
        crate::git::notify(&store.env, &p.project_id, crate::git::Job::Patch(p.id)).await;
    }
    Ok(())
}

pub async fn list_for_patch(store: &Store, patch_id: &str) -> ApiResult<Value> {
    store.require_patch(patch_id).await?;
    #[derive(Deserialize)]
    struct S {
        state: String,
    }
    let rows: Vec<S> = store
        .db
        .all(
            "SELECT state FROM challenges WHERE patch_id = ? ORDER BY created_at",
            args![patch_id],
        )
        .await?;
    let list: Vec<Value> = rows
        .iter()
        .filter_map(|r| serde_json::from_str::<Challenge>(&r.state).ok())
        .map(|c| view(&c))
        .collect();
    Ok(json!({ "challenges": list }))
}
