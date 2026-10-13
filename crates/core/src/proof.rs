//! Proofs: machine-checked evidence for a challenge.
//!
//! A test that behaves differently with and without a patch only proves the
//! patch changes behavior, which every patch does. So a proof is judged
//! against an oracle the finder does not control:
//!
//! - `Regression`: the project's *own* tests pass on the base commit and fail
//!   with the patch. The finder adds no files and writes no command: it names
//!   tests, which run through the ecosystem's standard runner as plain
//!   arguments, so the finder cannot smuggle in a check of its own (like
//!   "fail if the patch's text is present"). Objective: resolves the
//!   challenge by itself.
//! - `Exploit`: a finder-supplied test fails on base and passes with the
//!   patch, i.e. "with this patch, user `debug` gets admin". The sandbox
//!   verifies the behavior; a juror only has to judge a short test, not the
//!   whole patch.
//! - `Property`: a finder-supplied test of a property (e.g. "logins reject
//!   unknown users") passes on base and fails with the patch. Verified the
//!   same way, judged the same way.
//!
//! - `Suite`: not a challenge proof but a review run: the project's whole
//!   test suite on base and on the patch, as every review task runs it
//!   (see `review`). [`suite_outcome`] says what such a run shows.
//!
//! The sandbox (crates/prover) runs every attempt on a fresh copy of the
//! tree with no network; this module only decides what the runs mean, and
//! how many independent runners must agree before a verdict stands
//! ([`proof_quorum`]).

use serde::{Deserialize, Serialize};

use crate::LedgerEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofKind {
    Regression,
    Exploit,
    Property,
    Suite,
}

/// Dependency fetch before the tests run. Each is a fixed command that
/// downloads without executing project code; it is the only step with
/// network access. `None` runs the container with no internet at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    #[default]
    None,
    Cargo,
    Npm,
    Pip,
    Go,
}

impl Ecosystem {
    /// The fetch command, run in the project root with network.
    pub fn setup_command(self) -> Option<&'static str> {
        match self {
            Ecosystem::None => None,
            Ecosystem::Cargo => Some("cargo fetch"),
            Ecosystem::Npm => Some("npm ci --ignore-scripts --no-audit --no-fund"),
            // Wheels only: installing an sdist would run its setup.py with network.
            Ecosystem::Pip => Some(
                "python3 -m venv .venv && .venv/bin/pip install --only-binary=:all: -r requirements.txt",
            ),
            Ecosystem::Go => Some("go mod download"),
        }
    }

    /// Compiles the tests once per tree before any run, offline and
    /// isolated like the runs themselves, so run timeouts measure the tests
    /// rather than the compiler. `None` where there is nothing to build.
    pub fn build_argv(self) -> Option<&'static [&'static str]> {
        match self {
            Ecosystem::Cargo => Some(&[
                "cargo",
                "test",
                "--offline",
                "--workspace",
                "--tests",
                "--no-run",
            ]),
            Ecosystem::Go => Some(&["go", "test", "-count=1", "-run", "^$", "./..."]),
            Ecosystem::None | Ecosystem::Npm | Ecosystem::Pip => None,
        }
    }

    /// Environment for the offline test phase.
    pub fn offline_env(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Ecosystem::Cargo => &[("CARGO_NET_OFFLINE", "true")],
            Ecosystem::Npm => &[("npm_config_offline", "true")],
            Ecosystem::Pip => &[("PIP_NO_INDEX", "1"), ("VIRTUAL_ENV", ".venv")],
            Ecosystem::Go => &[("GOPROXY", "off"), ("GOFLAGS", "-mod=mod")],
            Ecosystem::None => &[],
        }
    }

    /// The whole test suite, as argv. `None` runs the project's own test
    /// script (`script`) with `sh`.
    pub fn suite_argv(self, script: &[String]) -> Vec<String> {
        match self {
            Ecosystem::Go => ["go", "test", "./...", "-count=1"]
                .map(String::from)
                .to_vec(),
            _ => self.test_argv(script),
        }
    }

    /// The standard test runner, as argv (never through a shell), with the
    /// finder's validated selectors. `None` runs one of the project's own
    /// scripts with `sh`.
    pub fn test_argv(self, tests: &[String]) -> Vec<String> {
        let mut argv: Vec<String> = match self {
            Ecosystem::None => vec!["sh".into()],
            // --tests: unit and integration tests, not doctests (which compile at run time).
            Ecosystem::Cargo => vec![
                "cargo".into(),
                "test".into(),
                "--offline".into(),
                "--workspace".into(),
                "--tests".into(),
                "--".into(),
            ],
            Ecosystem::Npm => vec!["npm".into(), "test".into(), "--".into()],
            Ecosystem::Pip => vec![
                ".venv/bin/python".into(),
                "-m".into(),
                "pytest".into(),
                "-q".into(),
                "-p".into(),
                "no:cacheprovider".into(),
            ],
            Ecosystem::Go => vec![
                "go".into(),
                "test".into(),
                "./...".into(),
                "-count=1".into(),
                "-run".into(),
            ],
        };
        match self {
            Ecosystem::Go => argv.push(format!("^({})$", tests.join("|"))),
            _ => argv.extend(tests.iter().cloned()),
        }
        argv
    }
}

/// A test selector: a test name, node id or script path. No shell
/// metacharacters, no options, no escaping the tree.
pub fn valid_selector(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 200
        && !t.starts_with('-')
        && !t.contains("..")
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:[]=,@+".contains(c))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofFile {
    pub path: String,
    pub content: String,
}

pub const MAX_REPEATS: u8 = 5;
pub const MAX_TIMEOUT_SECS: u32 = 120;
/// A whole test suite gets longer than a single proof test.
pub const MAX_SUITE_TIMEOUT_SECS: u32 = 1800;
pub const MAX_FILES: usize = 20;
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// Time allowed to compile a tree's tests before the runs.
pub const BUILD_TIMEOUT_SECS: u32 = 1200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofSpec {
    pub kind: ProofKind,
    #[serde(default)]
    pub ecosystem: Ecosystem,
    /// Regression proofs: the project's tests to run, by name or path.
    #[serde(default)]
    pub tests: Vec<String>,
    /// Exploit and property proofs: shell command run in the project root
    /// over the finder's files. Exit 0 means "passed".
    #[serde(default)]
    pub command: Option<String>,
    /// Files added to both trees (exploit and property proofs only).
    #[serde(default)]
    pub files: Vec<ProofFile>,
    /// Runs per tree; every run must agree.
    #[serde(default = "default_repeats")]
    pub repeats: u8,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u32,
}

fn default_repeats() -> u8 {
    3
}

fn default_timeout() -> u32 {
    60
}

/// A relative path that stays inside the tree.
pub fn safe_relative_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains('\0')
        && p.split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && p.split('/').next().is_none_or(|s| s != ".git")
}

impl ProofSpec {
    pub fn validate(&self) -> Result<(), String> {
        match self.kind {
            ProofKind::Regression => {
                if self.command.is_some() {
                    return Err("a regression proof names the project's tests in `tests`; it takes no command".into());
                }
                if self.tests.is_empty() || self.tests.len() > 20 {
                    return Err("name 1-20 of the project's tests in `tests`".into());
                }
                if let Some(bad) = self.tests.iter().find(|t| !valid_selector(t)) {
                    return Err(format!("invalid test selector: {bad}"));
                }
                if self.ecosystem == Ecosystem::None
                    && (self.tests.len() != 1 || !safe_relative_path(&self.tests[0]))
                {
                    return Err("without an ecosystem, `tests` is the path of one of the project's own test scripts".into());
                }
            }
            ProofKind::Suite => {
                if self.command.is_some() || !self.files.is_empty() {
                    return Err(
                        "a suite run uses the project's own test suite: no command, no files"
                            .into(),
                    );
                }
                let script_ok = match self.ecosystem {
                    Ecosystem::None => self.tests.len() == 1 && safe_relative_path(&self.tests[0]),
                    _ => self.tests.is_empty(),
                };
                if !script_ok {
                    return Err("a suite run names its test script only without an ecosystem (`tests`: [path])".into());
                }
            }
            ProofKind::Exploit | ProofKind::Property => {
                let cmd = self.command.as_deref().unwrap_or_default();
                if cmd.trim().is_empty() || cmd.len() > 2000 {
                    return Err("command must be 1-2000 characters".into());
                }
                if !self.tests.is_empty() {
                    return Err("`tests` is only for regression proofs".into());
                }
            }
        }
        if !(1..=MAX_REPEATS).contains(&self.repeats) {
            return Err(format!("repeats must be 1-{MAX_REPEATS}"));
        }
        let max_timeout = if self.kind == ProofKind::Suite {
            MAX_SUITE_TIMEOUT_SECS
        } else {
            MAX_TIMEOUT_SECS
        };
        if !(1..=max_timeout).contains(&self.timeout_secs) {
            return Err(format!("timeout_secs must be 1-{max_timeout}"));
        }
        if self.kind == ProofKind::Regression && !self.files.is_empty() {
            return Err("a regression proof uses the project's own tests: add no files (use exploit or property instead)".into());
        }
        if matches!(self.kind, ProofKind::Exploit | ProofKind::Property) && self.files.is_empty() {
            return Err("exploit and property proofs need the test file(s) they run".into());
        }
        if self.files.len() > MAX_FILES {
            return Err(format!("at most {MAX_FILES} files"));
        }
        for f in &self.files {
            if !safe_relative_path(&f.path) {
                return Err(format!("unsafe file path: {}", f.path));
            }
            if f.content.len() > MAX_FILE_BYTES {
                return Err(format!("{} is larger than {MAX_FILE_BYTES} bytes", f.path));
            }
        }
        Ok(())
    }

    /// The argv each run executes.
    pub fn argv(&self) -> Vec<String> {
        match self.kind {
            ProofKind::Regression => self.ecosystem.test_argv(&self.tests),
            ProofKind::Suite => self.ecosystem.suite_argv(&self.tests),
            _ => vec![
                "sh".into(),
                "-c".into(),
                self.command.clone().unwrap_or_default(),
            ],
        }
    }

    /// What each tree must do for the proof to hold.
    pub fn expected(&self) -> (Expect, Expect) {
        match self.kind {
            ProofKind::Regression | ProofKind::Property => (Expect::Pass, Expect::Fail),
            ProofKind::Exploit => (Expect::Fail, Expect::Pass),
            ProofKind::Suite => (Expect::Pass, Expect::Pass),
        }
    }

    /// Wall-clock budget a prover needs, including setup and builds.
    pub fn budget_secs(&self) -> u32 {
        let build = if self.ecosystem.build_argv().is_some() {
            2 * BUILD_TIMEOUT_SECS
        } else {
            0
        };
        2 * self.repeats as u32 * self.timeout_secs + 300 + build
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tree {
    Base,
    Patched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Passed,
    Failed,
    TimedOut,
    /// Not run: the prover's time budget ran out.
    Skipped,
}

impl RunStatus {
    fn meets(self, e: Expect) -> bool {
        match e {
            Expect::Pass => self == RunStatus::Passed,
            Expect::Fail => matches!(self, RunStatus::Failed | RunStatus::TimedOut),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub tree: Tree,
    pub attempt: u8,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Last few KB of combined stdout and stderr.
    pub output_tail: String,
    /// How many tests actually executed, where the runner's output says so
    /// (see [`count_tests`]). `Some(0)` means the selectors matched nothing.
    #[serde(default)]
    pub tests_run: Option<u32>,
}

fn number_before(text: &str, word: &str) -> Option<u32> {
    let i = text.find(word)?;
    text[..i].split_whitespace().last()?.parse().ok()
}

/// Counts executed tests from a runner's full output, for runners whose
/// summary format is known. `None` means no summary was printed at all
/// (typically: the build failed before any test ran).
pub fn count_tests(ecosystem: Ecosystem, output: &str) -> Option<u32> {
    match ecosystem {
        Ecosystem::Cargo => {
            // One "test result: ok. N passed; M failed; ..." line per test binary.
            let mut total = None;
            for line in output
                .lines()
                .filter(|l| l.trim_start().starts_with("test result:"))
            {
                let n = number_before(line, " passed").unwrap_or(0)
                    + number_before(line, " failed").unwrap_or(0);
                total = Some(total.unwrap_or(0) + n);
            }
            total
        }
        Ecosystem::Pip => {
            let summary = output.lines().rev().find(|l| {
                l.contains(" passed") || l.contains(" failed") || l.contains("no tests ran")
            })?;
            if summary.contains("no tests ran") {
                return Some(0);
            }
            Some(
                number_before(summary, " passed").unwrap_or(0)
                    + number_before(summary, " failed").unwrap_or(0),
            )
        }
        Ecosystem::None | Ecosystem::Npm | Ecosystem::Go => None,
    }
}

impl Ecosystem {
    /// Whether [`count_tests`] understands this runner's output.
    pub fn counts_tests(self) -> bool {
        matches!(self, Ecosystem::Cargo | Ecosystem::Pip)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupResult {
    pub ok: bool,
    pub error: Option<String>,
    /// Whether test commands provably had no network.
    pub network_isolated: bool,
    /// How: "no-internet container" or "network namespace".
    pub isolation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofReport {
    pub prover_version: String,
    pub setup: SetupResult,
    pub runs: Vec<RunResult>,
    /// CPU time (user + system) of everything the prover ran: dependency
    /// fetch, builds and runs. Measured by whoever ran the prover, so a
    /// runner's figure is a claim (see `review::median_cpu_ms`).
    #[serde(default)]
    pub cpu_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Objective: the project's own tests break. Resolves the challenge.
    Proven,
    /// The claimed behavior is verified; whether it is harmful needs a judgement.
    Demonstrated,
    /// The runs are consistent but do not show the claimed difference.
    NotReproduced,
    /// The runs disagree with each other.
    Flaky,
    /// The proof could not be run properly.
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    pub verdict: Verdict,
    pub reason: String,
}

fn assessment(verdict: Verdict, reason: impl Into<String>) -> Assessment {
    Assessment {
        verdict,
        reason: reason.into(),
    }
}

/// Decides what a prover's runs establish. Fails closed: anything short of
/// complete, isolated, unanimous runs is not a proof.
pub fn assess(spec: &ProofSpec, report: &ProofReport) -> Assessment {
    if spec.kind == ProofKind::Suite {
        return assessment(
            Verdict::Inconclusive,
            "a suite run is review evidence, not a challenge proof",
        );
    }
    if let Err(e) = spec.validate() {
        return assessment(Verdict::Inconclusive, format!("invalid spec: {e}"));
    }
    if !report.setup.ok {
        return assessment(
            Verdict::Inconclusive,
            report
                .setup
                .error
                .clone()
                .unwrap_or_else(|| "setup failed".into()),
        );
    }
    if !report.setup.network_isolated {
        return assessment(
            Verdict::Inconclusive,
            "tests did not run without network, so their results cannot count",
        );
    }
    let (want_base, want_patched) = spec.expected();
    let of = |t: Tree| report.runs.iter().filter(move |r| r.tree == t);
    for t in [Tree::Base, Tree::Patched] {
        if of(t).count() != spec.repeats as usize {
            return assessment(
                Verdict::Inconclusive,
                format!("expected {} runs per tree", spec.repeats),
            );
        }
    }
    if report.runs.iter().any(|r| r.status == RunStatus::Skipped) {
        return assessment(
            Verdict::Inconclusive,
            "time budget ran out before every run finished",
        );
    }

    let meets = |t: Tree, e: Expect| of(t).filter(|r| r.status.meets(e)).count();
    let n = spec.repeats as usize;
    let (base_ok, patched_ok) = (
        meets(Tree::Base, want_base),
        meets(Tree::Patched, want_patched),
    );

    if base_ok == n && patched_ok == n {
        let only_timeouts = want_patched == Expect::Fail
            && of(Tree::Patched).all(|r| r.status == RunStatus::TimedOut);
        if only_timeouts {
            return assessment(
                Verdict::Demonstrated,
                "the patched tree only timed out; a hang may be a bug or a slow runner",
            );
        }
        if spec.kind == ProofKind::Regression && spec.ecosystem.counts_tests() {
            // A runner that ran nothing exits 0: a vacuous pass is not a pass.
            if of(Tree::Base).any(|r| r.tests_run.unwrap_or(0) == 0) {
                return assessment(
                    Verdict::Inconclusive,
                    "the named tests did not run on the base commit (no test matched)",
                );
            }
            if of(Tree::Patched)
                .any(|r| r.status == RunStatus::Failed && r.tests_run.unwrap_or(0) == 0)
            {
                return assessment(
                    Verdict::Demonstrated,
                    "the patched tree fails before running the named tests (it likely does not build); a person should confirm",
                );
            }
        }
        return match spec.kind {
            ProofKind::Regression => assessment(
                Verdict::Proven,
                "the project's own tests pass on base and fail with the patch, in every run",
            ),
            ProofKind::Exploit => assessment(
                Verdict::Demonstrated,
                "the exploit fails on base and succeeds with the patch, in every run",
            ),
            ProofKind::Property => assessment(
                Verdict::Demonstrated,
                "the property holds on base and is broken by the patch, in every run",
            ),
            ProofKind::Suite => unreachable!("handled above"),
        };
    }
    let consistent = |k: usize| k == 0 || k == n;
    if consistent(base_ok) && consistent(patched_ok) {
        let mut why = Vec::new();
        if base_ok == 0 {
            why.push(format!("base did not {:?} as claimed", want_base).to_lowercase());
        }
        if patched_ok == 0 {
            why.push(format!("patched tree did not {:?} as claimed", want_patched).to_lowercase());
        }
        return assessment(Verdict::NotReproduced, why.join("; "));
    }
    assessment(
        Verdict::Flaky,
        format!("runs disagree: base {base_ok}/{n}, patched {patched_ok}/{n} as claimed"),
    )
}

// ---------------------------------------------------------------------------
// Proof runs
// ---------------------------------------------------------------------------
//
// Nobody runs a proof centrally. A proof (and the verification of a
// contributed canary) is a job any disinterested operator's agent can claim,
// run in its own sandbox and report on. The server assesses each report
// with [`assess`] and a verdict stands once enough independent, trusted
// runners agree ([`proof_quorum`]). A runner's report is a claim: one
// runner, or a crowd of fresh accounts, settles nothing.

/// Distinct operators that must report the same verdict.
pub const MIN_RUNNERS: usize = 3;
/// Combined trust (`score::trust`, 0.25 for an unknown operator up to 1)
/// those operators must carry. Three newcomers carry 0.75, and even
/// [`MAX_RUNNERS`] of them fall short, so new accounts alone prove nothing.
pub const QUORUM_TRUST: f64 = 2.0;
/// Conclusive runs a job collects before it is given up as split.
pub const MAX_RUNNERS: usize = 7;
/// Rep for a run that agreed with the settled verdict, and for one that
/// contradicted it.
pub const PROOF_RUN_AGREED: f64 = 2.0;
pub const PROOF_RUN_DISAGREED: f64 = -5.0;

/// One runner's report of a proof job, as the server assessed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofClaim {
    pub runner: String,
    /// The runner's trust when it reported.
    pub trust: f64,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quorum {
    /// Not enough agreeing runs yet.
    Pending,
    /// Enough independent, trusted runners agree.
    Settled(Verdict),
    /// [`MAX_RUNNERS`] conclusive runs came in without a quorum.
    Split,
}

/// Conclusive claims, one per operator (its first).
fn conclusive_claims(claims: &[ProofClaim]) -> Vec<&ProofClaim> {
    let mut seen = std::collections::HashSet::new();
    claims
        .iter()
        .filter(|c| c.verdict != Verdict::Inconclusive && seen.insert(c.runner.as_str()))
        .collect()
}

/// What the runs of a proof job establish. A verdict is settled when at
/// least [`MIN_RUNNERS`] operators report it, their trust adds up to
/// [`QUORUM_TRUST`], and it outweighs every other verdict combined.
/// Inconclusive runs count for nothing. Fails closed: disagreement leaves
/// the job pending, and then split.
pub fn proof_quorum(claims: &[ProofClaim]) -> Quorum {
    let claims = conclusive_claims(claims);
    let total: f64 = claims.iter().map(|c| c.trust.max(0.0)).sum();
    for v in [
        Verdict::Proven,
        Verdict::Demonstrated,
        Verdict::NotReproduced,
        Verdict::Flaky,
    ] {
        let agreeing: Vec<_> = claims.iter().filter(|c| c.verdict == v).collect();
        let trust: f64 = agreeing.iter().map(|c| c.trust.max(0.0)).sum();
        if agreeing.len() >= MIN_RUNNERS && trust >= QUORUM_TRUST && trust > total - trust {
            return Quorum::Settled(v);
        }
    }
    if claims.len() >= MAX_RUNNERS {
        Quorum::Split
    } else {
        Quorum::Pending
    }
}

/// Grades the runs of a settled job: runners who reported the settled
/// verdict earn rep, runners who reported another lose more. Inconclusive
/// runs earn and cost nothing.
pub fn grade_proof_runs(settled: Verdict, claims: &[ProofClaim]) -> Vec<LedgerEntry> {
    conclusive_claims(claims)
        .into_iter()
        .map(|c| {
            if c.verdict == settled {
                LedgerEntry::new(&c.runner, PROOF_RUN_AGREED, "ran a proof")
            } else {
                LedgerEntry::new(
                    &c.runner,
                    PROOF_RUN_DISAGREED,
                    "proof run disagreed with the other runners",
                )
            }
        })
        .collect()
}

/// Wall-clock time a runner has to report a job it claimed: the prover's
/// budget plus time to fetch the source and start a sandbox.
pub fn run_lease_secs(spec: &ProofSpec) -> u32 {
    spec.budget_secs() + 900
}

/// What a suite run (a review task's run) shows about a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The suite passes on base and with the patch.
    Passed,
    /// The suite passes on base and fails (or hangs) with the patch.
    Regressed,
    /// The suite already fails on base, so the run says nothing about the patch.
    BaseFails,
    /// Runs of the same tree disagree.
    Flaky,
    /// The run could not be done properly (setup, isolation, missing runs).
    Inconclusive,
}

impl RunOutcome {
    /// Whether the run says something definite that other runners can check.
    pub fn conclusive(self) -> bool {
        !matches!(self, RunOutcome::Inconclusive)
    }
}

/// Decides what a suite run shows. Fails closed like [`assess`]: a run that
/// wasn't isolated, complete and non-vacuous is inconclusive.
pub fn suite_outcome(spec: &ProofSpec, report: &ProofReport) -> RunOutcome {
    if spec.kind != ProofKind::Suite
        || spec.validate().is_err()
        || !report.setup.ok
        || !report.setup.network_isolated
    {
        return RunOutcome::Inconclusive;
    }
    let of = |t: Tree| report.runs.iter().filter(move |r| r.tree == t);
    let n = spec.repeats as usize;
    if [Tree::Base, Tree::Patched]
        .iter()
        .any(|t| of(*t).count() != n)
        || report.runs.iter().any(|r| r.status == RunStatus::Skipped)
    {
        return RunOutcome::Inconclusive;
    }
    // A runner that ran nothing exits 0: a vacuous pass is not a pass.
    if spec.ecosystem.counts_tests() && of(Tree::Base).any(|r| r.tests_run.unwrap_or(0) == 0) {
        return RunOutcome::Inconclusive;
    }
    let passed = |t: Tree| of(t).filter(|r| r.status == RunStatus::Passed).count();
    match (passed(Tree::Base), passed(Tree::Patched)) {
        (0, _) => RunOutcome::BaseFails,
        (b, _) if b < n => RunOutcome::Flaky,
        (_, p) if p == n => RunOutcome::Passed,
        (_, 0) => RunOutcome::Regressed,
        _ => RunOutcome::Flaky,
    }
}

/// What the Worker sends the prover: one line of JSON, then the raw
/// `.tar.gz` source bytes (no base64 inflation of large trees).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProveRequest {
    pub spec: ProofSpec,
    pub patch: String,
}

pub fn encode_prove_request(req: &ProveRequest, source_tgz: &[u8]) -> Vec<u8> {
    let mut body = serde_json::to_vec(req).expect("serializable");
    body.push(b'\n');
    body.extend_from_slice(source_tgz);
    body
}

pub fn decode_prove_request(body: &[u8]) -> Result<(ProveRequest, &[u8]), String> {
    let nl = body
        .iter()
        .position(|b| *b == b'\n')
        .ok_or("missing header line")?;
    let req: ProveRequest =
        serde_json::from_slice(&body[..nl]).map_err(|e| format!("bad header: {e}"))?;
    Ok((req, &body[nl + 1..]))
}
