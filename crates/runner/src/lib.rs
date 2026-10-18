//! A proof runner on Cloudflare: a participant, not part of the platform.
//!
//! Every minute it asks a Collective server for a proof job with each of
//! its agent keys (`POST /v1/proofs/next`), runs the job in a container of
//! its own (crates/prover, one container per run, destroyed afterwards) and
//! submits the sandbox's report. The run is driven by short Durable Object
//! alarms (start, then a poll every 30 seconds), because one alarm may not
//! last more than 15 minutes and a proof may take an hour; the container's
//! inactivity timeout is set at every step, or Cloudflare would stop it
//! between two alarms. It has no access to the server's database:
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

/// What an alarm step asks for next: another alarm after a wait, or nothing
/// because the run is over (reported, or given up).
enum Step {
    Again(Duration),
    Done,
}

/// How often a running proof is asked for its result. Each poll is its own
/// alarm, so no event lasts longer than the prover's health wait, well
/// under the 15-minute wall-time limit of a Durable Object event.
const POLL: Duration = Duration::from_secs(30);

/// Keeps the container running between alarms. Cloudflare stops a container
/// shortly after its Durable Object goes idle, which it does between two
/// polls, unless an inactivity timeout is set; and the timeout does not
/// survive a Durable Object restart, so every step sets it again.
async fn keep_alive(container: &Container, secs: f64) -> Result<()> {
    use worker::wasm_bindgen::JsCast;
    let raw: &worker::worker_sys::Container = container.as_ref().unchecked_ref();
    let this: &JsValue = raw.as_ref();
    let err = |e: JsValue| Error::RustError(format!("setInactivityTimeout: {e:?}"));
    let f: js_sys::Function =
        js_sys::Reflect::get(this, &JsValue::from_str("setInactivityTimeout"))
            .map_err(err)?
            .dyn_into()
            .map_err(|_| Error::RustError("setInactivityTimeout is not a function".into()))?;
    // At most 6 hours, the platform's limit.
    let ms = secs.clamp(60.0, 6.0 * 3600.0) * 1000.0;
    let promise = f.call1(this, &JsValue::from_f64(ms)).map_err(err)?;
    worker::wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map_err(err)?;
    Ok(())
}

/// Fetches the job and its source from the server, starts the container
/// and hands the prover the proof to run on its own thread.
async fn start(state: &State, env: &Env, run_id: &str, key: &str) -> Result<Step> {
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
    let budget = spec.budget_secs();
    if !container.running() {
        // Proofs with no dependency fetch and no environment to realise
        // never get internet at all.
        let offline = !spec.needs_network();
        let mut vars = vec![("PROVER_BUDGET_SECS", budget.to_string())];
        if offline {
            vars.push(("PROVER_NO_INTERNET", "1".into()));
        }
        start_container(&container, !offline, &vars)?;
    }
    keep_alive(&container, budget as f64 + 120.0).await?;
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
    let mut resp = port.fetch("http://prover/prove/start", Some(init)).await?;
    if resp.status_code() != 202 {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::RustError(format!("prover: {text}")));
    }
    // The prover has its own budget; past it plus a grace period, it is
    // not coming back.
    let deadline = Date::now().as_millis() as f64 + (budget as f64 + 120.0) * 1000.0;
    let storage = state.storage();
    storage.put("deadline", deadline).await?;
    storage.put("phase", "poll").await?;
    Ok(Step::Again(POLL))
}

/// Asks the prover for its report, and submits it when it is there.
async fn poll(state: &State, env: &Env, run_id: &str, key: &str) -> Result<Step> {
    let deadline: f64 = state.storage().get("deadline").await?.unwrap_or(0.0);
    if Date::now().as_millis() as f64 > deadline {
        return Err(Error::RustError(
            "prover did not finish within its budget".into(),
        ));
    }
    let container = state
        .container()
        .ok_or_else(|| Error::RustError("no container bound to ProofRunner".into()))?;
    if !container.running() {
        return Err(Error::RustError(
            "prover container stopped before reporting".into(),
        ));
    }
    keep_alive(
        &container,
        (deadline - Date::now().as_millis() as f64) / 1000.0,
    )
    .await?;
    let port = container.get_tcp_port(PROVER_PORT)?;
    let mut resp = port.fetch("http://prover/prove/result", None).await?;
    let text = resp.text().await?;
    match resp.status_code() {
        202 => return Ok(Step::Again(POLL)),
        200 => {}
        s => return Err(Error::RustError(format!("prover: HTTP {s}: {text}"))),
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
    console_log!("run {run_id}: {} ({})", v["verdict"], v["reason"]);
    Ok(Step::Done)
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

    /// One short step per alarm: start the proof, then poll for its result
    /// every `POLL` until it is reported or given up. A failed run reports
    /// nothing: the claim lapses and the job is offered to other runners.
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
            self.finish(Some("no agent key")).await;
            return Response::empty();
        };
        let polling = storage.get::<String>("phase").await?.as_deref() == Some("poll");
        let step = if polling {
            poll(&self.state, &self.env, &run_id, &key).await
        } else {
            start(&self.state, &self.env, &run_id, &key).await
        };
        match step {
            Ok(Step::Again(wait)) => storage.set_alarm(wait).await?,
            Ok(Step::Done) => self.finish(None).await,
            Err(e) => {
                console_error!("run {run_id}: {e}");
                self.finish(Some(&e.to_string())).await;
            }
        }
        Response::empty()
    }
}

impl ProofRunner {
    /// Ends this object's run: the container goes, and so does the state
    /// that would make the next alarm do anything.
    async fn finish(&self, error: Option<&str>) {
        if let Some(c) = self.state.container() {
            let _ = c.destroy(error).await;
        }
        let _ = self.state.storage().delete_all().await;
    }
}
