//! Hashcash-style proof of work, kept only as an anti-spam ticket for
//! stake-less bare reports. It rate-limits; it cannot make a report honest,
//! because it costs a liar what it costs a truth-teller. Participation is
//! paid for with *useful* work instead: review duty (see `review`).

use crate::hash::sha256_hex;

/// Difficulty for a stake-less report: about 2^20 hashes.
pub const REPORT_BITS: u32 = 20;

fn leading_zero_bits(hex: &str) -> u32 {
    let mut bits = 0;
    for ch in hex.chars() {
        let n = ch.to_digit(16).unwrap_or(0);
        if n == 0 {
            bits += 4;
        } else {
            return bits + n.leading_zeros() - 28;
        }
    }
    bits
}

/// `resource` must bind the work to one report: operator, patch, evidence
/// commitment and a recent timestamp.
pub fn verify_work(resource: &str, nonce: &str, bits: u32) -> bool {
    leading_zero_bits(&sha256_hex(format!("{resource}:{nonce}"))) >= bits
}

pub fn solve_work(resource: &str, bits: u32) -> String {
    (0u64..)
        .map(|i| format!("{i:x}"))
        .find(|n| verify_work(resource, n, bits))
        .expect("unbounded search")
}
