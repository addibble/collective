//! Jury escalation, after Kleros: jurors are drawn with probability
//! proportional to staked rep, vote by commit–reveal so nobody can copy the
//! crowd, and are paid for agreeing with the final majority (a Schelling
//! point) out of the stakes of those who did not.

use std::collections::HashSet;

use crate::challenge::Verdict;
use crate::hash::{sha256_hex, SeededRng};
use crate::LedgerEntry;

pub const BASE_SIZE: usize = 3;
/// After this many appeals the platform's security council decides.
pub const MAX_APPEALS: u32 = 3;
/// Share of a juror's seat stake lost for voting against the majority or not revealing.
pub const PENALTY: f64 = 0.2;
/// Stake a drawn juror locks per seat.
pub const SEAT_STAKE: f64 = 20.0;

/// Kleros-style growth: each appeal draws twice the jurors plus one.
pub fn jury_size(appeal: u32) -> usize {
    (0..appeal).fold(BASE_SIZE, |n, _| 2 * n + 1)
}

#[derive(Debug, Clone)]
pub struct JuryCandidate {
    pub operator_id: String,
    pub stake: f64,
}

/// Draws `size` distinct jurors weighted by stake. `exclude` must hold
/// every operator with an interest in the outcome. `seed` should be
/// committed before the candidate list closes so neither side can grind it.
pub fn draw_jury(
    candidates: &[JuryCandidate],
    size: usize,
    seed: &str,
    exclude: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let mut pool: Vec<&JuryCandidate> = candidates
        .iter()
        .filter(|c| c.stake >= SEAT_STAKE && !exclude.contains(&c.operator_id))
        .collect();
    if pool.len() < size {
        return Err(format!("need {size} eligible jurors, have {}", pool.len()));
    }
    let mut rng = SeededRng::new(seed);
    let mut chosen = Vec::with_capacity(size);
    while chosen.len() < size {
        let weights: Vec<f64> = pool.iter().map(|c| c.stake).collect();
        let i = rng
            .weighted_index(&weights)
            .expect("pool is non-empty with positive stakes");
        chosen.push(pool.remove(i).operator_id.clone());
    }
    Ok(chosen)
}

fn vote_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Clean => "clean",
        Verdict::Problem => "problem",
    }
}

pub fn commit_vote(operator_id: &str, vote: Verdict, salt: &str) -> String {
    sha256_hex(format!("{operator_id}:{}:{salt}", vote_str(vote)))
}

pub fn verify_reveal(commitment: &str, operator_id: &str, vote: Verdict, salt: &str) -> bool {
    commit_vote(operator_id, vote, salt) == commitment
}

#[derive(Debug, Clone)]
pub struct Ballot {
    pub operator_id: String,
    /// `None` if the juror never revealed.
    pub vote: Option<Verdict>,
}

/// Tallies revealed votes. A tie returns no verdict: appeal, or past
/// [`MAX_APPEALS`], the council.
pub fn tally(ballots: &[Ballot]) -> (Option<Verdict>, Vec<LedgerEntry>) {
    let clean = ballots
        .iter()
        .filter(|b| b.vote == Some(Verdict::Clean))
        .count();
    let problem = ballots
        .iter()
        .filter(|b| b.vote == Some(Verdict::Problem))
        .count();
    let penalty = SEAT_STAKE * PENALTY;
    let mut entries = Vec::new();
    let verdict = match clean.cmp(&problem) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(Verdict::Clean),
        std::cmp::Ordering::Less => Some(Verdict::Problem),
    };
    let Some(v) = verdict else {
        // Nobody is coherent with a tie; only non-revealers are penalized.
        for b in ballots.iter().filter(|b| b.vote.is_none()) {
            entries.push(LedgerEntry::new(
                &b.operator_id,
                -penalty,
                "juror did not reveal",
            ));
        }
        return (None, entries);
    };
    let coherent: Vec<&Ballot> = ballots.iter().filter(|b| b.vote == Some(v)).collect();
    let incoherent: Vec<&Ballot> = ballots.iter().filter(|b| b.vote != Some(v)).collect();
    for b in &incoherent {
        let why = if b.vote.is_some() {
            "juror voted against the majority"
        } else {
            "juror did not reveal"
        };
        entries.push(LedgerEntry::new(&b.operator_id, -penalty, why));
    }
    let reward = incoherent.len() as f64 * penalty / coherent.len() as f64;
    for b in coherent {
        entries.push(LedgerEntry::new(&b.operator_id, reward, "coherent juror"));
    }
    (Some(v), entries)
}
