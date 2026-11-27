//! Reversible inter-channel transforms (format 1.1, sub-frame mode 7 + ext id).
//!
//! Ordinary floating-point addition and subtraction are **not** reversible
//! (`(a + b) - b != a` in general, and `-0.0` / NaN payloads would be lost),
//! so no transform here touches float values. Two transforms are provided:
//!
//! * **FloatSplitRef** (ext 0) — integer domain. The coarse integers of
//!   channel `k` (exact integer images of its float32 values at grid `g`,
//!   see [`crate::ieee754`]) are predicted from the coarse integers of one or
//!   two earlier channels at the same grid:
//!   `d[i] = c_k[i] − ((Σ w_r · c_{j_r}[i]) >> 6)` with integer weights
//!   `w_r ∈ [−512, 511]` (wrapping `i64`). The decoder adds the identical
//!   integer prediction back, so reconstruction is exact by construction.
//!   `w = 64` is a plain difference (left/side), `w = 32` a half, etc.
//!   The channel's own extra bits are coded unchanged.
//! * **XorRef** (ext 1) — bitwise. The raw bit patterns are XORed with an
//!   earlier channel's: `x[i] = s_k[i] ^ s_j[i]`, and `x` is coded with any
//!   simple mode. XOR is its own inverse on every pattern; dual-mono
//!   channels become Constant(0), polarity-inverted ones Constant(0x80000000).
//!
//! Layouts:
//! ```text
//! FloatSplitRef: nref−1:1 { γ(k−j) weight:s10 }×nref  FloatSplit phase-1 (header + coarse stream of d)
//!                phase 2: FloatSplit extras of channel k
//! XorRef:        γ(k−j) inner:3 inner payload (Constant/Raw/XorLz/FieldVlc/XorChimp/Rle of x)
//! ```

use crate::bitio::{BitReader, BitWriter};
use crate::variable_integer::{gamma_bits, read_gamma, write_gamma};
use crate::{Error, Result};

pub const WEIGHT_SHIFT: u32 = 6;
pub const WEIGHT_BITS: u32 = 10;
const WMIN: i64 = -(1 << (WEIGHT_BITS - 1));
const WMAX: i64 = (1 << (WEIGHT_BITS - 1)) - 1;
/// Only the nearest `MAX_REF_DISTANCE` earlier channels are considered as
/// references (bounds encoder effort for many-channel files).
pub const MAX_REF_DISTANCE: usize = 8;

/// Inter-channel references of one channel: `(channel, weight)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refs(pub Vec<(usize, i32)>);

impl Refs {
    pub fn header_bits(&self, k: usize) -> u64 {
        1 + self.0.iter().map(|&(j, _)| gamma_bits((k - j) as u64) + WEIGHT_BITS as u64).sum::<u64>()
    }

    pub fn write(&self, w: &mut BitWriter, k: usize) {
        w.write_bits(self.0.len() as u64 - 1, 1);
        for &(j, wt) in &self.0 {
            write_gamma(w, (k - j) as u64);
            w.write_bits(wt as u32 as u64, WEIGHT_BITS);
        }
    }

    pub fn read(r: &mut BitReader, k: usize) -> Result<Refs> {
        let n = r.read_bits(1)? as usize + 1;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let d = read_gamma(r)?;
            if d == 0 || d > k as u64 {
                return Err(Error::Corrupt("inter-channel reference out of range"));
            }
            let raw = r.read_bits(WEIGHT_BITS)? as u32;
            let sh = 32 - WEIGHT_BITS;
            v.push((k - d as usize, ((raw << sh) as i32) >> sh));
        }
        Ok(Refs(v))
    }
}

/// Integer prediction of channel `k`'s coarse value at `i` (wrapping).
#[inline]
pub fn predict(refs: &Refs, coarse: &[Vec<i64>], i: usize) -> i64 {
    let mut acc = 0i64;
    for &(j, w) in &refs.0 {
        acc = acc.wrapping_add((w as i64).wrapping_mul(coarse[j][i]));
    }
    acc >> WEIGHT_SHIFT
}

/// `d = c − prediction` for the encoder.
pub fn residual_stream(refs: &Refs, c: &[i64], coarse: &[Vec<i64>]) -> Vec<i64> {
    c.iter().enumerate().map(|(i, &v)| v.wrapping_sub(predict(refs, coarse, i))).collect()
}

/// Decoder: turn `d` (in place) back into the coarse values of channel `k`.
pub fn reconstruct(refs: &Refs, k: usize, coarse: &mut [Vec<i64>]) {
    let (before, rest) = coarse.split_at_mut(k);
    let ck = &mut rest[0];
    #[allow(clippy::needless_range_loop)]
    for i in 0..ck.len() {
        ck[i] = ck[i].wrapping_add(predict(refs, before, i));
    }
}

fn q(w: f64) -> i32 {
    (w * (1 << WEIGHT_SHIFT) as f64).round().clamp(WMIN as f64, WMAX as f64) as i32
}

/// Least-squares weight for `c ≈ w · a` (encoder only; floats choose the
/// weight, the transform itself is integer).
pub fn ls_weight(c: &[i64], a: &[i64]) -> i32 {
    let (mut ca, mut aa) = (0.0f64, 0.0f64);
    for (&x, &y) in c.iter().zip(a) {
        ca += x as f64 * y as f64;
        aa += y as f64 * y as f64;
    }
    if aa <= 0.0 || !ca.is_finite() || !aa.is_finite() {
        return 0;
    }
    q(ca / aa)
}

/// Least-squares weights for `c ≈ w1 · a + w2 · b`.
pub fn ls_weights2(c: &[i64], a: &[i64], b: &[i64]) -> Option<(i32, i32)> {
    let (mut aa, mut bb, mut ab, mut ca, mut cb) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for i in 0..c.len() {
        let (x, y, z) = (c[i] as f64, a[i] as f64, b[i] as f64);
        aa += y * y;
        bb += z * z;
        ab += y * z;
        ca += x * y;
        cb += x * z;
    }
    let det = aa * bb - ab * ab;
    if !det.is_finite() || det.abs() < 1e-9 * (aa * bb).max(1e-300) {
        return None;
    }
    Some((q((ca * bb - cb * ab) / det), q((cb * aa - ca * ab) / det)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstruct_inverts_residual_for_extreme_values() {
        let big = (1i64 << 59) - 3;
        let a: Vec<i64> = (0..200).map(|i| if i % 3 == 0 { big } else { -big + i }).collect();
        let b: Vec<i64> = (0..200).map(|i| (i * 7919) % 1000 - 500).collect();
        let c: Vec<i64> = (0..200).map(|i| a[i] / 2 - b[i] + 3).collect();
        for refs in [Refs(vec![(0, 64)]), Refs(vec![(0, 32), (1, -64)]), Refs(vec![(1, -512), (0, 511)])] {
            let mut coarse = vec![a.clone(), b.clone(), Vec::new()];
            let d = residual_stream(&refs, &c, &coarse);
            coarse[2] = d;
            reconstruct(&refs, 2, &mut coarse);
            assert_eq!(coarse[2], c);
        }
    }

    #[test]
    fn header_roundtrip() {
        let refs = Refs(vec![(3, -512), (1, 511)]);
        let mut w = BitWriter::new();
        refs.write(&mut w, 5);
        assert_eq!(w.bit_len(), refs.header_bits(5));
        let b = w.into_bytes();
        assert_eq!(Refs::read(&mut BitReader::new(&b), 5).unwrap(), refs);
    }

    #[test]
    fn ls_recovers_simple_relations() {
        let a: Vec<i64> = (0..1000).map(|i| ((i as f64 * 0.1).sin() * 1e6) as i64).collect();
        let b: Vec<i64> = (0..1000).map(|i| ((i as f64 * 0.37).cos() * 1e6) as i64).collect();
        let c: Vec<i64> = a.iter().zip(&b).map(|(x, y)| x / 2 + y / 2).collect();
        assert_eq!(ls_weight(&a, &a), 64);
        assert_eq!(ls_weights2(&c, &a, &b), Some((32, 32)));
    }
}
