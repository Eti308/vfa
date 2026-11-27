//! Per-channel sub-frame coding modes.
//!
//! | id | mode        | idea                                                         |
//! |----|-------------|--------------------------------------------------------------|
//! | 0  | Constant    | every sample has the same bit pattern (e.g. digital silence) |
//! | 1  | Raw         | 32 bits per sample — guaranteed fallback                     |
//! | 2  | XorLz       | XOR with previous pattern, leading/trailing-zero window      |
//! | 3  | FieldVlc    | sign + Rice(Δexponent) + Rice(trailing zeros) + significant mantissa bits |
//! | 4  | FloatSplit  | exact grid split: predicted coarse integers + exponent-dependent extra bits |
//!
//! Modes 2 and 3 are direct implementations of the "sample-wise variable
//! bit length" idea (leading/trailing-zero coding, exponent coding). Mode 4
//! adds prediction across samples; it is the workhorse for real audio.
//! All modes are exact on arbitrary bit patterns (FloatSplit stores
//! non-finite values as exceptions).

use crate::bitio::{BitReader, BitWriter};
use crate::ieee754::{
    self, extra_bits_for, join_from_grid, small_exp_bound, split_at_grid, Split, MAX_COARSE_BITS, MIN_LSB,
};
use crate::variable_integer::{
    best_rice_param, gamma_bits, read_gamma, read_rice, rice_cost, unzigzag, write_gamma, write_rice, zigzag,
};
use crate::{Error, Result};

pub const MODE_BITS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Mode {
    Constant = 0,
    Raw = 1,
    XorLz = 2,
    FieldVlc = 3,
    FloatSplit = 4,
    /// Multi-reference XOR coding, see [`crate::xor_codec`] (format 1.1).
    XorChimp = 5,
    /// Runs / repeated sequences, see [`crate::rle_codec`] (format 1.1).
    Rle = 6,
    // ids >= 8 are internal: on the wire they are written as the extended
    // mode 7 followed by a 4-bit extension id (`id - 8`).
    /// FloatSplit with integer inter-channel prediction, see [`crate::multichannel`].
    FloatSplitRef = 8,
    /// XOR with an earlier channel's bit patterns, see [`crate::multichannel`].
    XorRef = 9,
}

/// Wire id announcing an extended mode (followed by a 4-bit extension id).
pub const MODE_EXTENDED: u64 = 7;

impl Mode {
    pub const ALL: [Mode; 9] = [
        Mode::Constant,
        Mode::Raw,
        Mode::XorLz,
        Mode::FieldVlc,
        Mode::FloatSplit,
        Mode::XorChimp,
        Mode::Rle,
        Mode::FloatSplitRef,
        Mode::XorRef,
    ];

    /// Size of the mode field on the wire.
    pub fn wire_bits(self) -> u64 {
        if self as u8 >= 8 {
            MODE_BITS as u64 + 4
        } else {
            MODE_BITS as u64
        }
    }

    pub fn write_id(self, w: &mut BitWriter) {
        if self as u8 >= 8 {
            w.write_bits(MODE_EXTENDED, MODE_BITS);
            w.write_bits(self as u64 - 8, 4);
        } else {
            w.write_bits(self as u64, MODE_BITS);
        }
    }

    pub fn read_id(r: &mut BitReader) -> Result<Mode> {
        let id = r.read_bits(MODE_BITS)?;
        if id != MODE_EXTENDED {
            return Mode::from_id(id);
        }
        match r.read_bits(4)? {
            0 => Ok(Mode::FloatSplitRef),
            1 => Ok(Mode::XorRef),
            _ => Err(Error::Corrupt("invalid extended sub-frame mode")),
        }
    }

    pub fn from_id(id: u64) -> Result<Mode> {
        Ok(match id {
            0 => Mode::Constant,
            1 => Mode::Raw,
            2 => Mode::XorLz,
            3 => Mode::FieldVlc,
            4 => Mode::FloatSplit,
            5 => Mode::XorChimp,
            6 => Mode::Rle,
            _ => return Err(Error::Corrupt("invalid sub-frame mode")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Constant => "constant",
            Mode::Raw => "raw",
            Mode::XorLz => "xorlz",
            Mode::FieldVlc => "fieldvlc",
            Mode::FloatSplit => "floatsplit",
            Mode::XorChimp => "xorchimp",
            Mode::Rle => "rle",
            Mode::FloatSplitRef => "floatsplitref",
            Mode::XorRef => "xorref",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == s)
    }
}

// ================================================================ Constant

pub fn encode_constant(w: &mut BitWriter, value: u32) {
    w.write_bits(value as u64, 32);
}

pub fn decode_constant(r: &mut BitReader, out: &mut [u32]) -> Result<()> {
    let v = r.read_bits(32)? as u32;
    out.fill(v);
    Ok(())
}

// ================================================================ Raw

pub fn encode_raw(w: &mut BitWriter, s: &[u32]) {
    for &v in s {
        w.write_bits(v as u64, 32);
    }
}

pub fn decode_raw(r: &mut BitReader, out: &mut [u32]) -> Result<()> {
    if r.remaining() < out.len() * 32 {
        return Err(Error::UnexpectedEof);
    }
    for v in out.iter_mut() {
        *v = r.read_bits(32)? as u32;
    }
    Ok(())
}

// ================================================================ XorLz

/// Gorilla-style XOR coding. Per sample after the first:
/// `0` = identical to previous;
/// `10` + bits = XOR fits the previous leading/trailing-zero window;
/// `11` + lz:5 + (len-1):5 + len bits = new window.
pub fn encode_xorlz(w: &mut BitWriter, s: &[u32]) {
    let Some(&first) = s.first() else { return };
    w.write_bits(first as u64, 32);
    let mut prev = first;
    let mut window: Option<(u32, u32)> = None;
    for &v in &s[1..] {
        let x = v ^ prev;
        prev = v;
        if x == 0 {
            w.write_bit(false);
            continue;
        }
        let lz = x.leading_zeros();
        let tz = x.trailing_zeros();
        match window {
            Some((wlz, wtz)) if lz >= wlz && tz >= wtz => {
                w.write_bits(0b10, 2);
                w.write_bits((x >> wtz) as u64, 32 - wlz - wtz);
            }
            _ => {
                let len = 32 - lz - tz;
                w.write_bits(0b11, 2);
                w.write_bits(lz as u64, 5);
                w.write_bits(len as u64 - 1, 5);
                w.write_bits((x >> tz) as u64, len);
                window = Some((lz, tz));
            }
        }
    }
}

pub fn decode_xorlz(r: &mut BitReader, out: &mut [u32]) -> Result<()> {
    if out.is_empty() {
        return Ok(());
    }
    let mut prev = r.read_bits(32)? as u32;
    out[0] = prev;
    let mut window: Option<(u32, u32)> = None;
    for v in out[1..].iter_mut() {
        if r.read_bit()? {
            let x = if r.read_bit()? {
                let lz = r.read_bits(5)? as u32;
                let len = r.read_bits(5)? as u32 + 1;
                if lz + len > 32 {
                    return Err(Error::Corrupt("xorlz window exceeds 32 bits"));
                }
                let tz = 32 - lz - len;
                window = Some((lz, tz));
                (r.read_bits(len)? as u32) << tz
            } else {
                let (wlz, wtz) = window.ok_or(Error::Corrupt("xorlz window used before definition"))?;
                (r.read_bits(32 - wlz - wtz)? as u32) << wtz
            };
            prev ^= x;
        }
        *v = prev;
    }
    Ok(())
}

// ================================================================ FieldVlc

const FV_KE_BITS: u32 = 4;
const FV_KT_BITS: u32 = 3;

/// Field-wise variable length coding without inter-sample prediction other
/// than the exponent delta: `k_e:4 k_t:3`, then per sample
/// `sign:1 Rice(zigzag(Δexp), k_e) Rice(tz, k_t) [mantissa bits above the lowest set bit]`
/// where `tz` = trailing zeros of the 23-bit mantissa (23 for a zero mantissa).
pub fn encode_fieldvlc(w: &mut BitWriter, s: &[u32]) {
    let (de, tz) = fieldvlc_symbols(s);
    let ke = best_rice_param(&de).0.min((1 << FV_KE_BITS) - 1);
    let kt = best_rice_param(&tz).0.min((1 << FV_KT_BITS) - 1);
    w.write_bits(ke as u64, FV_KE_BITS);
    w.write_bits(kt as u64, FV_KT_BITS);
    for (i, &v) in s.iter().enumerate() {
        w.write_bit(v >> 31 != 0);
        write_rice(w, de[i], ke);
        let t = tz[i] as u32;
        write_rice(w, t as u64, kt);
        if t < 23 {
            let m = v & ieee754::MANT_MASK;
            w.write_bits((m >> (t + 1)) as u64, 22 - t);
        }
    }
}

fn fieldvlc_symbols(s: &[u32]) -> (Vec<u64>, Vec<u64>) {
    let mut prev_e = 127i64;
    let mut de = Vec::with_capacity(s.len());
    let mut tz = Vec::with_capacity(s.len());
    for &v in s {
        let e = ((v >> 23) & 0xFF) as i64;
        de.push(zigzag(e - prev_e));
        prev_e = e;
        let m = v & ieee754::MANT_MASK;
        tz.push(if m == 0 { 23 } else { m.trailing_zeros() as u64 });
    }
    (de, tz)
}

/// Exact size of [`encode_fieldvlc`] in bits.
pub fn fieldvlc_cost(s: &[u32]) -> u64 {
    let (de, tz) = fieldvlc_symbols(s);
    let ke = best_rice_param(&de).0.min((1 << FV_KE_BITS) - 1);
    let kt = best_rice_param(&tz).0.min((1 << FV_KT_BITS) - 1);
    let mant: u64 = tz.iter().map(|&t| 22u64.saturating_sub(t) * (t < 23) as u64).sum();
    (FV_KE_BITS + FV_KT_BITS) as u64 + s.len() as u64 + rice_cost(&de, ke) + rice_cost(&tz, kt) + mant
}

pub fn decode_fieldvlc(r: &mut BitReader, out: &mut [u32]) -> Result<()> {
    let ke = r.read_bits(FV_KE_BITS)? as u32;
    let kt = r.read_bits(FV_KT_BITS)? as u32;
    let mut prev_e = 127i64;
    for v in out.iter_mut() {
        let sign = r.read_bits(1)? as u32;
        let e = prev_e.wrapping_add(unzigzag(read_rice(r, ke)?));
        if !(0..=255).contains(&e) {
            return Err(Error::Corrupt("fieldvlc exponent out of range"));
        }
        prev_e = e;
        let t = read_rice(r, kt)?;
        let mant = match t {
            23 => 0,
            t if t < 23 => {
                let t = t as u32;
                (((r.read_bits(22 - t)? as u32) << 1) | 1) << t
            }
            _ => return Err(Error::Corrupt("fieldvlc trailing-zero count > 23")),
        };
        *v = (sign << 31) | ((e as u32) << 23) | mant;
    }
    Ok(())
}

// ================================================================ FloatSplit

/// Grid exponent storage: `g + GRID_OFFSET` in `GRID_BITS` bits.
const GRID_OFFSET: i32 = 149;
const GRID_BITS: u32 = 9;
pub const GRID_MIN: i32 = MIN_LSB;
pub const GRID_MAX: i32 = 127;

/// A channel split at grid exponent `g` (see [`crate::ieee754`]).
#[derive(Clone, Debug)]
pub struct SplitChannel {
    pub g: i32,
    /// Coarse integers; exception positions hold a prediction-friendly
    /// placeholder (the previous coarse value).
    pub coarse: Vec<i64>,
    /// `(index, raw bits)` of samples stored verbatim (Inf/NaN/out of range).
    pub exceptions: Vec<(usize, u32)>,
    /// Precision floor `min(h, g)`: no sample has a set bit of weight
    /// below `2^floor`, so extra bits below it are known zeros and omitted.
    pub floor: i32,
    /// Whether any small (|x| < 2^g or −0.0) samples exist.
    pub has_small: bool,
    /// Exact size of the phase-2 extras in bits.
    pub extras_bits: u64,
    /// Exact size of the phase-1 split header (grid, flag, exceptions) in bits.
    pub header_bits: u64,
}

pub fn split_channel(s: &[u32], g: i32) -> SplitChannel {
    let mut coarse = Vec::with_capacity(s.len());
    let mut exceptions = Vec::new();
    let mut extras = 0u64;
    let mut small_payload = 0u64;
    let mut zero_like = 0u64;
    let mut has_small = false;
    let eb = small_exp_bound(g);
    let floor = precision_floor(s).min(g);
    let mut prev = 0i64;
    for (i, &v) in s.iter().enumerate() {
        match split_at_grid(v, g) {
            Split::Coarse { c, n_extra, .. } => {
                extras += (n_extra - known_zeros(g - n_extra as i32, floor, n_extra)) as u64;
                coarse.push(c);
                prev = c;
            }
            Split::Zero => {
                zero_like += 1;
                coarse.push(0);
                prev = 0;
            }
            Split::Small => {
                zero_like += 1;
                has_small = true;
                let e = (v >> 23) & 0xFF;
                small_payload += 1 + gamma_bits((eb - e) as u64 + 1) + 23 - small_known_zeros(e, floor) as u64;
                coarse.push(0);
                prev = 0;
            }
            Split::Exception => {
                exceptions.push((i, v));
                coarse.push(prev);
            }
        }
    }
    if has_small {
        extras += zero_like + small_payload;
    }
    let mut header_bits =
        GRID_BITS as u64 + gamma_bits((g - floor) as u64 + 1) + 1 + gamma_bits(exceptions.len() as u64 + 1);
    let mut last: i64 = -1;
    for &(i, _) in &exceptions {
        header_bits += gamma_bits((i as i64 - last) as u64) + 32;
        last = i as i64;
    }
    SplitChannel { g, coarse, exceptions, floor, has_small, extras_bits: extras, header_bits }
}

/// Weight exponent of the lowest set bit over all finite non-zero samples
/// (`i32::MAX` if there are none).
pub fn precision_floor(s: &[u32]) -> i32 {
    s.iter()
        .filter_map(|&v| ieee754::magnitude(v).map(|(sig, lsb, _)| lsb + sig.trailing_zeros() as i32))
        .min()
        .unwrap_or(i32::MAX)
}

/// Number of the `n` extra bits (lowest weight `2^lsb`) that lie below the floor.
#[inline]
fn known_zeros(lsb: i32, floor: i32, n: u32) -> u32 {
    (floor.saturating_sub(lsb)).clamp(0, n as i32) as u32
}

/// Mantissa bits of a small sample with biased exponent `e` below the floor.
#[inline]
fn small_known_zeros(e: u32, floor: i32) -> u32 {
    let lsb = if e == 0 { MIN_LSB } else { e as i32 - 150 };
    known_zeros(lsb, floor, 23)
}

/// Phase-1 split header:
/// `grid:9 γ(grid-floor+1) has_small:1 γ(n_exc+1) {γ(Δidx) raw:32}*`.
pub fn write_split_header(w: &mut BitWriter, sc: &SplitChannel) {
    w.write_bits((sc.g + GRID_OFFSET) as u64, GRID_BITS);
    write_gamma(w, (sc.g - sc.floor) as u64 + 1);
    w.write_bit(sc.has_small);
    write_gamma(w, sc.exceptions.len() as u64 + 1);
    let mut last: i64 = -1;
    for &(i, v) in &sc.exceptions {
        write_gamma(w, (i as i64 - last) as u64);
        w.write_bits(v as u64, 32);
        last = i as i64;
    }
}

pub struct SplitHeader {
    pub g: i32,
    pub floor: i32,
    pub has_small: bool,
    pub exceptions: Vec<(usize, u32)>,
}

pub fn read_split_header(r: &mut BitReader, n: usize) -> Result<SplitHeader> {
    let g = r.read_bits(GRID_BITS)? as i32 - GRID_OFFSET;
    if !(GRID_MIN..=GRID_MAX).contains(&g) {
        return Err(Error::Corrupt("grid exponent out of range"));
    }
    let d = read_gamma(r)? - 1;
    if d > (GRID_MAX - GRID_MIN) as u64 {
        return Err(Error::Corrupt("precision floor out of range"));
    }
    let floor = g - d as i32;
    let has_small = r.read_bit()?;
    let count = read_gamma(r)? - 1;
    if count > n as u64 {
        return Err(Error::Corrupt("more exceptions than samples"));
    }
    let mut exceptions = Vec::with_capacity(count as usize);
    let mut last: i64 = -1;
    for _ in 0..count {
        let d = read_gamma(r)?;
        let idx = last.saturating_add(d.min(i64::MAX as u64) as i64);
        if idx >= n as i64 {
            return Err(Error::Corrupt("exception index out of range"));
        }
        exceptions.push((idx as usize, r.read_bits(32)? as u32));
        last = idx;
    }
    Ok(SplitHeader { g, floor, has_small, exceptions })
}

/// Phase-2 extras: for each non-exception sample, in order:
/// `c != 0`: the `n_extra(c, g)` low significand bits, minus those below the floor;
/// `c == 0` and `has_small`: `0` (= +0.0) or `1 sign:1 γ(eb-exp+1) mantissa`
/// (23 bits minus those below the floor).
pub fn write_split_extras(w: &mut BitWriter, s: &[u32], sc: &SplitChannel) {
    let eb = small_exp_bound(sc.g);
    for &v in s {
        match split_at_grid(v, sc.g) {
            Split::Coarse { n_extra, extra, .. } => {
                let z = known_zeros(sc.g - n_extra as i32, sc.floor, n_extra);
                w.write_bits((extra >> z) as u64, n_extra - z)
            }
            Split::Zero => {
                if sc.has_small {
                    w.write_bit(false)
                }
            }
            Split::Small => {
                w.write_bit(true);
                w.write_bit(v >> 31 != 0);
                let e = (v >> 23) & 0xFF;
                write_gamma(w, (eb - e) as u64 + 1);
                let z = small_known_zeros(e, sc.floor);
                w.write_bits(((v & ieee754::MANT_MASK) >> z) as u64, 23 - z);
            }
            Split::Exception => {}
        }
    }
}

pub fn read_split_extras(r: &mut BitReader, coarse: &[i64], h: &SplitHeader, out: &mut [u32]) -> Result<()> {
    let eb = small_exp_bound(h.g);
    let mut exc = h.exceptions.iter().peekable();
    for (i, (&c, o)) in coarse.iter().zip(out.iter_mut()).enumerate() {
        if let Some(&&(ei, v)) = exc.peek() {
            if ei == i {
                *o = v;
                exc.next();
                continue;
            }
        }
        *o = if c != 0 {
            let n = extra_bits_for(c, h.g).map_err(Error::Corrupt)?;
            let z = known_zeros(h.g - n as i32, h.floor, n);
            let extra = (r.read_bits(n - z)? as u32) << z;
            join_from_grid(c, h.g, n, extra)
        } else if h.has_small && r.read_bit()? {
            let sign = r.read_bits(1)? as u32;
            let d = read_gamma(r)? - 1;
            if d > eb as u64 {
                return Err(Error::Corrupt("small-sample exponent out of range"));
            }
            let exp = eb - d as u32;
            let z = small_known_zeros(exp, h.floor);
            let mant = (r.read_bits(23 - z)? as u32) << z;
            (sign << 31) | (exp << 23) | mant
        } else {
            0
        };
    }
    Ok(())
}

/// Candidate grid exponents for a channel. `offsets` are subtracted from
/// `max_msb - 23` (the grid at which the loudest sample has no extra bits).
pub fn grid_candidates(s: &[u32], offsets: &[i32]) -> Vec<i32> {
    let max_msb = s.iter().filter_map(|&v| ieee754::magnitude(v).map(|m| m.2)).max().unwrap_or(i32::MIN);
    let min_lsb = precision_floor(s);
    if max_msb == i32::MIN {
        return vec![0];
    }
    // Lowest useful grid: below the precision floor the coarse values only gain
    // trailing zeros; and coarse values must fit MAX_COARSE_BITS.
    let g_floor = min_lsb.max(max_msb - (MAX_COARSE_BITS as i32 - 1));
    let mut c: Vec<i32> = std::iter::once(g_floor)
        .chain(offsets.iter().map(|&k| max_msb - 23 - k))
        .map(|g| g.clamp(g_floor, max_msb).clamp(GRID_MIN, GRID_MAX))
        .collect();
    c.sort_unstable();
    c.dedup();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_set() -> Vec<u32> {
        let mut v = vec![
            0,
            0x8000_0000,
            1,
            0x8000_0001,
            0x007F_FFFF,
            0x0080_0000,
            1.0f32.to_bits(),
            (-1.0f32).to_bits(),
            0.5f32.to_bits(),
            f32::MAX.to_bits(),
            f32::MIN.to_bits(),
            f32::INFINITY.to_bits(),
            f32::NEG_INFINITY.to_bits(),
            0x7FC0_0000,
            0x7F80_0001,
            0xFFFF_FFFF,
            1e10f32.to_bits(),
            (-1e10f32).to_bits(),
        ];
        let mut x = 0x1234_5678u32;
        for i in 0..500 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push(if i % 3 == 0 { x } else { ((i as f32 * 0.1).sin() * 0.7).to_bits() });
        }
        v
    }

    #[test]
    fn raw_xorlz_fieldvlc_roundtrip() {
        let s = sample_set();
        type Enc = fn(&mut BitWriter, &[u32]);
        type Dec = fn(&mut BitReader, &mut [u32]) -> Result<()>;
        let codecs: [(Enc, Dec); 3] =
            [(encode_raw, decode_raw), (encode_xorlz, decode_xorlz), (encode_fieldvlc, decode_fieldvlc)];
        for (enc, dec) in codecs {
            let mut w = BitWriter::new();
            enc(&mut w, &s);
            let b = w.into_bytes();
            let mut out = vec![0; s.len()];
            dec(&mut BitReader::new(&b), &mut out).unwrap();
            assert_eq!(out, s);
        }
        let mut w = BitWriter::new();
        encode_fieldvlc(&mut w, &s);
        assert_eq!(w.bit_len(), fieldvlc_cost(&s));
    }

    #[test]
    fn floatsplit_roundtrip_all_grids() {
        let s = sample_set();
        let mut grids = grid_candidates(&s, &[-8, 0, 8]);
        grids.extend([-149, -100, -30, 0, 30, 127]);
        for g in grids {
            let sc = split_channel(&s, g);
            let mut w = BitWriter::new();
            write_split_header(&mut w, &sc);
            assert_eq!(w.bit_len(), sc.header_bits);
            let before = w.bit_len();
            write_split_extras(&mut w, &s, &sc);
            assert_eq!(w.bit_len() - before, sc.extras_bits, "g={g}");
            let b = w.into_bytes();
            let mut r = BitReader::new(&b);
            let h = read_split_header(&mut r, s.len()).unwrap();
            let mut out = vec![0; s.len()];
            read_split_extras(&mut r, &sc.coarse, &h, &mut out).unwrap();
            assert_eq!(out, s, "g={g}");
        }
    }
}
