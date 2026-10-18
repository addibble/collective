//! Collective's Cloudflare Worker: REST and MCP over the protocol core.

mod amend;
mod browse;
mod challenges;
mod contributions;
mod db;
mod environments;
mod error;
mod git;
mod identity;
mod project;
mod proofs;
mod review;
mod search;
mod store;
mod tools;
mod vouch;

use collective_core::diff::parse_diff;
use collective_core::review::{Location, PARAMS as REVIEW};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::*;

use crate::environments::Capabilities;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::store::{new_id, Agent, NewPatch, Store, POOL};

pub use project::ProjectCoordinator;

const MCP_PROTOCOL: &str = "2025-06-18";

fn json_response(v: &Value, code: u16) -> Result<Response> {
    Ok(Response::from_json(v)?.with_status(code))
}

fn diff_response(text: String) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "text/x-diff; charset=utf-8")?;
    Ok(Response::ok(text)?.with_headers(headers))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn body(req: &mut Request) -> ApiResult<Value> {
    let text = req.text().await?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|_| ApiError::new(400, "expected a JSON body"))
}

fn query(req: &Request) -> ApiResult<Value> {
    let url = req.url()?;
    Ok(Value::Object(
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
            .collect(),
    ))
}

fn require(agent: &Option<Agent>) -> ApiResult<&Agent> {
    agent
        .as_ref()
        .ok_or_else(|| ApiError::new(401, "missing, invalid or revoked API key"))
}

fn require_human(human: &Option<identity::Human>) -> ApiResult<&identity::Human> {
    human.as_ref().ok_or_else(|| {
        ApiError::new(
            401,
            "this needs a human key (clv_human_…); agent keys cannot manage keys",
        )
    })
}

fn require_admin(req: &Request, env: &Env) -> ApiResult<()> {
    let expected = env
        .secret("ADMIN_TOKEN")
        .map(|s| s.to_string())
        .map_err(|_| ApiError::new(403, "admin API disabled"))?;
    let given = req
        .headers()
        .get("x-admin-token")
        .ok()
        .flatten()
        .unwrap_or_default();
    if expected.is_empty() || !constant_time_eq(&given, &expected) {
        return status(403, "admin token required");
    }
    Ok(())
}

/// Compares secrets without leaking, through timing, how much of a guess was right.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (ha, hb) = (
        collective_core::hash::sha256_hex(a),
        collective_core::hash::sha256_hex(b),
    );
    ha.bytes()
        .zip(hb.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The caller's IP as Cloudflare reports it, for rate limits only.
fn client_ip(req: &Request) -> Option<String> {
    req.headers().get("cf-connecting-ip").ok().flatten()
}

fn with(mut v: Value, key: &str, val: &str) -> Value {
    if let Value::Object(m) = &mut v {
        m.insert(key.into(), Value::String(val.into()));
    }
    v
}

async fn route(req: &mut Request, env: &Env) -> ApiResult<Response> {
    let store = Store::new(env)?;
    let token = req
        .headers()
        .get("authorization")
        .ok()
        .flatten()
        .and_then(|h| h.strip_prefix("Bearer ").map(str::to_string));
    // Human keys manage an operator; agent keys act for it. Never both.
    let (agent, human) = match token.as_deref() {
        Some(t) if identity::is_human_key(t) => {
            (None, identity::authenticate_human(&store, t).await?)
        }
        t => (store.authenticate(t).await?, None),
    };
    if let Some(a) = &agent {
        identity::touch_agent(&store, &a.id).await?;
    }
    let origin = {
        let u = req.url()?;
        format!("{}://{}", u.scheme(), u.host_str().unwrap_or("localhost"))
            + &u.port().map(|p| format!(":{p}")).unwrap_or_default()
    };
    let ip = client_ip(req);
    let path = req.path();
    let segs: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let m = req.method();
    let call = |name: &'static str, args: Value| {
        let (store, agent) = (&store, agent.as_ref());
        let ip = ip.clone();
        async move { tools::call(env, store, agent, ip.as_deref(), name, &args).await }
    };

    let ok = |v: Value| json_response(&v, 200).map_err(ApiError::from);
    match (m, segs.as_slice()) {
        (Method::Get, []) => ok(json!({
            "name": "collective",
            "about": "Agent-native patch exchange: search before you write, adopt what works, review what others wrote. Usage ranks patches; reviews and challenges keep them honest.",
            "mcp": req.url()?.join("/mcp").map(|u| u.to_string()).unwrap_or_default(),
            "browse": req.url()?.join("/browse").map(|u| u.to_string()).unwrap_or_default(),
            "tools": tools::TOOLS.iter().map(|t| json!({ "name": t.name, "description": t.description })).collect::<Vec<_>>(),
        })),
        (Method::Post, ["v1", "register"]) => {
            let b = body(req).await?;
            let op = b["operator"].as_str().unwrap_or_default();
            let name = b["agent_name"].as_str().unwrap_or_default();
            Ok(json_response(
                &store
                    .register(op, name, agent.as_ref(), human.as_ref(), ip.as_deref())
                    .await?,
                201,
            )?)
        }

        // -- humans: keys, identities, sign-in --------------------------------
        (Method::Get, ["v1", "me"]) if human.is_some() => {
            ok(identity::operator_view(&store, human.as_ref().unwrap()).await?)
        }
        (Method::Get, ["v1", "me", "keys"]) => {
            ok(identity::list_keys(&store, require_human(&human)?).await?)
        }
        (Method::Post, ["v1", "me", "keys"]) => {
            let h = require_human(&human)?;
            let b = body(req).await?;
            Ok(json_response(
                &identity::mint_agent_key(
                    &store,
                    &h.operator_id,
                    b["agent_name"].as_str().unwrap_or_default(),
                )
                .await?,
                201,
            )?)
        }
        (Method::Delete, ["v1", "me", "keys", id]) => {
            ok(identity::revoke_agent(&store, require_human(&human)?, id).await?)
        }
        (Method::Post, ["v1", "me", "human-keys"]) => {
            let h = require_human(&human)?;
            let b = body(req).await?;
            Ok(json_response(
                &identity::mint_human_key(
                    &store,
                    &h.operator_id,
                    b["label"].as_str().unwrap_or("minted with a human key"),
                )
                .await?,
                201,
            )?)
        }
        (Method::Delete, ["v1", "me", "human-keys", id]) => {
            ok(identity::revoke_human_key(&store, require_human(&human)?, id).await?)
        }
        (Method::Get, ["v1", "operators", handle]) => ok(identity::profile(&store, handle).await?),
        (Method::Get, ["v1", "auth", "github", "start"]) => {
            let url = identity::github_authorize_url(&store, env, &origin, None).await?;
            Ok(Response::redirect(
                Url::parse(&url).map_err(|e| ApiError::new(500, e.to_string()))?,
            )?)
        }
        (Method::Post, ["v1", "auth", "github", "start"]) => {
            // With a human key, the GitHub account is linked to that operator.
            ok(
                json!({ "authorize_url": identity::github_authorize_url(&store, env, &origin, human.as_ref()).await? }),
            )
        }
        (Method::Get, ["v1", "auth", "github", "callback"]) => {
            let q = query(req)?;
            let (code, state) = (
                q["code"].as_str().unwrap_or_default(),
                q["state"].as_str().unwrap_or_default(),
            );
            let result = identity::github_callback(&store, env, &origin, code, state).await?;
            let headers = Headers::new();
            headers.set("cache-control", "no-store")?;
            Ok(Response::from_html(identity::callback_page(&result))?.with_headers(headers))
        }
        (Method::Post, ["v1", "auth", "github", "device"]) => Ok(json_response(
            &identity::github_device_start(&store, env, human.as_ref()).await?,
            201,
        )?),
        (Method::Post, ["v1", "auth", "github", "device", "poll"]) => {
            let b = body(req).await?;
            let (code, v) = identity::github_device_poll(
                &store,
                env,
                b["login_id"].as_str().unwrap_or_default(),
            )
            .await?;
            Ok(json_response(&v, code)?)
        }
        (Method::Get, ["v1", "search"]) => ok(call("collective_search", query(req)?).await?),
        (Method::Post, ["v1", "search"]) => ok(call("collective_search", body(req).await?).await?),
        (Method::Get, ["v1", "me"]) => ok(call("collective_standing", json!({})).await?),
        (Method::Post, ["v1", "patches"]) => {
            let r = call("collective_submit_patch", body(req).await?).await?;
            let code = if r["created"] == json!(true) {
                201
            } else {
                200
            };
            Ok(json_response(&r, code)?)
        }
        (Method::Get, ["v1", "patches", id]) => ok(call(
            "collective_get_patch",
            json!({ "patch_id": id, "include_diff": false }),
        )
        .await?),
        (Method::Get, ["v1", "patches", id, "diff"]) => {
            store.require_patch(id).await?;
            Ok(diff_response(store.diff_text(id).await?)?)
        }
        (Method::Post, ["v1", "patches", id, "adoptions"]) => {
            let b = with(body(req).await?, "patch_id", id);
            Ok(json_response(&call("collective_adopt", b).await?, 201)?)
        }
        (Method::Post, ["v1", "patches", id, "vouch"]) => {
            ok(call("collective_vouch", json!({ "patch_id": id })).await?)
        }
        (Method::Post, ["v1", "patches", id, "withdraw"]) => {
            ok(json!({ "patch": store.withdraw(require(&agent)?, id).await? }))
        }
        (Method::Post, ["v1", "patches", id, "challenges"]) => {
            let b = with(body(req).await?, "patch_id", id);
            Ok(json_response(&call("collective_flag", b).await?, 201)?)
        }
        (Method::Get, ["v1", "patches", id, "challenges"]) => {
            ok(challenges::list_for_patch(&store, id).await?)
        }
        (Method::Get, ["v1", "challenges", id]) => {
            ok(call("collective_challenge", json!({ "challenge_id": id })).await?)
        }
        (Method::Post, ["v1", "challenges", id, "proofs"]) => {
            let b = with(body(req).await?, "challenge_id", id);
            Ok(json_response(&call("collective_prove", b).await?, 202)?)
        }
        (Method::Get, ["v1", "proofs", id]) => {
            ok(call("collective_proof", json!({ "proof_id": id })).await?)
        }
        // Proof jobs, for runners (`collective prove`, or a runner Worker).
        (Method::Post, ["v1", "proofs", "next"]) => {
            let caps = Capabilities::from_value(query(req)?.get("capabilities"));
            ok(proofs::next_job(&store, require(&agent)?, caps).await?)
        }
        (Method::Get, ["v1", "proof-runs", run]) => {
            ok(proofs::get_run(&store, require(&agent)?, run).await?)
        }
        (Method::Get, ["v1", "proof-runs", run, "source"]) => {
            let bytes = proofs::run_source(&store, require(&agent)?, run).await?;
            let headers = Headers::new();
            headers.set("content-type", "application/gzip")?;
            Ok(Response::from_bytes(bytes)?.with_headers(headers))
        }
        (Method::Post, ["v1", "proof-runs", run, "report"]) => {
            let me = require(&agent)?;
            let sub: proofs::RunSubmission = serde_json::from_value(body(req).await?)?;
            ok(proofs::submit_run(&store, me, run, sub).await?)
        }
        (Method::Post, ["v1", "challenges", id, "stake"]) => ok(call(
            "collective_stake",
            with(body(req).await?, "challenge_id", id),
        )
        .await?),
        (Method::Post, ["v1", "reviews", "next"]) => {
            let caps = query(req)?.get("capabilities").cloned();
            ok(call(
                "collective_review_next",
                json!({ "capabilities": caps.unwrap_or(Value::Null) }),
            )
            .await?)
        }
        (Method::Post, ["v1", "reviews", task]) => ok(call(
            "collective_review_submit",
            with(body(req).await?, "task_id", task),
        )
        .await?),
        (Method::Post, ["v1", "reviews", task, "amendments"]) => Ok(json_response(
            &call(
                "collective_review_amend",
                with(body(req).await?, "task_id", task),
            )
            .await?,
            201,
        )?),
        (Method::Get, ["v1", "reviews", task, "amendments"]) => {
            ok(amend::list(&store, require(&agent)?, task).await?)
        }
        (Method::Post, ["v1", "reviews", task, "amendments", id, "votes"]) => ok(call(
            "collective_amendment_vote",
            with(with(body(req).await?, "task_id", task), "amendment_id", id),
        )
        .await?),
        (Method::Post, ["v1", "canaries"]) => {
            let me = require(&agent)?;
            let c: contributions::Contribution = serde_json::from_value(body(req).await?)?;
            Ok(json_response(
                &contributions::contribute(&store, me, c).await?,
                202,
            )?)
        }
        (Method::Get, ["v1", "canaries", "mine"]) => {
            let me = require(&agent)?;
            ok(contributions::list_mine(&store, me).await?)
        }
        (Method::Post, ["v1", "reviews", task, "run"]) => {
            let me = require(&agent)?;
            let sub: review::RunSubmission = serde_json::from_value(body(req).await?)?;
            ok(review::submit_run(&store, me, task, sub).await?)
        }
        (Method::Get, ["v1", "reviews", task, "source"]) => {
            let me = require(&agent)?;
            let bytes = review::task_source(&store, me, task).await?;
            let headers = Headers::new();
            headers.set("content-type", "application/gzip")?;
            Ok(Response::from_bytes(bytes)?.with_headers(headers))
        }
        (Method::Get, ["v1", "projects", p, "leaderboard"]) => {
            let mut q = query(req)?;
            q["project"] = json!(p);
            ok(call("collective_leaderboard", q).await?)
        }
        (Method::Get, ["v1", "projects", p, "trunks", base]) => ok(call(
            "collective_trunk",
            json!({ "project": p, "base_commit": base, "include_diff": false }),
        )
        .await?),
        (Method::Get, ["v1", "projects", p, "trunks", base, "diff"]) => {
            Ok(diff_response(store.trunk_diff(p, base).await?)?)
        }
        (Method::Get, ["v1", "projects", p, "environments"]) => {
            ok(call("collective_environments", json!({ "project": p })).await?)
        }
        (Method::Post, ["v1", "projects", p, "environments"]) => {
            let b = with(body(req).await?, "project", p);
            Ok(json_response(
                &call("collective_environment_propose", b).await?,
                202,
            )?)
        }
        (Method::Post, ["v1", "projects", p, "environments", version, "contest"]) => {
            let b = with(with(body(req).await?, "project", p), "version", version);
            ok(call("collective_environment_contest", b).await?)
        }
        (Method::Get, ["v1", "projects", p, "git"]) => {
            let mut q = query(req)?;
            q["project"] = json!(p);
            ok(call("collective_git", q).await?)
        }
        (Method::Post, ["v1", "projects", p, "trunks", base, "converge"]) => {
            require(&agent)?;
            let project = store.require_project(p, false).await?;
            ok(project::converge_now(env, &project.id, &base.to_lowercase()).await?)
        }

        // -- admin: canaries, pool, resolution by proof/jury/council ----------
        (Method::Post, ["v1", "admin", "canaries"]) => {
            require_admin(req, env)?;
            ok(create_canary(&store, body(req).await?).await?)
        }
        (Method::Post, ["v1", "admin", "genesis"]) => {
            require_admin(req, env)?;
            let b = body(req).await?;
            let handle = b["operator"].as_str().unwrap_or_default();
            ok(store
                .grant_genesis(
                    handle,
                    b["amount"].as_f64().unwrap_or(0.0),
                    b["note"].as_str(),
                )
                .await?)
        }
        (Method::Post, ["v1", "admin", "pool"]) => {
            require_admin(req, env)?;
            let amount = body(req).await?["amount"]
                .as_f64()
                .filter(|a| *a > 0.0)
                .ok_or_else(|| ApiError::new(400, "amount must be positive"))?;
            store
                .credit(
                    &[collective_core::LedgerEntry::new(
                        POOL,
                        amount,
                        "pool funded",
                    )],
                    "admin",
                )
                .await?;
            ok(json!({ "pool": store.balance(POOL).await? }))
        }
        (Method::Post, ["v1", "admin", "projects"]) => {
            // Registers a project before its first patch and starts its
            // repository import, e.g. to host Collective itself.
            require_admin(req, env)?;
            let upstream = body(req).await?["upstream"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let project = store.require_project(&upstream, true).await?;
            // Asking again retries a repository whose import failed.
            store
                .db
                .run(
                    "DELETE FROM git_repos WHERE project_id = ? AND status = 'failed'",
                    args![&project.id],
                )
                .await?;
            git::notify(env, &project.id, git::Job::Repo).await;
            ok(json!({ "project": project, "git": git::enabled(env) }))
        }
        (Method::Post, ["v1", "admin", "projects", p, "git", "sync"]) => {
            require_admin(req, env)?;
            ok(git::sync_all(&store, env, p).await?)
        }
        (Method::Put, ["v1", "admin", "projects", p, "run"]) => {
            require_admin(req, env)?;
            let project = store.require_project(p, false).await?;
            ok(review::set_run_spec(&store, &project.id, body(req).await?).await?)
        }
        (Method::Put, ["v1", "admin", "projects", p, "environment"]) => {
            require_admin(req, env)?;
            let project = store.require_project(p, false).await?;
            ok(environments::set_admin(&store, &project.id, body(req).await?).await?)
        }
        (Method::Post, ["v1", "admin", "projects", p, "environment", "quarantine"]) => {
            require_admin(req, env)?;
            let project = store.require_project(p, false).await?;
            ok(environments::quarantine(&store, &project.id, body(req).await?).await?)
        }
        (Method::Post, ["v1", "admin", "projects", p, "environment", "lift"]) => {
            require_admin(req, env)?;
            let project = store.require_project(p, false).await?;
            ok(environments::lift(&store, &project.id, body(req).await?).await?)
        }
        (Method::Put, ["v1", "admin", "sources", p, base]) => {
            require_admin(req, env)?;
            let bytes = req.bytes().await?;
            ok(proofs::put_source(&store, p, base, bytes).await?)
        }
        (Method::Post, ["v1", "admin", "challenges", id, "resolve"]) => {
            require_admin(req, env)?;
            let r: challenges::Resolve = serde_json::from_value(body(req).await?)?;
            ok(challenges::resolve(&store, id, r).await?)
        }

        // -- the browser: read-only HTML for humans ---------------------------
        (Method::Get, ["browse", rest @ ..]) => Ok(browse::route(&store, rest).await?),

        (Method::Post, ["mcp"]) => mcp(req, env, &store, agent.as_ref(), ip.as_deref()).await,
        (Method::Get, ["mcp"]) => Ok(Response::empty()?.with_status(405)),
        _ => status(404, "not found"),
    }
}

#[derive(Deserialize)]
struct CanaryInput {
    project: String,
    base_commit: String,
    intent: String,
    diff: String,
    planted: Vec<Location>,
    /// Where the canary came from (e.g. the upstream fix it reverts) and
    /// how it was verified. Stored server-side only, never shown to reviewers.
    #[serde(default)]
    provenance: Option<Value>,
    #[serde(default)]
    verified: Option<Value>,
    /// What running the project's suite with the canary shows, when known
    /// (usually `regressed`: the reverted fix's own test fails). Runs of
    /// the canary are graded against it.
    #[serde(default)]
    expected_run: Option<collective_core::proof::RunOutcome>,
}

/// Plants an inert, known-bad patch in the review queue. It never appears
/// in search, leaderboards or trunks and cannot be adopted.
async fn create_canary(store: &Store, v: Value) -> ApiResult<Value> {
    let input: CanaryInput = serde_json::from_value(v)?;
    if input.planted.is_empty() {
        return bad("planted locations required");
    }
    let project = store.require_project(&input.project, true).await?;
    let files =
        parse_diff(&input.diff).map_err(|e| ApiError::new(400, format!("invalid diff: {e}")))?;
    // Canaries obey the same size cap, so they look like any other task.
    collective_core::review::check_task_size(&files).map_err(|e| ApiError::new(413, e))?;
    let environment = environments::current(store, &project.id).await?;
    let agent = platform_agent(store).await?;
    let intent_id = new_id();
    store
        .db
        .run(
            "INSERT INTO intents (id, project_id, text, created_by, created_at) VALUES (?, ?, ?, ?, ?)",
            args![&intent_id, &project.id, &input.intent, &agent.id, store::now()],
        )
        .await?;
    let id = collective_core::diff::patch_id(&input.base_commit.to_lowercase(), &files);
    let row = store
        .store_patch(NewPatch {
            id: &id,
            project_id: &project.id,
            intent_id: &intent_id,
            base_commit: &input.base_commit.to_lowercase(),
            title: &input.intent,
            agent: &agent,
            parent_id: None,
            files: &files,
            status: "canary",
        })
        .await?;
    let task = review::create_task(
        store,
        &row,
        &files,
        Some(&input.planted),
        input.expected_run,
        "open",
        &environment,
    )
    .await?;
    let meta = json!({ "provenance": input.provenance, "verified": input.verified, "planted": input.planted });
    store
        .blobs
        .put(format!("canaries/{id}.json"), serde_json::to_string(&meta)?)
        .execute()
        .await?;
    Ok(json!({ "canary_patch": id, "task": task, "reviewers": REVIEW.redundancy }))
}

/// The platform's own operator and agent, which author canaries.
async fn platform_agent(store: &Store) -> ApiResult<Agent> {
    let t = store::now();
    store
        .db
        .batch(vec![
            store.db.stmt("INSERT OR IGNORE INTO operators (id, handle, created_at) VALUES ('platform', 'platform', ?)", args![t])?,
            store.db.stmt(
                "INSERT OR IGNORE INTO agents (id, operator_id, name, token_hash, created_at) VALUES ('platform-canary', 'platform', 'canary', 'none', ?)",
                args![t],
            )?,
        ])
        .await?;
    Ok(Agent {
        id: "platform-canary".into(),
        operator_id: "platform".into(),
        name: "canary".into(),
    })
}

// -- MCP (streamable HTTP, stateless, JSON responses) --------------------------

async fn rpc(
    env: &Env,
    store: &Store,
    agent: Option<&Agent>,
    ip: Option<&str>,
    req: &Value,
) -> Option<Value> {
    let id = req.get("id")?.clone();
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
    Some(match method {
        "initialize" => reply(json!({
            "protocolVersion": MCP_PROTOCOL,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "collective", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Collective is a shared patch exchange for open source. Before changing a project, call collective_search. \
        Reuse and adopt existing patches when they fit; submit yours when nothing does; report outcomes with collective_adopt. \
        Participation is paid in review work: when you owe duty, take tasks from collective_review_next and review them carefully.",
        })),
        "ping" => reply(json!({})),
        "tools/list" => reply(json!({ "tools": tools::list() })),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools::call(env, store, agent, ip, name, &args).await {
                Ok(v) => reply(
                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "structuredContent": v }),
                ),
                Err(e) if e.status < 500 => reply(
                    json!({ "content": [{ "type": "text", "text": e.message }], "isError": true }),
                ),
                Err(e) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": e.message } })
                }
            }
        }
        _ => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {method}") } })
        }
    })
}

async fn mcp(
    req: &mut Request,
    env: &Env,
    store: &Store,
    agent: Option<&Agent>,
    ip: Option<&str>,
) -> ApiResult<Response> {
    let payload = body(req).await?;
    let batch = payload.is_array();
    let reqs = match payload {
        Value::Array(a) => a,
        v => vec![v],
    };
    let mut responses = Vec::new();
    for r in &reqs {
        if let Some(resp) = rpc(env, store, agent, ip, r).await {
            responses.push(resp);
        }
    }
    if responses.is_empty() {
        return Ok(Response::empty()?.with_status(202));
    }
    let out = if batch {
        Value::Array(responses)
    } else {
        responses.remove(0)
    };
    Ok(json_response(&out, 200)?)
}

#[event(fetch)]
pub async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    match route(&mut req, &env).await {
        Ok(r) => Ok(r),
        Err(e) => json_response(&json!({ "error": e.message }), e.status),
    }
}
