//! Challenge protocol: flag a patch as harmful, escalate with bonds, settle
//! with slashing, bans and a bounty. See docs/CHALLENGES.md for the
//! rationale and the protocols it borrows from (Augur, UMA, Kleros, Truebit).
//!
//! Units are reputation points ("rep"): non-transferable stake. Every staked
//! amount is debited when staked; settlement emits credits.
//!
//! The staking game is binary ("is there a real problem in this patch?").
//! Severity and intent are decided at resolution, so a finder never loses
//! for picking the wrong label for a real bug.

use serde::{Deserialize, Serialize};

use crate::{round2, LedgerEntry};

/// The platform bounty pool, which posts bonds for challenges raised by
/// review quorum. It is never paid a backer's share of a bounty.
pub const PLATFORM_POOL: &str = "platform:pool";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Clean,
    Problem,
}

impl Verdict {
    pub fn other(self) -> Verdict {
        match self {
            Verdict::Clean => Verdict::Problem,
            Verdict::Problem => Verdict::Clean,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Malicious,
    Accidental,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedVia {
    /// A sandbox-verified failing test: objective evidence beats stake.
    Proof,
    Timeout,
    Jury,
    Council,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Staking,
    Jury,
    Resolved,
}

pub struct Params {
    /// Smallest bond that quarantines a patch on flagging.
    pub min_flag_bond: f64,
    /// How long each staking round stays open.
    pub round_ms: i64,
    /// Total stake at which the dispute leaves the bond game for a jury.
    pub jury_threshold: f64,
    pub max_rounds: u32,
    /// Share of the losing side's stake withheld for the pool and jurors.
    pub fee: f64,
    /// Share of the bounty paid to the first flagger.
    pub finder_share: f64,
    /// Rep lost per unit of adoption weight by operators who vouched for a problem patch.
    pub adopter_slash: f64,
}

pub const PARAMS: Params = Params {
    min_flag_bond: 10.0,
    round_ms: 24 * 3_600_000,
    jury_threshold: 2_000.0,
    max_rounds: 6,
    fee: 0.1,
    finder_share: 0.8,
    adopter_slash: 2.0,
};

/// Share of the author's balance slashed. A malicious author forfeits
/// everything and is banned (see [`Settlement::bans`]); an honest mistake
/// costs in proportion to how serious it was.
pub fn author_slash(intent: Intent, severity: Severity) -> f64 {
    let base: f64 = match intent {
        Intent::Malicious => return 1.0,
        Intent::Unknown => 0.25,
        Intent::Accidental => 0.1,
    };
    let factor = match severity {
        Severity::Low => 0.25,
        Severity::Medium => 0.5,
        Severity::High => 1.0,
        Severity::Critical => 2.0,
    };
    (base * factor).min(1.0)
}

/// Bounty paid from the platform pool, multiplied by ln(1 + independent
/// adopters). For a flag the finder chose to raise, a patch nobody else used
/// pays only the slashed stakes, so planting a bug with one identity and
/// flagging it with another earns nothing from the pool. A finding from
/// *assigned* review gets at least the base amount: reviewers cannot pick
/// their shards, so they cannot farm it, and catching a problem before
/// anyone adopts it is the most valuable catch of all.
pub fn severity_bounty(s: Severity) -> f64 {
    match s {
        Severity::Low => 20.0,
        Severity::Medium => 100.0,
        Severity::High => 500.0,
        Severity::Critical => 2_500.0,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contribution {
    pub operator_id: String,
    pub side: Verdict,
    pub amount: f64,
    pub round: u32,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub verdict: Verdict,
    pub via: ResolvedVia,
    pub severity: Severity,
    pub intent: Intent,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    pub id: String,
    pub patch_id: String,
    pub flagged_by: String,
    /// As claimed; may be revised at resolution.
    pub severity: Severity,
    /// sha256 of the evidence and a salt, fixing the finder's priority before the evidence is revealed.
    pub evidence_commitment: String,
    pub opened_at: i64,
    pub contributions: Vec<Contribution>,
    /// (round, side) pairs whose bond was filled; only filled stakes are at risk.
    pub filled: Vec<(u32, Verdict)>,
    pub round: u32,
    pub tentative: Verdict,
    pub round_ends_at: i64,
    pub phase: Phase,
    pub resolution: Option<Resolution>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChallengeError(pub String);

impl std::fmt::Display for ChallengeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChallengeError {}

fn fail<T>(msg: impl Into<String>) -> Result<T, ChallengeError> {
    Err(ChallengeError(msg.into()))
}

pub struct OpenChallenge<'a> {
    pub id: &'a str,
    pub patch_id: &'a str,
    pub flagged_by: &'a str,
    pub severity: Severity,
    pub evidence_commitment: &'a str,
    /// 0 for a bare report, which callers allow only after a proof of work.
    pub bond: f64,
    pub now: i64,
}

impl Challenge {
    /// Opens a challenge. A bond of at least `min_flag_bond` quarantines the
    /// patch at once (the flagger acts as Augur's designated reporter). A
    /// bare report is recorded but the patch stays live until someone stakes.
    pub fn open(o: OpenChallenge) -> Result<Challenge, ChallengeError> {
        if o.bond < 0.0 {
            return fail("bond must be non-negative");
        }
        if o.bond > 0.0 && o.bond < PARAMS.min_flag_bond {
            return fail(format!("a bond must be at least {}", PARAMS.min_flag_bond));
        }
        let bonded = o.bond > 0.0;
        Ok(Challenge {
            id: o.id.into(),
            patch_id: o.patch_id.into(),
            flagged_by: o.flagged_by.into(),
            severity: o.severity,
            evidence_commitment: o.evidence_commitment.into(),
            opened_at: o.now,
            contributions: if bonded {
                vec![Contribution {
                    operator_id: o.flagged_by.into(),
                    side: Verdict::Problem,
                    amount: o.bond,
                    round: 0,
                    at: o.now,
                }]
            } else {
                vec![]
            },
            filled: if bonded {
                vec![(0, Verdict::Problem)]
            } else {
                vec![]
            },
            round: 0,
            tentative: if bonded {
                Verdict::Problem
            } else {
                Verdict::Clean
            },
            round_ends_at: o.now + PARAMS.round_ms,
            phase: Phase::Staking,
            resolution: None,
        })
    }

    fn is_filled(&self, c: &Contribution) -> bool {
        self.filled.contains(&(c.round, c.side))
    }

    fn filled_total(&self, side: Option<Verdict>) -> f64 {
        self.contributions
            .iter()
            .filter(|c| self.is_filled(c) && side.is_none_or(|s| c.side == s))
            .map(|c| c.amount)
            .sum()
    }

    /// Whether the patch should be held out of trunks and shown with a warning.
    pub fn quarantined(&self) -> bool {
        match &self.resolution {
            Some(r) => r.verdict == Verdict::Problem,
            None => self.tentative == Verdict::Problem,
        }
    }

    /// Stake needed to overturn the tentative outcome this round: Augur's
    /// escalating bond, 2·(total at stake) − 3·(already on the challenger's
    /// side). Each overturn roughly doubles the stake, so a dispute either
    /// settles quickly or becomes expensive enough to justify a jury.
    pub fn bond_to_flip(&self) -> f64 {
        let side = self.tentative.other();
        (2.0 * self.filled_total(None) - 3.0 * self.filled_total(Some(side)))
            .max(PARAMS.min_flag_bond)
    }

    /// Contributed so far this round towards overturning.
    pub fn pending(&self) -> f64 {
        let side = self.tentative.other();
        self.contributions
            .iter()
            .filter(|c| c.round == self.round && c.side == side)
            .map(|c| c.amount)
            .sum()
    }

    /// Adds stake against the tentative outcome; several operators may fill
    /// one bond (crowdsourced, as in Augur). Returns the amount accepted;
    /// any excess over the bond is not taken.
    pub fn contribute(
        &mut self,
        operator_id: &str,
        side: Verdict,
        amount: f64,
        now: i64,
    ) -> Result<f64, ChallengeError> {
        self.advance(now);
        if self.phase != Phase::Staking {
            return fail(format!("challenge is in phase {:?}", self.phase));
        }
        if side == self.tentative {
            return fail(format!(
                "{side:?} is already the tentative outcome; stake against it or wait"
            ));
        }
        if amount.is_nan() || amount <= 0.0 {
            return fail("amount must be positive");
        }
        let need = self.bond_to_flip() - self.pending();
        let accepted = amount.min(need);
        self.contributions.push(Contribution {
            operator_id: operator_id.into(),
            side,
            amount: accepted,
            round: self.round,
            at: now,
        });
        if accepted >= need {
            self.filled.push((self.round, side));
            self.tentative = side;
            self.round += 1;
            self.round_ends_at = now + PARAMS.round_ms;
            if self.filled_total(None) >= PARAMS.jury_threshold || self.round >= PARAMS.max_rounds {
                self.phase = Phase::Jury;
            }
        }
        Ok(accepted)
    }

    /// Resolves by timeout if the current round expired without being overturned.
    pub fn advance(&mut self, now: i64) {
        if self.phase == Phase::Staking && now >= self.round_ends_at {
            let r = Resolution {
                verdict: self.tentative,
                via: ResolvedVia::Timeout,
                severity: self.severity,
                intent: Intent::Unknown,
                at: self.round_ends_at,
            };
            self.phase = Phase::Resolved;
            self.resolution = Some(r);
        }
    }

    /// Final resolution by proof (any phase), jury or council.
    pub fn resolve(&mut self, mut r: Resolution) -> Result<(), ChallengeError> {
        if self.phase == Phase::Resolved {
            return fail("already resolved");
        }
        if r.via == ResolvedVia::Timeout {
            return fail("timeouts resolve through advance()");
        }
        if r.verdict == Verdict::Clean {
            r.intent = Intent::Unknown;
        }
        self.phase = Phase::Resolved;
        self.resolution = Some(r);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Exposure {
    pub operator_id: String,
    pub amount: f64,
}

#[derive(Debug, Clone)]
pub struct SettlementContext {
    pub author_operator: String,
    pub author_balance: f64,
    /// Operators that adopted the patch with a positive outcome, with their adoption weight.
    pub adopters: Vec<Exposure>,
    /// Standing "I reviewed this, it's clean" stakes, including review-duty
    /// verdicts of "clean" on the patch's shards.
    pub attestations: Vec<Exposure>,
    /// Platform bounty pool available for this payout.
    pub pool: f64,
    /// The problem was found by randomly assigned review, not a chosen flag.
    pub found_by_assigned_review: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settlement {
    pub entries: Vec<LedgerEntry>,
    /// Net change to the platform pool (fees and slashes in, bounty out).
    pub pool_delta: f64,
    pub bounty: f64,
    /// Operators to ban, with every agent key they hold revoked.
    pub bans: Vec<String>,
    /// Other patches by banned operators should be put under bare reports.
    pub rereview_author: bool,
}

pub fn settle(c: &Challenge, ctx: &SettlementContext) -> Result<Settlement, ChallengeError> {
    let Some(res) = c.resolution.as_ref() else {
        return fail("settle after resolution");
    };
    let mut entries = Vec::new();
    let mut pool = 0.0;

    // 1. Unfilled partial contributions are returned untouched.
    for x in c.contributions.iter().filter(|x| !c.is_filled(x)) {
        entries.push(LedgerEntry::new(
            &x.operator_id,
            x.amount,
            "refund: bond not filled",
        ));
    }

    // 2. Filled stakes: winners get theirs back plus the losers', less the fee.
    let filled: Vec<&Contribution> = c.contributions.iter().filter(|x| c.is_filled(x)).collect();
    let winners: Vec<&&Contribution> = filled.iter().filter(|x| x.side == res.verdict).collect();
    let losing: f64 = filled
        .iter()
        .filter(|x| x.side != res.verdict)
        .map(|x| x.amount)
        .sum();
    let winning: f64 = winners.iter().map(|x| x.amount).sum();
    let fee = losing * PARAMS.fee;
    pool += fee;
    for x in &winners {
        entries.push(LedgerEntry::new(
            &x.operator_id,
            round2(x.amount + (losing - fee) * x.amount / winning),
            "stake returned with winnings",
        ));
    }
    if winning == 0.0 {
        // Nobody backed the outcome (e.g. a proof overturned a bare report).
        pool += losing - fee;
    }

    if res.verdict == Verdict::Clean {
        return Ok(Settlement {
            entries,
            pool_delta: round2(pool),
            bounty: 0.0,
            bans: vec![],
            rereview_author: false,
        });
    }

    // 3. A real problem: slash those who vouched for it, then pay the bounty.
    let mut slashed = 0.0;
    for a in &ctx.attestations {
        entries.push(LedgerEntry::new(
            &a.operator_id,
            -a.amount,
            "attested clean on a problem patch",
        ));
        slashed += a.amount;
    }
    let independent: Vec<&Exposure> = ctx
        .adopters
        .iter()
        .filter(|a| a.operator_id != ctx.author_operator)
        .collect();
    for a in &independent {
        let amount = round2(PARAMS.adopter_slash * a.amount);
        entries.push(LedgerEntry::new(
            &a.operator_id,
            -amount,
            "adopted a problem patch",
        ));
        slashed += amount;
    }
    let author_loss = round2(ctx.author_balance.max(0.0) * author_slash(res.intent, res.severity));
    if author_loss > 0.0 {
        entries.push(LedgerEntry::new(
            &ctx.author_operator,
            -author_loss,
            format!("authored a problem patch ({:?})", res.intent).to_lowercase(),
        ));
        slashed += author_loss;
    }

    let exposure = (independent.len() as f64).ln_1p();
    let exposure = if ctx.found_by_assigned_review {
        exposure.max(1.0)
    } else {
        exposure
    };
    let from_pool = (ctx.pool + pool).min(severity_bounty(res.severity) * exposure);
    let bounty = round2(slashed + from_pool);
    pool -= from_pool;

    let backers: Vec<&&Contribution> = winners
        .iter()
        .filter(|x| x.operator_id != c.flagged_by && x.operator_id != PLATFORM_POOL)
        .copied()
        .collect();
    let backer_total: f64 = backers.iter().map(|x| x.amount).sum();
    let finder_cut = if backer_total > 0.0 {
        PARAMS.finder_share
    } else {
        1.0
    };
    entries.push(LedgerEntry::new(
        &c.flagged_by,
        round2(bounty * finder_cut),
        format!("bounty: {:?} problem found", res.severity).to_lowercase(),
    ));
    for x in backers {
        entries.push(LedgerEntry::new(
            &x.operator_id,
            round2(bounty * (1.0 - finder_cut) * x.amount / backer_total),
            "bounty share for backing the finding",
        ));
    }

    let malicious = res.intent == Intent::Malicious;
    Ok(Settlement {
        entries,
        pool_delta: round2(pool),
        bounty,
        bans: if malicious {
            vec![ctx.author_operator.clone()]
        } else {
            vec![]
        },
        rereview_author: malicious,
    })
}
