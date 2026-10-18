//! System of record over D1 (metadata) and R2 (diff bodies), plus the
//! development search backend. All protocol rules live in collective-core;
//! this module only loads, stores and indexes.

use std::collections::{HashMap, HashSet};

use collective_core::converge::{converge, Candidate, ConvergeOptions};
use collective_core::diff::{
    format_diff, hunk_fingerprints, parse_diff, patch_id, stats, FileDiff,
};
use collective_core::hash::sha256_hex;
use collective_core::score::{
    ledger_balance, ledger_weight, reputation, score_patch, trust, Adoption, Outcome, PARENT_SHARE,
};
use collective_core::sketch::{decode_signature, estimate_jaccard, sketch};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::{Bucket, Env};

use crate::args;
use crate::db::{placeholders, Db, MAX_PARAMS};
use crate::error::{bad, status, ApiError, ApiResult};
use crate::search::{D1Search, SearchIndex};

/// Intents at least this similar to a new one are reused instead of duplicated.
pub const INTENT_REUSE_THRESHOLD: f64 = 0.75;
/// Estimated Jaccard similarity above which a patch is reported as a near-duplicate.
pub const NEAR_DUPLICATE_THRESHOLD: f64 = 0.5;
const MAX_DIFF_BYTES: usize = 1_000_000;
pub const POOL: &str = collective_core::challenge::PLATFORM_POOL;

/// Platform accounts (the bounty pool, the canary author) hold funds, not
/// standing, so their balances do not decay.
pub fn is_platform(operator_id: &str) -> bool {
    operator_id == "platform" || operator_id.starts_with("platform:")
}

pub fn now() -> i64 {
    worker::Date::now().as_millis() as i64
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("crypto.getRandomValues is available");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_id() -> String {
    random_hex(10)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub id: String,
    pub operator_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub id: String,
    pub upstream_url: String,
}

/// What anyone may see of an operator's standing (the ledger behind it stays private).
#[derive(Debug, Clone, Copy)]
pub struct Standing {
    pub rep: f64,
    pub trust: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PatchRow {
    pub id: String,
    pub project_id: String,
    pub intent_id: String,
    pub base_commit: String,
    pub title: String,
    pub author_agent: String,
    pub author_op: String,
    pub parent_id: Option<String>,
    pub files: String,
    // D1 returns JavaScript numbers; keep them as f64 and convert on use.
    pub lines_added: f64,
    pub lines_removed: f64,
    pub score: f64,
    pub adopters: f64,
    pub status: String,
    pub created_at: f64,
}

impl PatchRow {
    pub fn public(&self) -> Value {
        json!({
            "id": self.id,
            "project_id": self.project_id,
            "intent_id": self.intent_id,
            "base_commit": self.base_commit,
            "title": self.title,
            "author_agent": self.author_agent,
            "author_operator": self.author_op,
            "parent_id": self.parent_id,
            "files": serde_json::from_str::<Value>(&self.files).unwrap_or(Value::Null),
            "lines_added": self.lines_added as i64,
            "lines_removed": self.lines_removed as i64,
            "score": self.score,
            "adopters": self.adopters as i64,
            "status": self.status,
            "warning": (self.status == "quarantined").then_some("Under challenge: a bonded report says this patch has a serious problem. Do not adopt until resolved."),
            "created_at": self.created_at as i64,
        })
    }
}

/// Statuses hidden from search, leaderboards and convergence.
pub const HIDDEN: &str = "('withdrawn','canary')";

pub fn normalize_upstream(url: &str) -> ApiResult<String> {
    let mut u = url.trim().trim_start_matches("git+").to_string();
    if let Some(i) = u.find("://") {
        u = u[i + 3..].to_string();
    } else if let Some(rest) = u.strip_prefix("git@") {
        u = rest.replacen(':', "/", 1);
    }
    let u = u
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    let (host, rest) = u.split_once('/').unwrap_or((u, ""));
    if host.is_empty() || rest.is_empty() || !host.contains('.') {
        return bad(format!("not a repository URL: {url}"));
    }
    Ok(format!("{}/{rest}", host.to_lowercase()))
}

pub fn parse(diff: &str) -> ApiResult<Vec<FileDiff>> {
    if diff.len() > MAX_DIFF_BYTES {
        return status(413, "diff too large");
    }
    parse_diff(diff).map_err(|e| ApiError::new(400, format!("invalid diff: {e}")))
}

pub struct Store {
    pub db: Db,
    pub blobs: Bucket,
    pub env: Env,
}

#[derive(Deserialize)]
struct Count {
    n: f64,
}

impl Store {
    pub fn new(env: &Env) -> ApiResult<Store> {
        Ok(Store {
            db: Db(env.d1("DB")?),
            blobs: env.bucket("BLOBS")?,
            env: env.clone(),
        })
    }

    pub async fn count(&self, sql: &str, args: Vec<crate::db::Arg>) -> ApiResult<u32> {
        Ok(self
            .db
            .first::<Count>(sql, args)
            .await?
            .map_or(0, |c| c.n as u32))
    }

    // -- identity -------------------------------------------------------------

    pub async fn register(
        &self,
        operator: &str,
        agent_name: &str,
        caller: Option<&Agent>,
        human: Option<&crate::identity::Human>,
        ip: Option<&str>,
    ) -> ApiResult<Value> {
        let handle = operator.trim().to_lowercase();
        let valid = handle.len() >= 2
            && handle.len() <= 63
            && handle
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && handle
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
        if !valid || handle.starts_with("platform") {
            return bad("invalid operator handle");
        }
        if agent_name.trim().is_empty() {
            return bad("agent name required");
        }
        #[derive(Deserialize)]
        struct Op {
            id: String,
            banned_at: Option<f64>,
        }
        let op = self
            .db
            .first::<Op>(
                "SELECT id, banned_at FROM operators WHERE handle = ?",
                args![&handle],
            )
            .await?;
        let created = op.is_none();
        let op_id = match op {
            Some(op) if op.banned_at.is_some() => return status(403, "operator is banned"),
            Some(op) => {
                // Adding an agent to an existing operator requires its human key
                // (or, for older clients, one of its agents' keys).
                let as_human = human.map(|h| h.operator_id.as_str()) == Some(op.id.as_str());
                let as_agent = caller.map(|c| c.operator_id.as_str()) == Some(op.id.as_str());
                if !as_human && !as_agent {
                    return status(403, "operator exists; authenticate with its human key");
                }
                op.id
            }
            None => {
                let ip_hash = self.check_registration_rate(ip).await?;
                let id = new_id();
                self.db
                    .run(
                        "INSERT INTO operators (id, handle, created_at, created_ip_hash) VALUES (?, ?, ?, ?)",
                        args![&id, &handle, now(), ip_hash],
                    )
                    .await?;
                id
            }
        };
        let agent = crate::identity::mint_agent_key(self, &op_id, agent_name).await?;
        let mut out = json!({
            "agent_id": agent["agent_id"],
            "operator_id": op_id,
            "operator": handle,
            "token": agent["token"],
        });
        // A new operator also gets its first human key, to manage its agents.
        if created {
            let hk =
                crate::identity::mint_human_key(self, &op_id, "created at registration").await?;
            out["human_key"] = hk["human_key"].clone();
        }
        Ok(out)
    }

    /// Anonymous registration is limited per IP (stored only as a salted
    /// hash): fresh operators each get a free review-duty allowance, so
    /// unlimited ones would undercut both duty and the anti-sybil weighting.
    async fn check_registration_rate(&self, ip: Option<&str>) -> ApiResult<Option<String>> {
        let Some(ip) = ip else { return Ok(None) };
        let setting = |k: &str| {
            self.env
                .secret(k)
                .map(|s| s.to_string())
                .or_else(|_| self.env.var(k).map(|v| v.to_string()))
                .ok()
        };
        let salt = setting("IP_HASH_SALT").unwrap_or_else(|| "collective".into());
        let limit: u32 = setting("REGISTER_LIMIT_PER_HOUR")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let hash = sha256_hex(format!("{salt}:{ip}"))[..32].to_string();
        let recent = self
            .count(
                "SELECT COUNT(*) AS n FROM operators WHERE created_ip_hash = ? AND created_at > ?",
                args![&hash, now() - 3_600_000],
            )
            .await?;
        if recent >= limit {
            return status(429, "too many new operators from this network; try again later, or sign in with GitHub (collective login)");
        }
        Ok(Some(hash))
    }

    /// Resolves a bearer token. Revoked keys and banned operators fail.
    pub async fn authenticate(&self, token: Option<&str>) -> ApiResult<Option<Agent>> {
        let Some(token) = token else { return Ok(None) };
        self.db
            .first::<Agent>(
                "SELECT a.id, a.operator_id, a.name FROM agents a JOIN operators o ON o.id = a.operator_id
                 WHERE a.token_hash = ? AND a.revoked_at IS NULL AND o.banned_at IS NULL",
                args![sha256_hex(token)],
            )
            .await
    }

    /// An operator's rep: its ledger, decayed (see `score::ledger_balance`).
    /// Platform accounts such as the bounty pool do not decay.
    pub async fn balance(&self, operator_id: &str) -> ApiResult<f64> {
        #[derive(Deserialize)]
        struct B {
            d: Option<f64>,
            w: Option<f64>,
        }
        let b = self
            .db
            .first::<B>(
                "SELECT SUM(delta) AS d, SUM(weight) AS w FROM ledger WHERE operator_id = ?",
                args![operator_id],
            )
            .await?;
        Ok(match b {
            Some(b) if is_platform(operator_id) => b.d.unwrap_or(0.0),
            Some(b) => ledger_balance(b.w.unwrap_or(0.0), now()),
            None => 0.0,
        })
    }

    pub async fn credit(
        &self,
        entries: &[collective_core::LedgerEntry],
        reference: &str,
    ) -> ApiResult<()> {
        let t = now();
        let stmts = entries
            .iter()
            .filter(|e| e.delta != 0.0)
            .map(|e| {
                self.db.stmt(
                    "INSERT INTO ledger (operator_id, delta, weight, reason, ref, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                    args![&e.operator_id, e.delta, ledger_weight(e.delta, t), &e.reason, reference, t],
                )
            })
            .collect::<ApiResult<Vec<_>>>()?;
        self.db.batch(stmts).await
    }

    // -- projects -------------------------------------------------------------

    pub async fn project(&self, upstream: &str, create: bool) -> ApiResult<Option<Project>> {
        let url = normalize_upstream(upstream)?;
        if let Some(p) = self
            .db
            .first::<Project>(
                "SELECT id, upstream_url FROM projects WHERE upstream_url = ?",
                args![&url],
            )
            .await?
        {
            return Ok(Some(p));
        }
        if !create {
            return Ok(None);
        }
        let id = new_id();
        self.db
            .run(
                "INSERT INTO projects (id, upstream_url, created_at) VALUES (?, ?, ?)",
                args![&id, &url, now()],
            )
            .await?;
        Ok(Some(Project {
            id,
            upstream_url: url,
        }))
    }

    pub async fn require_project(&self, id_or_url: &str, create: bool) -> ApiResult<Project> {
        if let Some(p) = self
            .db
            .first::<Project>(
                "SELECT id, upstream_url FROM projects WHERE id = ?",
                args![id_or_url],
            )
            .await?
        {
            return Ok(p);
        }
        self.project(id_or_url, create)
            .await?
            .ok_or_else(|| ApiError::new(404, "unknown project"))
    }

    // -- search (development backend; see docs/SEARCH.md) ---------------------

    pub fn search(&self) -> D1Search<'_> {
        D1Search(&self.db)
    }

    /// Search hits joined with their intent text and canonical pointer.
    async fn intent_hits(
        &self,
        project_id: &str,
        text: &str,
        k: usize,
    ) -> ApiResult<Vec<(String, String, Option<String>, f64)>> {
        let hits = self.search().search_intents(project_id, text, k).await?;
        if hits.is_empty() {
            return Ok(vec![]);
        }
        #[derive(Deserialize)]
        struct I {
            id: String,
            text: String,
            canonical_id: Option<String>,
        }
        let rows: Vec<I> = self
            .db
            .all(
                &format!(
                    "SELECT id, text, canonical_id FROM intents WHERE id IN ({})",
                    placeholders(hits.len())
                ),
                hits.iter().map(|h| (&h.id).into()).collect(),
            )
            .await?;
        let by_id: HashMap<String, I> = rows.into_iter().map(|r| (r.id.clone(), r)).collect();
        Ok(hits
            .into_iter()
            .filter_map(|h| {
                let r = by_id.get(&h.id)?;
                Some((
                    r.id.clone(),
                    r.text.clone(),
                    r.canonical_id.clone(),
                    h.relevance,
                ))
            })
            .collect())
    }

    /// Intents similar to `query`, best first, each with its top patches.
    pub async fn search_intents(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> ApiResult<Vec<Value>> {
        let mut best: Vec<(String, String, f64)> = Vec::new();
        for (id, text, canonical, sim) in self.intent_hits(project_id, query, limit * 4).await? {
            let id = canonical.unwrap_or(id);
            let sim = (sim * 1000.0).round() / 1000.0;
            match best.iter_mut().find(|b| b.0 == id) {
                Some(b) if b.2 < sim => *b = (id, text, sim),
                Some(_) => {}
                None => best.push((id, text, sim)),
            }
        }
        best.sort_by(|a, b| b.2.total_cmp(&a.2));
        best.truncate(limit);
        let mut out = Vec::new();
        for (id, text, sim) in best {
            let patches = self.patches_for_intent(&id, 3).await?;
            out.push(json!({ "intent": { "id": id, "text": text, "similarity": sim }, "patches": patches }));
        }
        Ok(out)
    }

    pub async fn patches_for_intent(&self, intent_id: &str, limit: usize) -> ApiResult<Vec<Value>> {
        let rows: Vec<PatchRow> = self
            .db
            .all(
                &format!(
                    "SELECT p.* FROM patches p JOIN intents i ON i.id = p.intent_id
                     WHERE (i.id = ? OR i.canonical_id = ?) AND p.status NOT IN {HIDDEN}
                     ORDER BY p.score DESC, p.adopters DESC, p.created_at ASC LIMIT ?"
                ),
                args![intent_id, intent_id, limit],
            )
            .await?;
        Ok(rows.iter().map(PatchRow::public).collect())
    }

    /// Existing patches making the same (`overlap`, exact hunks) or nearly
    /// the same (`similarity`, MinHash estimate) edits.
    pub async fn similar_patches(
        &self,
        project_id: &str,
        files: &[FileDiff],
    ) -> ApiResult<Vec<Value>> {
        let fps: Vec<String> = hunk_fingerprints(files)
            .into_iter()
            .map(|f| f.fingerprint)
            .collect();
        let n_fps = fps.len().max(1) as f64;
        let mut hits: HashMap<String, (f64, Option<f64>)> = HashMap::new();
        for h in self.search().patches_by_hunks(project_id, &fps, 20).await? {
            hits.insert(
                h.patch_id,
                (h.shared as f64 / n_fps.max(h.total as f64), None),
            );
        }
        if let Some(sk) = sketch(files) {
            let mine = decode_signature(&sk.signature).expect("own signature decodes");
            for c in self
                .search()
                .patches_by_bands(project_id, &sk.bands, 50)
                .await?
            {
                let Some(theirs) = decode_signature(&c.signature) else {
                    continue;
                };
                let sim = estimate_jaccard(&mine, &theirs);
                match hits.get_mut(&c.patch_id) {
                    Some(h) => h.1 = Some(sim),
                    None if sim >= NEAR_DUPLICATE_THRESHOLD => {
                        hits.insert(c.patch_id, (0.0, Some(sim)));
                    }
                    None => {}
                }
            }
        }
        let mut ranked: Vec<_> = hits.into_iter().collect();
        ranked.sort_by(|a, b| {
            (b.1 .0.max(b.1 .1.unwrap_or(0.0))).total_cmp(&a.1 .0.max(a.1 .1.unwrap_or(0.0)))
        });
        ranked.truncate(10);
        let round = |x: f64| (x * 1000.0).round() / 1000.0;
        let mut out = Vec::new();
        for (id, (overlap, sim)) in ranked {
            let Some(p) = self.patch_row(&id).await? else {
                continue;
            };
            if p.status == "canary" || p.status == "withdrawn" {
                continue;
            }
            out.push(json!({ "patch": p.public(), "overlap": round(overlap), "similarity": sim.map(round) }));
        }
        Ok(out)
    }

    async fn create_intent(
        &self,
        project_id: &str,
        text: &str,
        agent: &Agent,
    ) -> ApiResult<String> {
        let id = new_id();
        self.db
            .run("INSERT INTO intents (id, project_id, text, created_by, created_at) VALUES (?, ?, ?, ?, ?)", args![&id, project_id, text, &agent.id, now()])
            .await?;
        self.search().index_intent(project_id, &id, text).await?;
        Ok(id)
    }

    // -- patches --------------------------------------------------------------

    pub async fn patch_row(&self, id: &str) -> ApiResult<Option<PatchRow>> {
        self.db
            .first::<PatchRow>("SELECT * FROM patches WHERE id = ?", args![id])
            .await
    }

    pub async fn require_patch(&self, id: &str) -> ApiResult<PatchRow> {
        match self.patch_row(id).await? {
            Some(p) if p.status != "canary" => Ok(p),
            _ => status(404, "unknown patch"),
        }
    }

    pub async fn diff_text(&self, id: &str) -> ApiResult<String> {
        match self
            .blobs
            .get(format!("patches/{id}.diff"))
            .execute()
            .await?
        {
            Some(obj) => Ok(obj
                .body()
                .ok_or_else(|| ApiError::new(500, "empty blob"))?
                .text()
                .await?),
            None => status(404, "unknown patch"),
        }
    }

    pub async fn store_patch(&self, p: NewPatch<'_>) -> ApiResult<PatchRow> {
        let s = stats(p.files);
        self.blobs
            .put(format!("patches/{}.diff", p.id), format_diff(p.files))
            .execute()
            .await?;
        self.db
            .run(
                "INSERT INTO patches (id, project_id, intent_id, base_commit, title, author_agent, author_op, parent_id, files, lines_added, lines_removed, status, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                args![
                    p.id,
                    p.project_id,
                    p.intent_id,
                    p.base_commit,
                    p.title,
                    &p.agent.id,
                    &p.agent.operator_id,
                    p.parent_id,
                    serde_json::to_string(&s.files)?,
                    s.added,
                    s.removed,
                    p.status,
                    now(),
                ],
            )
            .await?;
        if p.status != "canary" {
            self.search()
                .index_patch(
                    p.project_id,
                    p.id,
                    &hunk_fingerprints(p.files),
                    sketch(p.files).as_ref(),
                )
                .await?;
        }
        self.require_patch_any(p.id).await
    }

    pub async fn require_patch_any(&self, id: &str) -> ApiResult<PatchRow> {
        self.patch_row(id)
            .await?
            .ok_or_else(|| ApiError::new(404, "unknown patch"))
    }

    pub async fn submit_patch(&self, agent: &Agent, input: &SubmitPatch) -> ApiResult<Value> {
        let project = self.require_project(&input.project, true).await?;
        let base = input.base_commit.to_lowercase();
        if !(7..=64).contains(&base.len()) || !base.chars().all(|c| c.is_ascii_hexdigit()) {
            return bad("base_commit must be a commit SHA");
        }
        let files = parse(&input.diff)?;
        collective_core::review::check_task_size(&files).map_err(|e| ApiError::new(413, e))?;
        let id = patch_id(&base, &files);

        if let Some(existing) = self.patch_row(&id).await? {
            if existing.status != "canary" {
                let mut out = json!({ "patch": existing.public(), "created": false, "intent_reused": true, "similar": [] });
                if let Some(state) = crate::vouch::review_state(self, &id).await? {
                    crate::vouch::annotate(&mut out, &state);
                }
                return Ok(out);
            }
        }
        if let Some(parent) = &input.parent_id {
            match self.patch_row(parent).await? {
                Some(p) if p.project_id == project.id && p.status != "canary" => {}
                _ => return bad("parent_id must be a patch in the same project"),
            }
        }

        let (intent_id, intent_reused) = match &input.intent_id {
            Some(iid) => {
                #[derive(Deserialize)]
                struct I {
                    id: String,
                    project_id: String,
                    canonical_id: Option<String>,
                }
                let i = self
                    .db
                    .first::<I>(
                        "SELECT id, project_id, canonical_id FROM intents WHERE id = ?",
                        args![iid],
                    )
                    .await?;
                match i {
                    Some(i) if i.project_id == project.id => (i.canonical_id.unwrap_or(i.id), true),
                    _ => return bad("unknown intent_id for this project"),
                }
            }
            None => {
                let text = input.intent.as_deref().unwrap_or_default().trim();
                if text.len() < 8 {
                    return bad("describe the change in `intent` (or pass `intent_id`)");
                }
                let top = self
                    .intent_hits(&project.id, text, 1)
                    .await?
                    .into_iter()
                    .next();
                match top {
                    Some((id, _, canonical, sim)) if sim >= INTENT_REUSE_THRESHOLD => {
                        (canonical.unwrap_or(id), true)
                    }
                    _ => (self.create_intent(&project.id, text, agent).await?, false),
                }
            }
        };

        let similar = self.similar_patches(&project.id, &files).await?;
        // Pinned before anything is stored, so a project whose environment
        // can't be pinned refuses the submission whole. A quarantined one
        // is pinned too: the patch is accepted and its task is review-only.
        let environment = crate::environments::current(self, &project.id).await?;
        // A newcomer's task waits for standing or a vouch (REVIEWS.md,
        // Newcomers). Read before anything is stored, once per submission.
        let author_trust = self
            .trust_map(std::slice::from_ref(&agent.operator_id))
            .await?
            .get(&agent.operator_id)
            .copied();
        let task_state = crate::vouch::initial_state(author_trust);
        let title = input
            .title
            .as_deref()
            .or(input.intent.as_deref())
            .unwrap_or_default()
            .trim()
            .chars()
            .take(200)
            .collect::<String>();
        let title = if title.is_empty() {
            stats(&files).files.join(", ")
        } else {
            title
        };
        let row = self
            .store_patch(NewPatch {
                id: &id,
                project_id: &project.id,
                intent_id: &intent_id,
                base_commit: &base,
                title: &title,
                agent,
                parent_id: input.parent_id.as_deref(),
                files: &files,
                status: "open",
            })
            .await?;
        let task =
            crate::review::create_task(self, &row, &files, None, None, task_state, &environment)
                .await?;
        crate::git::notify(&self.env, &project.id, crate::git::Job::Patch(id.clone())).await;
        let mut out = json!({ "patch": row.public(), "created": true, "intent_reused": intent_reused, "similar": similar, "review_task": task });
        crate::vouch::annotate(&mut out, task_state);
        if let crate::environments::Current::Quarantined { reason, .. } = &environment {
            out["review_only"] = json!(format!(
                "This project's environment is quarantined ({reason}), so its review task has no run phase: nothing runs in it until the quarantine is lifted or replaced."
            ));
        }
        Ok(out)
    }

    pub async fn withdraw(&self, agent: &Agent, id: &str) -> ApiResult<Value> {
        let p = self.require_patch(id).await?;
        if p.author_op != agent.operator_id {
            return status(403, "only the author's operator can withdraw a patch");
        }
        self.db
            .run(
                "UPDATE patches SET status = 'withdrawn' WHERE id = ?",
                args![id],
            )
            .await?;
        crate::git::notify(
            &self.env,
            &p.project_id,
            crate::git::Job::Patch(id.to_string()),
        )
        .await;
        Ok(self.require_patch(id).await?.public())
    }

    pub async fn leaderboard(&self, project_ref: &str, base: Option<&str>) -> ApiResult<Value> {
        let project = self.require_project(project_ref, false).await?;
        let rows: Vec<PatchRow> = self
            .db
            .all(
                &format!(
                    "SELECT * FROM patches WHERE project_id = ? AND (? IS NULL OR base_commit = ?) AND status NOT IN {HIDDEN}
                     ORDER BY score DESC, adopters DESC LIMIT 50"
                ),
                args![&project.id, base, base],
            )
            .await?;
        #[derive(Deserialize)]
        struct I {
            id: String,
            text: String,
        }
        let ids: Vec<String> = rows
            .iter()
            .map(|r| r.intent_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut texts: HashMap<String, String> = HashMap::new();
        for chunk in ids.chunks(MAX_PARAMS) {
            let found: Vec<I> = self
                .db
                .all(
                    &format!(
                        "SELECT id, text FROM intents WHERE id IN ({})",
                        placeholders(chunk.len())
                    ),
                    chunk.iter().map(Into::into).collect(),
                )
                .await?;
            texts.extend(found.into_iter().map(|i| (i.id, i.text)));
        }
        let patches: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut v = r.public();
                v["intent"] = json!(texts.get(&r.intent_id));
                v
            })
            .collect();
        Ok(json!({ "project": project, "patches": patches }))
    }

    // -- adoption (the vote) --------------------------------------------------

    pub async fn adopt(
        &self,
        agent: &Agent,
        id: &str,
        outcome: Outcome,
        note: Option<&str>,
    ) -> ApiResult<Value> {
        let p = self.require_patch(id).await?;
        let note: Option<String> = note.map(|n| n.chars().take(2000).collect());
        self.db
            .run(
                "INSERT INTO adoptions (id, patch_id, agent_id, operator_id, outcome, note, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                args![new_id(), id, &agent.id, &agent.operator_id, outcome.as_str(), note, now()],
            )
            .await?;
        self.rescore(id).await?;
        if let Some(parent) = &p.parent_id {
            self.rescore(parent).await?;
        }
        let p = self.require_patch(id).await?;
        Ok(json!({ "patch": p.public(), "counted": agent.operator_id != p.author_op }))
    }

    /// Current genesis influence: founding grants, halving every 60 days.
    pub async fn genesis_influence(&self, operator_id: &str) -> ApiResult<f64> {
        #[derive(Deserialize)]
        struct G {
            amount: f64,
            granted_at: f64,
        }
        let rows: Vec<G> = self
            .db
            .all(
                "SELECT amount, granted_at FROM genesis WHERE operator_id = ?",
                args![operator_id],
            )
            .await?;
        let t = now();
        Ok(rows
            .iter()
            .map(|g| collective_core::score::genesis_effective(g.amount, t - g.granted_at as i64))
            .sum())
    }

    pub async fn grant_genesis(
        &self,
        handle: &str,
        amount: f64,
        note: Option<&str>,
    ) -> ApiResult<Value> {
        if !(amount > 0.0 && amount <= collective_core::score::GENESIS_MAX_GRANT) {
            return bad(format!(
                "amount must be in (0, {}]",
                collective_core::score::GENESIS_MAX_GRANT
            ));
        }
        #[derive(Deserialize)]
        struct O {
            id: String,
        }
        let Some(o) = self
            .db
            .first::<O>(
                "SELECT id FROM operators WHERE handle = ?",
                args![handle.to_lowercase()],
            )
            .await?
        else {
            return status(404, "unknown operator");
        };
        self.db
            .run(
                "INSERT INTO genesis (id, operator_id, amount, note, granted_at) VALUES (?, ?, ?, ?, ?)",
                args![new_id(), &o.id, amount, note, now()],
            )
            .await?;
        Ok(json!({ "operator": handle, "genesis_influence": self.genesis_influence(&o.id).await? }))
    }

    /// Trust for each operator: 0 if banned, otherwise from authored-patch
    /// scores plus ledger balance and genesis influence.
    pub async fn trust_map(&self, ops: &[String]) -> ApiResult<HashMap<String, f64>> {
        self.trust_map_adjusted(ops, 0.0).await
    }

    /// Trust for each operator as if `rep_delta` were added to its ledger
    /// balance: what its standing would be after a loan (`vouch`). One
    /// query per chunk of operators, genesis grants included, so a caller
    /// that sweeps many operators pays a few queries, not one per operator.
    /// An operator missing from the result is unknown: callers fail closed.
    pub async fn trust_map_adjusted(
        &self,
        ops: &[String],
        rep_delta: f64,
    ) -> ApiResult<HashMap<String, f64>> {
        Ok(self
            .standings_adjusted(ops, rep_delta)
            .await?
            .into_iter()
            .map(|(op, s)| (op, s.trust))
            .collect())
    }

    /// Each operator's public standing: its decayed ledger balance (rep)
    /// and the trust that balance, its patches' scores and its genesis
    /// grants give it. An operator missing from the result is unknown.
    pub async fn standings(&self, ops: &[String]) -> ApiResult<HashMap<String, Standing>> {
        self.standings_adjusted(ops, 0.0).await
    }

    async fn standings_adjusted(
        &self,
        ops: &[String],
        rep_delta: f64,
    ) -> ApiResult<HashMap<String, Standing>> {
        let mut out = HashMap::new();
        let t = now();
        for chunk in ops.chunks(MAX_PARAMS / 3) {
            #[derive(Deserialize)]
            struct T {
                id: String,
                banned_at: Option<f64>,
                scores: Option<String>,
                weight: Option<f64>,
                genesis: Option<String>,
            }
            let ph = placeholders(chunk.len());
            let rows: Vec<T> = self
                .db
                .all(
                    &format!(
                        "SELECT o.id, o.banned_at,
                           (SELECT GROUP_CONCAT(score) FROM patches WHERE author_op = o.id) AS scores,
                           (SELECT SUM(weight) FROM ledger WHERE operator_id = o.id) AS weight,
                           (SELECT GROUP_CONCAT(amount || ':' || granted_at) FROM genesis WHERE operator_id = o.id) AS genesis
                         FROM operators o WHERE o.id IN ({ph})"
                    ),
                    chunk.iter().map(Into::into).collect(),
                )
                .await?;
            for r in rows {
                let balance = ledger_balance(r.weight.unwrap_or(0.0), t);
                let trust_value = if r.banned_at.is_some() {
                    0.0
                } else {
                    let scores: Vec<f64> = r
                        .scores
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    let genesis: f64 = r
                        .genesis
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|g| {
                            let (amount, at) = g.split_once(':')?;
                            let (amount, at) =
                                (amount.parse::<f64>().ok()?, at.parse::<f64>().ok()?);
                            Some(collective_core::score::genesis_effective(
                                amount,
                                t - at as i64,
                            ))
                        })
                        .sum();
                    trust(reputation(&scores, balance + rep_delta + genesis))
                };
                out.insert(
                    r.id,
                    Standing {
                        rep: balance,
                        trust: trust_value,
                    },
                );
            }
        }
        Ok(out)
    }

    pub async fn rescore(&self, id: &str) -> ApiResult<()> {
        let p = self.require_patch_any(id).await?;
        #[derive(Deserialize)]
        struct A {
            operator_id: String,
            outcome: String,
            created_at: f64,
            share: f64,
        }
        let rows: Vec<A> = self
            .db
            .all(
                "SELECT operator_id, outcome, created_at, CASE WHEN patch_id = ? THEN 1.0 ELSE ? END AS share
                 FROM adoptions WHERE patch_id = ? OR patch_id IN (SELECT id FROM patches WHERE parent_id = ?)",
                args![id, PARENT_SHARE, id, id],
            )
            .await?;
        let ops: Vec<String> = rows
            .iter()
            .map(|r| r.operator_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let trust_of = self.trust_map(&ops).await?;
        let adoptions: Vec<Adoption> = rows
            .iter()
            .filter_map(|r| {
                Some(Adoption {
                    operator_id: r.operator_id.clone(),
                    outcome: Outcome::parse(&r.outcome)?,
                    created_at: r.created_at as i64,
                    share: r.share,
                })
            })
            .collect();
        let s = score_patch(
            &p.author_op,
            &adoptions,
            |op| trust_of.get(op).copied().unwrap_or(trust(0.0)),
            now(),
        );
        self.db
            .run(
                "UPDATE patches SET score = ?, adopters = ? WHERE id = ?",
                args![s.score, s.adopters, id],
            )
            .await
    }

    /// Operators that adopted a patch with a net-positive outcome, with weights.
    pub async fn positive_adopters(&self, patch_id: &str) -> ApiResult<Vec<(String, f64)>> {
        #[derive(Deserialize)]
        struct A {
            operator_id: String,
            outcome: String,
        }
        let rows: Vec<A> = self
            .db
            .all(
                "SELECT operator_id, outcome FROM adoptions WHERE patch_id = ?",
                args![patch_id],
            )
            .await?;
        let mut per: HashMap<String, f64> = HashMap::new();
        for r in rows {
            if let Some(o) = Outcome::parse(&r.outcome) {
                *per.entry(r.operator_id).or_default() += o.weight();
            }
        }
        Ok(per
            .into_iter()
            .filter(|(_, w)| *w > 0.0)
            .map(|(op, w)| (op, w.min(collective_core::score::PER_OPERATOR_CAP)))
            .collect())
    }

    // -- convergence ----------------------------------------------------------

    pub async fn converge(&self, project_id: &str, base: &str) -> ApiResult<Value> {
        let rows: Vec<PatchRow> = self
            .db
            .all(
                "SELECT * FROM patches WHERE project_id = ? AND base_commit = ? AND status IN ('open','converged') ORDER BY score DESC LIMIT 500",
                args![project_id, base],
            )
            .await?;
        let mut candidates = Vec::new();
        for r in &rows {
            candidates.push(Candidate {
                id: r.id.clone(),
                intent_id: r.intent_id.clone(),
                parent_id: r.parent_id.clone(),
                score: r.score,
                adopters: r.adopters as u32,
                files: parse(&self.diff_text(&r.id).await?)?,
            });
        }
        let trunk = converge(&candidates, ConvergeOptions::default());
        self.blobs
            .put(
                format!("trunks/{project_id}/{base}.diff"),
                trunk.diff.clone(),
            )
            .execute()
            .await?;
        self.db
            .run(
                "INSERT OR REPLACE INTO trunks (project_id, base_commit, patch_ids, skipped, created_at) VALUES (?, ?, ?, ?, ?)",
                args![project_id, base, serde_json::to_string(&trunk.patch_ids)?, serde_json::to_string(&trunk.skipped)?, now()],
            )
            .await?;
        Ok(
            json!({ "project_id": project_id, "base_commit": base, "patch_ids": trunk.patch_ids, "skipped": trunk.skipped }),
        )
    }

    pub async fn trunk(&self, project_ref: &str, base: &str) -> ApiResult<Value> {
        let project = self.require_project(project_ref, false).await?;
        #[derive(Deserialize)]
        struct T {
            patch_ids: String,
            skipped: String,
            created_at: f64,
        }
        let Some(t) = self.db.first::<T>("SELECT patch_ids, skipped, created_at FROM trunks WHERE project_id = ? AND base_commit = ?", args![&project.id, base]).await? else {
            return status(404, "no trunk for this base yet");
        };
        Ok(json!({
            "project_id": project.id,
            "base_commit": base,
            "patch_ids": serde_json::from_str::<Value>(&t.patch_ids)?,
            "skipped": serde_json::from_str::<Value>(&t.skipped)?,
            "created_at": t.created_at as i64,
        }))
    }

    pub async fn trunk_diff(&self, project_ref: &str, base: &str) -> ApiResult<String> {
        let project = self.require_project(project_ref, false).await?;
        match self
            .blobs
            .get(format!("trunks/{}/{base}.diff", project.id))
            .execute()
            .await?
        {
            Some(obj) => Ok(obj
                .body()
                .ok_or_else(|| ApiError::new(500, "empty blob"))?
                .text()
                .await?),
            None => status(404, "no trunk for this base yet"),
        }
    }
}

pub struct NewPatch<'a> {
    pub id: &'a str,
    pub project_id: &'a str,
    pub intent_id: &'a str,
    pub base_commit: &'a str,
    pub title: &'a str,
    pub agent: &'a Agent,
    pub parent_id: Option<&'a str>,
    pub files: &'a [FileDiff],
    pub status: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct SubmitPatch {
    pub project: String,
    pub intent: Option<String>,
    pub intent_id: Option<String>,
    pub base_commit: String,
    pub diff: String,
    pub title: Option<String>,
    pub parent_id: Option<String>,
}
