//! Usage-as-vote scoring.
//!
//! A patch's score is the decayed, trust-weighted sum of what happened when
//! other operators' agents used it:
//!   1. using a patch and having it work is the strongest signal; using it
//!      and having it fail is a negative one;
//!   2. votes only count across operator boundaries;
//!   3. no single operator can dominate (per-operator cap);
//!   4. new operators count for little until their own work is used;
//!   5. old usage fades.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Applied cleanly to the adopter's tree.
    Applied,
    /// The adopter's test suite passed with it.
    TestsPassed,
    /// Kept in something the adopter delivered.
    Shipped,
    /// Did not apply or broke tests.
    Failed,
    /// Used, then had to be backed out.
    Reverted,
}

impl Outcome {
    pub const ALL: [Outcome; 5] = [
        Outcome::Applied,
        Outcome::TestsPassed,
        Outcome::Shipped,
        Outcome::Failed,
        Outcome::Reverted,
    ];

    pub fn weight(self) -> f64 {
        match self {
            Outcome::Applied => 0.5,
            Outcome::TestsPassed => 1.0,
            Outcome::Shipped => 2.0,
            Outcome::Failed => -1.0,
            Outcome::Reverted => -2.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::TestsPassed => "tests_passed",
            Outcome::Shipped => "shipped",
            Outcome::Failed => "failed",
            Outcome::Reverted => "reverted",
        }
    }

    pub fn parse(s: &str) -> Option<Outcome> {
        Outcome::ALL.into_iter().find(|o| o.as_str() == s)
    }
}

pub const HALF_LIFE_DAYS: f64 = 90.0;
pub const PER_OPERATOR_CAP: f64 = 2.0;
/// Share of a derived patch's adoption weight credited to its parent.
pub const PARENT_SHARE: f64 = 0.5;
const DAY_MS: f64 = 86_400_000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Adoption {
    pub operator_id: String,
    pub outcome: Outcome,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    /// 1 for direct adoptions, [`PARENT_SHARE`] for adoptions of a derived patch.
    pub share: f64,
}

/// Trust in an operator's votes: 0 if banned, else from 0.25 (unknown) towards 1 (proven).
pub fn trust(reputation: f64) -> f64 {
    0.25 + 0.75 * (1.0 - (-reputation.max(0.0)).exp())
}

/// Reputation from the scores an operator's patches earned from others,
/// plus its review-ledger balance.
pub fn reputation(authored_scores: &[f64], ledger_balance: f64) -> f64 {
    (authored_scores.iter().map(|s| s.max(0.0)).sum::<f64>() + ledger_balance.max(0.0)).ln_1p()
}

/// Genesis influence halves every this many days, so founders' starting
/// weight gives way to weight earned by work others use.
pub const GENESIS_HALF_LIFE_DAYS: f64 = 60.0;
/// Most genesis influence one grant can carry.
pub const GENESIS_MAX_GRANT: f64 = 100.0;

/// What a genesis grant of `amount` is worth `age_ms` after it was made.
pub fn genesis_effective(amount: f64, age_ms: i64) -> f64 {
    amount.clamp(0.0, GENESIS_MAX_GRANT)
        * 0.5f64.powf((age_ms.max(0) as f64 / DAY_MS) / GENESIS_HALF_LIFE_DAYS)
}

/// Ledger rep halves every this many days, so standing has to be kept up
/// by continued review and use; a dormant or sold account fades.
pub const LEDGER_HALF_LIFE_DAYS: f64 = 180.0;
/// Reference time for ledger weights (2026-01-01T00:00:00Z).
pub const LEDGER_EPOCH_MS: i64 = 1_767_225_600_000;

fn ledger_growth(at_ms: i64) -> f64 {
    2f64.powf(((at_ms - LEDGER_EPOCH_MS) as f64 / DAY_MS) / LEDGER_HALF_LIFE_DAYS)
}

/// What to store with a ledger entry so that balances decay without
/// rewriting history: `delta` scaled up by how far past the epoch it was
/// made. Sum the weights and pass them to [`ledger_balance`].
pub fn ledger_weight(delta: f64, at_ms: i64) -> f64 {
    delta * ledger_growth(at_ms)
}

/// A decayed balance from summed [`ledger_weight`]s: every entry counts
/// `delta · ½^(age / half-life)`, credits and penalties alike.
pub fn ledger_balance(weight_sum: f64, now_ms: i64) -> f64 {
    let b = weight_sum / ledger_growth(now_ms);
    (b * 1e4).round() / 1e4
}

pub fn decay(age_ms: i64) -> f64 {
    0.5f64.powf((age_ms.max(0) as f64 / DAY_MS) / HALF_LIFE_DAYS)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PatchScore {
    pub score: f64,
    /// Operators whose net contribution is positive.
    pub adopters: u32,
}

pub fn score_patch(
    author_operator: &str,
    adoptions: &[Adoption],
    trust_of: impl Fn(&str) -> f64,
    now: i64,
) -> PatchScore {
    let mut per_operator: BTreeMap<&str, f64> = BTreeMap::new();
    for a in adoptions
        .iter()
        .filter(|a| a.operator_id != author_operator)
    {
        *per_operator.entry(&a.operator_id).or_default() +=
            a.outcome.weight() * decay(now - a.created_at) * a.share;
    }
    let (mut score, mut adopters) = (0.0, 0);
    for (op, raw) in per_operator {
        let capped = raw.clamp(-PER_OPERATOR_CAP, PER_OPERATOR_CAP);
        score += capped * trust_of(op);
        if capped > 0.0 {
            adopters += 1;
        }
    }
    PatchScore {
        score: (score * 1000.0).round() / 1000.0,
        adopters,
    }
}
