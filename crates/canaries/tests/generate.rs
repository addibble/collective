#[path = "../../core/tests/common.rs"]
mod common;

use collective_canaries::*;
use collective_core::diff::parse_diff;
use collective_core::proof::Ecosystem;
use common::Repo;

fn selection() -> Selection {
    Selection {
        project: "https://github.com/example/svg".into(),
        ecosystem: Ecosystem::Cargo,
        source_extensions: vec!["rs".into()],
        sources: vec![],
    }
}

fn src(commit: &str, kind: SourceKind) -> Source {
    Source {
        commit: commit.into(),
        kind,
        reference: "example#1".into(),
        summary: "fix".into(),
    }
}

#[test]
fn source_paths_exclude_tests_and_build_scripts() {
    let ext = vec!["rs".to_string()];
    assert!(is_source_path("crates/resvg/src/filter/mod.rs", &ext));
    assert!(!is_source_path(
        "crates/resvg/tests/integration/render.rs",
        &ext
    ));
    assert!(!is_source_path("build.rs", &ext));
    assert!(!is_source_path("CHANGELOG.md", &ext));
}

#[test]
fn finds_added_rust_tests_in_both_styles() {
    let diff = "+++ b/tests/render.rs\n+#[test] fn filters_huge_region() { assert_eq!(render(\"x\"), 0); }\n+#[test]\n+fn markers_recursive() {}\n fn not_added() {}\n+fn helper() {}\n";
    assert_eq!(
        added_rust_tests(diff),
        ["filters_huge_region", "markers_recursive"]
    );
}

#[test]
fn planted_locations_cover_additions_and_removal_points() {
    let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,4 +10,3 @@\n ctx\n-    if too_big { return None; }\n ctx2\n ctx3\n@@ -30,2 +29,3 @@\n ctx\n+    unchecked();\n ctx\n";
    let locs = planted_locations(&parse_diff(diff).unwrap());
    assert_eq!((locs[0].start, locs[0].end), (10, 11)); // removed between new lines 10 and 11
    assert_eq!((locs[1].start, locs[1].end), (30, 30)); // added as new line 30
}

#[test]
fn reverses_a_fix_but_keeps_its_test() {
    let r = Repo::new(&[
        (
            "src/lib.rs",
            "pub fn area(w: u32, h: u32) -> u32 {\n    w * h\n}\n",
        ),
        ("tests/t.rs", ""),
    ]);
    let fixed = r.diff(&[
        ("src/lib.rs", Some("pub fn area(w: u32, h: u32) -> u32 {\n    w.saturating_mul(h)\n}\n".into())),
        ("tests/t.rs", Some("#[test] fn huge_area_does_not_panic() { assert_eq!(svg::area(u32::MAX, 2), u32::MAX); }\n".into())),
    ]);
    // Commit the fix so it becomes history.
    let apply = std::process::Command::new("git")
        .args(["apply", "-"])
        .current_dir(&r.dir)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    apply
        .stdin
        .as_ref()
        .unwrap()
        .write_all(fixed.as_bytes())
        .unwrap();
    assert!(apply.wait_with_output().unwrap().status.success());
    for args in [
        &["add", "-A"][..],
        &["commit", "-qm", "Fix overflow in area"][..],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&r.dir)
            .status()
            .unwrap()
            .success());
    }

    let c = generate(
        &r.dir,
        &selection(),
        &src("HEAD", SourceKind::RobustnessFix),
        200,
    )
    .unwrap();
    assert!(
        c.diff.contains("-    w.saturating_mul(h)") && c.diff.contains("+    w * h"),
        "{}",
        c.diff
    );
    assert!(!c.diff.contains("tests/t.rs"), "tests must stay");
    assert_eq!(c.tests, ["huge_area_does_not_panic"]);
    assert_eq!(c.planted[0].path, "src/lib.rs");
    assert_eq!((c.planted[0].start, c.planted[0].end), (2, 2));
    assert!(
        !c.intent.to_lowercase().contains("fix") && !c.intent.contains("overflow"),
        "cover must not reveal: {}",
        c.intent
    );

    // A commit with no tests cannot be verified, so it is skipped.
    assert!(generate(
        &r.dir,
        &selection(),
        &src("HEAD~1", SourceKind::Feature),
        200
    )
    .is_err());
}
