//! Environments: the system layer a proof or review run executes in.
//!
//! A project's environment is a Nix package set realised by substitution
//! only (docs/ENVIRONMENTS.md): a `nixpkgs` commit and a list of attribute
//! names, fetched signed from `cache.nixos.org` with local builds disabled.
//! It is data with no commands in it, so it can be read whole, and every
//! runner that realises it gets the same bytes.
//!
//! It is orthogonal to [`crate::proof::Ecosystem`], which names the
//! package-manager fetch of the project's own dependencies. [`Environment::Stock`]
//! is the prover image as it is, for projects that declare nothing.
//!
//! This module decides what a record means: whether it is valid, its
//! canonical form and id, the command that realises it, and how the run's
//! variables are composed from the store paths it yields. Running Nix and
//! looking at the store is the prover's job; it hands the facts back in.

use serde::{Deserialize, Serialize};

use crate::hash::sha256_hex;
use crate::proof::{quorum_of, Quorum, RunOutcome};
use crate::LedgerEntry;

/// A Nix package set: attribute names under one pinned nixpkgs commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixEnvironment {
    /// A commit of NixOS/nixpkgs (40 lowercase hex), never a branch, so the
    /// same record means the same thing forever.
    pub nixpkgs: String,
    /// The NAR hash of that tree (`sha256-` + base64), which makes the fetch
    /// of nixpkgs itself verified rather than trusted to GitHub.
    #[serde(default)]
    pub nixpkgs_hash: Option<String>,
    /// Attribute paths under that nixpkgs, e.g. `openssl.dev`.
    pub packages: Vec<String>,
}

/// What a run executes in. Unknown fields are refused rather than ignored:
/// a record that asks for something this prover would not do must not be
/// mistaken for one it does.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "Wire", into = "Wire")]
pub enum Environment {
    /// The stock prover image.
    #[default]
    Stock,
    Nix(NixEnvironment),
}

/// The record on the wire: `{"stock": {}}` or `{"nix": {...}}`. A plain
/// unit variant would serialise as the bare string `"stock"`, which isn't
/// the shape of the record.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Wire {
    Stock(Empty),
    Nix(NixEnvironment),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

impl From<Wire> for Environment {
    fn from(w: Wire) -> Self {
        match w {
            Wire::Stock(Empty {}) => Environment::Stock,
            Wire::Nix(n) => Environment::Nix(n),
        }
    }
}

impl From<Environment> for Wire {
    fn from(e: Environment) -> Self {
        match e {
            Environment::Stock => Wire::Stock(Empty {}),
            Environment::Nix(n) => Wire::Nix(n),
        }
    }
}

/// At most this many packages: a record is meant to be read whole.
pub const MAX_PACKAGES: usize = 64;
/// Longest attribute path accepted.
pub const MAX_PACKAGE_LEN: usize = 128;
/// Time the prover allows for realising an environment, on top of the
/// proof's own budget (a cold Rust toolchain is ~300 MiB).
pub const REALISE_TIMEOUT_SECS: u32 = 600;

/// Trust (`score::trust`) an operator needs to propose an environment. A
/// proposal spends a quorum of runners' compute, so a newcomer (0.25) must
/// earn standing first; genesis influence counts.
pub const MIN_PROPOSER_TRUST: f64 = 0.5;
/// Rep for a proposal a quorum of runners adopted, and for one they refused.
/// Same scale as runner grading (`proof::PROOF_RUN_AGREED`): the debit is
/// larger, so standing is something to lose.
pub const PROPOSAL_ADOPTED_REP: f64 = 2.0;
pub const PROPOSAL_REFUSED_REP: f64 = -5.0;

/// Whether an operator with this trust may propose an environment. Fails
/// closed on NaN.
pub fn may_propose(trust: f64) -> bool {
    trust >= MIN_PROPOSER_TRUST
}

/// Proposals a project may have waiting for runners at once, and proposals
/// one operator may make in a day: each costs a quorum of runners a suite
/// run.
pub const MAX_OPEN_PROPOSALS_PER_PROJECT: u32 = 3;
pub const MAX_PROPOSALS_PER_OPERATOR_PER_DAY: u32 = 3;

/// What becomes of a proposal once its proof has run its course.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalDecision {
    /// A quorum of runners passed the project's suite in it.
    Adopt,
    /// A quorum of runners found it doesn't work: the suite fails on base
    /// in it, regresses, is flaky, or can't be run in it at all
    /// (inconclusive, e.g. a package that doesn't exist or isn't cached).
    Refuse(RunOutcome),
    /// No quorum: the runners split, or too few came in time. Refused as
    /// inconclusive, and nobody is graded, as a split proof isn't.
    Lapse,
}

impl ProposalDecision {
    /// The version's state afterwards.
    pub fn state(self) -> &'static str {
        match self {
            ProposalDecision::Adopt => "adopted",
            _ => "refused",
        }
    }

    /// The outcome recorded for it: what the runners settled on, and
    /// inconclusive for a lapse.
    pub fn outcome(self) -> RunOutcome {
        match self {
            ProposalDecision::Adopt => RunOutcome::Passed,
            ProposalDecision::Refuse(o) => o,
            ProposalDecision::Lapse => RunOutcome::Inconclusive,
        }
    }

    /// The proposer's grade: like a runner's, credited when a quorum
    /// adopted it and debited more when a quorum refused it. A lapse says
    /// nothing about the proposal, so it costs nothing.
    pub fn proposer_entry(self, proposer: &str) -> Option<LedgerEntry> {
        match self {
            ProposalDecision::Adopt => Some(LedgerEntry::new(
                proposer,
                PROPOSAL_ADOPTED_REP,
                "environment_adopted",
            )),
            ProposalDecision::Refuse(_) => Some(LedgerEntry::new(
                proposer,
                PROPOSAL_REFUSED_REP,
                "environment_refused",
            )),
            ProposalDecision::Lapse => None,
        }
    }
}

/// Maps the quorum of a proposal's runs to its fate: `None` while pending.
pub fn decide_proposal(q: Quorum<RunOutcome>) -> Option<ProposalDecision> {
    match q {
        Quorum::Pending => None,
        Quorum::Settled(RunOutcome::Passed) => Some(ProposalDecision::Adopt),
        Quorum::Settled(o) => Some(ProposalDecision::Refuse(o)),
        Quorum::Split => Some(ProposalDecision::Lapse),
    }
}

// ---------------------------------------------------------------------------
// Quarantine and contests
// ---------------------------------------------------------------------------
//
// An admin can quarantine a project's current environment: while it is
// quarantined the project is review-only (patches are accepted, their tasks
// have no run phase, and proofs and canary verifications are refused), so
// nothing new runs in it. The admin's quarantine is a claim, not a stake.
// Operators with standing contest it by staking a bond on a side: `lift`
// (it is fine, return it to service) or `keep` (it stays quarantined). A
// quorum of stakes settles the contest by the thresholds of
// `proof::proof_quorum`; losers forfeit their bonds to the winners. With
// no quorum after [`CONTEST_DAYS`], every bond comes back and the
// quarantine stands. An admin may lift a quarantine directly at any time.

/// Rep an operator stakes on one side of a quarantine. It is debited when
/// the stake is made (ledger reason [`CONTEST_BOND`]), from ledger rep
/// only: genesis influence counts towards standing but can't be staked.
pub const CONTEST_BOND_REP: f64 = 5.0;
/// Days a contest stays open, from the quarantine, before it settles as a
/// timeout.
pub const CONTEST_DAYS: u32 = 3;
const DAY_MS: i64 = 86_400_000;

/// Ledger reasons: the bond when staked; a winner's bond and share of the
/// forfeits; a loser's settlement (the bond debit stands); a bond returned
/// when the contest timed out or an admin lifted the quarantine first.
pub const CONTEST_BOND: &str = "contest_bond";
pub const CONTEST_WON: &str = "contest_won";
pub const CONTEST_LOST: &str = "contest_lost";
pub const CONTEST_RETURNED: &str = "contest_returned";

/// A side of a contest over a quarantined environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContestSide {
    /// The environment is fine: return it to service.
    Lift,
    /// The quarantine is right: it stays.
    Keep,
}

impl ContestSide {
    pub fn as_str(self) -> &'static str {
        match self {
            ContestSide::Lift => "lift",
            ContestSide::Keep => "keep",
        }
    }

    pub fn parse(s: &str) -> Option<ContestSide> {
        match s {
            "lift" => Some(ContestSide::Lift),
            "keep" => Some(ContestSide::Keep),
            _ => None,
        }
    }

    /// The version's state once this side has won.
    pub fn state(self) -> &'static str {
        match self {
            ContestSide::Lift => "adopted",
            ContestSide::Keep => "quarantined",
        }
    }
}

/// How a contest settled, as recorded: the winning side, or `timeout`.
pub fn contest_settled(winner: Option<ContestSide>) -> &'static str {
    winner.map_or("timeout", ContestSide::as_str)
}

/// One operator's stake on a quarantine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContestClaim {
    pub operator: String,
    /// The operator's trust when it staked.
    pub trust: f64,
    pub side: ContestSide,
    /// Rep it staked (debited when it staked).
    pub bond: f64,
}

/// Whether an operator with this trust may stake on a quarantine: the
/// same standing as proposing an environment. Fails closed on NaN.
pub fn may_contest(trust: f64) -> bool {
    may_propose(trust)
}

/// Whether an operator has the ledger rep to post a contest bond. Fails
/// closed on NaN.
pub fn may_post_bond(available_rep: f64) -> bool {
    available_rep >= CONTEST_BOND_REP
}

/// The debit for a stake.
pub fn contest_bond_entry(operator: &str) -> LedgerEntry {
    LedgerEntry::new(operator, -CONTEST_BOND_REP, CONTEST_BOND)
}

/// One stake per operator, its first: the store's key enforces it, and
/// the rules don't count a repeat either.
fn first_per_operator(claims: &[ContestClaim]) -> Vec<&ContestClaim> {
    let mut seen = std::collections::HashSet::new();
    claims
        .iter()
        .filter(|c| seen.insert(c.operator.as_str()))
        .collect()
}

/// What the stakes on a quarantine establish, by the thresholds of
/// [`crate::proof::proof_quorum`]: a side wins when at least 3 operators
/// back it, their trust adds up to 2.0, and it outweighs the other side.
/// `Split` (many stakes, no winner) settles nothing: the contest stays open
/// until a side wins or it times out.
pub fn contest_quorum(claims: &[ContestClaim]) -> Quorum<ContestSide> {
    let votes: Vec<(f64, ContestSide)> = first_per_operator(claims)
        .into_iter()
        .map(|c| (c.trust, c.side))
        .collect();
    quorum_of(&votes, &[ContestSide::Lift, ContestSide::Keep])
}

/// The ledger entries that settle a contest. With a winner: each winner
/// gets its bond back plus an equal share of the losers' bonds
/// ([`CONTEST_WON`]); each loser forfeits its bond, which was debited when
/// it staked, so its [`CONTEST_LOST`] entry is zero and only records the
/// outcome. On a timeout (`None`) every bond comes back
/// ([`CONTEST_RETURNED`]): nobody was shown wrong.
pub fn contest_entries(winner: Option<ContestSide>, claims: &[ContestClaim]) -> Vec<LedgerEntry> {
    let claims = first_per_operator(claims);
    let Some(winner) = winner else {
        return claims
            .into_iter()
            .map(|c| LedgerEntry::new(&c.operator, c.bond, CONTEST_RETURNED))
            .collect();
    };
    let winners = claims.iter().filter(|c| c.side == winner).count();
    let forfeits: f64 = claims
        .iter()
        .filter(|c| c.side != winner)
        .map(|c| c.bond.max(0.0))
        .sum();
    let share = if winners > 0 {
        forfeits / winners as f64
    } else {
        0.0
    };
    claims
        .into_iter()
        .map(|c| {
            if c.side == winner {
                LedgerEntry::new(&c.operator, crate::round2(c.bond + share), CONTEST_WON)
            } else {
                LedgerEntry::new(&c.operator, 0.0, CONTEST_LOST)
            }
        })
        .collect()
}

/// Whether a contest opened by a quarantine at `quarantined_at` has run
/// out of time at `now` (both in milliseconds).
pub fn contest_expired(quarantined_at: i64, now: i64) -> bool {
    now.saturating_sub(quarantined_at) >= CONTEST_DAYS as i64 * DAY_MS
}

/// The newest quarantine time that has run out at `now`: contests opened
/// at or before it settle as a timeout.
pub fn contest_cutoff(now: i64) -> i64 {
    now - CONTEST_DAYS as i64 * DAY_MS
}

/// The ledger reference of one quarantine's contest. The quarantine time
/// is part of it, so a version quarantined again starts a new contest.
pub fn contest_ref(project_id: &str, version: u32, quarantined_at: i64) -> String {
    format!("contest:{project_id}:{version}:{quarantined_at}")
}

fn valid_rev(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `sha256-` and the 44-character padded base64 of a 32-byte digest, as
/// Nix prints NAR hashes (SRI form).
fn valid_nar_hash(s: &str) -> bool {
    let Some(b64) = s.strip_prefix("sha256-") else {
        return false;
    };
    b64.len() == 44
        && b64.ends_with('=')
        && b64[..43]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// An attribute path: `^[A-Za-z0-9_][A-Za-z0-9_.-]*$`, no empty segment
/// between dots. No leading `-`, so it can never be read as an option; no
/// `/`, `#`, `?` or whitespace, so it can't alter the flake reference.
fn valid_package(p: &str) -> bool {
    let first_ok = p
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
    first_ok
        && p.len() <= MAX_PACKAGE_LEN
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && p.split('.').all(|seg| !seg.is_empty())
}

impl Environment {
    pub fn validate(&self) -> Result<(), String> {
        let Environment::Nix(n) = self else {
            return Ok(());
        };
        if !valid_rev(&n.nixpkgs) {
            return Err(
                "nixpkgs must be a 40-character lowercase commit hash, not a branch or channel"
                    .into(),
            );
        }
        if let Some(h) = &n.nixpkgs_hash {
            if !valid_nar_hash(h) {
                return Err(
                    "nixpkgs_hash must be `sha256-` and 44 base64 characters (from `nix flake metadata`)"
                        .into(),
                );
            }
        }
        if n.packages.is_empty() || n.packages.len() > MAX_PACKAGES {
            return Err(format!("name 1-{MAX_PACKAGES} packages"));
        }
        if let Some(bad) = n.packages.iter().find(|p| !valid_package(p)) {
            return Err(format!("invalid package attribute: {bad}"));
        }
        Ok(())
    }

    /// The canonical form: packages sorted and de-duplicated, so the same
    /// set written in any order is the same record.
    pub fn canonical(self) -> Environment {
        match self {
            Environment::Stock => Environment::Stock,
            Environment::Nix(mut n) => {
                n.packages.sort();
                n.packages.dedup();
                Environment::Nix(n)
            }
        }
    }

    /// `env_` and the first 32 hex characters of the SHA-256 of the
    /// canonical JSON. Identical proposals share an id.
    pub fn id(&self) -> String {
        let json = serde_json::to_string(&self.clone().canonical()).expect("serializable");
        format!("env_{}", &sha256_hex(json)[..32])
    }
}

impl NixEnvironment {
    /// One flake reference per package, in order. The hash is
    /// percent-encoded because base64 uses `+`, `/` and `=`, which mean
    /// other things in a URL query.
    pub fn flake_refs(&self) -> Vec<String> {
        let query = match &self.nixpkgs_hash {
            Some(h) => format!(
                "?narHash={}",
                h.replace('+', "%2B")
                    .replace('/', "%2F")
                    .replace('=', "%3D")
            ),
            None => String::new(),
        };
        self.packages
            .iter()
            .map(|p| format!("github:NixOS/nixpkgs/{}{query}#{p}", self.nixpkgs))
            .collect()
    }

    /// The realisation command: substitution only (`--max-jobs 0`), no
    /// import-from-derivation (which would build during evaluation), one
    /// store path printed per reference.
    pub fn realise_argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = [
            "nix",
            "build",
            "--max-jobs",
            "0",
            "--no-link",
            "--print-out-paths",
            "--option",
            "allow-import-from-derivation",
            "false",
        ]
        .map(String::from)
        .to_vec();
        argv.extend(self.flake_refs());
        argv
    }
}

/// A realised store path and what the prover found in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorePath {
    pub path: String,
    pub has_bin: bool,
    pub has_lib: bool,
    pub has_include: bool,
    pub has_lib_pkgconfig: bool,
    pub has_share_pkgconfig: bool,
}

/// The run's variables, composed from realised store paths: a small,
/// deterministic slice of what nixpkgs' setup hooks do in a dev shell,
/// done here so that nothing is built.
///
/// In input order, each path contributing only what it has:
/// - `PATH`: each `bin`. A fragment: the caller prepends it to the image's.
/// - `PKG_CONFIG_PATH`: each `lib/pkgconfig` and `share/pkgconfig`.
/// - `NIX_CFLAGS_COMPILE` and `NIX_LDFLAGS`, read by the nixpkgs compiler
///   wrapper: `-isystem <p>/include` and `-L<p>/lib`.
/// - `LIBRARY_PATH` and `CPATH`, the same directories for a compiler that
///   isn't wrapped.
///
/// A path listed twice counts once. Variables with no content are left out.
pub fn compose_env(paths: &[StorePath]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let paths: Vec<&StorePath> = paths
        .iter()
        .filter(|p| seen.insert(p.path.as_str()))
        .collect();
    let collect = |f: &dyn Fn(&StorePath) -> Vec<String>, sep: &str| {
        paths
            .iter()
            .flat_map(|p| f(p))
            .collect::<Vec<_>>()
            .join(sep)
    };
    let when = |cond: bool, s: String| if cond { vec![s] } else { vec![] };
    let vars = [
        (
            "PATH",
            collect(&|p| when(p.has_bin, format!("{}/bin", p.path)), ":"),
        ),
        (
            "PKG_CONFIG_PATH",
            collect(
                &|p| {
                    let mut v = when(p.has_lib_pkgconfig, format!("{}/lib/pkgconfig", p.path));
                    v.extend(when(
                        p.has_share_pkgconfig,
                        format!("{}/share/pkgconfig", p.path),
                    ));
                    v
                },
                ":",
            ),
        ),
        (
            "NIX_CFLAGS_COMPILE",
            collect(
                &|p| when(p.has_include, format!("-isystem {}/include", p.path)),
                " ",
            ),
        ),
        (
            "NIX_LDFLAGS",
            collect(&|p| when(p.has_lib, format!("-L{}/lib", p.path)), " "),
        ),
        (
            "LIBRARY_PATH",
            collect(&|p| when(p.has_lib, format!("{}/lib", p.path)), ":"),
        ),
        (
            "CPATH",
            collect(&|p| when(p.has_include, format!("{}/include", p.path)), ":"),
        ),
    ];
    vars.into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}
