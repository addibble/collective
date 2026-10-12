//! Proofs over D1 and the sandbox container. The container runs
//! crates/prover; collective_core::proof decides what its runs mean.

use std::time::Duration;

use collective_core::challenge::{Challenge, Intent, Phase, ResolvedVia, Verdict};
use collective_core::proof::{
    assess, encode_prove_request, Ecosystem, ProofReport, ProofSpec, ProveRequest,
    Verdict as ProofVerdict,
};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::wasm_bindgen::JsValue;
use worker::*;

use crate::args;
use crate::challenges;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::store::{new_id, now, Agent, Store};

/// Proofs an operator may submit per challenge: each one costs a sandbox run.
pub const MAX_PER_OPERATOR: u32 = 3;
/// Proofs an operator may submit per day across all challenges: each one
/// starts a container, so cheap bare reports must not multiply them.
pub const MAX_PER_OPERATOR_PER_DAY: u32 = 10;
/// Largest upstream source tarball a proof will fetch (Worker memory is limited).
pub const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const PROVER_PORT: u16 = 8080;

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
    Ok(load(store, id).await?.view(true))
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

/// Queues a proof. Only the finder or operators who staked on "problem" may
/// submit: running a sandbox costs real compute.
pub async fn submit(
    store: &Store,
    env: &Env,
    agent: &Agent,
    challenge_id: &str,
    spec: ProofSpec,
) -> ApiResult<Value> {
    spec.validate().map_err(|e| ApiError::new(400, e))?;
    if spec.kind == collective_core::proof::ProofKind::Suite {
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
    // One Durable Object, and so one container, per proof.
    let stub = env.durable_object("PROOF")?.id_from_name(&id)?.get_stub()?;
    stub.fetch_with_str(&format!("https://runner/start?proof={id}"))
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

async fn set_status(store: &Store, id: &str, status: &str, reason: Option<&str>) -> ApiResult<()> {
    store
        .db
        .run(
            "UPDATE proofs SET status = ?, reason = COALESCE(?, reason) WHERE id = ?",
            args![status, reason, id],
        )
        .await
}

/// Records a finished run and applies it: a proven regression resolves
/// the challenge; a demonstration is evidence for the jury or council.
async fn complete(
    store: &Store,
    row: &ProofRow,
    spec: &ProofSpec,
    report: &ProofReport,
) -> ApiResult<()> {
    let a = assess(spec, report);
    let verdict = serde_json::to_value(a.verdict)?
        .as_str()
        .unwrap_or_default()
        .to_string();
    store
        .db
        .run(
            "UPDATE proofs SET status = 'done', verdict = ?, reason = ?, report = ?, finished_at = ? WHERE id = ?",
            args![verdict, &a.reason, serde_json::to_string(report)?, now(), &row.id],
        )
        .await?;
    if a.verdict == ProofVerdict::Proven {
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
    Ok(())
}

/// Starts the container with a plain options object. workers-rs 0.8.7's
/// `ContainerStartupOptions` drops `enableInternet` and `env` when they are
/// set (inverted checks in its `From<_> for Object`), so we build the object
/// the runtime expects ourselves.
fn start_container(
    container: &Container,
    enable_internet: bool,
    env: &[(&str, String)],
) -> ApiResult<()> {
    use worker::wasm_bindgen::JsCast;
    let set = |o: &js_sys::Object, k: &str, v: &JsValue| {
        js_sys::Reflect::set(o, &JsValue::from_str(k), v).map(|_| ())
    };
    let env_obj = js_sys::Object::new();
    let opts = js_sys::Object::new();
    let built = env
        .iter()
        .try_for_each(|(k, v)| set(&env_obj, k, &JsValue::from_str(v)))
        .and_then(|_| {
            set(
                &opts,
                "enableInternet",
                &JsValue::from_bool(enable_internet),
            )
        })
        .and_then(|_| set(&opts, "env", &env_obj));
    built.map_err(|e| ApiError::new(500, format!("container options: {e:?}")))?;
    let raw: &worker::worker_sys::Container = container.as_ref().unchecked_ref();
    raw.start(&opts)
        .map_err(|e| ApiError::new(500, format!("container start: {e:?}")))
}

/// Runs one proof end to end inside the container.
async fn run(state: &State, store: &Store, id: &str) -> ApiResult<()> {
    let row = load(store, id).await?;
    if row.status != "queued" {
        return Ok(());
    }
    set_status(store, id, "running", None).await?;
    let spec: ProofSpec = serde_json::from_str(&row.spec)?;
    let patch = store.require_patch_any(&row.patch_id).await?;
    let project = store.require_project(&patch.project_id, false).await?;
    let source = source_tarball(
        store,
        &project.id,
        &project.upstream_url,
        &patch.base_commit,
    )
    .await?;
    let body = encode_prove_request(
        &ProveRequest {
            spec: spec.clone(),
            patch: store.diff_text(&patch.id).await?,
        },
        &source,
    );

    let container = state
        .container()
        .ok_or_else(|| ApiError::new(500, "no container bound to ProofRunner"))?;
    if !container.running() {
        // Proofs without a dependency fetch never get internet at all.
        let offline = spec.ecosystem == Ecosystem::None;
        let mut env = vec![("PROVER_BUDGET_SECS", spec.budget_secs().to_string())];
        if offline {
            env.push(("PROVER_NO_INTERNET", "1".into()));
        }
        start_container(&container, !offline, &env)?;
    }
    let port = container.get_tcp_port(PROVER_PORT)?;
    let mut ready = false;
    for _ in 0..90 {
        if port
            .fetch("http://prover/health", None)
            .await
            .is_ok_and(|r| r.status_code() == 200)
        {
            ready = true;
            break;
        }
        Delay::from(Duration::from_secs(1)).await;
    }
    if !ready {
        return status(503, "prover container did not become ready");
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(JsValue::from(js_sys::Uint8Array::from(
            body.as_slice(),
        ))));
    let mut resp = port.fetch("http://prover/prove", Some(init)).await?;
    let text = resp.text().await?;
    if resp.status_code() != 200 {
        return status(502, format!("prover: {text}"));
    }
    let report: ProofReport = serde_json::from_str(&text)?;
    complete(store, &row, &spec, &report).await?;
    let _ = container.destroy(None).await;
    Ok(())
}

#[durable_object]
pub struct ProofRunner {
    state: State,
    env: Env,
}

impl DurableObject for ProofRunner {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let Some(id) = req
            .url()?
            .query_pairs()
            .find(|(k, _)| k == "proof")
            .map(|(_, v)| v.into_owned())
        else {
            return Response::error("proof required", 400);
        };
        self.state.storage().put("proof", &id).await?;
        // Run from an alarm so the submitting request returns at once.
        self.state
            .storage()
            .set_alarm(Duration::from_millis(1))
            .await?;
        Response::ok("queued")
    }

    async fn alarm(&self) -> Result<Response> {
        let Some(id) = self.state.storage().get::<String>("proof").await? else {
            return Response::empty();
        };
        let store = Store::new(&self.env).map_err(|e| Error::RustError(e.message))?;
        if let Err(e) = run(&self.state, &store, &id).await {
            console_error!("proof {id}: {}", e.message);
            let _ = store
                .db
                .run(
                    "UPDATE proofs SET status = 'error', reason = ?, finished_at = ? WHERE id = ?",
                    args![&e.message, now(), &id],
                )
                .await;
            if let Some(c) = self.state.container() {
                let _ = c.destroy(Some(&e.message)).await;
            }
        }
        Response::empty()
    }
}
