//! Canary generation from an upstream project's own history.
//!
//! A canary is a known-bad patch planted in the review queue (see
//! docs/REVIEWS.md). Inventing bad patches gives no ground truth; reversing
//! documented changes does. For a commit that fixed a bug or added a
//! feature, the canary is the *reverse* of its source changes, applied to
//! the commit itself:
//!
//! - reversing a robustness fix reintroduces the crash or hang it fixed;
//! - reversing a feature silently removes standards support it added.
//!
//! The commit's own tests are kept, so each canary can be checked as a
//! `regression` proof: those tests pass on the base and fail with the
//! canary. Only canaries that verify are planted.
//!
//! Canaries are inert by construction: they live only in the review queue,
//! are never adoptable, searchable or merged, and their provenance is never
//! shown to reviewers.

use std::path::Path;
use std::process::Command;

use collective_core::diff::{format_diff, parse_diff, stats, FileDiff};
use collective_core::hash::sha256_hex;
use collective_core::review::Location;
use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A fix for a crash, hang or other bug; reverting it reintroduces the bug.
    BugFix,
    /// A fix for a crash, hang or resource blow-up on hostile input: the
    /// denial-of-service class for libraries that process untrusted files.
    RobustnessFix,
    /// A feature or standards-alignment change; reverting it removes it.
    Feature,
}

/// One upstream commit to turn into a canary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub commit: String,
    pub kind: SourceKind,
    /// Where the change is documented: a PR, issue or advisory.
    pub reference: String,
    pub summary: String,
}

/// A list of sources for one project, kept in the repository (canaries/*.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub project: String,
    pub ecosystem: collective_core::proof::Ecosystem,
    /// File extensions that count as source (others, and anything under a
    /// test directory, are left alone).
    pub source_extensions: Vec<String>,
    pub sources: Vec<Source>,
}

/// Never shown to reviewers: stored server-side with the canary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub kind: SourceKind,
    pub source_commit: String,
    pub reference: String,
    pub summary: String,
    pub generator_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Canary {
    pub project: String,
    pub base_commit: String,
    /// Neutral cover description, as an ordinary patch would have.
    pub intent: String,
    pub diff: String,
    pub planted: Vec<Location>,
    /// The project's tests that the source commit added; they verify the canary.
    pub tests: Vec<String>,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<collective_core::proof::Assessment>,
}

fn under_test_dir(path: &str) -> bool {
    path.split('/').any(|seg| {
        matches!(
            seg,
            "test" | "tests" | "benches" | "examples" | "fuzz" | "testdata"
        )
    })
}

pub fn is_source_path(path: &str, extensions: &[String]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    !under_test_dir(path)
        && name != "build.rs"
        && extensions
            .iter()
            .any(|ext| name.ends_with(&format!(".{ext}")))
}

/// Where the planted problem is, in new-side line numbers of the patched
/// file: the lines a hunk adds, or, where it only removes, the point of
/// removal. This is what a reviewer must point at to catch the canary.
pub fn planted_locations(files: &[FileDiff]) -> Vec<Location> {
    let mut out = Vec::new();
    for f in files {
        let Some(path) = f.new_path.clone() else {
            continue;
        };
        for h in &f.hunks {
            let mut line = h.new_start;
            let mut span: Option<(u32, u32)> = None;
            let mut widen =
                |a: u32, b: u32| span = Some(span.map_or((a, b), |(s, e)| (s.min(a), e.max(b))));
            // Walk change blocks: runs of '-' and '+' lines between context.
            let mut i = 0;
            while i < h.lines.len() {
                let tag = h.lines[i].as_bytes().first().copied();
                if tag == Some(b' ') {
                    line += 1;
                    i += 1;
                    continue;
                }
                if tag != Some(b'-') && tag != Some(b'+') {
                    i += 1;
                    continue;
                }
                let (start, mut added) = (line, 0u32);
                while i < h.lines.len()
                    && matches!(h.lines[i].as_bytes().first(), Some(b'-' | b'+' | b'\\'))
                {
                    if h.lines[i].starts_with('+') {
                        added += 1;
                        line += 1;
                    }
                    i += 1;
                }
                if added > 0 {
                    widen(start, start + added - 1);
                } else {
                    // A pure removal sits between new lines start-1 and start.
                    widen(start.saturating_sub(1).max(1), start);
                }
            }
            if let Some((start, end)) = span {
                out.push(Location {
                    path: path.clone(),
                    start,
                    end,
                });
            }
        }
    }
    out
}

/// Names of `#[test]` functions added by a diff (Rust test convention).
pub fn added_rust_tests(diff_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev_attr = false;
    for line in diff_text.lines() {
        let Some(body) = line.strip_prefix('+').filter(|_| !line.starts_with("+++")) else {
            prev_attr = false;
            continue;
        };
        let has_attr = body.contains("#[test]");
        if has_attr || prev_attr {
            if let Some(rest) = body.split("fn ").nth(1) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        prev_attr = has_attr && !body.contains("fn ");
    }
    out
}

/// Rust tests at `commit` that refer to a fixture file, found by the
/// fixture's last path components without extension (how fixture-driven
/// suites such as resvg's name their inputs).
pub fn tests_using_fixture(
    repo: &Path,
    commit: &str,
    fixture: &str,
) -> Result<Vec<String>, String> {
    let without_ext = fixture.rsplit_once('.').map_or(fixture, |(stem, _)| stem);
    let parts: Vec<&str> = without_ext.rsplit('/').take(2).collect();
    let needle = format!("{}/{}\"", parts.get(1).copied().unwrap_or(""), parts[0]);
    let out = Command::new("git")
        .args(["grep", "-h", "-F", &needle, commit, "--", "*.rs"])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    // git grep exits 1 when nothing matches.
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|l| l.split("fn ").nth(1))
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|n| !n.is_empty())
        .collect())
}

/// A neutral, ordinary-looking description: reviewers must find the
/// problem in the code, not in the label.
pub fn cover_intent(files: &[FileDiff], seed: &str) -> String {
    let area = files
        .first()
        .map(|f| {
            let p = f.path();
            let stem = p.rsplit('/').next().unwrap_or(p).trim_end_matches(".rs");
            if stem == "mod" || stem == "lib" {
                // .../<crate>/src/lib.rs names the crate; .../<module>/mod.rs the module.
                let mut parents = p.rsplit('/').skip(1);
                match parents.next() {
                    Some("src") => parents.next().unwrap_or(stem).to_string(),
                    Some(dir) => dir.to_string(),
                    None => stem.to_string(),
                }
            } else {
                stem.to_string()
            }
        })
        .unwrap_or_else(|| "core".into())
        .replace(['_', '-'], " ");
    const TEMPLATES: [&str; 5] = [
        "Simplify {} handling",
        "Clean up {} code path",
        "Streamline {} logic",
        "Tidy {} implementation",
        "Reduce complexity in {}",
    ];
    let i = u32::from_str_radix(&sha256_hex(seed)[..4], 16).unwrap_or(0) as usize % TEMPLATES.len();
    TEMPLATES[i].replace("{}", &area)
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// Builds one canary from one upstream commit. Errors say why a source was
/// skipped (no source changes, too large, no tests to verify it with).
pub fn generate(
    repo: &Path,
    sel: &Selection,
    src: &Source,
    max_changed_lines: u32,
) -> Result<Canary, String> {
    let commit = git(repo, &["rev-parse", &format!("{}^{{commit}}", src.commit)])?
        .trim()
        .to_string();
    let changed = git(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--no-renames",
            &commit,
        ],
    )?;
    let source_paths: Vec<&str> = changed
        .lines()
        .filter(|p| is_source_path(p, &sel.source_extensions))
        .collect();
    if source_paths.is_empty() {
        return Err("no source changes".into());
    }
    let test_paths: Vec<&str> = changed.lines().filter(|p| under_test_dir(p)).collect();

    // The reverse of the commit's source changes, against the commit itself.
    let parent = format!("{commit}^");
    let mut args = vec![
        "diff",
        "--no-renames",
        "--no-color",
        commit.as_str(),
        parent.as_str(),
        "--",
    ];
    args.extend(source_paths.iter());
    let raw = git(repo, &args)?;
    let files = parse_diff(&raw).map_err(|e| format!("unsupported diff: {e}"))?;
    let s = stats(&files);
    if s.added + s.removed > max_changed_lines {
        return Err(format!(
            "too large for one review ({} changed lines, max {max_changed_lines})",
            s.added + s.removed
        ));
    }

    let mut tests = if test_paths.is_empty() {
        vec![]
    } else {
        let mut args = vec!["show", "--format=", "--no-color", &commit, "--"];
        args.extend(test_paths.iter());
        added_rust_tests(&git(repo, &args)?)
    };
    // Tests whose fixtures (reference images, inputs) the commit changed.
    for fixture in test_paths.iter().filter(|p| !p.ends_with(".rs")) {
        for name in tests_using_fixture(repo, &commit, fixture)? {
            if !tests.contains(&name) {
                tests.push(name);
            }
        }
    }
    if tests.is_empty() {
        return Err("the commit added no tests, so the canary could not be verified".into());
    }

    Ok(Canary {
        project: sel.project.clone(),
        base_commit: commit.clone(),
        intent: cover_intent(&files, &commit),
        diff: format_diff(&files),
        planted: planted_locations(&files),
        tests,
        provenance: Provenance {
            kind: src.kind,
            source_commit: commit,
            reference: src.reference.clone(),
            summary: src.summary.clone(),
            generator_version: VERSION.into(),
        },
        verified: None,
    })
}

/// The regression proof that verifies a canary.
pub fn verification_spec(sel: &Selection, c: &Canary) -> collective_core::proof::ProofSpec {
    collective_core::proof::ProofSpec {
        kind: collective_core::proof::ProofKind::Regression,
        ecosystem: sel.ecosystem,
        tests: c.tests.clone(),
        command: None,
        files: vec![],
        repeats: 1,
        timeout_secs: collective_core::proof::MAX_TIMEOUT_SECS,
    }
}
