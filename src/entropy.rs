//! Alternative entropy coders for prediction residuals (format 1.1).
//!
//! A residual section of a coarse stream starts with the 4-bit partition
//! order of partitioned Rice coding (§ 7.1 of SPEC.md). Orders 0–8 are Rice;
//! the otherwise unused value **15** escapes to one of the coders here:
//!
//! ```text
//! 1111 coder:2      1 Huffman · 2 rANS · 3 adaptive binary range coder
//! ```
//!
//! All three code the same symbol alphabet. A zigzagged residual `u` maps
//! to a symbol plus raw low bits:
//!
//! * `u < 16`: symbol `u`, no raw bits
//! * otherwise with `L = bitlen(u)` (5 … 64): symbol `16 + 2(L−5) + b` where
//!   `b` is the bit below the leading one, followed by the `L−2` low bits raw.
//!
//! That gives 136 symbols. The symbols are entropy coded; the raw bits are
//! stored verbatim (they are close to uniform).
//!
//! * **Huffman**: `lo:8 hi:8 len[lo..=hi]:4`, then per value the code and its raw bits.
//! * **rANS** (static, 12-bit frequencies, 32-bit state, byte renormalisation):
//!   `lo:8 hi:8 γ(freq+1)[lo..=hi]`, `γ(nbytes+1)`, bytes, then all raw bits in order.
//! * **Range** (adaptive, LZMA-style binary coder, 8-bit symbol tree, context =
//!   bucket of the previous symbol): `γ(nbytes+1)`, bytes, then all raw bits.
//!
//! The encoder evaluates the exact size of every allowed coder (including
//! tables and flush bytes) and keeps the smallest, Rice included.

use crate::bitio::{mask, BitReader, BitWriter};
use crate::variable_integer::{bit_len, read_gamma, read_partitioned_body, write_gamma};
use crate::{Error, Result};

pub const NSYM: usize = 136;
pub const ESCAPE_ORDER: u64 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Coder {
    Rice = 0,
    Huffman = 1,
    Rans = 2,
    Range = 3,
}

impl Coder {
    pub const ALL: [Coder; 4] = [Coder::Rice, Coder::Huffman, Coder::Rans, Coder::Range];
    pub fn name(self) -> &'static str {
        match self {
            Coder::Rice => "rice",
            Coder::Huffman => "huffman",
            Coder::Rans => "rans",
            Coder::Range => "range",
        }
    }
    pub fn parse(s: &str) -> Option<Coder> {
        Coder::ALL.into_iter().find(|c| c.name() == s)
    }
}

// ------------------------------------------------------------ symbols

#[inline]
pub fn to_symbol(u: u64) -> (usize, u32, u64) {
    if u < 16 {
        (u as usize, 0, 0)
    } else {
        let l = bit_len(u);
        let b = (u >> (l - 2)) & 1;
        (16 + 2 * (l as usize - 5) + b as usize, l - 2, u & mask(l - 2))
    }
}

#[inline]
pub fn from_symbol(sym: usize, raw: u64) -> u64 {
    if sym < 16 {
        sym as u64
    } else {
        let k = sym - 16;
        let l = (k / 2 + 5) as u32;
        (1u64 << (l - 1)) | (((k & 1) as u64) << (l - 2)) | raw
    }
}

#[inline]
pub fn raw_bits_of(sym: usize) -> u32 {
    if sym < 16 {
        0
    } else {
        ((sym - 16) / 2 + 5) as u32 - 2
    }
}

fn histogram(vals: &[u64]) -> ([u32; NSYM], u64) {
    let mut h = [0u32; NSYM];
    let mut raw = 0u64;
    for &u in vals {
        let (s, n, _) = to_symbol(u);
        h[s] += 1;
        raw += n as u64;
    }
    (h, raw)
}

fn used_range(h: &[u32; NSYM]) -> Option<(usize, usize)> {
    let lo = h.iter().position(|&c| c > 0)?;
    let hi = NSYM - 1 - h.iter().rev().position(|&c| c > 0)?;
    Some((lo, hi))
}

// ------------------------------------------------------------ Huffman

const MAX_CODE_LEN: u32 = 15;

/// Code lengths (0 = unused), limited to 15 bits by repeatedly flattening
/// the histogram. Deterministic tie-breaking by symbol index.
fn huffman_lengths(h: &[u32; NSYM]) -> [u8; NSYM] {
    let mut freq: Vec<u64> = h.iter().map(|&c| c as u64).collect();
    loop {
        let mut lens = [0u8; NSYM];
        let used: Vec<usize> = (0..NSYM).filter(|&s| freq[s] > 0).collect();
        if used.len() == 1 {
            lens[used[0]] = 1;
            return lens;
        }
        // nodes: (weight, id); leaves 0..NSYM, internal from NSYM
        let mut parent = vec![usize::MAX; 2 * NSYM];
        let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, usize)>> =
            used.iter().map(|&s| std::cmp::Reverse((freq[s], s))).collect();
        let mut next = NSYM;
        while heap.len() > 1 {
            let std::cmp::Reverse((wa, a)) = heap.pop().unwrap();
            let std::cmp::Reverse((wb, b)) = heap.pop().unwrap();
            parent[a] = next;
            parent[b] = next;
            heap.push(std::cmp::Reverse((wa + wb, next)));
            next += 1;
        }
        let mut max = 0;
        for &s in &used {
            let mut d = 0u32;
            let mut n = s;
            while parent[n] != usize::MAX {
                n = parent[n];
                d += 1;
            }
            lens[s] = d as u8;
            max = max.max(d);
        }
        if max <= MAX_CODE_LEN {
            return lens;
        }
        for f in freq.iter_mut().filter(|f| **f > 0) {
            *f = (*f).div_ceil(2);
        }
    }
}

/// Canonical codes from lengths: `codes[s]` valid where `lens[s] > 0`.
fn canonical_codes(lens: &[u8; NSYM]) -> [u32; NSYM] {
    let mut codes = [0u32; NSYM];
    let mut code = 0u32;
    for l in 1..=MAX_CODE_LEN as u8 {
        for s in 0..NSYM {
            if lens[s] == l {
                codes[s] = code;
                code += 1;
            }
        }
        code <<= 1;
    }
    codes
}

pub fn encode_huffman(vals: &[u64]) -> BitWriter {
    let (h, _) = histogram(vals);
    let mut w = BitWriter::new();
    let Some((lo, hi)) = used_range(&h) else {
        w.write_bits(1, 8); // lo > hi: empty
        w.write_bits(0, 8);
        return w;
    };
    let lens = huffman_lengths(&h);
    let codes = canonical_codes(&lens);
    w.write_bits(lo as u64, 8);
    w.write_bits(hi as u64, 8);
    for &l in &lens[lo..=hi] {
        w.write_bits(l as u64, 4);
    }
    for &u in vals {
        let (s, n, raw) = to_symbol(u);
        w.write_bits(codes[s] as u64, lens[s] as u32);
        w.write_bits(raw, n);
    }
    w
}

/// Exact size of [`encode_huffman`] without encoding.
pub fn huffman_cost(vals: &[u64]) -> u64 {
    let (h, raw) = histogram(vals);
    let Some((lo, hi)) = used_range(&h) else { return 16 };
    let lens = huffman_lengths(&h);
    16 + 4 * (hi - lo + 1) as u64 + raw + (0..NSYM).map(|s| h[s] as u64 * lens[s] as u64).sum::<u64>()
}

fn decode_huffman(r: &mut BitReader, out: &mut [u64]) -> Result<()> {
    let lo = r.read_bits(8)? as usize;
    let hi = r.read_bits(8)? as usize;
    if out.is_empty() && lo > hi {
        return Ok(());
    }
    if lo > hi || hi >= NSYM {
        return Err(Error::Corrupt("huffman symbol range"));
    }
    let mut lens = [0u8; NSYM];
    for l in lens[lo..=hi].iter_mut() {
        *l = r.read_bits(4)? as u8;
    }
    // canonical decoding tables
    let mut count = [0u32; 16];
    for &l in &lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut sorted = Vec::with_capacity(NSYM);
    for l in 1..=15u8 {
        sorted.extend((0..NSYM).filter(|&s| lens[s] == l));
    }
    if sorted.is_empty() {
        return Err(Error::Corrupt("empty huffman table"));
    }
    // first code and first sorted index per length; Kraft check
    let mut first = [0u32; 16];
    let mut offset = [0usize; 16];
    let (mut code, mut idx) = (0u32, 0usize);
    for l in 1..=15 {
        first[l] = code;
        offset[l] = idx;
        code += count[l];
        idx += count[l] as usize;
        if code > (1 << l) {
            return Err(Error::Corrupt("huffman lengths violate Kraft inequality"));
        }
        code <<= 1;
    }
    for v in out.iter_mut() {
        let mut c = 0u32;
        let mut sym = None;
        for l in 1..=15 {
            c = (c << 1) | r.read_bits(1)? as u32;
            if c.wrapping_sub(first[l]) < count[l] {
                sym = Some(sorted[offset[l] + (c - first[l]) as usize]);
                break;
            }
        }
        let s = sym.ok_or(Error::Corrupt("invalid huffman code"))?;
        let raw = r.read_bits(raw_bits_of(s))?;
        *v = from_symbol(s, raw);
    }
    Ok(())
}

// ------------------------------------------------------------ rANS

const M_BITS: u32 = 12;
const M: u32 = 1 << M_BITS;
const RANS_L: u32 = 1 << 23;

/// Normalise a histogram to frequencies summing to `M` (each used >= 1).
fn normalize(h: &[u32; NSYM]) -> [u32; NSYM] {
    let n: u64 = h.iter().map(|&c| c as u64).sum();
    let mut f = [0u32; NSYM];
    if n == 0 {
        return f;
    }
    for s in 0..NSYM {
        if h[s] > 0 {
            f[s] = ((h[s] as u64 * M as u64 / n) as u32).max(1);
        }
    }
    let mut sum: i64 = f.iter().map(|&x| x as i64).sum();
    while sum != M as i64 {
        // adjust the largest frequency (deterministic: lowest index on ties)
        let big = (0..NSYM).max_by_key(|&s| (f[s], std::cmp::Reverse(s))).unwrap();
        if sum > M as i64 {
            let step = (sum - M as i64).min(f[big] as i64 - 1);
            if step == 0 {
                // all at 1 except none reducible: reduce any symbol > 1
                let s = (0..NSYM).find(|&s| f[s] > 1).unwrap();
                f[s] -= 1;
                sum -= 1;
                continue;
            }
            f[big] -= step as u32;
            sum -= step;
        } else {
            f[big] += (M as i64 - sum) as u32;
            sum = M as i64;
        }
    }
    f
}

fn write_freq_table(w: &mut BitWriter, f: &[u32; NSYM]) -> Option<(usize, usize)> {
    let r = used_range_f(f);
    match r {
        None => {
            w.write_bits(1, 8);
            w.write_bits(0, 8);
        }
        Some((lo, hi)) => {
            w.write_bits(lo as u64, 8);
            w.write_bits(hi as u64, 8);
            for &x in &f[lo..=hi] {
                write_gamma(w, x as u64 + 1);
            }
        }
    }
    r
}

fn used_range_f(f: &[u32; NSYM]) -> Option<(usize, usize)> {
    let lo = f.iter().position(|&c| c > 0)?;
    let hi = NSYM - 1 - f.iter().rev().position(|&c| c > 0)?;
    Some((lo, hi))
}

fn read_freq_table(r: &mut BitReader, n: usize) -> Result<[u32; NSYM]> {
    let lo = r.read_bits(8)? as usize;
    let hi = r.read_bits(8)? as usize;
    let mut f = [0u32; NSYM];
    if lo > hi {
        return if n == 0 { Ok(f) } else { Err(Error::Corrupt("empty frequency table")) };
    }
    if hi >= NSYM {
        return Err(Error::Corrupt("frequency table range"));
    }
    let mut sum = 0u64;
    for x in f[lo..=hi].iter_mut() {
        let v = read_gamma(r)? - 1;
        if v > M as u64 {
            return Err(Error::Corrupt("frequency too large"));
        }
        *x = v as u32;
        sum += v;
    }
    if sum != M as u64 {
        return Err(Error::Corrupt("frequencies do not sum to M"));
    }
    Ok(f)
}

fn write_bytes(w: &mut BitWriter, bytes: &[u8]) {
    write_gamma(w, bytes.len() as u64 + 1);
    for &b in bytes {
        w.write_bits(b as u64, 8);
    }
}

fn read_bytes(r: &mut BitReader, limit: usize) -> Result<Vec<u8>> {
    let n = read_gamma(r)? - 1;
    if n as usize > limit || n as usize * 8 > r.remaining() {
        return Err(Error::Corrupt("entropy-coded byte count"));
    }
    (0..n).map(|_| Ok(r.read_bits(8)? as u8)).collect()
}

fn write_raw_bits(w: &mut BitWriter, vals: &[u64]) {
    for &u in vals {
        let (_, n, raw) = to_symbol(u);
        w.write_bits(raw, n);
    }
}

pub fn encode_rans(vals: &[u64]) -> BitWriter {
    let (h, _) = histogram(vals);
    let f = normalize(&h);
    let mut cum = [0u32; NSYM + 1];
    for s in 0..NSYM {
        cum[s + 1] = cum[s] + f[s];
    }
    let mut w = BitWriter::new();
    write_freq_table(&mut w, &f);
    let mut bytes = Vec::with_capacity(vals.len() * 2);
    let mut x: u32 = RANS_L;
    for &u in vals.iter().rev() {
        let s = to_symbol(u).0;
        let fs = f[s];
        let x_max = ((RANS_L >> M_BITS) << 8) * fs;
        while x >= x_max {
            bytes.push(x as u8);
            x >>= 8;
        }
        x = ((x / fs) << M_BITS) + (x % fs) + cum[s];
    }
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.reverse();
    write_bytes(&mut w, &bytes);
    write_raw_bits(&mut w, vals);
    w
}

fn decode_rans(r: &mut BitReader, out: &mut [u64]) -> Result<()> {
    let f = read_freq_table(r, out.len())?;
    let bytes = read_bytes(r, out.len() * 4 + 8)?;
    if out.is_empty() {
        return Ok(());
    }
    let mut cum = [0u32; NSYM + 1];
    for s in 0..NSYM {
        cum[s + 1] = cum[s] + f[s];
    }
    let mut lut = vec![0u8; M as usize];
    for s in 0..NSYM {
        lut[cum[s] as usize..cum[s + 1] as usize].fill(s as u8);
    }
    if bytes.len() < 4 {
        return Err(Error::Corrupt("rANS stream too short"));
    }
    let mut x = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as u64;
    let mut pos = 4;
    let mut syms = Vec::with_capacity(out.len());
    for _ in 0..out.len() {
        let slot = (x & (M as u64 - 1)) as u32;
        let s = lut[slot as usize] as usize;
        x = f[s] as u64 * (x >> M_BITS) + slot as u64 - cum[s] as u64;
        while x < RANS_L as u64 {
            let b = *bytes.get(pos).ok_or(Error::Corrupt("rANS stream exhausted"))?;
            x = (x << 8) | b as u64;
            pos += 1;
        }
        if x >= 1 << 31 {
            return Err(Error::Corrupt("rANS state out of range"));
        }
        syms.push(s);
    }
    for (v, s) in out.iter_mut().zip(syms) {
        *v = from_symbol(s, r.read_bits(raw_bits_of(s))?);
    }
    Ok(())
}

// ------------------------------------------------------------ adaptive range coder

const PROB_BITS: u32 = 11;
const PROB_INIT: u16 = 1 << (PROB_BITS - 1);
const MOVE_BITS: u32 = 4;
const TOP: u32 = 1 << 24;
const CONTEXTS: usize = 18;

#[inline]
fn context_of(prev_sym: usize) -> usize {
    (prev_sym / 8).min(CONTEXTS - 1)
}

struct RcEncoder {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl RcEncoder {
    fn new() -> Self {
        RcEncoder { low: 0, range: u32::MAX, cache: 0, cache_size: 1, out: Vec::new() }
    }
    fn shift_low(&mut self) {
        if self.low < 0xFF00_0000 || self.low >= 1 << 32 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }
    #[inline]
    fn bit(&mut self, p: &mut u16, bit: u32) {
        let bound = (self.range >> PROB_BITS) * *p as u32;
        if bit == 0 {
            self.range = bound;
            *p += ((1 << PROB_BITS) - *p) >> MOVE_BITS;
        } else {
            self.low += bound as u64;
            self.range -= bound;
            *p -= *p >> MOVE_BITS;
        }
        while self.range < TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }
    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

struct RcDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
}

impl<'a> RcDecoder<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < 5 || data[0] != 0 {
            return Err(Error::Corrupt("range coder stream header"));
        }
        let code = u32::from_be_bytes(data[1..5].try_into().unwrap());
        Ok(RcDecoder { data, pos: 5, range: u32::MAX, code })
    }
    #[inline]
    fn bit(&mut self, p: &mut u16) -> Result<u32> {
        let bound = (self.range >> PROB_BITS) * *p as u32;
        let bit = if self.code < bound {
            self.range = bound;
            *p += ((1 << PROB_BITS) - *p) >> MOVE_BITS;
            0
        } else {
            self.code -= bound;
            self.range -= bound;
            *p -= *p >> MOVE_BITS;
            1
        };
        while self.range < TOP {
            let b = *self.data.get(self.pos).ok_or(Error::Corrupt("range coder stream exhausted"))?;
            self.pos += 1;
            self.range <<= 8;
            self.code = (self.code << 8) | b as u32;
        }
        Ok(bit)
    }
}

pub fn encode_range(vals: &[u64]) -> BitWriter {
    let mut probs = vec![PROB_INIT; CONTEXTS * 256];
    let mut rc = RcEncoder::new();
    let mut prev = 0usize;
    for &u in vals {
        let s = to_symbol(u).0;
        let base = context_of(prev) * 256;
        let mut node = 1usize;
        for i in (0..8).rev() {
            let b = ((s >> i) & 1) as u32;
            rc.bit(&mut probs[base + node], b);
            node = (node << 1) | b as usize;
        }
        prev = s;
    }
    let bytes = rc.finish();
    let mut w = BitWriter::new();
    write_bytes(&mut w, &bytes);
    write_raw_bits(&mut w, vals);
    w
}

fn decode_range(r: &mut BitReader, out: &mut [u64]) -> Result<()> {
    let bytes = read_bytes(r, out.len() * 4 + 16)?;
    if out.is_empty() {
        return Ok(());
    }
    let mut rc = RcDecoder::new(&bytes)?;
    let mut probs = vec![PROB_INIT; CONTEXTS * 256];
    let mut prev = 0usize;
    let mut syms = Vec::with_capacity(out.len());
    for _ in 0..out.len() {
        let base = context_of(prev) * 256;
        let mut node = 1usize;
        for _ in 0..8 {
            node = (node << 1) | rc.bit(&mut probs[base + node])? as usize;
        }
        let s = node - 256;
        if s >= NSYM {
            return Err(Error::Corrupt("range coder symbol out of range"));
        }
        syms.push(s);
        prev = s;
    }
    for (v, s) in out.iter_mut().zip(syms) {
        *v = from_symbol(s, r.read_bits(raw_bits_of(s))?);
    }
    Ok(())
}

// ------------------------------------------------------------ selection / dispatch

/// Best coder among `allowed` for `vals`, with the exact residual-section
/// size in bits and (for non-Rice coders) the pre-encoded section.
pub fn choose(vals: &[u64], allowed: &[Coder], max_partition_order: u32) -> (Coder, u64, Option<BitWriter>) {
    let rice = crate::variable_integer::partitioned_cost(vals, max_partition_order);
    let mut best: (Coder, u64, Option<BitWriter>) = (Coder::Rice, rice, None);
    if vals.len() < 32 {
        return best;
    }
    let esc = 4 + 2; // escape order + coder id
    for &c in allowed {
        let enc = match c {
            Coder::Rice => continue,
            Coder::Huffman => {
                // exact cost without encoding first
                if huffman_cost(vals) + esc >= best.1 {
                    continue;
                }
                encode_huffman(vals)
            }
            Coder::Rans => {
                // lower bound: order-0 entropy of the symbols + raw bits
                let (h, raw) = histogram(vals);
                let n = vals.len() as f64;
                let ent: f64 = h.iter().filter(|&&c| c > 0).map(|&c| -(c as f64) * (c as f64 / n).log2()).sum();
                if (ent + raw as f64) as u64 + esc + 16 >= best.1 {
                    continue;
                }
                encode_rans(vals)
            }
            Coder::Range => encode_range(vals),
        };
        let bits = enc.bit_len() + esc;
        if bits < best.1 {
            best = (c, bits, Some(enc));
        }
    }
    best
}

/// Write the escape header for a non-Rice coder (the payload follows).
pub fn write_escape(w: &mut BitWriter, c: Coder) {
    w.write_bits(ESCAPE_ORDER, 4);
    w.write_bits(c as u64, 2);
}

/// Read a residual section of `out.len()` values (Rice or any coder).
pub fn read_residuals(r: &mut BitReader, out: &mut [u64]) -> Result<()> {
    let order = r.read_bits(4)?;
    if order != ESCAPE_ORDER {
        return read_partitioned_body(r, order as u32, out);
    }
    match r.read_bits(2)? {
        1 => decode_huffman(r, out),
        2 => decode_rans(r, out),
        3 => decode_range(r, out),
        _ => Err(Error::Corrupt("invalid residual coder")),
    }
}

/// Exact cost helper for reports.
pub fn coder_costs(vals: &[u64]) -> [(Coder, u64); 4] {
    [
        (Coder::Rice, crate::variable_integer::partitioned_cost(vals, 8)),
        (Coder::Huffman, huffman_cost(vals) + 6),
        (Coder::Rans, encode_rans(vals).bit_len() + 6),
        (Coder::Range, encode_range(vals).bit_len() + 6),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals() -> Vec<u64> {
        let mut x = 7u64;
        (0..5000)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let mag = (x >> 40) % (1 << (i % 30));
                crate::variable_integer::zigzag(if x & 1 == 0 { mag as i64 } else { -(mag as i64) })
            })
            .chain([0, 1, 15, 16, 17, u64::MAX, 1 << 63, (1 << 63) - 1])
            .collect()
    }

    #[test]
    fn symbol_mapping_is_bijective() {
        for u in (0..5000u64).chain([u64::MAX, 1 << 63, (1 << 63) + 12345, (1 << 40) - 1]) {
            let (s, n, raw) = to_symbol(u);
            assert!(s < NSYM);
            assert_eq!(n, raw_bits_of(s));
            assert_eq!(from_symbol(s, raw), u);
        }
    }

    #[test]
    fn all_coders_roundtrip() {
        let v = vals();
        for c in [Coder::Huffman, Coder::Rans, Coder::Range] {
            let enc = match c {
                Coder::Huffman => encode_huffman(&v),
                Coder::Rans => encode_rans(&v),
                _ => encode_range(&v),
            };
            let mut w = BitWriter::new();
            write_escape(&mut w, c);
            w.append(&enc);
            let b = w.into_bytes();
            let mut out = vec![0; v.len()];
            read_residuals(&mut BitReader::new(&b), &mut out).unwrap();
            assert_eq!(out, v, "{c:?}");
        }
        assert_eq!(huffman_cost(&v), encode_huffman(&v).bit_len());
    }

    #[test]
    fn choose_and_dispatch() {
        for v in [vals(), vec![3; 100], vec![0; 64], (0..200).map(|i| i % 7).collect()] {
            for allowed in [&Coder::ALL[..], &[Coder::Range], &[Coder::Rans], &[Coder::Huffman]] {
                let (c, bits, enc) = choose(&v, allowed, 8);
                let mut w = BitWriter::new();
                match enc {
                    None => crate::variable_integer::write_partitioned(&mut w, &v, 8),
                    Some(e) => {
                        write_escape(&mut w, c);
                        w.append(&e);
                    }
                }
                assert_eq!(w.bit_len(), bits);
                let b = w.into_bytes();
                let mut out = vec![0; v.len()];
                read_residuals(&mut BitReader::new(&b), &mut out).unwrap();
                assert_eq!(out, v);
            }
        }
    }

    #[test]
    fn single_symbol_streams() {
        let v = vec![5u64; 40];
        let b = encode_huffman(&v).into_bytes();
        let mut out = vec![0; 40];
        decode_huffman(&mut BitReader::new(&b), &mut out).unwrap();
        assert_eq!(out, v);
        let b = encode_rans(&v).into_bytes();
        decode_rans(&mut BitReader::new(&b), &mut out).unwrap();
        assert_eq!(out, v);
    }
}
