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
fn duty_scales_with_activity_after_a_free_allowance() {
    assert_eq!(duty_owed(0, 3, 0), 0); // free actions
    assert_eq!(duty_owed(1, 0, 0), 0); // a first submission is within the allowance
    assert_eq!(duty_owed(2, 4, 0), 7); // 2·3 + 4·1 − 3
    assert_eq!(duty_owed(2, 4, 5), 2);
    assert_eq!(duty_owed(2, 4, 50), 0);
}

#[test]
fn shards_bound_review_size_and_reassemble_into_valid_diffs() {
    let r = Repo::new(&[("a.txt", &lines(400, "line")), ("b.txt", &lines(10, "b"))]);
    let mut a = lines(400, "line");
    for i in (10..400).step_by(20) {
        a = a.replace(&format!("line {i}\n"), &format!("changed {i}\nadded {i}\n"));
    }
    let files = parse_diff(&r.diff(&[
        ("a.txt", Some(a)),
        ("b.txt", Some(lines(10, "b").replace("b 3\n", "bee\n"))),
    ]))
    .unwrap();
    let shards = shard_patch(&files, 20);
    assert!(shards.len() > 3);
    assert!(shards
        .iter()
        .all(|s| s.changed_lines <= 20 || s.hunks.len() == 1));
    assert_eq!(shards.last().unwrap().path, "b.txt");
    // Every shard is itself a diff git accepts.
    for s in &shards {
        let d = collective_core::diff::format_diff(&shard_diff(&files, s).unwrap());
        assert!(r.applies(&d), "{d}");
    }
    assert_ne!(shard_id("p", 0), shard_id("p", 1));
}

#[test]
fn task_picking_never_gives_authors_their_own_patch() {
    let open = vec![
        OpenShard {
            id: "mine".into(),
            author_operator: "me".into(),
            assigned: vec![],
            wanted: 3,
        },
        OpenShard {
            id: "done".into(),
            author_operator: "x".into(),
            assigned: vec!["a".into(), "b".into(), "c".into()],
            wanted: 3,
        },
        OpenShard {
            id: "seen".into(),
            author_operator: "x".into(),
            assigned: vec!["me".into()],
            wanted: 3,
        },
        OpenShard {
            id: "ok".into(),
            author_operator: "x".into(),
            assigned: vec![],
            wanted: 3,
        },
    ];
    for i in 0..20 {
        assert_eq!(pick_task("me", &open, &format!("s{i}")).unwrap().id, "ok");
    }
    assert!(pick_task("me", &open[..3], "s").is_none());
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
