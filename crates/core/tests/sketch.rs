use collective_core::diff::{parse_diff, FileDiff};
use collective_core::sketch::*;

fn diff(path: &str, added: &[String]) -> Vec<FileDiff> {
    let mut text = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,{} @@\n ctx\n",
        added.len() + 1
    );
    for l in added {
        text.push_str(&format!("+{l}\n"));
    }
    parse_diff(&text).unwrap()
}

fn body() -> Vec<String> {
    (0..20)
        .map(|i| format!("    out.append(render_row(row, column_{i}))"))
        .collect()
}

#[test]
fn near_duplicates_score_high_and_share_a_band() {
    let a = sketch(&diff("src/list.py", &body())).unwrap();
    let mut tweaked = body();
    tweaked[7] = "    out.append(render_row(row, col7))".into();
    let b = sketch(&diff("src/list.py", &tweaked)).unwrap();
    let other: Vec<String> = (0..20)
        .map(|i| format!("    total += weight_{i} * value"))
        .collect();
    let c = sketch(&diff("src/list.py", &other)).unwrap();
    let sa = decode_signature(&a.signature).unwrap();
    assert!(estimate_jaccard(&sa, &decode_signature(&b.signature).unwrap()) > 0.6);
    assert!(estimate_jaccard(&sa, &decode_signature(&c.signature).unwrap()) < 0.2);
    assert!(a.bands.iter().any(|k| b.bands.contains(k)));
    assert!(!a.bands.iter().any(|k| c.bands.contains(k)));
}

#[test]
fn ignores_whitespace_and_directory_moves() {
    let a = sketch(&diff("src/list.py", &body())).unwrap();
    let moved: Vec<String> = body().iter().map(|l| format!("\t{}  ", l.trim())).collect();
    assert_eq!(
        a.signature,
        sketch(&diff("lib/v2/list.py", &moved)).unwrap().signature
    );
}

#[test]
fn estimates_jaccard_within_sampling_error() {
    let shared: Vec<String> = (0..60).map(|i| format!("s{i}")).collect();
    let x: Vec<String> = shared
        .iter()
        .cloned()
        .chain((0..40).map(|i| format!("x{i}")))
        .collect();
    let y: Vec<String> = shared
        .iter()
        .cloned()
        .chain((0..40).map(|i| format!("y{i}")))
        .collect();
    let est = estimate_jaccard(&signature(&x), &signature(&y));
    assert!((est - 60.0 / 140.0).abs() < 0.2, "{est}");
}

#[test]
fn whitespace_only_changes_have_no_features() {
    assert!(features(&diff("a.txt", &["   ".into(), "".into()])).is_empty());
}
