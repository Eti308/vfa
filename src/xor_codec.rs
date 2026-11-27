//! XOR-based bit-pattern coding with multi-reference windows ("XorChimp",
//! sub-frame mode 5).
//!
//! Every sample is XORed with one of the previous `N` raw bit patterns
//! (`N = 2^log2n`, 1 … 128). Only the bit region where the XOR is non-zero is
//! stored. No floating-point arithmetic is involved; the transform is a pure
//! function of the 32-bit patterns, so every pattern (±0, subnormals, ±Inf,
//! NaN payloads) is reproduced exactly.
//!
//! Compared with Gorilla-style coding (mode 2, [`crate::sample_codec`]), which
//! reuses the previous *exact* leading/trailing-zero window, this scheme
//! (after Chimp / Chimp128, Liakos et al., VLDB 2022):
//!
//! * rounds leading-zero counts down to 8 buckets (3 bits) and reuses the
//!   previous bucket with a 2-bit flag;
//! * has a separate code for XORs with many trailing zeros;
//! * may reference any of the last `N` samples, which captures periodic
//!   patterns and alternating values.
//!
//! Layout: `log2n:3 first:32`, then per further sample one of
//! ```text
//! 00 ref:log2n                                   XOR = 0 (exact repeat of sample i-1-ref)
//! 01 ref:log2n lzb:3 (len-1):5 bits:len          XOR with sample i-1-ref; trailing zeros implied
//! 10 bits:(32-LZ[prev_lzb])                      XOR with sample i-1, same leading-zero bucket
//! 11 lzb:3 bits:(32-LZ[lzb])                     XOR with sample i-1, new bucket
//! ```
//! The previous bucket starts at "none" (cases `10` invalid until set) and is
//! updated by every `01`, `10` and `11` code.

use crate::bitio::{BitReader, BitWriter};
use crate::{Error, Result};

/// Leading-zero buckets (Chimp).
const LZ: [u32; 8] = [0, 8, 12, 16, 18, 20, 22, 24];

#[inline]
fn bucket(lz: u32) -> u32 {
    // largest bucket index whose value is <= lz
    match lz {
        0..=7 => 0,
        8..=11 => 1,
        12..=15 => 2,
        16..=17 => 3,
        18..=19 => 4,
        20..=21 => 5,
        22..=23 => 6,
        _ => 7,
    }
}

/// Choice for one sample.
#[derive(Clone, Copy)]
enum Code {
    Same { r: u32 },
    Center { r: u32, lzb: u32, len: u32, bits: u32 },
    PrevBucket { x: u32 },
    NewBucket { lzb: u32, x: u32 },
}

fn choose(s: &[u32], i: usize, log2n: u32, prev_lzb: Option<u32>) -> (Code, u32) {
    let n = 1usize << log2n;
    let refs = n.min(i);
    let rb = log2n;
    // exact repeat anywhere in the window is always cheapest
    for r in 0..refs {
        if s[i] == s[i - 1 - r] {
            return (Code::Same { r: r as u32 }, 2 + rb);
        }
    }
    // XOR with the previous sample, bucket codes
    let x = s[i] ^ s[i - 1];
    let lzb = bucket(x.leading_zeros());
    let mut best = match prev_lzb {
        Some(p) if p == lzb => (Code::PrevBucket { x }, 2 + 32 - LZ[p as usize]),
        // a smaller previous bucket also covers x (only wastes leading bits)
        Some(p) if p < lzb && 32 - LZ[p as usize] <= 3 + 32 - LZ[lzb as usize] => {
            (Code::PrevBucket { x }, 2 + 32 - LZ[p as usize])
        }
        _ => (Code::NewBucket { lzb, x }, 2 + 3 + 32 - LZ[lzb as usize]),
    };
    // centre codes against every reference
    for r in 0..refs {
        let x = s[i] ^ s[i - 1 - r];
        let lzb = bucket(x.leading_zeros());
        let tz = x.trailing_zeros();
        let len = 32 - LZ[lzb as usize] - tz;
        let cost = 2 + rb + 3 + 5 + len;
        if cost < best.1 {
            best = (Code::Center { r: r as u32, lzb, len, bits: x >> tz }, cost);
        }
    }
    best
}

/// Encode with a given window size.
pub fn encode(w: &mut BitWriter, s: &[u32], log2n: u32) {
    debug_assert!(log2n <= 7);
    w.write_bits(log2n as u64, 3);
    let Some(&first) = s.first() else { return };
    w.write_bits(first as u64, 32);
    let mut prev_lzb: Option<u32> = None;
    for i in 1..s.len() {
        let (code, _) = choose(s, i, log2n, prev_lzb);
        match code {
            Code::Same { r } => {
                w.write_bits(0b00, 2);
                w.write_bits(r as u64, log2n);
            }
            Code::Center { r, lzb, len, bits } => {
                w.write_bits(0b01, 2);
                w.write_bits(r as u64, log2n);
                w.write_bits(lzb as u64, 3);
                w.write_bits(len as u64 - 1, 5);
                w.write_bits(bits as u64, len);
                prev_lzb = Some(lzb);
            }
            Code::PrevBucket { x } => {
                let p = prev_lzb.expect("previous bucket");
                w.write_bits(0b10, 2);
                w.write_bits(x as u64, 32 - LZ[p as usize]);
            }
            Code::NewBucket { lzb, x } => {
                w.write_bits(0b11, 2);
                w.write_bits(lzb as u64, 3);
                w.write_bits(x as u64, 32 - LZ[lzb as usize]);
                prev_lzb = Some(lzb);
            }
        }
    }
}

/// Exact encoded size in bits for window `2^log2n`.
pub fn cost(s: &[u32], log2n: u32) -> u64 {
    if s.is_empty() {
        return 3;
    }
    let mut prev_lzb = None;
    let mut bits = 3 + 32u64;
    for i in 1..s.len() {
        let (code, c) = choose(s, i, log2n, prev_lzb);
        match code {
            Code::Center { lzb, .. } | Code::NewBucket { lzb, .. } => prev_lzb = Some(lzb),
            _ => {}
        }
        bits += c as u64;
    }
    bits
}

/// Best window size among `candidates` (exact costs); returns `(log2n, bits)`.
pub fn best_window(s: &[u32], candidates: &[u32]) -> (u32, u64) {
    candidates.iter().map(|&l| (l, cost(s, l))).min_by_key(|&(l, c)| (c, l)).unwrap_or((0, cost(s, 0)))
}

pub fn decode(r: &mut BitReader, out: &mut [u32]) -> Result<()> {
    let log2n = r.read_bits(3)? as u32;
    if out.is_empty() {
        return Ok(());
    }
    out[0] = r.read_bits(32)? as u32;
    let mut prev_lzb: Option<u32> = None;
    for i in 1..out.len() {
        let flag = r.read_bits(2)?;
        out[i] = match flag {
            0b00 => {
                let rf = r.read_bits(log2n)? as usize;
                *out.get(i.wrapping_sub(1 + rf))
                    .filter(|_| rf < i)
                    .ok_or(Error::Corrupt("xorchimp reference before start"))?
            }
            0b01 => {
                let rf = r.read_bits(log2n)? as usize;
                if rf >= i {
                    return Err(Error::Corrupt("xorchimp reference before start"));
                }
                let lzb = r.read_bits(3)? as u32;
                let len = r.read_bits(5)? as u32 + 1;
                let lz = LZ[lzb as usize];
                if lz + len > 32 {
                    return Err(Error::Corrupt("xorchimp centre exceeds 32 bits"));
                }
                prev_lzb = Some(lzb);
                let tz = 32 - lz - len;
                out[i - 1 - rf] ^ ((r.read_bits(len)? as u32) << tz)
            }
            0b10 => {
                let p = prev_lzb.ok_or(Error::Corrupt("xorchimp bucket reused before definition"))?;
                out[i - 1] ^ (r.read_bits(32 - LZ[p as usize])? as u32)
            }
            _ => {
                let lzb = r.read_bits(3)? as u32;
                prev_lzb = Some(lzb);
                out[i - 1] ^ (r.read_bits(32 - LZ[lzb as usize])? as u32)
            }
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &[u32], log2n: u32) -> u64 {
        let mut w = BitWriter::new();
        encode(&mut w, s, log2n);
        assert_eq!(w.bit_len(), cost(s, log2n), "exact cost");
        let bits = w.bit_len();
        let b = w.into_bytes();
        let mut out = vec![0; s.len()];
        decode(&mut BitReader::new(&b), &mut out).unwrap();
        assert_eq!(out, s);
        bits
    }

    #[test]
    fn roundtrip_specials_and_noise() {
        let mut v = vec![0, 0x8000_0000, 0, 0x8000_0000, 1, 0x7FC0_0001, 0xFF80_0000, 0x7F80_0000, 0x7FFF_FFFF];
        let mut x = 0x2545_F491u32;
        for i in 0..3000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push(match i % 4 {
                0 => x,
                1 => ((i as f32 * 0.01).sin()).to_bits(),
                2 => v[v.len() - 7],
                _ => x & 0xFFFF_0000,
            });
        }
        for l in 0..=7 {
            rt(&v, l);
        }
        rt(&[], 3);
        rt(&[42], 3);
    }

    #[test]
    fn periodic_pattern_is_nearly_free_with_window() {
        let period: Vec<u32> = (0..48).map(|i| ((i as f32 * 0.13).sin()).to_bits()).collect();
        let s: Vec<u32> = period.iter().cycle().take(4800).cloned().collect();
        let small = rt(&s, 0);
        let wide = rt(&s, 6); // window 64 >= period 48
        assert!(wide < small / 3, "wide {wide} small {small}");
        assert!(wide < 4800 * 9);
    }
}
