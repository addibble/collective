//! Runs real proofs: real git repositories, real `git apply`, real network
//! namespaces. Needs `unshare -rn` to work (as in the prover's container).

#[path = "../../core/tests/common.rs"]
mod common;

use std::process::Command;
use std::time::Instant;

use collective_core::proof::*;
use collective_prover::{decode_request, encode_request, prove, Options, ProveRequest};
use common::Repo;

const ADD: &str = "#!/bin/sh\necho $(($1 + $2))\n";
const TEST: &str =
    "#!/bin/sh\n[ \"$(sh add.sh 2 3)\" = 5 ] || { echo 'add is broken'; exit 1; }\necho ok\n";
const LOGIN: &str = "#!/bin/sh\n[ \"$1\" = alice ] && echo user && exit 0\necho denied; exit 1\n";

fn project() -> Repo {
    Repo::new(&[("add.sh", ADD), ("test.sh", TEST), ("login.sh", LOGIN)])
}

fn tarball(r: &Repo) -> Vec<u8> {
    let out = Command::new("git")
        .args(["archive", "--format=tar.gz", "--prefix=src/", "HEAD"])
        .current_dir(&r.dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
}

fn opts() -> Options {
    Options {
        no_internet: false,
        allow_unisolated: false,
        budget: std::time::Duration::from_secs(120),
        setup_env: vec![],
    }
}

fn spec(kind: ProofKind, command: &str, files: &[(&str, &str)]) -> ProofSpec {
    let regression = kind == ProofKind::Regression;
    ProofSpec {
        kind,
        ecosystem: Ecosystem::None,
        tests: if regression {
            vec![command.into()]
        } else {
            vec![]
        },
        command: (!regression).then(|| command.into()),
        files: files
            .iter()
            .map(|(p, c)| ProofFile {
                path: p.to_string(),
                content: c.to_string(),
            })
            .collect(),
        repeats: 2,
        timeout_secs: 20,
    }
}

#[test]
fn proves_a_regression_with_the_projects_own_tests() {
    let r = project();
    let patch = r.diff(&[("add.sh", Some("#!/bin/sh\necho $(($1 + $2 + 1))\n".into()))]);
    let s = spec(ProofKind::Regression, "test.sh", &[]);
    let report = prove(&s, &tarball(&r), &patch, &opts());
    assert!(report.setup.ok, "{:?}", report.setup);
    assert!(report
        .setup
        .isolation
        .as_deref()
        .unwrap()
        .starts_with("network namespace"));
    let a = assess(&s, &report);
    assert_eq!(a.verdict, Verdict::Proven, "{a:?} {report:#?}");
    assert!(report
        .runs
        .iter()
        .any(|run| run.output_tail.contains("add is broken")));
}

#[test]
fn demonstrates_an_exploit_with_a_finder_test() {
    let r = project();
    let backdoored = LOGIN.replace(
        "[ \"$1\" = alice ]",
        "[ \"$1\" = debug ] && echo admin && exit 0\n[ \"$1\" = alice ]",
    );
    let patch = r.diff(&[("login.sh", Some(backdoored))]);
    let s = spec(
        ProofKind::Exploit,
        "sh proof/exploit.sh",
        &[("proof/exploit.sh", "sh login.sh debug | grep -qx admin")],
    );
    let a = assess(&s, &prove(&s, &tarball(&r), &patch, &opts()));
    assert_eq!(a.verdict, Verdict::Demonstrated, "{a:?}");

    // The same test against an innocent patch shows nothing.
    let innocent = r.diff(&[("add.sh", Some(format!("{ADD}# comment\n")))]);
    let a = assess(&s, &prove(&s, &tarball(&r), &innocent, &opts()));
    assert_eq!(a.verdict, Verdict::NotReproduced, "{a:?}");
}

#[test]
fn runs_have_no_network_and_no_inherited_environment() {
    let r = project();
    let patch = r.diff(&[("add.sh", Some(format!("{ADD}# comment\n")))]);
    std::env::set_var("PROVER_TEST_SECRET", "hunter2");
    // A root-owned directory outside the tree, made by the test itself so
    // the check doesn't depend on how the host's system directories are set up.
    let outside = std::env::temp_dir().join(format!("prover-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
    let as_root = std::fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0)
        .unwrap_or(false);
    // Each condition says which one failed, so CI logs are useful.
    let mut check = String::from(
        "fail() { echo \"FAIL: $1\"; exit 1; }; \
         test \"$(grep -c : /proc/net/dev)\" = 1 || fail 'more than one network interface'; \
         grep -q '^ *lo:' /proc/net/dev || fail 'no loopback'; \
         test -z \"$PROVER_TEST_SECRET\" || fail 'prover environment leaked'; \
         touch inside-ok || fail 'cannot write inside the tree'",
    );
    // Dropping to an unprivileged user only happens when the prover is root.
    if as_root {
        check += &format!(
            "; touch {}/pwned 2>/dev/null && fail 'wrote outside the tree'; true",
            outside.display()
        );
    }
    let s = spec(ProofKind::Property, &check, &[("proof/x", "")]);
    let report = prove(&s, &tarball(&r), &patch, &opts());
    assert!(
        report
            .runs
            .iter()
            .all(|run| run.status == RunStatus::Passed),
        "{report:#?}"
    );
    assert!(!outside.join("pwned").exists());
    let _ = std::fs::remove_dir_all(outside);
}

#[test]
fn timeouts_kill_the_whole_process_tree() {
    let r = project();
    let patch = r.diff(&[("add.sh", Some(format!("{ADD}# comment\n")))]);
    let mut s = spec(
        ProofKind::Property,
        "(sleep 60 &) ; sleep 60",
        &[("proof/x", "")],
    );
    s.timeout_secs = 1;
    s.repeats = 1;
    let t0 = Instant::now();
    let report = prove(&s, &tarball(&r), &patch, &opts());
    assert!(t0.elapsed().as_secs() < 15, "took {:?}", t0.elapsed());
    assert!(report
        .runs
        .iter()
        .all(|run| run.status == RunStatus::TimedOut));
    let leftover = Command::new("pgrep")
        .args(["-f", "sleep 60"])
        .output()
        .unwrap();
    assert!(
        leftover.stdout.is_empty(),
        "orphaned processes: {}",
        String::from_utf8_lossy(&leftover.stdout)
    );
}

#[test]
fn refuses_to_write_through_symlinks_the_patch_adds() {
    let r = project();
    let outside = std::env::temp_dir().join(format!("prover-escape-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, r.dir.join("evil")).unwrap();
    Command::new("git")
        .args(["add", "evil"])
        .current_dir(&r.dir)
        .status()
        .unwrap();
    let patch = Command::new("git")
        .args(["diff", "--cached"])
        .current_dir(&r.dir)
        .output()
        .unwrap()
        .stdout;
    Command::new("git")
        .args(["reset", "-q", "--hard"])
        .current_dir(&r.dir)
        .status()
        .unwrap();
    let s = spec(ProofKind::Exploit, "true", &[("evil/pwned", "x")]);
    let report = prove(
        &s,
        &tarball(&r),
        &String::from_utf8(patch).unwrap(),
        &opts(),
    );
    assert!(!report.setup.ok);
    assert!(report.setup.error.unwrap().contains("escapes"));
    assert!(!outside.join("pwned").exists());
    let _ = std::fs::remove_dir_all(outside);
}

#[test]
fn regressions_must_name_a_script_the_project_has() {
    let r = project();
    let patch = r.diff(&[("add.sh", Some(format!("{ADD}# comment\n")))]);
    let s = spec(ProofKind::Regression, "proof/fake.sh", &[]);
    let report = prove(&s, &tarball(&r), &patch, &opts());
    assert!(report
        .setup
        .error
        .unwrap()
        .contains("not a file in the base commit"));
}

#[test]
fn reports_patches_that_do_not_apply() {
    let r = project();
    let s = spec(ProofKind::Regression, "test.sh", &[]);
    let bogus =
        "diff --git a/nope.txt b/nope.txt\n--- a/nope.txt\n+++ b/nope.txt\n@@ -1 +1 @@\n-a\n+b\n";
    let report = prove(&s, &tarball(&r), bogus, &opts());
    assert!(report.setup.error.unwrap().contains("does not apply"));
}

#[test]
fn request_framing_round_trips() {
    let req = ProveRequest {
        spec: spec(ProofKind::Regression, "test.sh", &[]),
        patch: "diff\nwith\nnewlines".into(),
    };
    let body = encode_request(&req, b"\x1f\x8bbinary\nbytes");
    let (back, source) = decode_request(&body).unwrap();
    assert_eq!(back.patch, req.patch);
    assert_eq!(source, b"\x1f\x8bbinary\nbytes");
}
