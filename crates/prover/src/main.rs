//! collective-prover
//!
//!   collective-prover serve [PORT]                     HTTP API (default 8080), one proof at a time
//!   collective-prover run SPEC.json SOURCE.tar.gz PATCH.diff
//!
//! The HTTP API: `GET /health`; `POST /prove` (request body, report in the
//! response); and the same proof in two steps for hosts with short request
//! limits, `POST /prove/start` (202) then `GET /prove/result` (202 while it
//! runs, 200 with the report).
//!
//! Environment: PROVER_NO_INTERNET=1 (host runs us without internet),
//! PROVER_ALLOW_UNISOLATED=1 (development only), PROVER_BUDGET_SECS,
//! PROVER_NIX_BINARY (default: `nix` on PATH).

use std::io::Read;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};

use collective_core::proof::{assess, ProofSpec};
use collective_prover::{
    decode_request, detect_isolation, nix_available, prove, Isolation, Options, VERSION,
};
use tiny_http::{Header, Method, Response, Server};

const MAX_BODY: u64 = 512 * 1024 * 1024;

fn json_header() -> Header {
    Header::from_bytes("content-type", "application/json").expect("static header")
}

/// The one proof this process runs. `POST /prove` runs it inline and answers
/// with the report; `POST /prove/start` answers at once and runs it on a
/// thread, and `GET /prove/result` says 202 until the report is ready. The
/// second form is for hosts whose own requests are limited in wall time,
/// such as a Durable Object alarm (15 minutes), while a proof with a build
/// step and a Nix environment may take an hour or more.
enum Slot {
    Idle,
    Running,
    Done(String),
}

fn lock(s: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn serve(port: u16) {
    let server = Server::http(("0.0.0.0", port)).expect("bind");
    eprintln!("collective-prover {VERSION} listening on {port}");
    let slot = Arc::new(Mutex::new(Slot::Idle));
    let read_body = |req: &mut tiny_http::Request| {
        let mut body = Vec::new();
        req.as_reader()
            .take(MAX_BODY)
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())
            .map(|_| body)
    };
    let bad = |e: String| {
        Response::from_string(serde_json::json!({ "error": e }).to_string())
            .with_status_code(400)
            .with_header(json_header())
    };
    for mut req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or_default().to_string();
        let reply = match (req.method(), path.as_str()) {
            (Method::Post, "/prove/start") => {
                // Take the slot in one lock scope: the thread below locks it
                // again, and a std mutex is not reentrant.
                let taken = {
                    let mut g = lock(&slot);
                    matches!(*g, Slot::Idle) && {
                        *g = Slot::Running;
                        true
                    }
                };
                if !taken {
                    Response::from_string(
                        serde_json::json!({ "error": "this prover already has a proof" })
                            .to_string(),
                    )
                    .with_status_code(409)
                    .with_header(json_header())
                } else {
                    match read_body(&mut req)
                        .and_then(|b| decode_request(&b).map(|(r, source)| (r, source.to_vec())))
                    {
                        Ok((r, source)) => {
                            let slot = Arc::clone(&slot);
                            std::thread::spawn(move || {
                                let report =
                                    prove(&r.spec, &source, &r.patch, &Options::from_env());
                                let json = serde_json::to_string(&report).expect("serializable");
                                *lock(&slot) = Slot::Done(json);
                            });
                            Response::from_string(
                                serde_json::json!({ "started": true }).to_string(),
                            )
                            .with_status_code(202)
                            .with_header(json_header())
                        }
                        Err(e) => {
                            *lock(&slot) = Slot::Idle;
                            bad(e)
                        }
                    }
                }
            }
            (Method::Get, "/prove/result") => match lock(&slot).deref() {
                Slot::Idle => Response::from_string(
                    serde_json::json!({ "error": "no proof was started" }).to_string(),
                )
                .with_status_code(404)
                .with_header(json_header()),
                Slot::Running => {
                    Response::from_string(serde_json::json!({ "running": true }).to_string())
                        .with_status_code(202)
                        .with_header(json_header())
                }
                Slot::Done(json) => Response::from_string(json.clone()).with_header(json_header()),
            },
            (Method::Get, "/health") => {
                let opts = Options::from_env();
                let iso = detect_isolation(&opts);
                let body = serde_json::json!({
                    "ok": true,
                    "version": VERSION,
                    "network_isolation": iso != Isolation::None,
                    "nix": nix_available(&opts),
                });
                Response::from_string(body.to_string()).with_header(json_header())
            }
            (Method::Post, "/prove") => match read_body(&mut req) {
                Ok(body) => match decode_request(&body) {
                    Ok((r, source)) => {
                        let report = prove(&r.spec, source, &r.patch, &Options::from_env());
                        Response::from_string(serde_json::to_string(&report).expect("serializable"))
                            .with_header(json_header())
                    }
                    Err(e) => bad(e),
                },
                Err(e) => bad(e),
            },
            _ => Response::from_string("not found").with_status_code(404),
        };
        let _ = req.respond(reply);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") | None => serve(args.get(2).and_then(|p| p.parse().ok()).unwrap_or(8080)),
        Some("run") if args.len() == 5 => {
            let spec: ProofSpec =
                serde_json::from_str(&std::fs::read_to_string(&args[2]).expect("spec"))
                    .expect("spec JSON");
            let source = std::fs::read(&args[3]).expect("source");
            let patch = std::fs::read_to_string(&args[4]).expect("patch");
            let report = prove(&spec, &source, &patch, &Options::from_env());
            let assessment = assess(&spec, &report);
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "assessment": assessment, "report": report })
                )
                .expect("json")
            );
        }
        _ => {
            eprintln!(
                "usage: collective-prover serve [PORT] | run SPEC.json SOURCE.tar.gz PATCH.diff"
            );
            std::process::exit(2);
        }
    }
}
