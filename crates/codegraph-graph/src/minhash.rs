//! MinHash + band-LSH for near-duplicate blocking; a datasketch-compatible
//! drop-in.
//!
//! Per design §10.1: the permutation coefficients come from a fixed
//! deterministic PRNG (not numpy's MT19937), and shingles are hashed with blake3
//! rather than SHA-1. The hash family (Mersenne-prime affine permutations) and
//! the optimal-band search make blocking quality fully deterministic.

use std::collections::HashMap;

/// Mersenne prime modulus for the hash family (2^61 - 1).
const MP: u64 = (1 << 61) - 1;
/// 32-bit mask applied to permuted hashes (datasketch-compatible).
const MH: u64 = 0xFFFF_FFFF;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic `(a, b)` affine-permutation coefficients, seeded by a constant
/// so every `MinHash` shares the same family (required for LSH banding to work).
fn coeffs(num_perm: usize) -> (Vec<u64>, Vec<u64>) {
    let mut state: u64 = 1;
    let mut a = Vec::with_capacity(num_perm);
    let mut b = Vec::with_capacity(num_perm);
    for _ in 0..num_perm {
        a.push(splitmix64(&mut state) % (MP - 1) + 1); // [1, MP)
        b.push(splitmix64(&mut state) % MP); // [0, MP)
    }
    (a, b)
}

/// A MinHash sketch over a set of byte shingles.
pub struct MinHash {
    pub hashvalues: Vec<u64>,
    a: Vec<u64>,
    b: Vec<u64>,
}

impl MinHash {
    pub fn new(num_perm: usize) -> Self {
        let (a, b) = coeffs(num_perm);
        // Start every slot at the 32-bit max; `update` takes the running minimum.
        MinHash {
            hashvalues: vec![MH; num_perm],
            a,
            b,
        }
    }

    /// Fold one shingle into the sketch.
    pub fn update(&mut self, v: &[u8]) {
        let digest = blake3::hash(v);
        let bytes = digest.as_bytes();
        let hv = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u128;
        for i in 0..self.hashvalues.len() {
            let phv = ((self.a[i] as u128 * hv + self.b[i] as u128) % MP as u128) as u64 & MH;
            if phv < self.hashvalues[i] {
                self.hashvalues[i] = phv;
            }
        }
    }
}

/// Left-Riemann numerical integration (replaces scipy.integrate.quad) for the
/// LSH optimal-band search.
fn integrate(f: impl Fn(f64) -> f64, lo: f64, hi: f64, n: usize) -> f64 {
    let h = (hi - lo) / n as f64;
    (0..n).map(|i| f(lo + i as f64 * h)).sum::<f64>() * h
}

/// `(bands, rows)` minimising weighted FP+FN error for the threshold, matching
/// `_optimal_lsh_params`.
fn optimal_lsh_params(threshold: f64, num_perm: usize) -> (usize, usize) {
    let mut best_err = f64::INFINITY;
    let mut best = (1usize, 1usize);
    for b in 1..=num_perm {
        for r in 1..=(num_perm / b) {
            let (bf, rf) = (b as f64, r as f64);
            let fp = integrate(|s| 1.0 - (1.0 - s.powf(rf)).powf(bf), 0.0, threshold, 128);
            let fne = integrate(|s| (1.0 - s.powf(rf)).powf(bf), threshold, 1.0, 128);
            let err = 0.5 * fp + 0.5 * fne;
            if err < best_err {
                best_err = err;
                best = (b, r);
            }
        }
    }
    best
}

/// Band-hashing LSH over MinHash sketches. `query` returns candidate keys that
/// share at least one band with the probe.
pub struct MinHashLsh {
    b: usize,
    r: usize,
    tables: Vec<HashMap<Vec<u64>, Vec<String>>>,
    keys: std::collections::HashSet<String>,
}

impl MinHashLsh {
    pub fn new(threshold: f64, num_perm: usize) -> Self {
        let (b, r) = optimal_lsh_params(threshold, num_perm);
        MinHashLsh {
            b,
            r,
            tables: (0..b).map(|_| HashMap::new()).collect(),
            keys: std::collections::HashSet::new(),
        }
    }

    fn band(&self, hv: &[u64], i: usize) -> Vec<u64> {
        hv[i * self.r..(i + 1) * self.r].to_vec()
    }

    /// Insert a sketch under `key`. Duplicate keys are ignored.
    pub fn insert(&mut self, key: &str, m: &MinHash) {
        if !self.keys.insert(key.to_string()) {
            return;
        }
        for i in 0..self.b {
            let band = self.band(&m.hashvalues, i);
            self.tables[i]
                .entry(band)
                .or_default()
                .push(key.to_string());
        }
    }

    /// Keys sharing at least one band with `m`.
    pub fn query(&self, m: &MinHash) -> Vec<String> {
        let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in 0..self.b {
            let band = self.band(&m.hashvalues, i);
            if let Some(keys) = self.tables[i].get(&band) {
                out.extend(keys.iter().cloned());
            }
        }
        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch(text: &str) -> MinHash {
        let mut m = MinHash::new(128);
        let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = t.as_bytes();
        if bytes.len() < 3 {
            m.update(bytes);
        } else {
            for w in bytes.windows(3) {
                m.update(w);
            }
        }
        m
    }

    fn jaccard(a: &MinHash, b: &MinHash) -> f64 {
        let same = a
            .hashvalues
            .iter()
            .zip(&b.hashvalues)
            .filter(|(x, y)| x == y)
            .count();
        same as f64 / a.hashvalues.len() as f64
    }

    #[test]
    fn similar_strings_have_high_estimated_jaccard() {
        let a = sketch("the quick brown fox jumps");
        let b = sketch("the quick brown fox jumped");
        let c = sketch("totally different content here");
        assert!(jaccard(&a, &b) > 0.5, "near-identical should be similar");
        assert!(jaccard(&a, &c) < 0.3, "unrelated should be dissimilar");
    }

    #[test]
    fn lsh_blocks_similar_and_separates_dissimilar() {
        let mut lsh = MinHashLsh::new(0.7, 128);
        let a = sketch("knowledge graph extractor");
        let b = sketch("knowledge graph extracter"); // typo
        let c = sketch("unrelated banana smoothie");
        lsh.insert("a", &a);
        lsh.insert("c", &c);
        let hits = lsh.query(&b);
        assert!(
            hits.contains(&"a".to_string()),
            "typo variant should block with a"
        );
        assert!(
            !hits.contains(&"c".to_string()),
            "unrelated should not block"
        );
    }

    #[test]
    fn coeffs_are_deterministic() {
        let (a1, b1) = coeffs(8);
        let (a2, b2) = coeffs(8);
        assert_eq!(a1, a2);
        assert_eq!(b1, b2);
        assert!(a1.iter().all(|&x| (1..MP).contains(&x)));
        assert!(b1.iter().all(|&x| x < MP));
    }

    #[test]
    fn optimal_params_fit_num_perm() {
        let (b, r) = optimal_lsh_params(0.7, 128);
        assert!(b * r <= 128);
        assert!(b >= 1 && r >= 1);
    }
}
