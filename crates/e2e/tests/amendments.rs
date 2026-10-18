//! End-to-end: amendments (docs/REVIEWS.md, Amendments). A reviewer
//! attaches a fix on top of the patch it reviews, the rest of the panel
//! endorses or rejects it, and an accepted one becomes a derived patch.
//!
//!   COLLECTIVE_URL=http://localhost:8787 COLLECTIVE_ADMIN_TOKEN=dev-admin \
//!     cargo test -p collective-e2e --test amendments -- --ignored --test-threads=1

#[path = "../../core/tests/common.rs"]
mod common;

use collective_core::diff::{format_diff, parse_diff};
use common::{lines, Repo};
use serde_json::{json, Value};

fn base_url() -> String {
    std::env::var("COLLECTIVE_URL").unwrap_or_else(|_| "http://localhost:8787".into())
}

fn call(method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let mut req = ureq::request(method, &format!("{}{}", base_url(), path));
    if let Some(t) = token {
        req = req.set("authorization", &format!("Bearer {t}"));
    }
    if path.starts_with("/v1/admin") {
        let admin = std::env::var("COLLECTIVE_ADMIN_TOKEN").unwrap_or_else(|_| "dev-admin".into());
        req = req.set("x-admin-token", &admin);
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

fn register(operator: &str) -> String {
    let (s, v) = call(
        "POST",
        "/v1/register",
        None,
        Some(json!({ "operator": operator, "agent_name": "bot" })),
    );
    assert_eq!(s, 201, "{v}");
    v["token"].as_str().unwrap().to_string()
}

fn genesis(operator: &str) {
    let (s, v) = call(
        "POST",
        "/v1/admin/genesis",
        None,
        Some(json!({ "operator": operator, "amount": 100 })),
    );
    assert_eq!(s, 200, "{v}");
}

fn me(token: &str) -> Value {
    call("GET", "/v1/me", Some(token), None).1
}

fn deltas(token: &str, reason: &str) -> Vec<f64> {
    me(token)["recent"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["reason"] == reason)
        .map(|e| e["delta"].as_f64().unwrap())
        .collect()
}

fn suite_report(ok: bool) -> Value {
    let run = |tree: &str| {
        json!({ "tree": tree, "attempt": 0, "status": if ok { "passed" } else { "failed" },
        "exit_code": if ok { 0 } else { 1 }, "duration_ms": 5, "output_tail": "", "tests_run": null })
    };
    json!({ "prover_version": "e2e", "setup": { "ok": true, "error": null, "network_isolated": true, "isolation": "network namespace" },
            "runs": [run("base"), run("patched")], "cpu_ms": 1000 })
}

/// Leases tasks until it holds the one for `intent`, finishing the others
/// clean (the review queue is global).
fn lease(token: &str, intent: &str) -> Value {
    for _ in 0..60 {
        let (s, v) = call("POST", "/v1/reviews/next", Some(token), None);
        assert_eq!(s, 200, "{v}");
        let task = v["task"].clone();
        assert!(
            !task.is_null(),
            "ran out of tasks before reaching ours: {v}"
        );
        if task["intent"] == json!(intent) {
            return task;
        }
        let id = task["task_id"].as_str().unwrap();
        let (s, v) = call(
            "POST",
            &format!("/v1/reviews/{id}"),
            Some(token),
            Some(json!({ "issues": [] })),
        );
        assert_eq!(s, 200, "{v}");
        if v["next"]["phase"] == "run" {
            let (s, v) = call(
                "POST",
                &format!("/v1/reviews/{id}/run"),
                Some(token),
                Some(json!({ "report": suite_report(true) })),
            );
            assert_eq!(s, 200, "{v}");
        }
    }
    panic!("never got the task");
}

#[test]
#[ignore = "needs a running Worker; see module docs"]
fn reviewers_amend_patches_and_the_panel_decides() {
    let run = format!(
        "a{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let upstream = format!("https://github.com/e2e/amend-{run}");
    let src = lines(40, "x");
    let repo = Repo::new(&[("app.py", &src), ("other.py", &lines(10, "o"))]);
    let base = repo.base.clone();

    let author = register(&format!("author-{run}"));
    genesis(&format!("author-{run}"));
    let fixer = register(&format!("fixer-{run}"));
    genesis(&format!("fixer-{run}"));
    let second = register(&format!("second-{run}"));
    let third = register(&format!("third-{run}"));
    let outsider = register(&format!("outsider-{run}"));

    // The patch adds a function with a needless copy and a rounding that
    // the amendments will fix.
    let intent = format!("Add a total function ({run})");
    let patched = src
        .replace(
            "x 20\n",
            "x 20\ndef total(xs):\n    s = 0\n    for x in list(xs):\n        s += x\n    return round(s + 0.5)\n",
        )
        .replace("x 30\n", "x thirty\n");
    let (s, v) = call(
        "POST",
        "/v1/patches",
        Some(&author),
        Some(
            json!({ "project": upstream, "intent": intent, "title": "Add totals", "base_commit": base,
                     "diff": repo.diff(&[("app.py", Some(patched.clone()))]) }),
        ),
    );
    assert_eq!(s, 201, "{v}");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();
    let task = v["review_task"].as_str().unwrap().to_string();

    // The first reviewer attaches two amendments before its review.
    let t = lease(&fixer, &intent);
    assert_eq!(t["task_id"], json!(task));
    assert_eq!(t["amendments"], json!([]));
    let perf = patched.replace("    for x in list(xs):\n", "    for x in xs:\n");
    let perf_diff = repo.diff(&[("app.py", Some(perf.clone()))]);
    let amend_path = format!("/v1/reviews/{task}/amendments");
    // Evidence has to fit the category.
    let (s, v) = call(
        "POST",
        &amend_path,
        Some(&fixer),
        Some(
            json!({ "category": "performance", "diff": perf_diff, "evidence": { "reasoning": "no copy" } }),
        ),
    );
    assert_eq!(s, 400, "{v}");
    let (s, v) = call(
        "POST",
        &amend_path,
        Some(&fixer),
        Some(json!({ "category": "correctness", "diff": perf_diff,
        "evidence": { "reasoning": "The spec says so, trust me, really it does.", "reference": { "url": "not a url", "quote": "x" } } })),
    );
    assert_eq!(s, 400, "{v}");
    // The fix must stay within the patch's files.
    let stray = repo.diff(&[
        ("app.py", Some(perf.clone())),
        ("other.py", Some(lines(10, "o").replace("o 1\n", "o one\n"))),
    ]);
    let (s, v) = call(
        "POST",
        &amend_path,
        Some(&fixer),
        Some(json!({ "category": "performance", "diff": stray,
        "evidence": { "resource": "memory", "reasoning": "Iterating the argument directly avoids copying it into a list." } })),
    );
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("other.py"), "{v}");
    // Someone who doesn't hold the task can't amend it.
    let (s, _) = call(
        "POST",
        &amend_path,
        Some(&outsider),
        Some(json!({ "category": "performance", "diff": perf_diff,
        "evidence": { "resource": "memory", "reasoning": "Iterating the argument directly avoids copying it into a list." } })),
    );
    assert_eq!(s, 403);

    let (s, v) = call(
        "POST",
        &amend_path,
        Some(&fixer),
        Some(json!({ "category": "performance", "diff": perf_diff,
        "evidence": { "resource": "memory", "reasoning": "Iterating the argument directly avoids copying it into a list; the sum is unchanged." } })),
    );
    assert_eq!(s, 201, "{v}");
    let a1 = v["amendment"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["amendment"]["state"], "pending");
    assert_eq!(v["amendment"]["lines"], 2, "{v}");
    assert_eq!(v["amendment"]["changes"][0]["path"], "app.py");
    assert!(
        v["amendment"]["changes"][0]["amended"]
            .as_str()
            .unwrap()
            .contains("+    for x in xs:"),
        "{v}"
    );
    assert!(
        v["amendment"]["changes"][0]["patch"]
            .as_str()
            .unwrap()
            .contains("+    for x in list(xs):"),
        "{v}"
    );
    // The same fix twice is refused.
    let (s, _) = call(
        "POST",
        &amend_path,
        Some(&fixer),
        Some(json!({ "category": "performance", "diff": perf_diff,
        "evidence": { "resource": "memory", "reasoning": "Iterating the argument directly avoids copying it into a list; the sum is unchanged." } })),
    );
    assert_eq!(s, 409);
    // A correctness amendment that the panel will not buy.
    let wrong = patched.replace("    return round(s + 0.5)\n", "    return int(s)\n");
    let (s, v) = call(
        "POST",
        &amend_path,
        Some(&fixer),
        Some(
            json!({ "category": "correctness", "diff": repo.diff(&[("app.py", Some(wrong))]),
        "evidence": { "reasoning": "Totals are integral in the spec, so truncation is the specified behavior.",
                      "reference": { "url": "https://example.com/spec", "section": "3.1", "quote": "the total is an integer" } } }),
        ),
    );
    assert_eq!(s, 201, "{v}");
    let a2 = v["amendment"]["id"].as_str().unwrap().to_string();
    // The amender can't vote on its own.
    let (s, _) = call(
        "POST",
        &format!("{amend_path}/{a1}/votes"),
        Some(&fixer),
        Some(json!({ "verdict": "endorse" })),
    );
    assert_eq!(s, 403);
    // Nor can anyone whose review isn't in.
    let (s, _) = call(
        "POST",
        &format!("{amend_path}/{a1}/votes"),
        Some(&outsider),
        Some(json!({ "verdict": "endorse" })),
    );
    assert_eq!(s, 403);
    let (s, v) = call(
        "POST",
        &format!("/v1/reviews/{task}"),
        Some(&fixer),
        Some(json!({ "issues": [] })),
    );
    assert_eq!(s, 200, "{v}");

    // The second reviewer sees both, without knowing whose they are, and
    // votes in its review.
    let t = lease(&second, &intent);
    let listed = t["amendments"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "{t}");
    for a in listed {
        assert_eq!(a["mine"], false);
        assert!(
            a.get("operator_id").is_none() && a.get("votes").is_none(),
            "{a}"
        );
        assert!(a["diff"].as_str().unwrap().starts_with("diff --git"));
    }
    let (s, v) = call(
        "POST",
        &format!("/v1/reviews/{task}"),
        Some(&second),
        Some(json!({ "issues": [],
        "amendments": [{ "id": a1, "verdict": "endorse" }, { "id": a2, "verdict": "reject", "note": "the reference says integer, not truncated" }] })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["amendment_votes"][0]["state"], "pending", "{v}");
    // Still pending: one vote is no quorum, and the panel isn't done.
    let (s, v) = call("GET", &amend_path, Some(&second), None);
    assert_eq!(s, 200, "{v}");
    assert!(
        v["amendments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["state"] == "pending"),
        "{v}"
    );
    assert_eq!(v["amendments"][0]["my_vote"], "endorse");
    let (s, _) = call("GET", &amend_path, Some(&outsider), None);
    assert_eq!(s, 403);

    // The third reviewer completes the panel: the fix is accepted, the
    // wrong one rejected. It can no longer attach its own.
    let t = lease(&third, &intent);
    assert_eq!(t["task_id"], json!(task));
    let (s, v) = call(
        "POST",
        &format!("{amend_path}/{a1}/votes"),
        Some(&third),
        Some(json!({ "verdict": "endorse" })),
    );
    assert_eq!(s, 403, "votes need the review in first: {v}");
    let (s, v) = call(
        "POST",
        &format!("/v1/reviews/{task}"),
        Some(&third),
        Some(json!({ "issues": [],
        "amendments": [{ "id": a1, "verdict": "endorse" }, { "id": a2, "verdict": "reject" }] })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["amendment_votes"][0]["state"], "accepted", "{v}");
    assert_eq!(v["amendment_votes"][1]["state"], "rejected", "{v}");
    let (s, v) = call(
        "POST",
        &amend_path,
        Some(&third),
        Some(json!({ "category": "performance", "diff": perf_diff,
        "evidence": { "resource": "cpu", "reasoning": "Same fix again, too late for this panel to vote on it." } })),
    );
    // Review-only project: its task is done with the review, so it no
    // longer holds it (403); in the run phase it would hold it and be told
    // the panel is complete (409).
    assert!(s == 403 || s == 409, "{s} {v}");
    let (s, _) = call(
        "POST",
        &format!("{amend_path}/{a1}/votes"),
        Some(&second),
        Some(json!({ "verdict": "reject" })),
    );
    assert_eq!(s, 409, "votes are closed once resolved");

    // The accepted amendment is a derived patch by the fixer, on the same
    // base, with the same intent, and its own open review task.
    let (s, v) = call("GET", &amend_path, Some(&fixer), None);
    assert_eq!(s, 200, "{v}");
    let accepted = v["amendments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == json!(a1))
        .unwrap()
        .clone();
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted["mine"], true);
    let derived = accepted["result_patch_id"].as_str().unwrap().to_string();
    let (s, v) = call("GET", &format!("/v1/patches/{derived}"), None, None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["patch"]["parent_id"], json!(patch));
    assert_eq!(v["patch"]["base_commit"], json!(base));
    assert_eq!(v["patch"]["title"], "Add totals (amended: performance)");
    assert_eq!(v["patch"]["author_operator"], me(&fixer)["operator_id"]);
    assert_eq!(v["review"], "open", "the fixer has standing: {v}");
    assert_eq!(v["amends"]["patch_id"], json!(patch));
    assert_eq!(v["amends"]["category"], "performance");
    assert_eq!(v["amends"]["evidence"]["resource"], "memory");
    let (s, d) = call("GET", &format!("/v1/patches/{derived}/diff"), None, None);
    assert_eq!(s, 200);
    assert_eq!(
        d.as_str().unwrap(),
        format_diff(&parse_diff(&perf_diff).unwrap()),
        "the derived patch is the amended diff, normalized"
    );
    let (s, v) = call("GET", &format!("/v1/patches/{patch}"), None, None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["amendments"][0]["patch_id"], json!(derived));
    assert_eq!(v["review"], "clean");

    // Rep: +3 for the accepted amendment, −1 for the rejected one; the
    // voters earn nothing for voting.
    assert_eq!(deltas(&fixer, "amendment accepted"), vec![3.0]);
    assert_eq!(deltas(&fixer, "amendment rejected"), vec![-1.0]);
    assert!(deltas(&second, "amendment accepted").is_empty());
    // The derived patch is a submission the fixer owes duty for, like the
    // author's.
    assert_eq!(
        me(&fixer)["duty"]["required"],
        me(&author)["duty"]["required"],
        "one small patch each"
    );

    // Amending the derived patch: its panel can't include the fixer (its author).
    let (s, v) = call("POST", "/v1/reviews/next", Some(&fixer), None);
    assert_eq!(s, 200);
    assert_ne!(
        v["task"]["intent"],
        json!(intent),
        "an author never reviews its own patch: {v}"
    );
}
