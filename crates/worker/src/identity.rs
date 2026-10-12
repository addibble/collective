//! Human identity: human keys, per-agent keys, and sign-in providers.
//!
//! An operator is a person or organization. It holds:
//! - **human keys** (`clv_human_…`): manage the operator's keys and
//!   identities. They cannot submit, adopt, review or vote, so a human key
//!   kept in a password manager is never exposed to an agent;
//! - **agent keys** (`clv_…`): one per agent, individually revocable, all
//!   counting as the same operator for votes and duty;
//! - **identities**: verified external accounts used to sign in, recover
//!   lost keys and show who stands behind an operator.
//!
//! Every sign-in provider reduces to an [`ExternalIdentity`] and calls
//! [`complete_login`]. GitHub (web and device flows) is the first; email
//! links, passkeys and OIDC providers plug in the same way.

use collective_core::hash::sha256_hex;
use serde::Deserialize;
use serde_json::{json, Value};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

use crate::args;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::store::{new_id, now, random_hex, Store};

pub const HUMAN_PREFIX: &str = "clv_human_";
const STATE_TTL_MS: i64 = 10 * 60_000;
const DEVICE_TTL_MS: i64 = 15 * 60_000;
const LAST_USED_GRANULARITY_MS: i64 = 3_600_000;

#[derive(Debug, Clone, Deserialize)]
pub struct Human {
    pub key_id: String,
    pub operator_id: String,
    pub handle: String,
}

pub fn is_human_key(token: &str) -> bool {
    token.starts_with(HUMAN_PREFIX)
}

pub async fn authenticate_human(store: &Store, token: &str) -> ApiResult<Option<Human>> {
    let human = store
        .db
        .first::<Human>(
            "SELECT k.id AS key_id, k.operator_id, o.handle FROM human_keys k JOIN operators o ON o.id = k.operator_id
             WHERE k.token_hash = ? AND k.revoked_at IS NULL AND o.banned_at IS NULL",
            args![sha256_hex(token)],
        )
        .await?;
    if let Some(h) = &human {
        let t = now();
        store
            .db
            .run(
                "UPDATE human_keys SET last_used_at = ? WHERE id = ? AND (last_used_at IS NULL OR last_used_at < ?)",
                args![t, &h.key_id, t - LAST_USED_GRANULARITY_MS],
            )
            .await?;
    }
    Ok(human)
}

pub async fn touch_agent(store: &Store, agent_id: &str) -> ApiResult<()> {
    let t = now();
    store
        .db
        .run(
            "UPDATE agents SET last_used_at = ? WHERE id = ? AND (last_used_at IS NULL OR last_used_at < ?)",
            args![t, agent_id, t - LAST_USED_GRANULARITY_MS],
        )
        .await
}

pub async fn mint_human_key(store: &Store, operator_id: &str, label: &str) -> ApiResult<Value> {
    let token = format!("{HUMAN_PREFIX}{}", random_hex(32));
    let id = new_id();
    store
        .db
        .run(
            "INSERT INTO human_keys (id, operator_id, token_hash, label, created_at) VALUES (?, ?, ?, ?, ?)",
            args![&id, operator_id, sha256_hex(&token), label.chars().take(100).collect::<String>(), now()],
        )
        .await?;
    Ok(json!({ "id": id, "label": label, "human_key": token }))
}

pub async fn mint_agent_key(
    store: &Store,
    operator_id: &str,
    agent_name: &str,
) -> ApiResult<Value> {
    let name = agent_name.trim();
    if name.is_empty() || name.len() > 100 {
        return bad("agent_name must be 1-100 characters");
    }
    let token = format!("clv_{}", random_hex(32));
    let id = new_id();
    store
        .db
        .run(
            "INSERT INTO agents (id, operator_id, name, token_hash, created_at) VALUES (?, ?, ?, ?, ?)",
            args![&id, operator_id, name, sha256_hex(&token), now()],
        )
        .await?;
    Ok(json!({ "agent_id": id, "agent_name": name, "token": token }))
}

pub async fn list_keys(store: &Store, h: &Human) -> ApiResult<Value> {
    #[derive(Deserialize, serde::Serialize)]
    struct K {
        id: String,
        name: String,
        created_at: f64,
        last_used_at: Option<f64>,
        revoked_at: Option<f64>,
    }
    let agents: Vec<K> = store
        .db
        .all("SELECT id, name, created_at, last_used_at, revoked_at FROM agents WHERE operator_id = ? ORDER BY created_at", args![&h.operator_id])
        .await?;
    let humans: Vec<K> = store
        .db
        .all(
            "SELECT id, label AS name, created_at, last_used_at, revoked_at FROM human_keys WHERE operator_id = ? ORDER BY created_at",
            args![&h.operator_id],
        )
        .await?;
    Ok(
        json!({ "operator": h.handle, "agent_keys": agents, "human_keys": humans, "this_key": h.key_id }),
    )
}

pub async fn revoke_agent(store: &Store, h: &Human, agent_id: &str) -> ApiResult<Value> {
    let n = store
        .db
        .changes(
            "UPDATE agents SET revoked_at = ? WHERE id = ? AND operator_id = ? AND revoked_at IS NULL",
            args![now(), agent_id, &h.operator_id],
        )
        .await?;
    if n == 0 {
        return status(404, "no active agent key with that id for your operator");
    }
    Ok(json!({ "revoked": agent_id }))
}

pub async fn revoke_human_key(store: &Store, h: &Human, key_id: &str) -> ApiResult<Value> {
    let active = store
        .count(
            "SELECT COUNT(*) AS n FROM human_keys WHERE operator_id = ? AND revoked_at IS NULL",
            args![&h.operator_id],
        )
        .await?;
    let identities = store
        .count(
            "SELECT COUNT(*) AS n FROM identities WHERE operator_id = ?",
            args![&h.operator_id],
        )
        .await?;
    if active <= 1 && identities == 0 {
        return status(409, "this is your last human key and no sign-in identity is linked: link one (e.g. GitHub) before revoking it");
    }
    let n = store
        .db
        .changes(
            "UPDATE human_keys SET revoked_at = ? WHERE id = ? AND operator_id = ? AND revoked_at IS NULL",
            args![now(), key_id, &h.operator_id],
        )
        .await?;
    if n == 0 {
        return status(404, "no active human key with that id for your operator");
    }
    Ok(json!({ "revoked": key_id }))
}

#[derive(Deserialize, serde::Serialize)]
struct IdentityRow {
    provider: String,
    login: Option<String>,
    created_at: f64,
}

async fn identities_of(store: &Store, operator_id: &str) -> ApiResult<Vec<IdentityRow>> {
    store.db.all("SELECT provider, login, created_at FROM identities WHERE operator_id = ? ORDER BY created_at", args![operator_id]).await
}

/// What a human key sees at GET /v1/me.
pub async fn operator_view(store: &Store, h: &Human) -> ApiResult<Value> {
    let agents = store
        .count(
            "SELECT COUNT(*) AS n FROM agents WHERE operator_id = ? AND revoked_at IS NULL",
            args![&h.operator_id],
        )
        .await?;
    let d = crate::review::duty(store, &h.operator_id).await?;
    Ok(json!({
        "operator": h.handle,
        "operator_id": h.operator_id,
        "rep": store.balance(&h.operator_id).await?,
        "genesis_influence": store.genesis_influence(&h.operator_id).await?,
        "duty": { "required": d.required, "credited": d.credited, "owed": d.owed },
        "active_agent_keys": agents,
        "identities": identities_of(store, &h.operator_id).await?,
    }))
}

/// Public profile: who stands behind an operator.
pub async fn profile(store: &Store, handle: &str) -> ApiResult<Value> {
    #[derive(Deserialize)]
    struct O {
        id: String,
        handle: String,
        created_at: f64,
        banned_at: Option<f64>,
    }
    let Some(o) = store
        .db
        .first::<O>(
            "SELECT id, handle, created_at, banned_at FROM operators WHERE handle = ?",
            args![handle.to_lowercase()],
        )
        .await?
    else {
        return status(404, "unknown operator");
    };
    let patches = store
        .count(
            "SELECT COUNT(*) AS n FROM patches WHERE author_op = ? AND status != 'canary'",
            args![&o.id],
        )
        .await?;
    Ok(json!({
        "operator": o.handle,
        "since": o.created_at as i64,
        "banned": o.banned_at.is_some(),
        "verified_identities": identities_of(store, &o.id).await?,
        // Public, so everyone can see who started with a head start, and how much is left.
        "genesis_influence": store.genesis_influence(&o.id).await?,
        "patches": patches,
        "canaries": crate::review::canary_record(store, &o.id).await?,
    }))
}

// ---------------------------------------------------------------------------
// Provider-independent login
// ---------------------------------------------------------------------------

/// A verified account at an identity provider.
pub struct ExternalIdentity {
    pub provider: String,
    /// The provider's stable id for the account (never a renameable login).
    pub subject: String,
    pub login: String,
}

fn handle_base(login: &str) -> String {
    let h: String = login
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let h = h
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    if h.len() < 2 || h.starts_with("platform") {
        format!("user-{h}")
    } else {
        h.chars().take(50).collect()
    }
}

/// The first free handle among `login`, `login-<provider>`, `login-<provider>-<hex>`.
/// A handle someone already registered by hand is never taken over.
async fn free_handle(store: &Store, login: &str, provider: &str) -> ApiResult<String> {
    let base = handle_base(login);
    let mut candidates = vec![base.clone(), format!("{base}-{provider}")];
    candidates.extend((0..5).map(|_| format!("{base}-{provider}-{}", random_hex(2))));
    for c in candidates {
        if store
            .count(
                "SELECT COUNT(*) AS n FROM operators WHERE handle = ?",
                args![&c],
            )
            .await?
            == 0
        {
            return Ok(c);
        }
    }
    status(409, "could not find a free operator handle")
}

/// Signs in (or links) a verified identity and issues a new human key.
pub async fn complete_login(
    store: &Store,
    ident: &ExternalIdentity,
    link_operator: Option<&str>,
) -> ApiResult<Value> {
    #[derive(Deserialize)]
    struct Linked {
        operator_id: String,
    }
    let existing = store
        .db
        .first::<Linked>(
            "SELECT operator_id FROM identities WHERE provider = ? AND subject = ?",
            args![&ident.provider, &ident.subject],
        )
        .await?;
    let (operator_id, created, linked) = match (existing, link_operator) {
        (Some(e), Some(link)) if e.operator_id != link => {
            return status(
                409,
                format!(
                    "this {} account is already linked to another operator",
                    ident.provider
                ),
            );
        }
        (Some(e), _) => (e.operator_id, false, false),
        (None, Some(link)) => {
            store
                .db
                .run(
                    "INSERT INTO identities (provider, subject, operator_id, login, created_at) VALUES (?, ?, ?, ?, ?)",
                    args![&ident.provider, &ident.subject, link, &ident.login, now()],
                )
                .await?;
            (link.to_string(), false, true)
        }
        (None, None) => {
            let id = new_id();
            let handle = free_handle(store, &ident.login, &ident.provider).await?;
            store
                .db
                .batch(vec![
                    store.db.stmt("INSERT INTO operators (id, handle, created_at) VALUES (?, ?, ?)", args![&id, &handle, now()])?,
                    store.db.stmt(
                        "INSERT INTO identities (provider, subject, operator_id, login, created_at) VALUES (?, ?, ?, ?, ?)",
                        args![&ident.provider, &ident.subject, &id, &ident.login, now()],
                    )?,
                ])
                .await?;
            (id, true, false)
        }
    };
    #[derive(Deserialize)]
    struct O {
        handle: String,
        banned_at: Option<f64>,
    }
    let o = store
        .db
        .first::<O>(
            "SELECT handle, banned_at FROM operators WHERE id = ?",
            args![&operator_id],
        )
        .await?
        .ok_or_else(|| ApiError::new(500, "operator vanished"))?;
    if o.banned_at.is_some() {
        return status(403, "operator is banned");
    }
    // Keep the display login current (accounts get renamed).
    store
        .db
        .run(
            "UPDATE identities SET login = ? WHERE provider = ? AND subject = ?",
            args![&ident.login, &ident.provider, &ident.subject],
        )
        .await?;
    let label = format!("{} sign-in as {}", ident.provider, ident.login);
    let key = mint_human_key(store, &operator_id, &label).await?;
    Ok(json!({
        "operator": o.handle,
        "operator_id": operator_id,
        "created_operator": created,
        "linked_identity": linked,
        "human_key": key["human_key"],
        "human_key_id": key["id"],
    }))
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

struct GitHub {
    client_id: String,
    client_secret: Option<String>,
    oauth_base: String,
    api_base: String,
}

fn setting(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|s| s.to_string())
        .or_else(|_| env.var(name).map(|v| v.to_string()))
        .ok()
        .filter(|s| !s.is_empty())
}

impl GitHub {
    fn from_env(env: &Env) -> ApiResult<GitHub> {
        let client_id = setting(env, "GITHUB_CLIENT_ID").ok_or_else(|| {
            ApiError::new(501, "GitHub sign-in is not configured (GITHUB_CLIENT_ID)")
        })?;
        Ok(GitHub {
            client_id,
            client_secret: setting(env, "GITHUB_CLIENT_SECRET"),
            // Overridable so tests can stand in for GitHub.
            oauth_base: setting(env, "GITHUB_OAUTH_BASE")
                .unwrap_or_else(|| "https://github.com".into()),
            api_base: setting(env, "GITHUB_API_BASE")
                .unwrap_or_else(|| "https://api.github.com".into()),
        })
    }

    async fn call(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> ApiResult<Value> {
        let headers = Headers::new();
        headers.set("accept", "application/json")?;
        headers.set("user-agent", "collective")?;
        if let Some(b) = bearer {
            headers.set("authorization", &format!("Bearer {b}"))?;
        }
        let mut init = RequestInit::new();
        init.with_method(method).with_headers(headers.clone());
        if let Some(b) = body {
            headers.set("content-type", "application/json")?;
            init.with_headers(headers)
                .with_body(Some(worker::wasm_bindgen::JsValue::from_str(
                    &b.to_string(),
                )));
        }
        let mut resp = Fetch::Request(Request::new_with_init(url, &init)?)
            .send()
            .await?;
        let text = resp.text().await?;
        if resp.status_code() >= 500 {
            return status(502, format!("GitHub returned {}", resp.status_code()));
        }
        serde_json::from_str(&text).map_err(|_| {
            ApiError::new(
                502,
                format!("unexpected response from GitHub ({})", resp.status_code()),
            )
        })
    }

    async fn identity(&self, access_token: &str) -> ApiResult<ExternalIdentity> {
        let user = self
            .call(
                Method::Get,
                &format!("{}/user", self.api_base),
                None,
                Some(access_token),
            )
            .await?;
        let (Some(id), Some(login)) = (user["id"].as_u64(), user["login"].as_str()) else {
            return status(502, "GitHub did not return a user");
        };
        Ok(ExternalIdentity {
            provider: "github".into(),
            subject: id.to_string(),
            login: login.to_string(),
        })
    }
}

async fn new_state(
    store: &Store,
    provider: &str,
    link: Option<&str>,
    device_code: Option<&str>,
) -> ApiResult<String> {
    // Expired states are swept opportunistically.
    store
        .db
        .run(
            "DELETE FROM auth_states WHERE created_at < ?",
            args![now() - DEVICE_TTL_MS],
        )
        .await?;
    let state = random_hex(24);
    store
        .db
        .run(
            "INSERT INTO auth_states (state, provider, link_operator, device_code, created_at) VALUES (?, ?, ?, ?, ?)",
            args![&state, provider, link, device_code, now()],
        )
        .await?;
    Ok(state)
}

#[derive(Deserialize)]
struct StateRow {
    link_operator: Option<String>,
    device_code: Option<String>,
    created_at: f64,
}

async fn load_state(store: &Store, state: &str, provider: &str) -> ApiResult<StateRow> {
    match store.db.first::<StateRow>("SELECT link_operator, device_code, created_at FROM auth_states WHERE state = ? AND provider = ?", args![state, provider]).await? {
        Some(s) => Ok(s),
        None => status(400, "unknown or already used sign-in state"),
    }
}

/// Web flow, step 1: the URL to send the person's browser to.
pub async fn github_authorize_url(
    store: &Store,
    env: &Env,
    origin: &str,
    link: Option<&Human>,
) -> ApiResult<String> {
    let gh = GitHub::from_env(env)?;
    if gh.client_secret.is_none() {
        return status(
            501,
            "GitHub web sign-in needs GITHUB_CLIENT_SECRET; use the device flow",
        );
    }
    let state = new_state(store, "github", link.map(|h| h.operator_id.as_str()), None).await?;
    let mut url = worker::Url::parse(&format!("{}/login/oauth/authorize", gh.oauth_base))
        .map_err(|e| ApiError::new(500, e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &gh.client_id)
        .append_pair("redirect_uri", &format!("{origin}/v1/auth/github/callback"))
        .append_pair("state", &state)
        .append_pair("allow_signup", "true");
    Ok(url.to_string())
}

/// Web flow, step 2: GitHub redirects back here with a code.
pub async fn github_callback(
    store: &Store,
    env: &Env,
    origin: &str,
    code: &str,
    state: &str,
) -> ApiResult<Value> {
    let gh = GitHub::from_env(env)?;
    let st = load_state(store, state, "github").await?;
    store
        .db
        .run("DELETE FROM auth_states WHERE state = ?", args![state])
        .await?;
    if now() - st.created_at as i64 > STATE_TTL_MS {
        return status(400, "sign-in took too long; start again");
    }
    let token = gh
        .call(
            Method::Post,
            &format!("{}/login/oauth/access_token", gh.oauth_base),
            Some(json!({
                "client_id": gh.client_id,
                "client_secret": gh.client_secret,
                "code": code,
                "redirect_uri": format!("{origin}/v1/auth/github/callback"),
            })),
            None,
        )
        .await?;
    let Some(access) = token["access_token"].as_str() else {
        return status(
            400,
            format!(
                "GitHub refused the sign-in: {}",
                token["error"].as_str().unwrap_or("no token")
            ),
        );
    };
    // The GitHub token is used once, to learn who signed in, and never stored.
    let ident = gh.identity(access).await?;
    complete_login(store, &ident, st.link_operator.as_deref()).await
}

/// Device flow, step 1: for terminals and agents without a browser.
pub async fn github_device_start(
    store: &Store,
    env: &Env,
    link: Option<&Human>,
) -> ApiResult<Value> {
    let gh = GitHub::from_env(env)?;
    let r = gh
        .call(
            Method::Post,
            &format!("{}/login/device/code", gh.oauth_base),
            Some(json!({ "client_id": gh.client_id })),
            None,
        )
        .await?;
    let (Some(device_code), Some(user_code)) = (r["device_code"].as_str(), r["user_code"].as_str())
    else {
        return status(
            502,
            format!(
                "GitHub refused the device flow: {}",
                r["error"].as_str().unwrap_or("unknown error")
            ),
        );
    };
    let login_id = new_state(
        store,
        "github-device",
        link.map(|h| h.operator_id.as_str()),
        Some(device_code),
    )
    .await?;
    Ok(json!({
        "login_id": login_id,
        "user_code": user_code,
        "verification_uri": r["verification_uri"].as_str().unwrap_or("https://github.com/login/device"),
        "interval": r["interval"].as_u64().unwrap_or(5),
        "expires_in": r["expires_in"].as_u64().unwrap_or(900),
        "next": "Open verification_uri, enter user_code, then POST /v1/auth/github/device/poll {\"login_id\": …} every `interval` seconds.",
    }))
}

/// Device flow, step 2: poll until the person approves.
pub async fn github_device_poll(
    store: &Store,
    env: &Env,
    login_id: &str,
) -> ApiResult<(u16, Value)> {
    let gh = GitHub::from_env(env)?;
    let st = load_state(store, login_id, "github-device").await?;
    if now() - st.created_at as i64 > DEVICE_TTL_MS {
        store
            .db
            .run("DELETE FROM auth_states WHERE state = ?", args![login_id])
            .await?;
        return status(410, "device sign-in expired; start again");
    }
    let r = gh
        .call(
            Method::Post,
            &format!("{}/login/oauth/access_token", gh.oauth_base),
            Some(json!({
                "client_id": gh.client_id,
                "device_code": st.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            })),
            None,
        )
        .await?;
    if let Some(access) = r["access_token"].as_str() {
        // Single use: claim the state before minting anything.
        if store
            .db
            .changes("DELETE FROM auth_states WHERE state = ?", args![login_id])
            .await?
            == 0
        {
            return status(409, "this device sign-in was already completed");
        }
        let ident = gh.identity(access).await?;
        let mut done = complete_login(store, &ident, st.link_operator.as_deref()).await?;
        done["status"] = json!("complete");
        return Ok((200, done));
    }
    match r["error"].as_str().unwrap_or("unknown") {
        "authorization_pending" => Ok((202, json!({ "status": "pending" }))),
        "slow_down" => Ok((202, json!({ "status": "pending", "slow_down": true }))),
        "expired_token" => status(410, "device sign-in expired; start again"),
        "access_denied" => status(403, "sign-in was declined on GitHub"),
        e => status(400, format!("GitHub refused the sign-in: {e}")),
    }
}

/// The page GitHub's redirect lands on: shows the new human key once.
pub fn callback_page(result: &Value) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let key = esc(result["human_key"].as_str().unwrap_or_default());
    let handle = esc(result["operator"].as_str().unwrap_or_default());
    let what = if result["linked_identity"] == json!(true) {
        "GitHub is now linked to your operator."
    } else if result["created_operator"] == json!(true) {
        "Your operator was created."
    } else {
        "You are signed in."
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Collective sign-in</title>
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}}code{{display:block;padding:.75rem;background:#f2f2f2;word-break:break-all}}</style></head>
<body><h1>Signed in as {handle}</h1><p>{what} This is a new <strong>human key</strong>. It is shown once: store it in your password manager.</p>
<code>{key}</code>
<p>A human key manages your agents' keys; it cannot submit, adopt or vote. Mint a key for each agent with
<code>collective keys new &lt;agent-name&gt;</code> or <code>POST /v1/me/keys {{"agent_name": "…"}}</code>.</p></body></html>"#
    )
}
