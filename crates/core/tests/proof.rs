use collective_core::proof::*;

fn spec(kind: ProofKind) -> ProofSpec {
    let regression = kind == ProofKind::Regression;
    ProofSpec {
        kind,
        ecosystem: Ecosystem::None,
        tests: if regression {
            vec!["tests/run.sh".into()]
        } else {
            vec![]
        },
        command: (!regression).then(|| "sh tests/proof.sh".into()),
        files: if regression {
            vec![]
        } else {
            vec![ProofFile {
                path: "tests/proof.sh".into(),
                content: "exit 0".into(),
            }]
        },
        repeats: 2,
        timeout_secs: 30,
    }
}

fn report(base: &[RunStatus], patched: &[RunStatus]) -> ProofReport {
    let run = |tree, (i, s): (usize, &RunStatus)| RunResult {
        tree,
        attempt: i as u8,
        status: *s,
        exit_code: None,
        duration_ms: 1,
        output_tail: String::new(),
        tests_run: Some(1),
    };
    ProofReport {
        prover_version: "test".into(),
        setup: SetupResult {
            ok: true,
            error: None,
            network_isolated: true,
            isolation: Some("test".into()),
        },
        runs: base
            .iter()
            .enumerate()
            .map(|r| run(Tree::Base, r))
            .chain(patched.iter().enumerate().map(|r| run(Tree::Patched, r)))
            .collect(),
    }
}

use RunStatus::*;

#[test]
fn regression_with_unanimous_runs_is_proven() {
    let a = assess(
        &spec(ProofKind::Regression),
        &report(&[Passed, Passed], &[Failed, Failed]),
    );
    assert_eq!(a.verdict, Verdict::Proven);
}

#[test]
fn finder_tests_only_demonstrate() {
    assert_eq!(
        assess(
            &spec(ProofKind::Exploit),
            &report(&[Failed, Failed], &[Passed, Passed])
        )
        .verdict,
        Verdict::Demonstrated
    );
    assert_eq!(
        assess(
            &spec(ProofKind::Property),
            &report(&[Passed, Passed], &[Failed, Failed])
        )
        .verdict,
        Verdict::Demonstrated
    );
}

#[test]
fn hangs_alone_never_prove() {
    let a = assess(
        &spec(ProofKind::Regression),
        &report(&[Passed, Passed], &[TimedOut, TimedOut]),
    );
    assert_eq!(a.verdict, Verdict::Demonstrated);
    // A mix of failures and timeouts still proves it.
    assert_eq!(
        assess(
            &spec(ProofKind::Regression),
            &report(&[Passed, Passed], &[Failed, TimedOut])
        )
        .verdict,
        Verdict::Proven
    );
}

#[test]
fn disagreement_is_flaky_and_consistency_without_difference_is_not_reproduced() {
    assert_eq!(
        assess(
            &spec(ProofKind::Regression),
            &report(&[Passed, Failed], &[Failed, Failed])
        )
        .verdict,
        Verdict::Flaky
    );
    let a = assess(
        &spec(ProofKind::Regression),
        &report(&[Passed, Passed], &[Passed, Passed]),
    );
    assert_eq!(a.verdict, Verdict::NotReproduced);
    assert!(a.reason.contains("patched"), "{}", a.reason);
}

#[test]
fn fails_closed() {
    let mut r = report(&[Passed, Passed], &[Failed, Failed]);
    r.setup.network_isolated = false;
    assert_eq!(
        assess(&spec(ProofKind::Regression), &r).verdict,
        Verdict::Inconclusive
    );
    let mut r = report(&[Passed, Passed], &[Failed, Skipped]);
    assert_eq!(
        assess(&spec(ProofKind::Regression), &r).verdict,
        Verdict::Inconclusive
    );
    r = report(&[Passed], &[Failed]);
    assert_eq!(
        assess(&spec(ProofKind::Regression), &r).verdict,
        Verdict::Inconclusive
    );
    r = report(&[Passed, Passed], &[Failed, Failed]);
    r.setup = SetupResult {
        ok: false,
        error: Some("patch does not apply".into()),
        network_isolated: true,
        isolation: None,
    };
    assert_eq!(
        assess(&spec(ProofKind::Regression), &r).reason,
        "patch does not apply"
    );
}

#[test]
fn regression_proofs_cannot_inject_their_own_checks() {
    let mut s = spec(ProofKind::Regression);
    s.command = Some("sh test.sh && ! grep -q patched file".into());
    assert!(s.validate().is_err());
    s.command = None;
    for bad in [
        "test.sh; rm -rf /",
        "$(id)",
        "-rf",
        "a b",
        "../../etc/passwd",
        "x|y",
        "`id`",
    ] {
        s.tests = vec![bad.into()];
        assert!(s.validate().is_err(), "{bad}");
    }
    s.ecosystem = Ecosystem::Pip;
    s.tests = vec![
        "tests/test_login.py::test_rejects_unknown_users".into(),
        "tests/test_api.py::test_get[json]".into(),
    ];
    assert!(s.validate().is_ok());
    assert_eq!(s.argv()[..4], [".venv/bin/python", "-m", "pytest", "-q"]);
    s.ecosystem = Ecosystem::Go;
    s.tests = vec!["TestLogin".into(), "TestAPI".into()];
    assert_eq!(s.argv().last().unwrap(), "^(TestLogin|TestAPI)$");
}

#[test]
fn specs_are_validated() {
    let mut s = spec(ProofKind::Regression);
    s.files.push(ProofFile {
        path: "x".into(),
        content: String::new(),
    });
    assert!(s.validate().unwrap_err().contains("own tests"));
    let mut s = spec(ProofKind::Exploit);
    for bad in [
        "/etc/passwd",
        "../x",
        "a/../../b",
        ".git/hooks/pre-commit",
        "a//b",
        "",
    ] {
        s.files[0].path = bad.into();
        assert!(s.validate().is_err(), "{bad}");
    }
    s.files[0].path = "tests/ok_test.py".into();
    assert!(s.validate().is_ok());
    s.repeats = 0;
    assert!(s.validate().is_err());
}

#[test]
fn counts_tests_from_runner_output() {
    let cargo = "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\
                 running 2 tests\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 1702 filtered out\n";
    assert_eq!(count_tests(Ecosystem::Cargo, cargo), Some(2));
    assert_eq!(
        count_tests(
            Ecosystem::Cargo,
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1704 filtered out"
        ),
        Some(0)
    );
    assert_eq!(
        count_tests(Ecosystem::Cargo, "error: could not compile `resvg`"),
        None
    );
    assert_eq!(
        count_tests(Ecosystem::Pip, "==== 3 passed, 1 failed in 0.2s ===="),
        Some(4)
    );
    assert_eq!(
        count_tests(Ecosystem::Pip, "==== no tests ran in 0.01s ===="),
        Some(0)
    );
}

#[test]
fn regressions_must_actually_run_the_named_tests() {
    let mut s = spec(ProofKind::Regression);
    s.ecosystem = Ecosystem::Cargo;
    s.tests = vec!["svg_without_xmlns".into()];
    let with = |base: Option<u32>, patched: Option<u32>| {
        let mut r = report(&[Passed, Passed], &[Failed, Failed]);
        for run in &mut r.runs {
            run.tests_run = if run.tree == Tree::Base {
                base
            } else {
                patched
            };
        }
        assess(&s, &r)
    };
    // Matched nothing: both trees "pass" vacuously, or the patch looks guilty for free.
    assert_eq!(with(Some(0), Some(0)).verdict, Verdict::Inconclusive);
    // The patched tree never got as far as running tests: a build break, for a person to confirm.
    assert_eq!(with(Some(1), None).verdict, Verdict::Demonstrated);
    assert_eq!(with(Some(1), Some(1)).verdict, Verdict::Proven);
}
