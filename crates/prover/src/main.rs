//! collective-prover
//!
//!   collective-prover serve [PORT]                     HTTP API (default 8080), one proof at a time
//!   collective-prover run SPEC.json SOURCE.tar.gz PATCH.diff
//!
//! Environment: PROVER_NO_INTERNET=1 (host runs us without internet),
//! PROVER_ALLOW_UNISOLATED=1 (development only), PROVER_BUDGET_SECS,
//! PROVER_NIX_BINARY (default: `nix` on PATH).

use std::io::Read;

use collective_core::proof::{assess, ProofSpec};
use collective_prover::{
    decode_request, detect_isolation, nix_available, prove, Isolation, Options, VERSION,
};
use tiny_http::{Header, Method, Response, Server};

const MAX_BODY: u64 = 512 * 1024 * 1024;

fn json_header() -> Header {
    Header::from_bytes("content-type", "application/json").expect("static header")
}

fn serve(port: u16) {
    let server = Server::http(("0.0.0.0", port)).expect("bind");
    eprintln!("collective-prover {VERSION} listening on {port}");
    for mut req in server.incoming_requests() {
        let reply = match (req.method(), req.url()) {
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
            (Method::Post, "/prove") => {
                let mut body = Vec::new();
                match req
                    .as_reader()
                    .take(MAX_BODY)
                    .read_to_end(&mut body)
                    .map_err(|e| e.to_string())
                    .and_then(|_| decode_request(&body))
                {
                    Ok((r, source)) => {
                        let report = prove(&r.spec, source, &r.patch, &Options::from_env());
                        Response::from_string(serde_json::to_string(&report).expect("serializable"))
                            .with_header(json_header())
                    }
                    Err(e) => Response::from_string(serde_json::json!({ "error": e }).to_string())
                        .with_status_code(400)
                        .with_header(json_header()),
                }
            }
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
