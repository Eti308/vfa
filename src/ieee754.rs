//! IEEE-754 binary32 bit-pattern analysis.
//!
//! Everything here works on the raw `u32` bit pattern; no floating-point
//! arithmetic is ever applied to sample values. `f32::to_bits` / `from_bits`
//! are pure reinterpretations and are only used at API boundaries.
//!
//! ## Exact grid decomposition
//!
//! A finite, non-zero float has the exact value `±sig · 2^lsb` where `sig` is
//! the 24-bit significand (implicit bit included; 23 bits for subnormals) and
//! `lsb` the weight of its least significant bit. Its most significant set
//! bit has weight `2^msb`.
//!
//! For a *grid exponent* `g` (chosen per block and channel) every sample is
//! split into
//!
//! * a **coarse integer** `c = trunc(x / 2^g)` (sign-magnitude truncation), and
//! * the **extra bits** of the significand below `2^g`.
//!
//! The decoder can recover the number of extra bits from `c` alone: if
//! `c != 0` the msb of `x` is the msb of `c` shifted by `g`, which fixes the
//! exponent and therefore how many significand bits lie below the grid.
//! Samples with `|x| < 2^g` (`c == 0`) are "small" and are escaped
//! separately. This makes the per-sample bit length variable and
//! self-describing: large samples cost `coarse residual + 0` extra bits,
//! small samples cost exactly the precision they carry.

/// Sign bit mask.
pub const SIGN_MASK: u32 = 0x8000_0000;
/// Exponent field mask.
pub const EXP_MASK: u32 = 0x7F80_0000;
/// Mantissa (fraction) field mask.
pub const MANT_MASK: u32 = 0x007F_FFFF;
pub const EXP_BIAS: i32 = 127;
pub const MANT_BITS: u32 = 23;
/// Weight exponent of the least significant bit of subnormals (2^-149).
pub const MIN_LSB: i32 = -149;
/// Largest msb weight exponent of a finite float32.
pub const MAX_MSB: i32 = 127;
/// Coarse integers are restricted to this many magnitude bits so that stereo
/// sums/differences can never overflow an `i64`.
pub const MAX_COARSE_BITS: u32 = 60;

/// The three IEEE-754 fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    pub sign: u32,
    /// biased exponent, 0..=255
    pub exp: u32,
    /// 23-bit fraction
    pub mant: u32,
}

#[inline]
pub fn fields(bits: u32) -> Fields {
    Fields { sign: bits >> 31, exp: (bits >> 23) & 0xFF, mant: bits & MANT_MASK }
}

#[inline]
pub fn from_fields(f: Fields) -> u32 {
    (f.sign << 31) | ((f.exp & 0xFF) << 23) | (f.mant & MANT_MASK)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    PosZero,
    NegZero,
    Subnormal,
    Normal,
    Infinity,
    NaN,
}

#[inline]
pub fn classify(bits: u32) -> Class {
    let f = fields(bits);
    match (f.exp, f.mant) {
        (0, 0) if f.sign == 0 => Class::PosZero,
        (0, 0) => Class::NegZero,
        (0, _) => Class::Subnormal,
        (255, 0) => Class::Infinity,
        (255, _) => Class::NaN,
        _ => Class::Normal,
    }
}

#[inline]
pub fn is_finite(bits: u32) -> bool {
    bits & EXP_MASK != EXP_MASK
}

/// Exact magnitude of a finite non-zero float as `(sig, lsb, msb)` with
/// `|x| = sig · 2^lsb` and `2^msb <= |x| < 2^(msb+1)`.
/// Returns `None` for zeros, infinities and NaNs.
#[inline]
pub fn magnitude(bits: u32) -> Option<(u32, i32, i32)> {
    let exp = (bits >> 23) & 0xFF;
    let mant = bits & MANT_MASK;
    if exp == 255 || (exp == 0 && mant == 0) {
        return None;
    }
    let (sig, lsb) = if exp == 0 { (mant, MIN_LSB) } else { (mant | 0x0080_0000, exp as i32 - 150) };
    let msb = lsb + (31 - sig.leading_zeros()) as i32;
    Some((sig, lsb, msb))
}

/// Result of splitting one sample at grid exponent `g`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// Exactly +0.0 (coarse 0, no escape payload besides a flag).
    Zero,
    /// `|x| < 2^g`, or −0.0: coarse value 0, escaped explicitly.
    Small,
    /// `|x| >= 2^g`: coarse integer plus `n_extra` bits `extra`.
    Coarse { c: i64, n_extra: u32, extra: u32 },
    /// Infinity / NaN, or a magnitude too large for `MAX_COARSE_BITS`:
    /// must be stored verbatim as an exception.
    Exception,
}

/// Split one float at grid exponent `g` (see module docs). Exact.
#[inline]
pub fn split_at_grid(bits: u32, g: i32) -> Split {
    if bits == 0 {
        return Split::Zero;
    }
    let Some((sig, lsb, msb)) = magnitude(bits) else {
        // -0.0 is handled by the small-sample escape, Inf/NaN as exceptions
        return if bits == SIGN_MASK { Split::Small } else { Split::Exception };
    };
    if msb < g {
        return Split::Small;
    }
    let neg = bits & SIGN_MASK != 0;
    let (cabs, n_extra, extra) = if lsb >= g {
        if (msb - g + 1) as u32 > MAX_COARSE_BITS {
            return Split::Exception;
        }
        ((sig as u64) << (lsb - g), 0, 0)
    } else {
        // msb >= g > lsb, so 0 < n <= 23
        let n = (g - lsb) as u32;
        ((sig >> n) as u64, n, sig & ((1u32 << n) - 1))
    };
    let c = if neg { -(cabs as i64) } else { cabs as i64 };
    Split::Coarse { c, n_extra, extra }
}

/// For a non-zero coarse value `c` at grid `g`: the number of extra bits the
/// decoder must read. Errors if `c` cannot have come from a valid float.
#[inline]
pub fn extra_bits_for(c: i64, g: i32) -> Result<u32, &'static str> {
    let cabs = c.unsigned_abs();
    debug_assert!(cabs != 0);
    let len = 64 - cabs.leading_zeros();
    if len > MAX_COARSE_BITS {
        return Err("coarse value too large");
    }
    let msb = g + len as i32 - 1;
    if msb > MAX_MSB {
        return Err("coarse value exceeds float32 range");
    }
    let lsb = (msb - 23).max(MIN_LSB);
    if lsb >= g {
        let shift = (lsb - g) as u32; // < len <= 60
        if cabs & ((1u64 << shift) - 1) != 0 {
            return Err("coarse value has more precision than a float32");
        }
        Ok(0)
    } else {
        Ok((g - lsb) as u32)
    }
}

/// Inverse of [`split_at_grid`] for `Split::Coarse`. `n_extra` must be the
/// value returned by [`extra_bits_for`].
#[inline]
pub fn join_from_grid(c: i64, g: i32, n_extra: u32, extra: u32) -> u32 {
    let neg = c < 0;
    let cabs = c.unsigned_abs();
    let len = 64 - cabs.leading_zeros();
    let msb = g + len as i32 - 1;
    let lsb = (msb - 23).max(MIN_LSB);
    let sig: u32 = if n_extra == 0 {
        (cabs >> ((lsb - g).max(0) as u32)) as u32
    } else {
        ((cabs << n_extra) as u32) | (extra & ((1u32 << n_extra) - 1))
    };
    let (exp, mant) = if msb >= -126 { ((msb + EXP_BIAS) as u32, sig & MANT_MASK) } else { (0, sig) };
    ((neg as u32) << 31) | (exp << 23) | (mant & MANT_MASK)
}

/// Upper bound of the biased exponent field of a "small" sample at grid `g`
/// (`|x| < 2^g`). Used to code the exponent of escaped small samples
/// relative to the grid.
#[inline]
pub fn small_exp_bound(g: i32) -> u32 {
    (g + 126).clamp(0, 254) as u32
}

/// Decibels relative to full scale (1.0). Reporting only — never used by the
/// codec itself.
pub fn dbfs(bits: u32) -> Option<f64> {
    let v = f32::from_bits(bits) as f64;
    if v == 0.0 || !v.is_finite() {
        None
    } else {
        Some(20.0 * v.abs().log10())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bits: u32, g: i32) {
        match split_at_grid(bits, g) {
            Split::Coarse { c, n_extra, extra } => {
                assert_ne!(c, 0);
                assert_eq!(extra_bits_for(c, g).unwrap(), n_extra, "bits {bits:08x} g {g}");
                assert_eq!(join_from_grid(c, g, n_extra, extra), bits, "bits {bits:08x} g {g}");
            }
            Split::Zero => assert_eq!(bits, 0),
            Split::Small => {
                let m = magnitude(bits);
                assert!(bits == SIGN_MASK || m.unwrap().2 < g);
                if bits != SIGN_MASK {
                    let e = fields(bits).exp;
                    assert!(e <= small_exp_bound(g));
                }
            }
            Split::Exception => {
                assert!(!is_finite(bits) || magnitude(bits).unwrap().2 - g + 1 > MAX_COARSE_BITS as i32)
            }
        }
    }

    #[test]
    fn classify_specials() {
        assert_eq!(classify(0), Class::PosZero);
        assert_eq!(classify(SIGN_MASK), Class::NegZero);
        assert_eq!(classify(1), Class::Subnormal);
        assert_eq!(classify(1.0f32.to_bits()), Class::Normal);
        assert_eq!(classify(f32::INFINITY.to_bits()), Class::Infinity);
        assert_eq!(classify(f32::NEG_INFINITY.to_bits()), Class::Infinity);
        assert_eq!(classify(0x7FC0_0001), Class::NaN);
        assert_eq!(classify(0xFF80_0001), Class::NaN);
    }

    #[test]
    fn magnitude_exact() {
        assert_eq!(magnitude(1.0f32.to_bits()), Some((1 << 23, -23, 0)));
        assert_eq!(magnitude(0.5f32.to_bits()), Some((1 << 23, -24, -1)));
        assert_eq!(magnitude(1), Some((1, -149, -149)));
        assert_eq!(magnitude(f32::MAX.to_bits()), Some((0xFF_FFFF, 104, 127)));
    }

    #[test]
    fn split_examples() {
        // 1.0 at grid -23 -> coarse 2^23, no extras
        assert_eq!(split_at_grid(1.0f32.to_bits(), -23), Split::Coarse { c: 1 << 23, n_extra: 0, extra: 0 });
        // 1.5 at grid 0 -> coarse 1, one extra bit (the .5)
        let s = split_at_grid(1.5f32.to_bits(), 0);
        assert_eq!(s, Split::Coarse { c: 1, n_extra: 23, extra: 1 << 22 });
        assert_eq!(split_at_grid((-3.0f32).to_bits(), 1), Split::Coarse { c: -1, n_extra: 23, extra: 1 << 22 });
        assert_eq!(split_at_grid(0.25f32.to_bits(), 0), Split::Small);
        assert_eq!(split_at_grid(SIGN_MASK, 0), Split::Small);
        assert_eq!(split_at_grid(0, 0), Split::Zero);
        assert_eq!(split_at_grid(f32::NAN.to_bits(), 0), Split::Exception);
        // f32::MAX at grid -149 needs 277 bits -> exception
        assert_eq!(split_at_grid(f32::MAX.to_bits(), -149), Split::Exception);
    }

    #[test]
    fn exhaustive_exponents_all_grids() {
        // every exponent, several mantissas, every sign, a wide range of grids
        let mants = [0u32, 1, 2, 3, 0x40_0000, 0x7F_FFFF, 0x12_3457, 0x55_5555, 0x00_0100];
        for exp in 0u32..=255 {
            for &mant in &mants {
                for sign in 0..2u32 {
                    let bits = from_fields(Fields { sign, exp, mant });
                    for g in (-160..=140).step_by(3) {
                        roundtrip(bits, g);
                    }
                }
            }
        }
    }

    #[test]
    fn extra_bits_rejects_impossible() {
        // a 30-bit-wide coarse value at grid 0 cannot be a float (only 24 significant bits)
        assert!(extra_bits_for((1 << 29) | 1, 0).is_err());
        // msb beyond 127
        assert!(extra_bits_for(1 << 20, 120).is_err());
    }
}
