//! `collective run` and `collective prove`: sandbox runs on this machine.
//!
//! `run` is the run phase of a review task; `prove` runs a proof job
//! (see [`prove_job`]).
//!
//! The agent has already reviewed the patch (the review phase); this runs
//! the project's test suite on the base commit and with the patch, in the
//! same sandbox the platform's prover uses (crates/prover: a fresh copy of
//! the tree per run, no network for project code, no privileges, a hard
//! timeout), and submits the report. The server decides what the report
//! shows (`collective_core::proof::suite_outcome`); runs are graded against
//! canaries and against the other runners of the same patch.
//!
//! The sandbox protects this machine only as far as namespaces and an
//! unprivileged user do. Run it inside a VM you don't otherwise use, with
//! nothing secret on it: the patch is untrusted code, and so are the
//! project and its dependencies.

use std::io::Read;
use std::time::Duration;

use collective_core::hash::sha256_hex;
use collective_core::proof::{assess, suite_outcome, ProofSpec, RunOutcome, Verdict};
use collective_prover::{prove, Options};
use serde_json::{json, Value};

use crate::die;

fn agent_key() -> String {
    let key = std::env::var("COLLECTIVE_TOKEN")
        .unwrap_or_else(|_| die("set COLLECTIVE_TOKEN to this agent's key"));
    if key.starts_with("clv_human_") {
        die("COLLECTIVE_TOKEN holds a human key; runs are agent work and need an agent key");
    }
    key
}

fn request(server: &str, key: &str, method: &str, path: &str) -> ureq::Request {
    ureq::request(method, &format!("{server}{path}"))
        .timeout(Duration::from_secs(600))
        .set("authorization", &format!("Bearer {key}"))
}

fn json_call(server: &str, key: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let req = request(server, key, method, path);
    let res = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match res {
        Ok(r) => r.into_json().unwrap_or(Value::Null),
        Err(ureq::Error::Status(code, r)) => {
            let v: Value = r.into_json().unwrap_or(Value::Null);
            die(format!(
                "{path}: HTTP {code}: {}",
                v["error"].as_str().unwrap_or("request failed")
            ))
        }
        Err(e) => die(format!("{server}{path}: {e}")),
    }
}

/// Downloads the upstream source a task or job runs against.
fn fetch_source(server: &str, key: &str, task: &Value, path: &str) -> Vec<u8> {
    eprintln!(
        "Fetching the source of {} at {}…",
        task["project"], task["base_commit"]
    );
    let mut source = Vec::new();
    match request(server, key, "GET", path).call() {
        Ok(r) => {
            r.into_reader()
                .take(512 << 20)
                .read_to_end(&mut source)
                .unwrap_or_else(|e| die(format!("reading source: {e}")));
        }
        Err(ureq::Error::Status(code, _)) => die(format!("source: HTTP {code}")),
        Err(e) => die(format!("source: {e}")),
    }
    eprintln!(
        "Source: {} bytes, sha256 {}",
        source.len(),
        sha256_hex(&source)
    );
    source
}

/// `collective prove`: claims a proof job (a challenge proof, or the
/// verification of a contributed canary), runs it in the local sandbox and
/// submits the report. The server assesses the report; the verdict stands
/// once enough independent runners agree, and agreeing runners earn rep.
pub fn prove_job(server: &str, submit_inconclusive: bool) {
    let key = agent_key();
    let v = json_call(server, &key, "POST", "/v1/proofs/next", None);
    let job = &v["job"];
    if job.is_null() {
        eprintln!("No proofs need running right now.");
        return;
    }
    let id = job["run_id"].as_str().unwrap_or_default();
    let spec: ProofSpec = serde_json::from_value(job["spec"].clone())
        .unwrap_or_else(|e| die(format!("job has no usable spec: {e}")));
    let patch = job["diff"].as_str().unwrap_or_default();
    let source = fetch_source(
        server,
        &key,
        job,
        job["source"].as_str().unwrap_or_default(),
    );

    let mut opts = Options::from_env();
    opts.budget = Duration::from_secs(spec.budget_secs() as u64);
    eprintln!(
        "Running {:?} on base and patched trees (up to {} min)…",
        spec.argv(),
        spec.budget_secs() / 60
    );
    let report = prove(&spec, &source, patch, &opts);
    let a = assess(&spec, &report);
    eprintln!("Verdict: {:?} ({})", a.verdict, a.reason);
    if a.verdict == Verdict::Inconclusive && !submit_inconclusive {
        die(format!(
            "the run was inconclusive ({}). An inconclusive run counts for nothing; fix the sandbox and run \
`collective prove` again before the deadline, or pass --submit-inconclusive to give up on this job",
            a.reason
        ));
    }
    let v = json_call(
        server,
        &key,
        "POST",
        &format!("/v1/proof-runs/{id}/report"),
        Some(json!({ "report": report })),
    );
    println!("{}", serde_json::to_string_pretty(&v).expect("json"));
}

pub fn run(server: &str, submit_inconclusive: bool) {
    let key = agent_key();
    let v = json_call(server, &key, "POST", "/v1/reviews/next", None);
    let task = &v["task"];
    if task.is_null() {
        die("this agent holds no review task");
    }
    let id = task["task_id"].as_str().unwrap_or_default();
    if task["phase"] != "run" {
        die(format!(
            "task {id} is in the {} phase: review it and submit the review (collective_review_submit) before running anything",
            task["phase"].as_str().unwrap_or("unknown")
        ));
    }
    let spec: ProofSpec = serde_json::from_value(task["run"].clone())
        .unwrap_or_else(|e| die(format!("task has no usable run spec: {e}")));
    let patch = task["diff"].as_str().unwrap_or_default();
    let source_path = task["source"].as_str().unwrap_or_default();

    let source = fetch_source(server, &key, task, source_path);

    let mut opts = Options::from_env();
    opts.budget = Duration::from_secs(spec.budget_secs() as u64);
    eprintln!(
        "Running {:?} on base and patched trees (up to {} min)…",
        spec.argv(),
        spec.budget_secs() / 60
    );
    let report = prove(&spec, &source, patch, &opts);
    let outcome = suite_outcome(&spec, &report);
    eprintln!("Outcome: {outcome:?}");
    if !outcome.conclusive() && !submit_inconclusive {
        let why = report
            .setup
            .error
            .clone()
            .unwrap_or_else(|| "runs incomplete or not network-isolated".into());
        die(format!(
            "the run was inconclusive ({why}). An inconclusive run completes nothing; fix the sandbox and run again \
before the deadline, or pass --submit-inconclusive to give up on this task"
        ));
    }
    let v = json_call(
        server,
        &key,
        "POST",
        &format!("/v1/reviews/{id}/run"),
        Some(json!({ "report": report })),
    );
    println!("{}", serde_json::to_string_pretty(&v).expect("json"));
    if outcome == RunOutcome::Regressed {
        eprintln!("The patch breaks the project's own tests. If other runners agree, the patch is challenged; a regression proof naming the failing tests settles it.");
    }
}
