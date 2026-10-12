mod common;

use collective_core::challenge::Severity;
use collective_core::diff::parse_diff;
use collective_core::net;
use collective_core::review::*;
use common::{lines, Repo};

fn issue(path: &str, start: u32, kind: IssueKind, severity: Severity) -> Issue {
    Issue {
        path: path.into(),
        start,
        end: start,
        kind,
        severity,
        note: String::new(),
    }
}

fn review(who: &str, issues: Vec<Issue>) -> Review {
    Review {
        reviewer: who.into(),
        issues,
    }
}

#[test]
fn duty_scales_with_work_units_and_adoptions_after_a_free_allowance() {
    // Units: one per 500 changed lines, at least one.
    assert_eq!(task_units(1), 1);
    assert_eq!(task_units(500), 1);
    assert_eq!(task_units(501), 2);
    assert_eq!(task_units(2000), 4);
    // A task costs its reviewers plus the canaries mixed in around them.
    assert_eq!(review_cost(1), 4); // ⌈3 / 0.9⌉
    assert_eq!(review_cost(4), 14); // ⌈12 / 0.9⌉
    assert_eq!(duty_owed(0, 4, 0), 0); // free adoptions
    assert_eq!(duty_owed(1, 0, 0), 0); // a first small patch is within the allowance
    assert_eq!(duty_owed(4, 0, 0), 10); // a 2000-line patch pays for its review
    assert_eq!(duty_owed(2, 4, 0), 7); // ⌈6 / 0.9⌉ + 4 − 4
    assert_eq!(duty_owed(2, 4, 5), 2);
    assert_eq!(duty_owed(2, 4, 50), 0);
}

#[test]
fn review_supply_covers_demand_for_any_patch_size() {
    // Duty from a patch alone, before any adoptions, must pay for the
    // review work it consumes, canaries included.
    for units in 1..50 {
        let real = units * PARAMS.redundancy as u32;
        let with_canaries = real as f64 / (1.0 - PARAMS.canary_percent as f64 / 100.0);
        assert!(review_cost(units) as f64 >= with_canaries, "{units}");
    }
}

#[test]
fn rubber_stamping_loses_rep_on_average() {
    // A reviewer that calls everything clean: canaries cost it, and the
    // rest pay only completion (ignoring the rare real problems it also misses).
    let p = PARAMS.canary_percent as f64 / 100.0;
    let expected = p * PARAMS.canary_miss + (1.0 - p) * PARAMS.completion;
    assert!(expected < -0.5, "{expected}");
}

#[test]
fn a_task_is_a_whole_patch_up_to_the_cap() {
    let r = Repo::new(&[("a.txt", &lines(3000, "line")), ("b.txt", &lines(10, "b"))]);
    let edit = |every: usize| {
        let mut a = lines(3000, "line");
        for i in (10..3000).step_by(every) {
            a = a.replace(&format!("line {i}\n"), &format!("changed {i}\n"));
        }
        parse_diff(&r.diff(&[
            ("a.txt", Some(a)),
            ("b.txt", Some(lines(10, "b").replace("b 3\n", "bee\n"))),
        ]))
        .unwrap()
    };
    // Lines across every file count: 2 per changed line (− and +).
    let small = edit(20);
    assert_eq!(changed_lines(&small), 2 * 150 + 2);
    assert_eq!(check_task_size(&small), Ok(302));
    let big = edit(2);
    let err = check_task_size(&big).unwrap_err();
    assert!(err.contains("at most 2000"), "{err}");
    assert_ne!(task_id("p"), task_id("q"));
}

#[test]
fn running_follows_review_unless_the_reviewer_reports_danger() {
    let minor = issue("x.rs", 3, IssueKind::Bug, Severity::Low);
    let danger = issue("x.rs", 9, IssueKind::Backdoor, Severity::Medium);
    let high = issue("x.rs", 9, IssueKind::Bug, Severity::High);
    assert!(run_required(&[], true));
    assert!(run_required(std::slice::from_ref(&minor), true));
    assert!(!run_required(&[minor.clone(), danger], true));
    assert!(!run_required(&[high], true));
    // Nothing to run without a test suite.
    assert!(!run_required(&[], false));
}

#[test]
fn task_picking_never_gives_authors_their_own_patch() {
    let open = vec![
        OpenShard {
            id: "mine".into(),
            author_operator: "me".into(),
            assigned: vec![],
            wanted: 3,
            canary: false,
        },
        OpenShard {
            id: "done".into(),
            author_operator: "x".into(),
            assigned: vec!["a".into(), "b".into(), "c".into()],
            wanted: 3,
            canary: false,
        },
        OpenShard {
            id: "seen".into(),
            author_operator: "x".into(),
            assigned: vec!["me".into()],
            wanted: 3,
            canary: false,
        },
        OpenShard {
            id: "ok".into(),
            author_operator: "x".into(),
            assigned: vec![],
            wanted: 3,
            canary: false,
        },
    ];
    for i in 0..20 {
        assert_eq!(pick_task("me", &open, &format!("s{i}")).unwrap().id, "ok");
    }
    assert!(pick_task("me", &open[..3], "s").is_none());
}

fn shard(id: &str, canary: bool) -> OpenShard {
    OpenShard {
        id: id.into(),
        author_operator: "x".into(),
        assigned: vec![],
        wanted: 3,
        canary,
    }
}

fn canary_share(open: &[OpenShard]) -> f64 {
    let n = 4000;
    let hits = (0..n)
        .filter(|i| pick_task("me", open, &format!("s{i}")).unwrap().canary)
        .count();
    hits as f64 / n as f64
}

#[test]
fn canaries_keep_a_floor_share_of_assignments() {
    // One canary among a deep queue still comes up at the floor rate.
    let mut open: Vec<OpenShard> = (0..199).map(|i| shard(&format!("r{i}"), false)).collect();
    open.push(shard("c", true));
    let share = canary_share(&open);
    let floor = PARAMS.canary_percent as f64 / 100.0;
    assert!((share - floor).abs() < 0.02, "{share}");
    // When canaries are plentiful the pick is uniform.
    let even: Vec<OpenShard> = (0..20)
        .map(|i| shard(&format!("s{i}"), i % 2 == 0))
        .collect();
    assert!((canary_share(&even) - 0.5).abs() < 0.04);
    // With only one kind available, that kind is picked.
    assert!(pick_task("me", &[shard("c", true)], "s").unwrap().canary);
    assert!(!pick_task("me", &[shard("r", false)], "s").unwrap().canary);
}

#[test]
fn agreement_opens_a_challenge_and_lone_serious_reports_widen() {
    let a = review(
        "a",
        vec![issue("x.rs", 10, IssueKind::Backdoor, Severity::Critical)],
    );
    let b = review(
        "b",
        vec![issue("x.rs", 11, IssueKind::Vulnerability, Severity::High)],
    );
    let c = review("c", vec![]);
    match next_step(&[a.clone(), b], 3) {
        NextStep::Challenge { finding } => {
            assert_eq!(finding.reporters, ["a", "b"]);
            assert_eq!(finding.severity, Severity::Critical);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(next_step(std::slice::from_ref(&a), 3), NextStep::Pending);
    assert!(matches!(
        next_step(&[a, c.clone(), review("d", vec![])], 3),
        NextStep::Widen { wanted: 7, .. }
    ));
    let minor = review("e", vec![issue("x.rs", 3, IssueKind::Bug, Severity::Low)]);
    assert_eq!(
        next_step(&[minor, c, review("f", vec![])], 3),
        NextStep::Clean
    );
}

#[test]
fn canaries_reward_finders_and_punish_rubber_stamps() {
    let truth = Truth::Canary {
        planted: vec![Location {
            path: "x.rs".into(),
            start: 40,
            end: 42,
        }],
    };
    let reviews = [
        review(
            "sharp",
            vec![issue("x.rs", 41, IssueKind::Backdoor, Severity::Critical)],
        ),
        review("lazy", vec![]),
        review(
            "wrong-place",
            vec![issue("x.rs", 5, IssueKind::Bug, Severity::Low)],
        ),
    ];
    let e = grade(&truth, &reviews);
    assert_eq!(net(&e, "sharp"), PARAMS.canary_catch + PARAMS.completion);
    assert_eq!(net(&e, "lazy"), PARAMS.canary_miss);
    assert_eq!(net(&e, "wrong-place"), PARAMS.canary_miss);
    assert!(credits_duty(&e, "sharp"));
    assert!(!credits_duty(&e, "lazy"));
}

#[test]
fn resolved_findings_grade_hits_misses_and_false_positives() {
    let loc = |s| Location {
        path: "x.rs".into(),
        start: s,
        end: s,
    };
    let truth = Truth::Resolved {
        confirmed: vec![loc(10)],
        rejected: vec![loc(80)],
    };
    let e = grade(
        &truth,
        &[
            review(
                "hit",
                vec![issue("x.rs", 11, IssueKind::Bug, Severity::High)],
            ),
            review("missed", vec![]),
            review(
                "noisy",
                vec![
                    issue("x.rs", 10, IssueKind::Bug, Severity::High),
                    issue("x.rs", 80, IssueKind::Bug, Severity::Low),
                ],
            ),
        ],
    );
    assert_eq!(net(&e, "hit"), PARAMS.confirmed_hit + PARAMS.completion);
    assert_eq!(net(&e, "missed"), PARAMS.confirmed_miss);
    assert_eq!(
        net(&e, "noisy"),
        PARAMS.confirmed_hit + PARAMS.false_positive + PARAMS.completion
    );
    let e = grade(&Truth::Unchallenged, &[review("ok", vec![])]);
    assert!(credits_duty(&e, "ok"));
}

fn claim(who: &str, outcome: RunOutcome) -> RunClaim {
    RunClaim {
        runner: who.into(),
        outcome,
    }
}

use collective_core::proof::RunOutcome;

#[test]
fn canary_runs_have_a_known_outcome() {
    let claims = [
        claim("ran", RunOutcome::Regressed),
        claim("skipped", RunOutcome::Passed),
        claim("broken", RunOutcome::Inconclusive),
    ];
    let e = grade_runs(Some(RunOutcome::Regressed), &claims);
    assert_eq!(net(&e, "ran"), PARAMS.run_canary_correct);
    assert_eq!(net(&e, "skipped"), PARAMS.run_canary_misreported);
    assert_eq!(net(&e, "broken"), 0.0);
    assert!(run_completes(RunOutcome::Regressed, &e, "ran"));
    assert!(!run_completes(RunOutcome::Passed, &e, "skipped"));
    // An inconclusive run costs nothing but doesn't complete the task.
    assert!(!run_completes(RunOutcome::Inconclusive, &e, "broken"));
}

#[test]
fn real_runs_are_checked_against_the_other_runners() {
    use RunOutcome::*;
    assert_eq!(
        run_majority(&[
            claim("a", Regressed),
            claim("b", Regressed),
            claim("c", Passed)
        ]),
        Some(Regressed)
    );
    // No majority: one each, a tie, or too few conclusive claims.
    assert_eq!(
        run_majority(&[claim("a", Regressed), claim("b", Passed), claim("c", Flaky)]),
        None
    );
    assert_eq!(
        run_majority(&[
            claim("a", Regressed),
            claim("b", Regressed),
            claim("c", Passed),
            claim("d", Passed)
        ]),
        None
    );
    assert_eq!(
        run_majority(&[
            claim("a", Passed),
            claim("b", Inconclusive),
            claim("c", Inconclusive)
        ]),
        None
    );

    let claims = [
        claim("a", Regressed),
        claim("b", Regressed),
        claim("liar", Passed),
        claim("x", Inconclusive),
    ];
    let e = grade_runs(None, &claims);
    assert_eq!(net(&e, "liar"), PARAMS.run_disagreed);
    assert_eq!(net(&e, "a") + net(&e, "b") + net(&e, "x"), 0.0);
    // Disagreeing costs rep but not the credit: majorities can be wrong.
    assert!(run_completes(Passed, &e, "liar"));
    assert!(grade_runs(None, &claims[..2]).is_empty());
}
