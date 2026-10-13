//! MinHash sketches with locality-sensitive hashing (LSH) bands, for
//! near-duplicate patch detection.
//!
//! Exact hunk fingerprints only catch identical edits. MinHash estimates
//! the Jaccard similarity of two patches' feature sets from fixed-size
//! signatures, and LSH turns "find similar signatures" into a handful of
//! exact key lookups: each patch writes `BANDS` keys and each query reads
//! `BANDS` keys, whatever the corpus size. See docs/SEARCH.md.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::diff::{normalized_change, FileDiff};
use crate::hash::{fnv1a, mix};

pub const PERMUTATIONS: usize = 32;
pub const BANDS: usize = 8;
/// Rows per band. Candidate probability ≈ 1 − (1 − s⁴)⁸.
pub const ROWS: usize = PERMUTATIONS / BANDS;

fn seeds() -> [u32; PERMUTATIONS] {
    let mut s = [0u32; PERMUTATIONS];
    for (i, v) in s.iter_mut().enumerate() {
        *v = mix(0x9e37_79b9 ^ ((i as u32 + 1).wrapping_mul(0x7f4a_7c15)));
    }
    s
}

/// Features of a patch: each changed line (normalized) and each pair of
/// consecutive changed lines, so order matters a little. The file's base
/// name is folded in so identical one-liners in unrelated files don't
/// collide, while moves between directories still match. A binary file is
/// one feature: the content it ends up with.
pub fn features(files: &[FileDiff]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for f in files {
        let base = f.path().rsplit('/').next().unwrap_or_default();
        if let Some(bin) = &f.binary {
            out.insert(format!("{base}\0binary\0{}", bin.new_id));
        }
        for h in &f.hunks {
            let mut prev: Option<String> = None;
            for norm in h.lines.iter().filter_map(|l| normalized_change(l)) {
                out.insert(format!("{base}\0{norm}"));
                if let Some(p) = &prev {
                    out.insert(format!("{base}\0{p}\0{norm}"));
                }
                prev = Some(norm);
            }
        }
    }
    out.into_iter().collect()
}

pub type Signature = [u32; PERMUTATIONS];

pub fn signature<S: AsRef<str>>(features: &[S]) -> Signature {
    let seeds = seeds();
    let mut sig = [u32::MAX; PERMUTATIONS];
    for f in features {
        let h = fnv1a(f.as_ref().as_bytes());
        for (slot, seed) in sig.iter_mut().zip(seeds) {
            *slot = (*slot).min(mix(h ^ seed));
        }
    }
    sig
}

pub fn encode_signature(sig: &Signature) -> String {
    sig.iter().map(|v| format!("{v:08x}")).collect()
}

pub fn decode_signature(hex: &str) -> Option<Signature> {
    let mut sig = [0u32; PERMUTATIONS];
    for (i, slot) in sig.iter_mut().enumerate() {
        *slot = u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok()?;
    }
    Some(sig)
}

/// LSH band keys. Two patches become candidates if any band key matches.
pub fn bands(sig: &Signature) -> Vec<String> {
    (0..BANDS)
        .map(|b| {
            let mut h = 0x811c_9dc5u32 ^ b as u32;
            for r in 0..ROWS {
                h = mix(h ^ sig[b * ROWS + r]);
            }
            format!("{b}:{h:08x}")
        })
        .collect()
}

/// Estimated Jaccard similarity of the underlying feature sets.
pub fn estimate_jaccard(a: &Signature, b: &Signature) -> f64 {
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f64 / PERMUTATIONS as f64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchSketch {
    pub signature: String,
    pub bands: Vec<String>,
}

pub fn sketch(files: &[FileDiff]) -> Option<PatchSketch> {
    let feats = features(files);
    if feats.is_empty() {
        return None;
    }
    let sig = signature(&feats);
    Some(PatchSketch {
        signature: encode_signature(&sig),
        bands: bands(&sig),
    })
}
