use sha2::{Digest, Sha256};

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(data.as_ref());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 32-bit FNV-1a.
pub fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// murmur3 finalizer: a cheap, well-mixed 32-bit permutation.
pub fn mix(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// Deterministic PRNG (splitmix32) seeded from a string, so a draw can be
/// re-verified by anyone holding the seed.
pub struct SeededRng(u32);

impl SeededRng {
    pub fn new(seed: &str) -> Self {
        let hex = sha256_hex(seed);
        Self(u32::from_str_radix(&hex[..8], 16).unwrap_or(0))
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9);
        let mut z = self.0;
        z = (z ^ (z >> 16)).wrapping_mul(0x85eb_ca6b);
        z = (z ^ (z >> 13)).wrapping_mul(0xc2b2_ae35);
        (z ^ (z >> 16)) as f64 / 4_294_967_296.0
    }

    /// Picks an index with probability proportional to `weights[i]`.
    pub fn weighted_index(&mut self, weights: &[f64]) -> Option<usize> {
        let total: f64 = weights.iter().sum();
        if total.is_nan() || total <= 0.0 {
            return None;
        }
        let mut r = self.next_f64() * total;
        for (i, w) in weights.iter().enumerate() {
            r -= w;
            if r < 0.0 {
                return Some(i);
            }
        }
        Some(weights.len() - 1)
    }
}
