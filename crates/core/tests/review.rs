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
fn binary_files_count_towards_size_and_are_reported_whole() {
    let r = Repo::new(&[("a.txt", &lines(10, "a"))]);
    // Incompressible bytes: the encoded payload grows with the file.
    let noise = |len: usize| {
        let mut x = 0x2545_f491u32;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect::<Vec<u8>>()
    };
    let patch = |len: usize| {
        parse_diff(&r.diff_bytes(&[
            ("img.png", Some(&noise(len)[..])),
            (
                "a.txt",
                Some(lines(10, "a").replace("a 3\n", "ay\n").as_bytes()),
            ),
        ]))
        .unwrap()
    };
    let small = patch(5_200);
    let n = changed_lines(&small);
    assert!((100..=110).contains(&(n - 2)), "{n}: 52 bytes per line");
    assert_eq!(check_task_size(&small), Ok(n));
    assert!(check_task_size(&patch(200_000))
        .unwrap_err()
        .contains("at most 2000"));

    let issue = |path: &str, start, end| Issue {
        path: path.into(),
        start,
        end,
        kind: IssueKind::Backdoor,
        severity: Severity::Critical,
        note: String::new(),
    };
    assert!(check_issue_location(&small, &issue("a.txt", 3, 3)).is_ok());
    assert!(check_issue_location(&small, &issue("a.txt", 0, 3)).is_err());
    assert!(check_issue_location(&small, &issue("a.txt", 4, 3)).is_err());
    assert!(check_issue_location(&small, &issue("other.txt", 1, 1)).is_err());
    assert!(check_issue_location(&small, &issue("img.png", 1, 1)).is_ok());
    let e = check_issue_location(&small, &issue("img.png", 5, 9)).unwrap_err();
    assert!(e.contains("binary file"), "{e}");
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
            vouched_by: None,
        },
        OpenShard {
            id: "done".into(),
            author_operator: "x".into(),
            assigned: vec!["a".into(), "b".into(), "c".into()],
            wanted: 3,
            canary: false,
            vouched_by: None,
        },
        OpenShard {
            id: "seen".into(),
            author_operator: "x".into(),
            assigned: vec!["me".into()],
            wanted: 3,
            canary: false,
            vouched_by: None,
        },
        OpenShard {
            id: "ok".into(),
            author_operator: "x".into(),
            assigned: vec![],
            wanted: 3,
            canary: false,
            vouched_by: None,
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
        vouched_by: None,
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
        cpu_ms: None,
    }
}

#[test]
fn runs_are_worth_their_median_sandbox_cpu_time() {
    let min = 60_000;
    assert_eq!(run_units(1), 1);
    assert_eq!(run_units(10 * min), 1);
    assert_eq!(run_units(10 * min + 1), 2);
    assert_eq!(run_units(45 * min), 5);
    assert_eq!(run_units(1000 * min), PARAMS.max_run_units);
    let c = |who: &str, outcome, cpu| RunClaim {
        runner: who.into(),
        outcome,
        cpu_ms: cpu,
    };
    use RunOutcome::*;
    // One runner inflating its figure doesn't move the median.
    let claims = [
        c("a", Passed, Some(5 * min)),
        c("b", Passed, Some(7 * min)),
        c("liar", Passed, Some(900 * min)),
    ];
    assert_eq!(median_cpu_ms(&claims), Some(7 * min));
    // Inconclusive runs and missing figures don't count; one figure isn't enough.
    let claims = [
        c("a", Passed, Some(5 * min)),
        c("b", Inconclusive, Some(900 * min)),
        c("c", Passed, None),
    ];
    assert_eq!(median_cpu_ms(&claims), None);
    // With two, the lower one.
    assert_eq!(
        median_cpu_ms(&[c("a", Regressed, Some(3)), c("b", Regressed, Some(9))]),
        Some(3)
    );
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

#[test]
fn contributed_canaries_must_prove_fair() {
    assert_eq!(canary_fairness(0, 0), CanaryFairness::Pending);
    assert_eq!(canary_fairness(1, 5), CanaryFairness::Pending);
    assert_eq!(canary_fairness(2, 9), CanaryFairness::Fair);
    assert_eq!(canary_fairness(1, 6), CanaryFairness::Unfair);
    // Only reviewers with a record on platform canaries can sink one.
    assert!(proven_reviewer(2, 0));
    assert!(proven_reviewer(8, 2));
    assert!(!proven_reviewer(1, 0));
    assert!(!proven_reviewer(5, 3));
    assert_eq!(canary_credit(1), 3);
}

#[test]
fn contributed_canaries_pay_less_and_stay_off_the_jury_record() {
    let planted = vec![Location {
        path: "x.rs".into(),
        start: 10,
        end: 10,
    }];
    let sharp = review(
        "sharp",
        vec![issue("x.rs", 10, IssueKind::Bug, Severity::Medium)],
    );
    let lazy = review("lazy", vec![]);
    assert!(catches(&planted, &sharp));
    assert!(!catches(&planted, &lazy));
    let e = grade(&Truth::ContributedCanary { planted }, &[sharp, lazy]);
    assert_eq!(
        net(&e, "sharp"),
        PARAMS.contributed_canary_catch + PARAMS.completion
    );
    assert_eq!(net(&e, "lazy"), PARAMS.canary_miss);
    assert!(!credits_duty(&e, "lazy"));
    // The jury's canary record counts "caught a canary" and "missed a canary" only.
    assert!(e
        .iter()
        .all(|x| x.reason != "caught a canary" && x.reason != "missed a canary"));
}

// ---------------------------------------------------------------------------
// Newcomers: pending tasks and vouching
// ---------------------------------------------------------------------------

use collective_core::proof::RunOutcome as Run;
use collective_core::vouch::*;

#[test]
fn a_task_opens_only_for_an_author_with_standing() {
    assert_eq!(
        MIN_AUTHOR_TRUST,
        collective_core::environment::MIN_PROPOSER_TRUST
    );
    let newcomer = collective_core::score::trust(0.0);
    assert!(!task_opens(newcomer), "a newcomer's task waits");
    assert!(!task_opens(0.49));
    assert!(task_opens(MIN_AUTHOR_TRUST));
    assert!(task_opens(1.0));
    assert!(!task_opens(f64::NAN), "fails closed");
}

#[test]
fn vouching_needs_standing_after_the_loan_and_spendable_rep() {
    assert!(may_vouch(0.8, 25.0));
    assert!(may_vouch(MIN_AUTHOR_TRUST, VOUCH_LOAN_REP));
    assert!(!may_vouch(0.49, 100.0), "no standing once the loan is out");
    assert!(
        !may_vouch(0.9, VOUCH_LOAN_REP - 0.01),
        "genesis-only standing can't be lent"
    );
    assert!(!may_vouch(f64::NAN, 100.0));
    assert!(!may_vouch(0.9, f64::NAN));
    assert!(!may_vouch_for("op", "op"), "never for your own operator");
    assert!(may_vouch_for("founder", "newcomer"));
}

#[test]
fn a_loan_is_debited_returned_or_lost_and_never_earns() {
    let made = vouch_entries(VouchEvent::Made, "f");
    assert_eq!(
        made,
        vec![collective_core::LedgerEntry::new(
            "f",
            -VOUCH_LOAN_REP,
            VOUCH_LOAN
        )]
    );
    let back = vouch_entries(VouchEvent::Returned, "f");
    assert_eq!(back[0].reason, VOUCH_RETURNED);
    // Made then returned nets exactly zero: vouching is a favour, not income.
    assert_eq!(net(&[made.clone(), back].concat(), "f"), 0.0);
    // Made then lost: the debit stands.
    let lost = vouch_entries(VouchEvent::Lost, "f");
    assert!(lost.is_empty());
    assert_eq!(net(&[made, lost].concat(), "f"), -VOUCH_LOAN_REP);
    assert_eq!(vouch_ref("abc"), "vouch:abc");
}

#[test]
fn a_voucher_is_an_interested_party_and_never_reviews_the_task() {
    let i = interested("author", Some("founder"));
    assert!(i.contains("author") && i.contains("founder") && i.len() == 2);
    assert_eq!(interested("author", None).len(), 1);

    let open = vec![
        OpenShard {
            id: "vouched".into(),
            author_operator: "newcomer".into(),
            assigned: vec![],
            wanted: 3,
            canary: false,
            vouched_by: Some("founder".into()),
        },
        OpenShard {
            id: "other".into(),
            author_operator: "x".into(),
            assigned: vec![],
            wanted: 3,
            canary: false,
            vouched_by: None,
        },
    ];
    for i in 0..20 {
        let seed = format!("s{i}");
        assert_eq!(pick_task("founder", &open, &seed).unwrap().id, "other");
    }
    assert!(pick_task("founder", &open[..1], "s").is_none());
    assert!((0..20).any(|i| pick_task("r", &open, &format!("s{i}")).unwrap().id == "vouched"));
}

#[test]
fn a_loan_settles_when_the_task_resolves() {
    let facts = |found_bad, challenge_open, review, runs| VouchFacts {
        found_bad,
        challenge_open,
        review,
        runs,
    };
    use TaskReview::*;
    use TaskRuns::*;
    // Found bad by any challenge: lost, whatever else is going on.
    for review in [Underway, Clean, ChallengeCleared] {
        assert_eq!(
            settle_vouch(&facts(true, true, review, Pending)),
            Some(VouchEvent::Lost)
        );
    }
    // Waits while anything is undecided.
    assert_eq!(settle_vouch(&facts(false, false, Underway, NotOwed)), None);
    assert_eq!(settle_vouch(&facts(false, true, Clean, NotOwed)), None);
    assert_eq!(settle_vouch(&facts(false, false, Clean, Pending)), None);
    assert_eq!(
        settle_vouch(&facts(false, false, Clean, Settled(Some(Run::Regressed)))),
        None,
        "a regression majority's challenge decides it"
    );
    // Resolved without the patch being found bad: returned.
    for runs in [NotOwed, Settled(Some(Run::Passed)), Settled(None)] {
        assert_eq!(
            settle_vouch(&facts(false, false, Clean, runs)),
            Some(VouchEvent::Returned)
        );
    }
    assert_eq!(
        settle_vouch(&facts(false, false, ChallengeCleared, Pending)),
        Some(VouchEvent::Returned),
        "a cleared challenge was adjudicated"
    );
}
