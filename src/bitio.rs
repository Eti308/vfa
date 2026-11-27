//! MSB-first bit-level I/O.
//!
//! The writer packs bits most-significant-first into bytes; the final byte is
//! zero padded. The reader is bounds checked on every access and returns
//! [`Error::UnexpectedEof`] instead of panicking, so it is safe on corrupt
//! input.

use crate::{Error, Result};

#[derive(Default, Clone)]
pub struct BitWriter {
    buf: Vec<u8>,
    /// Pending bits, right-aligned. Always fewer than 8 between calls.
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize) -> Self {
        Self { buf: Vec::with_capacity(bytes), acc: 0, nbits: 0 }
    }

    /// Total number of bits written so far.
    #[inline]
    pub fn bit_len(&self) -> u64 {
        self.buf.len() as u64 * 8 + self.nbits as u64
    }

    /// Write the low `n` bits of `value` (`n <= 64`), MSB first.
    #[inline]
    pub fn write_bits(&mut self, value: u64, n: u32) {
        debug_assert!(n <= 64);
        if n > 32 {
            self.write_small((value >> 32) & mask(n - 32), n - 32);
            self.write_small(value & 0xFFFF_FFFF, 32);
        } else {
            self.write_small(value & mask(n), n);
        }
    }

    #[inline]
    fn write_small(&mut self, value: u64, n: u32) {
        // nbits < 8 and n <= 32, so the accumulator never overflows.
        if n == 0 {
            return;
        }
        self.acc = (self.acc << n) | value;
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.buf.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= mask(self.nbits);
    }

    #[inline]
    pub fn write_bit(&mut self, bit: bool) {
        self.write_small(bit as u64, 1);
    }

    /// Write `n` zero bits followed by a one bit (unary code of `n`).
    #[inline]
    pub fn write_unary(&mut self, mut n: u64) {
        while n >= 32 {
            self.write_small(0, 32);
            n -= 32;
        }
        self.write_small(1, n as u32 + 1);
    }

    /// Write `n` zero bits.
    pub fn write_zeros(&mut self, mut n: u64) {
        while n >= 32 {
            self.write_small(0, 32);
            n -= 32;
        }
        self.write_small(0, n as u32);
    }

    /// Append all bits of another writer.
    pub fn append(&mut self, other: &BitWriter) {
        if self.nbits == 0 {
            self.buf.extend_from_slice(&other.buf);
        } else {
            for &b in &other.buf {
                self.write_small(b as u64, 8);
            }
        }
        self.write_small(other.acc, other.nbits);
    }

    /// Pad with zero bits to the next byte boundary.
    pub fn align(&mut self) {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            self.write_small(0, pad);
        }
    }

    /// Finish and return the bytes (zero padded to a whole byte).
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align();
        self.buf
    }
}

#[inline(always)]
pub(crate) fn mask(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Bounds-checked MSB-first bit reader over a byte slice.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    len_bits: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, len_bits: data.len() * 8 }
    }

    #[inline]
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.len_bits - self.pos
    }

    /// Up to 64 bits starting at the current position, left aligned,
    /// zero filled past the end of the data.
    #[inline(always)]
    fn peek64(&self) -> u64 {
        let byte = self.pos >> 3;
        let off = (self.pos & 7) as u32;
        let d = self.data;
        let hi = if byte + 8 <= d.len() {
            u64::from_be_bytes(d[byte..byte + 8].try_into().unwrap())
        } else {
            let mut tmp = [0u8; 8];
            if byte < d.len() {
                tmp[..d.len() - byte].copy_from_slice(&d[byte..]);
            }
            u64::from_be_bytes(tmp)
        };
        if off == 0 {
            hi
        } else {
            let next = d.get(byte + 8).copied().unwrap_or(0) as u64;
            (hi << off) | (next >> (8 - off))
        }
    }

    /// Read `n <= 64` bits.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> Result<u64> {
        debug_assert!(n <= 64);
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.len_bits {
            return Err(Error::UnexpectedEof);
        }
        let v = self.peek64() >> (64 - n);
        self.pos += n as usize;
        Ok(v)
    }

    #[inline]
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Count zero bits up to and including the terminating one bit.
    /// Returns the number of zeros. Stops with `Ok(limit)` *without* consuming
    /// a terminator if `limit` zeros were read.
    #[inline]
    pub fn read_unary_limited(&mut self, limit: u32) -> Result<u32> {
        let mut count = 0u32;
        loop {
            let avail = self.len_bits - self.pos;
            if avail == 0 {
                return Err(Error::UnexpectedEof);
            }
            let window = self.peek64();
            let lz = window.leading_zeros();
            let usable = avail.min(64) as u32;
            if lz < usable {
                // terminator found inside the window
                if count + lz >= limit {
                    self.pos += (limit - count) as usize;
                    return Ok(limit);
                }
                self.pos += lz as usize + 1;
                return Ok(count + lz);
            }
            // all usable bits are zero
            if count + usable >= limit {
                self.pos += (limit - count) as usize;
                return Ok(limit);
            }
            count += usable;
            self.pos += usable as usize;
        }
    }

    /// Skip to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
        if self.pos > self.len_bits {
            self.pos = self.len_bits;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mixed_widths() {
        let mut w = BitWriter::new();
        let items: Vec<(u64, u32)> = (0..2000u64)
            .map(|i| {
                let n = (i * 7 % 65) as u32;
                (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) & mask(n), n)
            })
            .collect();
        for &(v, n) in &items {
            w.write_bits(v, n);
        }
        let total = w.bit_len();
        let bytes = w.into_bytes();
        assert_eq!(bytes.len() as u64, total.div_ceil(8));
        let mut r = BitReader::new(&bytes);
        for &(v, n) in &items {
            assert_eq!(r.read_bits(n).unwrap(), v);
        }
    }

    #[test]
    fn unary() {
        let mut w = BitWriter::new();
        for n in [0u64, 1, 5, 31, 32, 33, 63, 64, 100, 200] {
            w.write_unary(n);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for n in [0u32, 1, 5, 31, 32, 33, 63, 64, 100, 200] {
            assert_eq!(r.read_unary_limited(1000).unwrap(), n);
        }
    }

    #[test]
    fn unary_limit_does_not_consume_terminator() {
        let mut w = BitWriter::new();
        w.write_zeros(10);
        w.write_bits(0b101, 3);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_unary_limited(10).unwrap(), 10);
        assert_eq!(r.read_bits(3).unwrap(), 0b101);
    }

    #[test]
    fn eof_is_error_not_panic() {
        let bytes = [0u8; 2];
        let mut r = BitReader::new(&bytes);
        assert!(r.read_bits(17).is_err());
        assert!(r.read_unary_limited(100).is_err());
    }

    #[test]
    fn append_unaligned() {
        let mut a = BitWriter::new();
        a.write_bits(0b101, 3);
        let mut b = BitWriter::new();
        b.write_bits(0xABCD, 16);
        b.write_bits(0b11, 2);
        a.append(&b);
        let bytes = a.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(3).unwrap(), 0b101);
        assert_eq!(r.read_bits(16).unwrap(), 0xABCD);
        assert_eq!(r.read_bits(2).unwrap(), 0b11);
    }
}
