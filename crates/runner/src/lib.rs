//! A proof runner on Cloudflare: a participant, not part of the platform.
//!
//! Every minute it asks a Collective server for a proof job with each of
//! its agent keys (`POST /v1/proofs/next`), runs the job in a container of
//! its own (crates/prover, one container per run, destroyed afterwards) and
//! submits the sandbox's report. It has no access to the server's database:
//! it uses the same public API as `collective prove`, and its reports count
//! as one operator's claim towards a quorum (collective_core::proof).
//!
//! Configuration: `COLLECTIVE_URL` (the server), and the secret
//! `COLLECTIVE_TOKEN`: one agent key, or several separated by commas, each
//! run under its own operator's name. Never a human key.

use std::time::Duration;

use collective_core::proof::{encode_prove_request, ProofSpec, ProveRequest};
use serde_json::{json, Value};
use worker::wasm_bindgen::JsValue;
use worker::*;

const PROVER_PORT: u16 = 8080;

fn server(env: &Env) -> Result<String> {
    Ok(env
        .var("COLLECTIVE_URL")?
        .to_string()
        .trim_end_matches('/')
        .to_string())
}

/// The agent keys this runner works with. Human keys are refused: runs
/// are agent work.
fn keys(env: &Env) -> Vec<String> {
    env.secret("COLLECTIVE_TOKEN")
        .map(|s| s.to_string())
        .unwrap_or_default()
        .split(',')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty() && !k.starts_with("clv_human_"))
        .collect()
}

async fn api(
    env: &Env,
    key: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Response> {
    let headers = Headers::new();
    headers.set("authorization", &format!("Bearer {key}"))?;
    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers.clone());
    if let Some(b) = body {
        headers.set("content-type", "application/json")?;
        init.with_headers(headers)
            .with_body(Some(JsValue::from_str(&b.to_string())));
    }
    let req = Request::new_with_init(&format!("{}{path}", server(env)?), &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    if resp.status_code() >= 300 {
        return Err(Error::RustError(format!(
            "{path}: HTTP {}: {}",
            resp.status_code(),
            resp.text().await.unwrap_or_default()
        )));
    }
    Ok(resp)
}

/// Claims a job for each key and hands it to a runner object. A key that
/// already holds a job gets the same one back, and its object ignores the
/// repeat.
#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    for (i, key) in keys(&env).iter().enumerate() {
        if let Err(e) = claim(&env, i, key).await {
            console_error!("key {i}: {e}");
        }
    }
}

async fn claim(env: &Env, index: usize, key: &str) -> Result<()> {
    // The prover image has Nix, so this runner takes jobs in Nix
    // environments as well as stock ones.
    let v: Value = api(
        env,
        key,
        Method::Post,
        "/v1/proofs/next?capabilities=nix",
        None,
    )
    .await?
    .json()
    .await?;
    let Some(run) = v["job"]["run_id"].as_str() else {
        return Ok(());
    };
    // One Durable Object, and so one container, per run.
    let stub = env
        .durable_object("RUNNER")?
        .id_from_name(run)?
        .get_stub()?;
    stub.fetch_with_str(&format!("https://runner/start?run={run}&key={index}"))
        .await?;
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
) -> Result<()> {
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
    built.map_err(|e| Error::RustError(format!("container options: {e:?}")))?;
    let raw: &worker::worker_sys::Container = container.as_ref().unchecked_ref();
    raw.start(&opts)
        .map_err(|e| Error::RustError(format!("container start: {e:?}")))
}

/// Runs one job end to end: fetch it and its source from the server, run
/// it in this object's container, and submit the prover's report.
async fn run(state: &State, env: &Env, run_id: &str, key: &str) -> Result<()> {
    let v: Value = api(
        env,
        key,
        Method::Get,
        &format!("/v1/proof-runs/{run_id}"),
        None,
    )
    .await?
    .json()
    .await?;
    let job = &v["job"];
    let spec: ProofSpec = serde_json::from_value(job["spec"].clone())?;
    let source = api(
        env,
        key,
        Method::Get,
        job["source"].as_str().unwrap_or_default(),
        None,
    )
    .await?
    .bytes()
    .await?;
    let body = encode_prove_request(
        &ProveRequest {
            spec: spec.clone(),
            patch: job["diff"].as_str().unwrap_or_default().to_string(),
        },
        &source,
    );

    let container = state
        .container()
        .ok_or_else(|| Error::RustError("no container bound to ProofRunner".into()))?;
    if !container.running() {
        // Proofs with no dependency fetch and no environment to realise
        // never get internet at all.
        let offline = !spec.needs_network();
        let mut vars = vec![("PROVER_BUDGET_SECS", spec.budget_secs().to_string())];
        if offline {
            vars.push(("PROVER_NO_INTERNET", "1".into()));
        }
        start_container(&container, !offline, &vars)?;
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
        return Err(Error::RustError(
            "prover container did not become ready".into(),
        ));
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(JsValue::from(js_sys::Uint8Array::from(
            body.as_slice(),
        ))));
    let mut resp = port.fetch("http://prover/prove", Some(init)).await?;
    let text = resp.text().await?;
    if resp.status_code() != 200 {
        return Err(Error::RustError(format!("prover: {text}")));
    }
    let _ = container.destroy(None).await;
    // The report goes to the server as the prover wrote it: the server
    // decides what it shows.
    let report: Value = serde_json::from_str(&text)?;
    let v: Value = api(
        env,
        key,
        Method::Post,
        &format!("/v1/proof-runs/{run_id}/report"),
        Some(&json!({ "report": report })),
    )
    .await?
    .json()
    .await?;
    console_log!("run {run_id}: {}", v["verdict"]);
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
        let url = req.url()?;
        let arg = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        let (Some(run), Some(key)) = (arg("run"), arg("key")) else {
            return Response::error("run and key required", 400);
        };
        // A run is started once; the scheduler asks again every minute.
        if self.state.storage().get::<String>("run").await?.is_some() {
            return Response::ok("already started");
        }
        self.state.storage().put("run", &run).await?;
        self.state.storage().put("key", &key).await?;
        // Run from an alarm so the scheduler's request returns at once.
        self.state
            .storage()
            .set_alarm(Duration::from_millis(1))
            .await?;
        Response::ok("started")
    }

    async fn alarm(&self) -> Result<Response> {
        let storage = self.state.storage();
        let (Some(run_id), Some(index)) = (
            storage.get::<String>("run").await?,
            storage.get::<String>("key").await?,
        ) else {
            return Response::empty();
        };
        let Some(key) = index
            .parse::<usize>()
            .ok()
            .and_then(|i| keys(&self.env).into_iter().nth(i))
        else {
            console_error!("run {run_id}: no agent key {index}");
            return Response::empty();
        };
        // A failed run reports nothing: the claim lapses and the job is
        // offered to other runners.
        if let Err(e) = run(&self.state, &self.env, &run_id, &key).await {
            console_error!("run {run_id}: {e}");
            if let Some(c) = self.state.container() {
                let _ = c.destroy(Some(&e.to_string())).await;
            }
        }
        Response::empty()
    }
}
