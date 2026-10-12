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
fn duty_scales_with_shards_and_adoptions_after_a_free_allowance() {
    // A shard costs its reviewers plus the canaries mixed in around them.
    assert_eq!(shard_cost(1), 4); // ⌈3 / 0.9⌉
    assert_eq!(shard_cost(10), 34); // ⌈30 / 0.9⌉
    assert_eq!(duty_owed(0, 4, 0), 0); // free adoptions
    assert_eq!(duty_owed(1, 0, 0), 0); // a first one-shard patch is within the allowance
    assert_eq!(duty_owed(10, 0, 0), 30); // a big patch pays for its reviews
    assert_eq!(duty_owed(2, 4, 0), 7); // ⌈6 / 0.9⌉ + 4 − 4
    assert_eq!(duty_owed(2, 4, 5), 2);
    assert_eq!(duty_owed(2, 4, 50), 0);
}

#[test]
fn review_supply_covers_demand_for_any_patch_size() {
    // Duty from a patch alone, before any adoptions, must pay for the
    // reviews it consumes, canaries included.
    for shards in 1..50 {
        let real = shards * PARAMS.redundancy as u32;
        let with_canaries = real as f64 / (1.0 - PARAMS.canary_percent as f64 / 100.0);
        assert!(shard_cost(shards) as f64 >= with_canaries, "{shards}");
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
