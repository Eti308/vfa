//! Run-length / repeated-sequence coding (sub-frame mode 6, format 1.1).
//!
//! The channel is parsed into tokens over raw `u32` bit patterns, so `+0.0`
//! and `-0.0` (and distinct NaN payloads) are never merged:
//!
//! ```text
//! inner:3                       mode id used for the literal stream (1 Raw, 2 XorLz, 3 FieldVlc, 5 XorChimp)
//! tokens until N samples are covered:
//!   00 γ(len)                   LIT  – next `len` samples are taken from the literal stream
//!   01 γ(len)                   RUN  – repeat the previous sample `len` times (not at position 0)
//!   10 γ(dist) γ(len)           COPY – copy `len` samples starting `dist` back (1 ≤ dist ≤ i, may overlap)
//!   11                          invalid
//! literal stream                all LIT samples, concatenated, coded with `inner`
//! ```
//!
//! RUN covers digital silence and constant stretches; COPY covers repeated
//! sequences (loops, periodic test signals, repeated buffers). Coding the
//! literals with another sample-wise mode combines RLE with XOR/field coding.

use crate::bitio::{BitReader, BitWriter};
use crate::sample_codec::{self as sc, Mode};
use crate::variable_integer::{gamma_bits, read_gamma, write_gamma};
use crate::{xor_codec, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Lit(u32),
    Run(u32),
    Copy { dist: u32, len: u32 },
}

fn token_bits(t: &Token) -> u64 {
    match *t {
        Token::Lit(n) | Token::Run(n) => 2 + gamma_bits(n as u64),
        Token::Copy { dist, len } => 2 + gamma_bits(dist as u64) + gamma_bits(len as u64),
    }
}

const MAX_HASH_BITS: u32 = 15;
const CHAIN_DEPTH: usize = 32;
/// Assumed literal cost when deciding whether a run/copy pays off.
const LIT_BITS_ESTIMATE: u64 = 24;

#[inline]
fn hash2(a: u32, b: u32, bits: u32) -> usize {
    ((a as u64 | (b as u64) << 32).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
}

/// Greedy LZ-style parse. Returns tokens and the literal samples.
fn parse(s: &[u32]) -> (Vec<Token>, Vec<u32>) {
    let n = s.len();
    // table sized to the block (avoids clearing 32K entries for small blocks)
    let hb = (usize::BITS - n.max(64).leading_zeros()).min(MAX_HASH_BITS);
    let mut head = vec![usize::MAX; 1 << hb];
    let mut prev = vec![usize::MAX; n];
    let insert = |i: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if i + 1 < n {
            let h = hash2(s[i], s[i + 1], hb);
            prev[i] = head[h];
            head[h] = i;
        }
    };
    let mut tokens = Vec::new();
    let mut lits = Vec::new();
    let mut pending = 0u32;
    let mut i = 0;
    while i < n {
        let mut best: Option<(i64, Token)> = None;
        if i > 0 {
            let run = s[i..].iter().take_while(|&&v| v == s[i - 1]).count() as u32;
            if run >= 2 {
                let t = Token::Run(run);
                best = Some((run as i64 * LIT_BITS_ESTIMATE as i64 - token_bits(&t) as i64, t));
            }
        }
        if i + 1 < n {
            let mut p = head[hash2(s[i], s[i + 1], hb)];
            let mut depth = 0;
            while p != usize::MAX && depth < CHAIN_DEPTH {
                let len = s[i..].iter().zip(&s[p..]).take_while(|(a, b)| a == b).count() as u32;
                if len >= 3 {
                    let t = Token::Copy { dist: (i - p) as u32, len };
                    let gain = len as i64 * LIT_BITS_ESTIMATE as i64 - token_bits(&t) as i64;
                    if best.is_none_or(|b| gain > b.0) {
                        best = Some((gain, t));
                    }
                }
                p = prev[p];
                depth += 1;
            }
        }
        match best {
            // a new token also ends the pending literal token, which costs ~8 bits
            Some((gain, t)) if gain > 8 => {
                if pending > 0 {
                    tokens.push(Token::Lit(pending));
                    pending = 0;
                }
                let len = match t {
                    Token::Run(l) | Token::Copy { len: l, .. } => l as usize,
                    Token::Lit(_) => unreachable!(),
                };
                tokens.push(t);
                for k in i..i + len {
                    insert(k, &mut head, &mut prev);
                }
                i += len;
            }
            _ => {
                lits.push(s[i]);
                pending += 1;
                insert(i, &mut head, &mut prev);
                i += 1;
            }
        }
    }
    if pending > 0 {
        tokens.push(Token::Lit(pending));
    }
    (tokens, lits)
}

/// Best inner coder for the literal stream: `(mode, exact bits)`.
fn best_inner(lits: &[u32], level: u8) -> (Mode, u32, u64) {
    let mut best = (Mode::Raw, 0u32, 32 * lits.len() as u64);
    let fv = sc::fieldvlc_cost(lits);
    if fv < best.2 {
        best = (Mode::FieldVlc, 0, fv);
    }
    let windows: &[u32] = if level >= 3 { &[0, 4, 7] } else { &[0] };
    let (log2n, xc) = xor_codec::best_window(lits, windows);
    if xc < best.2 {
        best = (Mode::XorChimp, log2n, xc);
    }
    let mut w = BitWriter::new();
    sc::encode_xorlz(&mut w, lits);
    if w.bit_len() < best.2 {
        best = (Mode::XorLz, 0, w.bit_len());
    }
    best
}

/// Encode a channel with RLE if the parse finds any repetition; returns the
/// complete mode payload (without the 3-bit mode id) or `None`.
pub fn encode(s: &[u32], level: u8) -> Option<BitWriter> {
    let (tokens, lits) = parse(s);
    if !tokens.iter().any(|t| !matches!(t, Token::Lit(_))) {
        return None; // no repetition found: RLE cannot beat the inner coder alone
    }
    let (inner, log2n, _) = best_inner(&lits, level);
    let mut w = BitWriter::new();
    w.write_bits(inner as u64, 3);
    for t in &tokens {
        match *t {
            Token::Lit(n) => {
                w.write_bits(0b00, 2);
                write_gamma(&mut w, n as u64);
            }
            Token::Run(n) => {
                w.write_bits(0b01, 2);
                write_gamma(&mut w, n as u64);
            }
            Token::Copy { dist, len } => {
                w.write_bits(0b10, 2);
                write_gamma(&mut w, dist as u64);
                write_gamma(&mut w, len as u64);
            }
        }
    }
    match inner {
        Mode::Raw => sc::encode_raw(&mut w, &lits),
        Mode::FieldVlc => sc::encode_fieldvlc(&mut w, &lits),
        Mode::XorLz => sc::encode_xorlz(&mut w, &lits),
        Mode::XorChimp => xor_codec::encode(&mut w, &lits, log2n),
        _ => unreachable!(),
    }
    Some(w)
}

pub fn decode(r: &mut BitReader, out: &mut [u32]) -> Result<()> {
    let inner = Mode::from_id(r.read_bits(3)?)?;
    let n = out.len();
    let mut tokens = Vec::new();
    let mut covered = 0usize;
    let mut n_lits = 0usize;
    while covered < n {
        let kind = r.read_bits(2)?;
        let rem = (n - covered) as u64;
        let t = match kind {
            0b00 | 0b01 => {
                let len = read_gamma(r)?;
                if len > rem {
                    return Err(Error::Corrupt("rle token exceeds block"));
                }
                if kind == 0b00 {
                    n_lits += len as usize;
                    Token::Lit(len as u32)
                } else {
                    if covered == 0 {
                        return Err(Error::Corrupt("rle run at block start"));
                    }
                    Token::Run(len as u32)
                }
            }
            0b10 => {
                let dist = read_gamma(r)?;
                let len = read_gamma(r)?;
                if dist > covered as u64 || len > rem {
                    return Err(Error::Corrupt("rle copy out of range"));
                }
                Token::Copy { dist: dist as u32, len: len as u32 }
            }
            _ => return Err(Error::Corrupt("invalid rle token")),
        };
        covered += match t {
            Token::Lit(l) | Token::Run(l) | Token::Copy { len: l, .. } => l as usize,
        };
        tokens.push(t);
    }
    let mut lits = vec![0u32; n_lits];
    match inner {
        Mode::Raw => sc::decode_raw(r, &mut lits)?,
        Mode::FieldVlc => sc::decode_fieldvlc(r, &mut lits)?,
        Mode::XorLz => sc::decode_xorlz(r, &mut lits)?,
        Mode::XorChimp => xor_codec::decode(r, &mut lits)?,
        _ => return Err(Error::Corrupt("invalid rle literal coder")),
    }
    let (mut i, mut li) = (0usize, 0usize);
    for t in tokens {
        match t {
            Token::Lit(l) => {
                out[i..i + l as usize].copy_from_slice(&lits[li..li + l as usize]);
                li += l as usize;
                i += l as usize;
            }
            Token::Run(l) => {
                let v = out[i - 1];
                out[i..i + l as usize].fill(v);
                i += l as usize;
            }
            Token::Copy { dist, len } => {
                for k in 0..len as usize {
                    out[i + k] = out[i + k - dist as usize];
                }
                i += len as usize;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &[u32]) -> Option<u64> {
        let w = encode(s, 5)?;
        let bits = w.bit_len();
        let b = w.into_bytes();
        let mut out = vec![0; s.len()];
        decode(&mut BitReader::new(&b), &mut out).unwrap();
        assert_eq!(out, s);
        Some(bits)
    }

    #[test]
    fn signed_zero_runs_stay_distinct() {
        let mut s = vec![0u32; 500];
        s.extend(vec![0x8000_0000; 500]);
        s.extend([0, 0x8000_0000, 0, 0x8000_0000]);
        s.extend(vec![0x7FC0_1234; 100]);
        s.extend(vec![0x7FC0_1235; 100]);
        let bits = rt(&s).unwrap();
        assert!(bits < 400, "{bits}");
    }

    #[test]
    fn periodic_sequences_use_copies() {
        let cycle: Vec<u32> = (0..96).map(|i| ((i as f32 * 0.37).sin()).to_bits()).collect();
        let s: Vec<u32> = cycle.iter().cycle().take(4096).cloned().collect();
        let bits = rt(&s).unwrap();
        assert!(bits < 96 * 33 + 200, "{bits}");
    }

    #[test]
    fn no_repetition_returns_none() {
        let s: Vec<u32> = (0..1000u32).map(|i| i.wrapping_mul(0x9E37_79B9)).collect();
        assert!(rt(&s).is_none());
    }

    #[test]
    fn mixed_random() {
        let mut x = 1u32;
        let mut s = Vec::new();
        for i in 0..5000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            match x % 5 {
                0 => s.extend(vec![x; (x % 40) as usize]),
                1 if s.len() > 50 => {
                    let st = s.len() - 50;
                    let seg: Vec<u32> = s[st..st + (x % 50) as usize].to_vec();
                    s.extend(seg);
                }
                _ => s.push(x ^ i),
            }
        }
        rt(&s);
    }
}
