mod common;

use collective_core::converge::{converge, Candidate, ConvergeOptions};
use collective_core::diff::*;
use collective_core::score::{score_patch, trust, Adoption, Outcome};
use collective_core::text::{similarity, terms};
use common::{lines, Repo};

fn fixture() -> Repo {
    Repo::new(&[
        ("src/a.txt", &lines(60, "line")),
        ("src/b.txt", &lines(20, "b")),
    ])
}

#[test]
fn round_trips_git_output() {
    let r = fixture();
    let files = parse_diff(&r.edit_line("src/a.txt", 10, "changed")).unwrap();
    assert_eq!(files.len(), 1);
    assert!(r.applies(&format_diff(&files)));
}

#[test]
fn handles_created_and_deleted_files() {
    let r = Repo::new(&[("old.txt", "bye\n"), ("keep.txt", "x\n")]);
    let files = parse_diff(&r.diff(&[
        ("new/file.txt", Some("hello\nworld\n".into())),
        ("old.txt", None),
    ]))
    .unwrap();
    let paths: Vec<_> = files
        .iter()
        .map(|f| (f.old_path.clone(), f.new_path.clone()))
        .collect();
    assert!(paths.contains(&(None, Some("new/file.txt".into()))));
    assert!(paths.contains(&(Some("old.txt".into()), None)));
    assert!(r.applies(&format_diff(&files)));
}

#[test]
fn combines_non_overlapping_patches_that_git_applies() {
    let r = fixture();
    let p1 = parse_diff(&r.edit_line("src/a.txt", 10, "one")).unwrap();
    let p2 = parse_diff(&r.diff(&[(
        "src/a.txt",
        Some(lines(60, "line").replace("line 40\n", "line 40\ninserted a\ninserted b\n")),
    )]))
    .unwrap();
    let p3 = parse_diff(&r.edit_line("src/b.txt", 5, "three")).unwrap();
    assert!(find_conflict(&p1, &p2).is_none());
    let merged = format_diff(&combine(&[&p2, &p1, &p3]));
    assert!(r.applies(&merged), "{merged}");
    assert!(r.read("src/a.txt").contains("one\n"));
    assert!(r.read("src/a.txt").contains("inserted b\n"));
    assert!(r.read("src/b.txt").contains("three\n"));
}

#[test]
fn flags_overlapping_edits() {
    let r = fixture();
    let p1 = parse_diff(&r.edit_line("src/a.txt", 10, "one")).unwrap();
    let p2 = parse_diff(&r.edit_line("src/a.txt", 12, "two")).unwrap();
    assert_eq!(find_conflict(&p1, &p2).unwrap().path, "src/a.txt");
}

#[test]
fn fingerprints_ignore_position_and_whitespace() {
    let r = Repo::new(&[("f.txt", &lines(30, "line"))]);
    let a = parse_diff(&r.diff(&[(
        "f.txt",
        Some(lines(30, "line").replace("line 5\n", "line 5\n  foo(bar)\n")),
    )]))
    .unwrap();
    let b = parse_diff(&r.diff(&[(
        "f.txt",
        Some(lines(30, "line").replace("line 20\n", "line 20\nfoo(bar)\n")),
    )]))
    .unwrap();
    assert_eq!(
        hunk_fingerprints(&a)[0].fingerprint,
        hunk_fingerprints(&b)[0].fingerprint
    );
    assert_ne!(patch_id(&r.base, &a), patch_id(&r.base, &b));
}

#[test]
fn rejects_garbage() {
    assert!(parse_diff("hello").is_err());
}

#[test]
fn matches_reworded_intents_above_unrelated_ones() {
    let q = terms("Add a --json flag to the list command output");
    let close = terms("list command: support JSON output format");
    let far = terms("fix crash when config file is missing");
    assert!(similarity(&q, &close, |_| 1.0) > similarity(&q, &far, |_| 1.0));
    assert_eq!(similarity(&q, &far, |_| 1.0), 0.0);
}

fn adoption(op: &str, outcome: Outcome, created_at: i64) -> Adoption {
    Adoption {
        operator_id: op.into(),
        outcome,
        created_at,
        share: 1.0,
    }
}

#[test]
fn scoring_ignores_self_caps_operators_and_decays() {
    let t = 1_000_000_000_000;
    assert_eq!(
        score_patch(
            "me",
            &[adoption("me", Outcome::Shipped, t)],
            |_| trust(0.0),
            t
        )
        .score,
        0.0
    );
    let spam: Vec<_> = (0..50)
        .map(|_| adoption("x", Outcome::Shipped, t))
        .collect();
    let s = score_patch("me", &spam, |_| 1.0, t);
    assert_eq!((s.score, s.adopters), (2.0, 1));
    let s = score_patch(
        "me",
        &[
            adoption("a", Outcome::TestsPassed, t),
            adoption("b", Outcome::Failed, t),
            adoption("c", Outcome::TestsPassed, t - 90 * 86_400_000),
        ],
        |_| 1.0,
        t,
    );
    assert!((s.score - 0.5).abs() < 1e-9);
    assert_eq!(s.adopters, 2);
    assert_eq!(trust(0.0), 0.25);
    assert!(trust(5.0) > 0.99);
}

#[test]
fn converge_stacks_compatible_winners_and_explains_skips() {
    let r = fixture();
    let mk = |id: &str, intent: &str, score: f64, diff: String, parent: Option<&str>| Candidate {
        id: id.into(),
        intent_id: intent.into(),
        parent_id: parent.map(String::from),
        score,
        adopters: 3,
        files: parse_diff(&diff).unwrap(),
    };
    let trunk = converge(
        &[
            mk(
                "p-top",
                "i1",
                5.0,
                r.edit_line("src/a.txt", 10, "top"),
                None,
            ),
            mk(
                "p-rival",
                "i1",
                4.0,
                r.edit_line("src/a.txt", 50, "rival"),
                None,
            ),
            mk(
                "p-clash",
                "i2",
                3.0,
                r.edit_line("src/a.txt", 11, "clash"),
                None,
            ),
            mk(
                "p-other",
                "i3",
                2.0,
                r.edit_line("src/b.txt", 3, "other"),
                None,
            ),
            mk(
                "p-child",
                "i4",
                1.5,
                r.edit_line("src/a.txt", 30, "child"),
                Some("p-top"),
            ),
            mk(
                "p-weak",
                "i5",
                0.2,
                r.edit_line("src/a.txt", 55, "weak"),
                None,
            ),
        ],
        ConvergeOptions::default(),
    );
    assert_eq!(trunk.patch_ids, ["p-top", "p-other"]);
    let reason = |id: &str| {
        trunk
            .skipped
            .iter()
            .find(|s| s.patch_id == id)
            .unwrap()
            .reason
            .clone()
    };
    assert!(reason("p-rival").contains("intent"));
    assert!(reason("p-clash").contains("conflicts"));
    assert!(reason("p-child").contains("lineage"));
    assert!(reason("p-weak").contains("threshold"));
    assert!(r.applies(&trunk.diff));
}
