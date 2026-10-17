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
        environment: Default::default(),
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
        cpu_ms: None,
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

fn suite(ecosystem: Ecosystem, tests: &[&str]) -> ProofSpec {
    ProofSpec {
        kind: ProofKind::Suite,
        ecosystem,
        tests: tests.iter().map(|t| t.to_string()).collect(),
        command: None,
        files: vec![],
        repeats: 1,
        timeout_secs: 900,
        environment: Default::default(),
    }
}

#[test]
fn suite_runs_validate_and_use_the_whole_suite() {
    assert!(suite(Ecosystem::Cargo, &[]).validate().is_ok());
    assert!(suite(Ecosystem::None, &["test.sh"]).validate().is_ok());
    // A suite run is the project's own: no selectors, scripts outside the tree, files or commands.
    assert!(suite(Ecosystem::Cargo, &["only_this"]).validate().is_err());
    assert!(suite(Ecosystem::None, &[]).validate().is_err());
    assert!(suite(Ecosystem::None, &["../x.sh"]).validate().is_err());
    let mut s = suite(Ecosystem::Cargo, &[]);
    s.command = Some("true".into());
    assert!(s.validate().is_err());
    s = suite(Ecosystem::Cargo, &[]);
    s.timeout_secs = MAX_SUITE_TIMEOUT_SECS + 1;
    assert!(s.validate().is_err());
    assert_eq!(
        suite(Ecosystem::Go, &[]).argv(),
        ["go", "test", "./...", "-count=1"]
    );
    assert_eq!(
        suite(Ecosystem::None, &["test.sh"]).argv(),
        ["sh", "test.sh"]
    );
    // Never a challenge proof.
    let r = report(&[Passed], &[Failed]);
    assert_eq!(
        assess(&suite(Ecosystem::None, &["t.sh"]), &r).verdict,
        Verdict::Inconclusive
    );
}

#[test]
fn suite_outcomes_fail_closed() {
    let s = suite(Ecosystem::None, &["test.sh"]);
    assert_eq!(
        suite_outcome(&s, &report(&[Passed], &[Passed])),
        RunOutcome::Passed
    );
    assert_eq!(
        suite_outcome(&s, &report(&[Passed], &[Failed])),
        RunOutcome::Regressed
    );
    assert_eq!(
        suite_outcome(&s, &report(&[Passed], &[TimedOut])),
        RunOutcome::Regressed
    );
    assert_eq!(
        suite_outcome(&s, &report(&[Failed], &[Failed])),
        RunOutcome::BaseFails
    );
    let two = ProofSpec {
        repeats: 2,
        ..s.clone()
    };
    assert_eq!(
        suite_outcome(&two, &report(&[Passed, Failed], &[Passed, Passed])),
        RunOutcome::Flaky
    );
    assert_eq!(
        suite_outcome(&two, &report(&[Passed, Passed], &[Passed, Failed])),
        RunOutcome::Flaky
    );
    // Missing or skipped runs, failed setup and missing isolation are inconclusive.
    assert_eq!(
        suite_outcome(&two, &report(&[Passed], &[Passed])),
        RunOutcome::Inconclusive
    );
    assert_eq!(
        suite_outcome(&s, &report(&[Skipped], &[Passed])),
        RunOutcome::Inconclusive
    );
    let mut r = report(&[Passed], &[Passed]);
    r.setup.network_isolated = false;
    assert_eq!(suite_outcome(&s, &r), RunOutcome::Inconclusive);
    // A counted runner that ran no tests proves nothing.
    let mut cargo = report(&[Passed], &[Passed]);
    cargo.runs[0].tests_run = Some(0);
    assert_eq!(
        suite_outcome(&suite(Ecosystem::Cargo, &[]), &cargo),
        RunOutcome::Inconclusive
    );
    // Only suite specs are judged as suites.
    assert_eq!(
        suite_outcome(
            &spec(ProofKind::Regression),
            &report(&[Passed, Passed], &[Passed, Passed])
        ),
        RunOutcome::Inconclusive
    );
}

fn claim(runner: &str, trust: f64, verdict: Verdict) -> ProofClaim {
    ProofClaim {
        runner: runner.into(),
        trust,
        verdict,
    }
}

#[test]
fn a_proof_settles_only_when_enough_trusted_operators_agree() {
    use Verdict::*;
    // One runner settles nothing, however trusted.
    assert_eq!(proof_quorum(&[claim("a", 1.0, Proven)]), Quorum::Pending);
    assert_eq!(
        proof_quorum(&[claim("a", 1.0, Proven), claim("b", 1.0, Proven)]),
        Quorum::Pending
    );
    // Three operators with enough trust between them do.
    let three = [
        claim("a", 0.7, Proven),
        claim("b", 0.7, Proven),
        claim("c", 0.7, Proven),
    ];
    assert_eq!(proof_quorum(&three), Quorum::Settled(Proven));
    // The same operator reporting three times is one runner.
    assert_eq!(
        proof_quorum(&[
            claim("a", 1.0, Proven),
            claim("a", 1.0, Proven),
            claim("a", 1.0, Proven)
        ]),
        Quorum::Pending
    );
    // Inconclusive runs count for nothing.
    assert_eq!(
        proof_quorum(&[
            claim("a", 1.0, Proven),
            claim("b", 1.0, Proven),
            claim("c", 1.0, Inconclusive)
        ]),
        Quorum::Pending
    );
}

#[test]
fn new_accounts_alone_never_reach_quorum() {
    use Verdict::*;
    let newcomers: Vec<ProofClaim> = (0..MAX_RUNNERS)
        .map(|i| claim(&format!("new{i}"), 0.25, Proven))
        .collect();
    assert_eq!(proof_quorum(&newcomers[..3]), Quorum::Pending);
    assert_eq!(proof_quorum(&newcomers), Quorum::Split);
    // Two trusted runners still need a third operator, and any will do.
    assert_eq!(
        proof_quorum(&[
            claim("a", 1.0, Proven),
            claim("b", 1.0, Proven),
            claim("new", 0.25, Proven)
        ]),
        Quorum::Settled(Proven)
    );
}

#[test]
fn disagreement_fails_closed() {
    use Verdict::*;
    // Three say proven, but more trust says otherwise: no verdict.
    let mut claims = vec![
        claim("a", 0.7, Proven),
        claim("b", 0.7, Proven),
        claim("c", 0.7, Proven),
        claim("d", 1.0, NotReproduced),
        claim("e", 1.0, NotReproduced),
        claim("f", 0.25, Demonstrated),
    ];
    assert_eq!(proof_quorum(&claims), Quorum::Pending);
    // A fourth agreeing runner outweighs the dissent.
    claims.push(claim("g", 0.7, Proven));
    assert_eq!(proof_quorum(&claims), Quorum::Settled(Proven));
    // Without one, the job runs out of runners and is split.
    claims.pop();
    claims.push(claim("g", 0.25, Flaky));
    assert_eq!(proof_quorum(&claims), Quorum::Split);
}

#[test]
fn proof_runs_are_graded_against_the_settled_verdict() {
    use Verdict::*;
    let claims = [
        claim("a", 1.0, Proven),
        claim("b", 1.0, Proven),
        claim("c", 1.0, Proven),
        claim("d", 0.25, NotReproduced),
        claim("e", 0.25, Inconclusive),
    ];
    assert_eq!(proof_quorum(&claims), Quorum::Settled(Proven));
    let entries = grade_proof_runs(Proven, &claims);
    assert_eq!(collective_core::net(&entries, "a"), PROOF_RUN_AGREED);
    assert_eq!(collective_core::net(&entries, "d"), PROOF_RUN_DISAGREED);
    assert_eq!(collective_core::net(&entries, "e"), 0.0);
}

#[test]
fn a_run_lease_covers_the_provers_budget() {
    let s = spec(ProofKind::Regression);
    assert!(run_lease_secs(&s) > s.budget_secs());
}
