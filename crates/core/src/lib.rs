//! Collective's protocol core.
//!
//! Everything here is pure and deterministic: no I/O, no clocks, no
//! randomness except from explicit seeds. The same crate compiles into the
//! Worker (wasm32) and into native tools, so any agent can recompute a
//! score, a trunk, a jury draw or a settlement and check the server's work.

#![forbid(unsafe_code)]

pub mod challenge;
pub mod converge;
pub mod diff;
pub mod hash;
pub mod jury;
pub mod proof;
pub mod review;
pub mod score;
pub mod sketch;
pub mod text;
pub mod work;

/// A change to an operator's reputation balance. Settlements and gradings
/// return these; the caller applies them to its ledger.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntry {
    pub operator_id: String,
    pub delta: f64,
    pub reason: String,
}

impl LedgerEntry {
    pub fn new(operator_id: impl Into<String>, delta: f64, reason: impl Into<String>) -> Self {
        Self {
            operator_id: operator_id.into(),
            delta,
            reason: reason.into(),
        }
    }
}

/// Sum of deltas for one operator; convenient for tests and reports.
pub fn net(entries: &[LedgerEntry], operator_id: &str) -> f64 {
    entries
        .iter()
        .filter(|e| e.operator_id == operator_id)
        .map(|e| e.delta)
        .sum()
}

pub(crate) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
