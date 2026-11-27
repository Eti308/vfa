//! Variable-length integer codes.
//!
//! * zigzag mapping signed → unsigned
//! * LEB128 byte-oriented varints (container metadata)
//! * Elias γ and δ codes (bit oriented, parameter free)
//! * Rice codes with a bounded escape (Golomb codes with power-of-two divisor)
//! * partitioned Rice coding of residual vectors with per-partition
//!   parameters, all-zero partitions and a raw fallback

use crate::bitio::{mask, BitReader, BitWriter};
use crate::{Error, Result};

#[inline(always)]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline(always)]
pub fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

#[inline(always)]
pub fn bit_len(v: u64) -> u32 {
    64 - v.leading_zeros()
}

// ---------------------------------------------------------------- LEB128

pub fn write_leb128(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Returns `(value, bytes consumed)`.
pub fn read_leb128(data: &[u8]) -> Result<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in data.iter().enumerate().take(10) {
        let part = (b & 0x7F) as u64;
        if i == 9 && part > 1 {
            return Err(Error::Corrupt("LEB128 overflow"));
        }
        v |= part << (7 * i);
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    if data.len() < 10 {
        Err(Error::UnexpectedEof)
    } else {
        Err(Error::Corrupt("LEB128 too long"))
    }
}

// ---------------------------------------------------------------- Elias

/// Elias γ code of `n >= 1`: `len-1` zeros followed by `n` in `len` bits.
#[inline]
pub fn write_gamma(w: &mut BitWriter, n: u64) {
    debug_assert!(n >= 1);
    let len = bit_len(n);
    w.write_zeros((len - 1) as u64);
    w.write_bits(n, len);
}

#[inline]
pub fn read_gamma(r: &mut BitReader) -> Result<u64> {
    let z = r.read_unary_limited(64)?;
    if z >= 64 {
        return Err(Error::Corrupt("Elias gamma prefix too long"));
    }
    Ok((1u64 << z) | r.read_bits(z)?)
}

#[inline]
pub fn gamma_bits(n: u64) -> u64 {
    2 * bit_len(n) as u64 - 1
}

/// Elias δ code of `n >= 1`: γ(len) followed by the `len-1` low bits of `n`.
pub fn write_delta(w: &mut BitWriter, n: u64) {
    debug_assert!(n >= 1);
    let len = bit_len(n);
    write_gamma(w, len as u64);
    w.write_bits(n & mask(len - 1), len - 1);
}

pub fn read_delta(r: &mut BitReader) -> Result<u64> {
    let len = read_gamma(r)?;
    if len > 64 {
        return Err(Error::Corrupt("Elias delta length > 64"));
    }
    let len = len as u32;
    let low = r.read_bits(len - 1)?;
    Ok(if len == 64 { (1u64 << 63) | low } else { (1u64 << (len - 1)) | low })
}

pub fn delta_bits(n: u64) -> u64 {
    let len = bit_len(n) as u64;
    gamma_bits(len) + len - 1
}

// ---------------------------------------------------------------- Rice

/// Quotients `>= RICE_ESCAPE` are escaped: `RICE_ESCAPE` zeros, a 7-bit
/// length and the raw value. This bounds the worst case code length to
/// `32 + 7 + 64` bits and prevents unbounded unary runs on corrupt input.
pub const RICE_ESCAPE: u32 = 32;
/// Largest Rice parameter.
pub const RICE_MAX_K: u32 = 58;

#[inline(always)]
pub fn write_rice(w: &mut BitWriter, u: u64, k: u32) {
    let q = u >> k;
    if q < RICE_ESCAPE as u64 {
        // q zeros, a one, k low bits; combined into at most two writes
        if q as u32 + 1 + k <= 64 {
            w.write_bits((1u64 << k) | (u & mask(k)), q as u32 + 1 + k);
        } else {
            w.write_unary(q);
            w.write_bits(u & mask(k), k);
        }
    } else {
        w.write_zeros(RICE_ESCAPE as u64);
        let len = bit_len(u);
        w.write_bits(len as u64, 7);
        w.write_bits(u, len);
    }
}

#[inline(always)]
pub fn read_rice(r: &mut BitReader, k: u32) -> Result<u64> {
    let q = r.read_unary_limited(RICE_ESCAPE)?;
    if q < RICE_ESCAPE {
        Ok(((q as u64) << k) | r.read_bits(k)?)
    } else {
        let len = r.read_bits(7)? as u32;
        if len > 64 {
            return Err(Error::Corrupt("Rice escape length > 64"));
        }
        r.read_bits(len)
    }
}

#[inline(always)]
pub fn rice_bits(u: u64, k: u32) -> u64 {
    let q = u >> k;
    if q < RICE_ESCAPE as u64 {
        q + 1 + k as u64
    } else {
        RICE_ESCAPE as u64 + 7 + bit_len(u) as u64
    }
}

/// Exact cost of Rice coding `vals` with parameter `k`.
pub fn rice_cost(vals: &[u64], k: u32) -> u64 {
    vals.iter().map(|&u| rice_bits(u, k)).sum()
}

/// Best single Rice parameter for `vals` and its exact cost.
pub fn best_rice_param(vals: &[u64]) -> (u32, u64) {
    if vals.is_empty() {
        return (0, 0);
    }
    let sum: u128 = vals.iter().map(|&u| u as u128).sum();
    let mean = (sum / vals.len() as u128) as u64;
    // For a geometric distribution the optimum is close to log2(mean · ln 2).
    let k0 = bit_len(mean).saturating_sub(1).min(RICE_MAX_K);
    let lo = k0.saturating_sub(1);
    let hi = (k0 + 1).min(RICE_MAX_K);
    let mut best = (lo, u64::MAX);
    for k in lo..=hi {
        let c = rice_cost(vals, k);
        if c < best.1 {
            best = (k, c);
        }
    }
    best
}

// ------------------------------------------------- partitioned Rice

/// Largest partition order (2^8 = 256 partitions).
pub const MAX_PARTITION_ORDER: u32 = 8;
/// Partition parameter code: all values in the partition are zero.
pub const PARAM_ALL_ZERO: u32 = 61;
/// Partition parameter code: raw fixed-width values follow (7-bit width).
pub const PARAM_RAW: u32 = 62;

#[derive(Clone, Copy, Debug)]
enum PartCode {
    Rice(u32),
    Zero,
    Raw(u32),
}

fn best_partition_code(vals: &[u64]) -> (PartCode, u64) {
    if vals.iter().all(|&u| u == 0) {
        return (PartCode::Zero, 6);
    }
    let (k, cost) = best_rice_param(vals);
    let width = vals.iter().map(|&u| bit_len(u)).max().unwrap_or(0);
    let raw_cost = 7 + width as u64 * vals.len() as u64;
    if raw_cost < cost {
        (PartCode::Raw(width), 6 + raw_cost)
    } else {
        (PartCode::Rice(k), 6 + cost)
    }
}

#[inline]
fn partition_bounds(n: usize, order: u32, i: usize) -> (usize, usize) {
    ((i * n) >> order, ((i + 1) * n) >> order)
}

fn plan_partitions(vals: &[u64], max_order: u32) -> (u32, Vec<PartCode>, u64) {
    let n = vals.len();
    let mut best: Option<(u32, Vec<PartCode>, u64)> = None;
    for order in 0..=max_order.min(MAX_PARTITION_ORDER) {
        let parts = 1usize << order;
        if order > 0 && n < parts * 16 {
            break; // partitions would be too small to pay for their headers
        }
        let mut codes = Vec::with_capacity(parts);
        let mut total = 4u64;
        for i in 0..parts {
            let (a, b) = partition_bounds(n, order, i);
            let (code, cost) = best_partition_code(&vals[a..b]);
            codes.push(code);
            total += cost;
        }
        if best.as_ref().is_none_or(|b| total < b.2) {
            best = Some((order, codes, total));
        }
    }
    best.unwrap()
}

/// Estimated (exact) cost in bits of [`write_partitioned`].
pub fn partitioned_cost(vals: &[u64], max_order: u32) -> u64 {
    plan_partitions(vals, max_order).2
}

/// Write `vals` with adaptive partitioned Rice coding.
pub fn write_partitioned(w: &mut BitWriter, vals: &[u64], max_order: u32) {
    let (order, codes, _) = plan_partitions(vals, max_order);
    w.write_bits(order as u64, 4);
    for (i, code) in codes.iter().enumerate() {
        let (a, b) = partition_bounds(vals.len(), order, i);
        match *code {
            PartCode::Zero => w.write_bits(PARAM_ALL_ZERO as u64, 6),
            PartCode::Raw(width) => {
                w.write_bits(PARAM_RAW as u64, 6);
                w.write_bits(width as u64, 7);
                for &u in &vals[a..b] {
                    w.write_bits(u, width);
                }
            }
            PartCode::Rice(k) => {
                w.write_bits(k as u64, 6);
                for &u in &vals[a..b] {
                    write_rice(w, u, k);
                }
            }
        }
    }
}

/// Read `out.len()` values written by [`write_partitioned`].
pub fn read_partitioned(r: &mut BitReader, out: &mut [u64]) -> Result<()> {
    let order = r.read_bits(4)? as u32;
    read_partitioned_body(r, order, out)
}

/// Body of [`read_partitioned`] after the 4-bit partition order.
pub fn read_partitioned_body(r: &mut BitReader, order: u32, out: &mut [u64]) -> Result<()> {
    if order > MAX_PARTITION_ORDER {
        return Err(Error::Corrupt("partition order too large"));
    }
    let n = out.len();
    for i in 0..(1usize << order) {
        let (a, b) = partition_bounds(n, order, i);
        let part = &mut out[a..b];
        let code = r.read_bits(6)? as u32;
        match code {
            PARAM_ALL_ZERO => part.fill(0),
            PARAM_RAW => {
                let width = r.read_bits(7)? as u32;
                if width > 64 {
                    return Err(Error::Corrupt("raw partition width > 64"));
                }
                for v in part.iter_mut() {
                    *v = r.read_bits(width)?;
                }
            }
            k if k <= RICE_MAX_K => {
                for v in part.iter_mut() {
                    *v = read_rice(r, k)?;
                }
            }
            _ => return Err(Error::Corrupt("invalid Rice partition parameter")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_roundtrip() {
        for v in [0i64, 1, -1, 2, -2, i64::MAX, i64::MIN, 123456789, -987654321] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
    }

    #[test]
    fn leb128_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            write_leb128(&mut b, v);
            assert_eq!(read_leb128(&b).unwrap(), (v, b.len()));
        }
        assert!(read_leb128(&[0x80, 0x80]).is_err());
    }

    #[test]
    fn elias_roundtrip() {
        let vals = [1u64, 2, 3, 4, 7, 8, 1000, u32::MAX as u64, u64::MAX];
        let mut w = BitWriter::new();
        for &v in &vals {
            write_gamma(&mut w, v);
            write_delta(&mut w, v);
        }
        let expected: u64 = vals.iter().map(|&v| gamma_bits(v) + delta_bits(v)).sum();
        assert_eq!(w.bit_len(), expected);
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        for &v in &vals {
            assert_eq!(read_gamma(&mut r).unwrap(), v);
            assert_eq!(read_delta(&mut r).unwrap(), v);
        }
    }

    #[test]
    fn rice_roundtrip_with_escape() {
        let vals = [0u64, 1, 5, 100, 1 << 20, u64::MAX, 31, 32, 33];
        for k in [0u32, 1, 3, 10, 30, 58] {
            let mut w = BitWriter::new();
            for &v in &vals {
                write_rice(&mut w, v, k);
            }
            assert_eq!(w.bit_len(), rice_cost(&vals, k));
            let b = w.into_bytes();
            let mut r = BitReader::new(&b);
            for &v in &vals {
                assert_eq!(read_rice(&mut r, k).unwrap(), v, "k={k}");
            }
        }
    }

    #[test]
    fn partitioned_roundtrip() {
        let mut vals: Vec<u64> = (0..4096u64).map(|i| (i * 2654435761) % 1000).collect();
        vals[100..900].fill(0);
        vals[2000] = u64::MAX;
        let mut w = BitWriter::new();
        write_partitioned(&mut w, &vals, 8);
        assert_eq!(w.bit_len(), partitioned_cost(&vals, 8));
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        let mut out = vec![0; vals.len()];
        read_partitioned(&mut r, &mut out).unwrap();
        assert_eq!(out, vals);
    }
}
