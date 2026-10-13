//! End-to-end: patches and trunks become branches in the project's
//! Artifacts repository (docs/GIT.md). Needs a Worker with the ARTIFACTS
//! binding, which always reaches the real service (wrangler dev uses the
//! remote binding), network access to GitHub, and git:
//!
//!   COLLECTIVE_URL=http://localhost:8787 COLLECTIVE_ADMIN_TOKEN=dev-admin \
//!     cargo test -p collective-e2e --test git_hosting -- --ignored --test-threads=1
//!
//! Upstream is octocat/Hello-World: tiny, public and unchanged for years.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

const UPSTREAM: &str = "https://github.com/octocat/Hello-World";
const BASE: &str = "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d";

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

/// Polls the git endpoint until `pick` returns something.
fn wait_for<T>(path: &str, token: &str, what: &str, pick: impl Fn(&Value) -> Option<T>) -> T {
    let mut last = Value::Null;
    for _ in 0..200 {
        let (s, v) = call("GET", path, Some(token), None);
        assert_eq!(s, 200, "{v}");
        if let Some(t) = pick(&v) {
            return t;
        }
        last = v;
        std::thread::sleep(Duration::from_secs(3));
    }
    panic!("timed out waiting for {what}: {last}");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Fetches `branch` with the read token and returns the fetched commit.
fn fetch(dir: &Path, access: &Value, branch: &str) -> String {
    let header = format!(
        "http.extraHeader=Authorization: Bearer {}",
        access["token"].as_str().unwrap()
    );
    let remote = access["remote"].as_str().unwrap();
    git(dir, &["-c", &header, "fetch", "-q", remote, branch]);
    git(dir, &["rev-parse", "FETCH_HEAD"]).trim().to_string()
}

fn branch_status(v: &Value, key: &str, want: &str) -> Option<Value> {
    let s = v[key]["status"].as_str()?;
    if s == "failed" {
        panic!("{key} branch failed: {v}");
    }
    (s == want && v["status"] == "ready").then(|| v.clone())
}

#[test]
#[ignore]
fn patches_and_trunks_become_branches() {
    let run = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let (s, v) = call(
        "POST",
        "/v1/admin/pool",
        None,
        Some(json!({ "amount": 1000 })),
    );
    assert_eq!(s, 200, "{v}");
    let (s, v) = call(
        "POST",
        "/v1/admin/projects",
        None,
        Some(json!({ "upstream": UPSTREAM })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["git"], true,
        "the Worker needs the ARTIFACTS binding: {v}"
    );
    let project = v["project"]["id"].as_str().unwrap().to_string();
    let author = register(&format!("git-author-{run}"));
    let git_path = format!("/v1/projects/{project}/git");

    // Anonymous callers get no token.
    let (s, _) = call("GET", &git_path, None, None);
    assert_eq!(s, 401);

    wait_for(&git_path, &author, "the import", |v| {
        (v["status"] == "ready").then_some(())
    });

    let line = format!("From Collective {run}");
    let diff = format!(
        "diff --git a/README b/README\n--- a/README\n+++ b/README\n@@ -1 +1,2 @@\n Hello World!\n+{line}\n"
    );
    let (s, v) = call(
        "POST",
        "/v1/patches",
        Some(&author),
        Some(
            json!({ "project": UPSTREAM, "base_commit": BASE, "intent": format!("Greet from Collective ({run})"), "diff": diff }),
        ),
    );
    assert_eq!(s, 201, "{v}");
    let patch = v["patch"]["id"].as_str().unwrap().to_string();

    let access = wait_for(
        &format!("{git_path}?patch_id={patch}"),
        &author,
        "the patch branch",
        |v| branch_status(v, "patch", "published"),
    );
    let commit = access["patch"]["commit"].as_str().unwrap().to_string();

    let dir = std::env::temp_dir().join(format!("collective-git-e2e-{run}"));
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    assert_eq!(fetch(&dir, &access, &format!("patch/{patch}")), commit);
    assert_eq!(git(&dir, &["rev-parse", "FETCH_HEAD^"]).trim(), BASE);
    assert_eq!(
        git(&dir, &["show", "FETCH_HEAD:README"]),
        format!("Hello World!\n{line}\n")
    );
    let message = git(&dir, &["log", "-1", "--format=%B", "FETCH_HEAD"]);
    assert!(
        message.contains(&format!("Collective-Patch: {patch}")),
        "{message}"
    );

    // Two operators review, then adopt: the patch converges into the trunk.
    for name in ["carol", "dave"] {
        let t = register(&format!("git-{name}-{run}"));
        for _ in 0..100 {
            let (s, v) = call("POST", "/v1/reviews/next", Some(&t), None);
            assert_eq!(s, 200, "{v}");
            let Some(task) = v["task"].as_object() else {
                break;
            };
            let id = task["task_id"].as_str().unwrap();
            let (s, v) = call(
                "POST",
                &format!("/v1/reviews/{id}"),
                Some(&t),
                Some(json!({ "issues": [] })),
            );
            assert_eq!(s, 200, "{v}");
        }
        let (s, v) = call(
            "POST",
            &format!("/v1/patches/{patch}/adoptions"),
            Some(&t),
            Some(json!({ "outcome": "tests_passed" })),
        );
        assert_eq!(s, 201, "{v}");
    }
    let (s, v) = call(
        "POST",
        &format!("/v1/projects/{project}/trunks/{BASE}/converge"),
        Some(&author),
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert!(
        v["patch_ids"].as_array().unwrap().contains(&json!(patch)),
        "{v}"
    );
    let access = wait_for(
        &format!("{git_path}?base_commit={BASE}"),
        &author,
        "the trunk branch",
        |v| {
            let t = branch_status(v, "trunk", "published")?;
            // A trunk built before this run's patch converged is stale.
            let c = t["trunk"]["commit"].as_str()?.to_string();
            Some((t, c))
        },
    );
    let (access, trunk) = access;
    assert_eq!(fetch(&dir, &access, &format!("trunk/{BASE}")), trunk);
    let parents = git(&dir, &["log", "-1", "--format=%P", "FETCH_HEAD"]);
    assert!(parents.starts_with(BASE), "{parents}");
    assert!(
        parents.contains(&commit),
        "the trunk merges the patch commit: {parents}"
    );
    assert!(git(&dir, &["show", "FETCH_HEAD:README"]).contains(&line));

    // Withdrawing the patch removes its branch.
    let (s, v) = call(
        "POST",
        &format!("/v1/patches/{patch}/withdraw"),
        Some(&author),
        None,
    );
    assert_eq!(s, 200, "{v}");
    let access = wait_for(
        &format!("/v1/projects/{project}/git"),
        &author,
        "the branch removal",
        |v| {
            let header = format!(
                "http.extraHeader=Authorization: Bearer {}",
                v["token"].as_str()?
            );
            let refs = git(&dir, &["-c", &header, "ls-remote", v["remote"].as_str()?]);
            (!refs.contains(&format!("refs/heads/patch/{patch}"))).then(|| v.clone())
        },
    );
    assert_eq!(access["status"], "ready");
    let _ = std::fs::remove_dir_all(&dir);
}
