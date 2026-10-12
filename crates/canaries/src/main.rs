//! collective-canaries
//!
//!   generate SELECTION.json REPO_DIR OUT.jsonl     revert each source commit into a canary
//!   verify   SELECTION.json REPO_DIR IN.jsonl OUT.jsonl PROVER_URL
//!                                                  run each canary as a regression proof in the sandbox
//!   upload   IN.jsonl COLLECTIVE_URL               plant verified canaries (needs COLLECTIVE_ADMIN_TOKEN)

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use collective_canaries::{generate, verification_spec, Canary, Selection};
use collective_core::proof::{assess, encode_prove_request, ProofReport, ProveRequest, Verdict};
use collective_core::review::PARAMS;

fn read_jsonl(path: &str) -> Vec<Canary> {
    fs::read_to_string(path)
        .expect("input")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("canary JSON"))
        .collect()
}

fn write_jsonl(path: &str, canaries: &[Canary]) {
    let mut f = fs::File::create(path).expect("output");
    for c in canaries {
        writeln!(f, "{}", serde_json::to_string(c).expect("json")).expect("write");
    }
}

fn selection(path: &str) -> Selection {
    serde_json::from_str(&fs::read_to_string(path).expect("selection")).expect("selection JSON")
}

fn cmd_generate(sel_path: &str, repo: &str, out: &str) {
    let sel = selection(sel_path);
    let mut canaries = Vec::new();
    for src in &sel.sources {
        match generate(Path::new(repo), &sel, src, PARAMS.max_task_lines) {
            Ok(c) => {
                eprintln!(
                    "ok    {} {:?}: {} test(s), {} planted location(s), cover \"{}\"",
                    src.commit,
                    src.kind,
                    c.tests.len(),
                    c.planted.len(),
                    c.intent
                );
                canaries.push(c);
            }
            Err(e) => eprintln!("skip  {} {:?}: {e}", src.commit, src.kind),
        }
    }
    write_jsonl(out, &canaries);
    eprintln!("{} canaries written to {out}", canaries.len());
}

fn cmd_verify(sel_path: &str, repo: &str, input: &str, out: &str, prover: &str) {
    let sel = selection(sel_path);
    let mut canaries = read_jsonl(input);
    for i in 0..canaries.len() {
        let c = &mut canaries[i];
        let tgz = Command::new("git")
            .args([
                "archive",
                "--format=tar.gz",
                "--prefix=src/",
                &c.base_commit,
            ])
            .current_dir(repo)
            .output()
            .expect("git archive")
            .stdout;
        let spec = verification_spec(&sel, c);
        let body = encode_prove_request(
            &ProveRequest {
                spec: spec.clone(),
                patch: c.diff.clone(),
            },
            &tgz,
        );
        let started = std::time::Instant::now();
        let resp = ureq::post(&format!("{prover}/prove"))
            .timeout(std::time::Duration::from_secs(
                spec.budget_secs() as u64 + 60,
            ))
            .send_bytes(&body);
        let assessment = match resp
            .map_err(|e| e.to_string())
            .and_then(|r| r.into_json::<ProofReport>().map_err(|e| e.to_string()))
        {
            Ok(report) => assess(&spec, &report),
            Err(e) => collective_core::proof::Assessment {
                verdict: Verdict::Inconclusive,
                reason: format!("prover request failed: {e}"),
            },
        };
        eprintln!(
            "{:?}  {} ({:?}) in {:?}: {}",
            assessment.verdict,
            &c.provenance.source_commit[..10],
            c.provenance.kind,
            started.elapsed(),
            assessment.reason
        );
        c.verified = Some(assessment);
        // Save progress after each canary: verification is slow.
        write_jsonl(out, &canaries);
    }
}

fn cmd_upload(input: &str, url: &str) {
    let token = std::env::var("COLLECTIVE_ADMIN_TOKEN").expect("COLLECTIVE_ADMIN_TOKEN");
    for c in read_jsonl(input) {
        if c.verified.as_ref().map(|a| a.verdict) != Some(Verdict::Proven) {
            eprintln!(
                "skip  {}: not verified as proven",
                &c.provenance.source_commit[..10]
            );
            continue;
        }
        let r = ureq::post(&format!("{url}/v1/admin/canaries"))
            .set("x-admin-token", &token)
            .send_json(serde_json::to_value(&c).expect("json"));
        match r {
            Ok(resp) => eprintln!(
                "plant {}: {}",
                &c.provenance.source_commit[..10],
                resp.into_string().unwrap_or_default()
            ),
            Err(ureq::Error::Status(code, resp)) => eprintln!(
                "error {}: {code} {}",
                &c.provenance.source_commit[..10],
                resp.into_string().unwrap_or_default()
            ),
            Err(e) => eprintln!("error {}: {e}", &c.provenance.source_commit[..10]),
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("generate") if a.len() == 5 => cmd_generate(&a[2], &a[3], &a[4]),
        Some("verify") if a.len() == 7 => cmd_verify(&a[2], &a[3], &a[4], &a[5], &a[6]),
        Some("upload") if a.len() == 4 => cmd_upload(&a[2], &a[3]),
        _ => {
            eprintln!("usage: collective-canaries generate SELECTION REPO OUT | verify SELECTION REPO IN OUT PROVER_URL | upload IN COLLECTIVE_URL");
            std::process::exit(2);
        }
    }
}
