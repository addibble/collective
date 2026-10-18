mod common;

use collective_core::amend::*;
use collective_core::diff::parse_diff;
use collective_core::net;
use collective_core::review::PARAMS as REVIEW;
use common::{lines, Repo};

fn perf(reasoning: &str) -> Evidence {
    Evidence::Performance {
        resource: Resource::Cpu,
        reasoning: reasoning.into(),
    }
}

fn correct(url: &str, quote: &str) -> Evidence {
    Evidence::Correctness {
        reference: Reference {
            url: url.into(),
            section: "4.2".into(),
            quote: quote.into(),
        },
        reasoning: "The patch rounds half away from zero; the spec says half to even.".into(),
    }
}

#[test]
fn evidence_must_have_the_shape_its_category_demands() {
    assert!(check_evidence(&perf(
        "Replaces a Vec clone per call with a slice borrow; nothing else changes."
    ))
    .is_ok());
    assert!(
        check_evidence(&perf("faster")).is_err(),
        "a word is not reasoning"
    );
    assert!(check_evidence(&perf(&"x".repeat(MAX_REASONING_CHARS + 1))).is_err());
    let ok = correct(
        "https://www.rfc-editor.org/rfc/rfc8259#section-6",
        "numeric values that cannot be represented",
    );
    assert!(check_evidence(&ok).is_ok());
    assert!(
        check_evidence(&correct("", "quoted")).is_err(),
        "a reference needs a URL"
    );
    assert!(check_evidence(&correct("ftp://example.com/spec", "quoted")).is_err());
    assert!(check_evidence(&correct("https://example.com/a b", "quoted")).is_err());
    assert!(
        check_evidence(&correct("https://example.com/spec", "  ")).is_err(),
        "a reference needs the passage"
    );
    assert!(check_evidence(&correct(
        "https://example.com/spec",
        &"q".repeat(MAX_QUOTE_CHARS + 1)
    ))
    .is_err());
    // The tag is the category, so JSON from the API round-trips.
    let v = serde_json::to_value(&ok).unwrap();
    assert_eq!(v["category"], "correctness");
    assert_eq!(
        serde_json::from_value::<Evidence>(v).unwrap().category(),
        Category::Correctness
    );
}

/// A patch and an amended version of it, as git produces them against the
/// same base; the fix is applied on top of the patch's edits.
fn pair(
    repo: &Repo,
    src: &str,
    patched: &str,
    amended: &str,
) -> (
    Vec<collective_core::diff::FileDiff>,
    Vec<collective_core::diff::FileDiff>,
) {
    let p = repo.diff(&[("app.py", Some(patched.replace("SRC", src)))]);
    let a = repo.diff(&[("app.py", Some(amended.replace("SRC", src)))]);
    (parse_diff(&p).unwrap(), parse_diff(&a).unwrap())
}

#[test]
fn an_amendment_is_the_patch_with_a_small_fix_on_top() {
    let src = lines(60, "x");
    let repo = Repo::new(&[("app.py", &src), ("other.py", &lines(10, "o"))]);
    // The patch adds a function near line 20 and tweaks line 50.
    let patched = src.replace("x 20\n", "x 20\ndef total(xs):\n    s = 0\n    for x in list(xs):\n        s += x\n    return s\n").replace("x 50\n", "x fifty\n");
    // The amendment drops a needless copy inside the added function.
    let amended = patched.replace("    for x in list(xs):\n", "    for x in xs:\n");
    let (p, a) = pair(&repo, "", &patched, &amended);
    let c = check_amendment(&p, &a).unwrap();
    assert_eq!(c.lines, 2, "one line out, one line in");
    assert_eq!(c.deltas.len(), 1);
    assert_eq!(c.deltas[0].path, "app.py");
    assert_eq!(c.deltas[0].replaced.len(), 1, "the hunk the fix rewrites");
    assert_eq!(c.deltas[0].amended.len(), 1);
    assert!(c.deltas[0].amended[0]
        .lines
        .iter()
        .any(|l| l == "+    for x in xs:"));
    // The hunk at line 50 is untouched: it is neither replaced nor amended.
    assert!(!c.deltas[0]
        .replaced
        .iter()
        .any(|h| h.lines.iter().any(|l| l == "+x fifty")));

    // The patch itself amends nothing.
    assert!(check_amendment(&p, &p)
        .unwrap_err()
        .contains("nothing changes"));
    // Dropping the patch's other edit is not a fix on top of it.
    let a2 = repo.diff(&[("app.py", Some(amended.replace("x fifty\n", "x 50\n")))]);
    let c2 = check_amendment(&p, &parse_diff(&a2).unwrap()).unwrap();
    assert_eq!(c2.lines, 4, "the dropped edit counts against the budget");
    // A file the patch doesn't change is out of scope.
    let a3 = repo.diff(&[
        ("app.py", Some(amended.clone())),
        (
            "other.py",
            Some(lines(10, "o").replace("o 5\n", "o five\n")),
        ),
    ]);
    assert!(check_amendment(&p, &parse_diff(&a3).unwrap())
        .unwrap_err()
        .contains("other.py"));
    // Leaving a patched file as in the base drops the patch's change there.
    let p2 = repo.diff(&[
        ("app.py", Some(patched.clone())),
        (
            "other.py",
            Some(lines(10, "o").replace("o 5\n", "o five\n")),
        ),
    ]);
    assert!(check_amendment(&parse_diff(&p2).unwrap(), &a)
        .unwrap_err()
        .contains("leaves other.py"));
    // A fix that only re-spaces the patch's lines changes no code.
    let a4 = repo.diff(&[(
        "app.py",
        Some(patched.replace("    s = 0\n", "    s  =  0\n")),
    )]);
    assert!(check_amendment(&p, &parse_diff(&a4).unwrap())
        .unwrap_err()
        .contains("nothing changes"));
    // The same lines in another order is a change to the whole hunk.
    let a5 = repo.diff(&[(
        "app.py",
        Some(patched.replace(
            "    s = 0\n    for x in list(xs):\n",
            "    for x in list(xs):\n    s = 0\n",
        )),
    )]);
    let c5 = check_amendment(&p, &parse_diff(&a5).unwrap()).unwrap();
    assert_eq!(c5.lines, 10, "five added lines, out and in");
}

#[test]
fn an_amendment_that_rewrites_the_patch_is_a_patch_of_its_own() {
    let src = lines(600, "x");
    let repo = Repo::new(&[("app.py", &src)]);
    let mut patched = src.clone();
    for i in (10..310).step_by(2) {
        patched = patched.replace(&format!("x {i}\n"), &format!("y {i}\n"));
    }
    let mut amended = patched.clone();
    for i in (10..310).step_by(2) {
        amended = amended.replace(&format!("y {i}\n"), &format!("z {i}\n"));
    }
    let (p, a) = pair(&repo, "", &patched, &amended);
    let e = check_amendment(&p, &a).unwrap_err();
    assert!(e.contains(&format!("at most {MAX_AMENDMENT_LINES}")), "{e}");
    // Changing the created/deleted shape of a file is not amending it.
    let created = repo.diff(&[("new.py", Some("print(1)\n".into()))]);
    let edited = repo.diff(&[("app.py", Some(src.replace("x 1\n", "x one\n")))]);
    let e = check_amendment(
        &parse_diff(&created).unwrap(),
        &parse_diff(&edited).unwrap(),
    )
    .unwrap_err();
    assert!(e.contains("app.py"), "{e}");
}

#[test]
fn binary_changes_pass_through_or_fail() {
    let repo = Repo::new(&[("app.py", "a\n"), ("logo.png", "\u{0}old")]);
    let p = repo.diff_bytes(&[("app.py", Some(b"a\nb\n")), ("logo.png", Some(b"\x00new"))]);
    let a = repo.diff_bytes(&[
        ("app.py", Some(b"a\nb\nc\n")),
        ("logo.png", Some(b"\x00new")),
    ]);
    let (p, a) = (parse_diff(&p).unwrap(), parse_diff(&a).unwrap());
    let c = check_amendment(&p, &a).unwrap();
    assert_eq!(c.lines, 1);
    assert_eq!(
        c.deltas.len(),
        1,
        "the unchanged binary file is not a delta"
    );
    let other = repo.diff_bytes(&[
        ("app.py", Some(b"a\nb\nc\n")),
        ("logo.png", Some(b"\x00other")),
    ]);
    assert!(check_amendment(&p, &parse_diff(&other).unwrap())
        .unwrap_err()
        .contains("binary"));
}

fn vote(who: &str, v: Verdict) -> Vote {
    Vote {
        voter: who.into(),
        verdict: v,
    }
}

#[test]
fn a_quorum_of_the_panel_decides_and_a_split_panel_lapses() {
    use Verdict::*;
    assert_eq!(REVIEW.quorum, 2, "the cases below assume the review quorum");
    assert_eq!(decide(&[], false), State::Pending);
    assert_eq!(decide(&[vote("b", Endorse)], false), State::Pending);
    assert_eq!(
        decide(&[vote("b", Endorse), vote("c", Endorse)], false),
        State::Accepted
    );
    assert_eq!(
        decide(&[vote("b", Reject), vote("c", Reject)], false),
        State::Rejected
    );
    assert_eq!(
        decide(&[vote("b", Endorse), vote("c", Reject)], false),
        State::Pending
    );
    assert_eq!(
        decide(&[vote("b", Endorse), vote("c", Reject)], true),
        State::Lapsed
    );
    assert_eq!(decide(&[vote("b", Endorse)], true), State::Lapsed);
    assert_eq!(decide(&[], true), State::Lapsed);
    // A voter counts once, whatever it sends.
    assert_eq!(
        decide(&[vote("b", Endorse), vote("b", Endorse)], false),
        State::Pending
    );
    // A wide panel: endorsements must outnumber rejections, and vice versa.
    assert_eq!(
        decide(
            &[
                vote("b", Endorse),
                vote("c", Endorse),
                vote("d", Reject),
                vote("e", Reject),
                vote("f", Reject)
            ],
            false
        ),
        State::Rejected
    );
    assert_eq!(
        decide(
            &[
                vote("b", Endorse),
                vote("c", Endorse),
                vote("d", Reject),
                vote("e", Reject)
            ],
            false
        ),
        State::Pending
    );
    assert_eq!(
        decide(
            &[
                vote("b", Endorse),
                vote("c", Endorse),
                vote("d", Reject),
                vote("e", Reject)
            ],
            true
        ),
        State::Lapsed,
        "a tie is no verdict"
    );
    assert_eq!(
        decide(
            &[
                vote("b", Endorse),
                vote("c", Endorse),
                vote("d", Endorse),
                vote("e", Reject),
                vote("f", Reject)
            ],
            false
        ),
        State::Accepted
    );
}

#[test]
fn only_the_rest_of_the_panel_votes_and_only_verdicts_pay() {
    let panel = vec!["a".to_string(), "b".to_string()];
    assert!(may_vote("b", "a", &panel).is_ok());
    assert!(may_vote("a", "a", &panel).is_err(), "not your own");
    assert!(may_vote("z", "a", &panel).is_err(), "not off the panel");
    assert_eq!(net(&entries(State::Accepted, "a"), "a"), ACCEPTED_REP);
    assert_eq!(net(&entries(State::Rejected, "a"), "a"), REJECTED_REP);
    assert!(entries(State::Lapsed, "a").is_empty());
    assert!(entries(State::Pending, "a").is_empty());
    assert_eq!(
        amended_title("Add totals", Category::Performance),
        "Add totals (amended: performance)"
    );
    assert!(amended_title(&"t".repeat(300), Category::Correctness).len() <= 200);
    assert_eq!(Category::parse("correctness"), Some(Category::Correctness));
    assert_eq!(State::parse("lapsed"), Some(State::Lapsed));
}
