//! Float32 WAV reading and writing.
//!
//! The reader keeps every byte before the sample data (`prefix`: RIFF header,
//! `fmt `, `fact`, `LIST`, `bext`, ... chunks) and after it (`suffix`: pad
//! byte, trailing chunks), so that a decoded file can be reproduced byte for
//! byte. Samples are kept as raw `u32` bit patterns, never as `f32` values.

use crate::{Error, Result};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// KSDATAFORMAT_SUBTYPE_* GUID tail shared by PCM and IEEE_FLOAT.
const GUID_TAIL: [u8; 14] = [0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71];

#[derive(Clone, Debug)]
pub struct WavData {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved raw float32 bit patterns.
    pub samples: Vec<u32>,
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
}

impl WavData {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels as usize
    }
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Parse a float32 WAV file.
pub fn parse(bytes: &[u8]) -> Result<WavData> {
    if bytes.len() < 12 {
        return Err(Error::InvalidInput("file too short for WAV".into()));
    }
    if &bytes[0..4] == b"RF64" {
        return Err(Error::Unsupported("RF64 WAV files are not supported yet".into()));
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::InvalidInput("not a RIFF/WAVE file".into()));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u32, u16)> = None; // channels, rate, bits
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(bytes, pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if size < 16 || body + size > bytes.len() {
                return Err(Error::InvalidInput("malformed fmt chunk".into()));
            }
            let f = &bytes[body..body + size];
            let mut tag = u16le(f, 0);
            let channels = u16le(f, 2);
            let rate = u32le(f, 4);
            let block_align = u16le(f, 12);
            let bits = u16le(f, 14);
            if tag == WAVE_FORMAT_EXTENSIBLE {
                if size < 40 || f[26..40] != GUID_TAIL {
                    return Err(Error::Unsupported("WAVE_FORMAT_EXTENSIBLE with unknown sub-format".into()));
                }
                tag = u16le(f, 24);
            }
            match (tag, bits) {
                (WAVE_FORMAT_IEEE_FLOAT, 32) => {}
                (WAVE_FORMAT_IEEE_FLOAT, 64) => {
                    return Err(Error::Unsupported("float64 WAV (planned for a later stage)".into()))
                }
                (WAVE_FORMAT_PCM, b) => {
                    return Err(Error::Unsupported(format!("{b}-bit integer PCM; VFA stage 1 codes float32 only")))
                }
                (t, b) => return Err(Error::Unsupported(format!("WAV format tag {t:#x}, {b} bits"))),
            }
            if channels == 0 || block_align as u32 != 4 * channels as u32 {
                return Err(Error::InvalidInput("inconsistent channel count / block align".into()));
            }
            fmt = Some((channels, rate, bits));
        } else if id == b"data" {
            let (channels, sample_rate, _) = fmt.ok_or(Error::InvalidInput("data chunk before fmt chunk".into()))?;
            // tolerate truncated files / streaming sizes (0xFFFFFFFF)
            let avail = (bytes.len() - body).min(size);
            let frame_bytes = 4 * channels as usize;
            let frames = avail / frame_bytes;
            let data_end = body + frames * frame_bytes;
            let samples =
                bytes[body..data_end].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
            return Ok(WavData {
                sample_rate,
                channels,
                samples,
                prefix: bytes[..body].to_vec(),
                suffix: bytes[data_end..].to_vec(),
            });
        }
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(Error::InvalidInput("no data chunk found".into()))
}

/// Reassemble a WAV file from preserved prefix/suffix bytes and samples.
pub fn assemble(prefix: &[u8], samples: &[u32], suffix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + samples.len() * 4 + suffix.len());
    out.extend_from_slice(prefix);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.extend_from_slice(suffix);
    out
}

/// A canonical float32 WAV (IEEE_FLOAT for <= 2 channels, EXTENSIBLE above).
pub fn write_float32(sample_rate: u32, channels: u16, samples: &[u32]) -> Vec<u8> {
    let frames = samples.len() / channels as usize;
    let data_len = (samples.len() * 4) as u32;
    let mut h = Vec::with_capacity(80);
    let ext = channels > 2;
    let fmt_len: u32 = if ext { 40 } else { 18 };
    let riff_len = 4 + (8 + fmt_len) + (8 + 4) + (8 + data_len);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff_len.to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&fmt_len.to_le_bytes());
    h.extend_from_slice(&(if ext { WAVE_FORMAT_EXTENSIBLE } else { WAVE_FORMAT_IEEE_FLOAT }).to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&(sample_rate * 4 * channels as u32).to_le_bytes());
    h.extend_from_slice(&(4 * channels).to_le_bytes());
    h.extend_from_slice(&32u16.to_le_bytes());
    if ext {
        h.extend_from_slice(&22u16.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes()); // valid bits
        h.extend_from_slice(&0u32.to_le_bytes()); // channel mask: unspecified
        h.extend_from_slice(&WAVE_FORMAT_IEEE_FLOAT.to_le_bytes());
        h.extend_from_slice(&GUID_TAIL);
    } else {
        h.extend_from_slice(&0u16.to_le_bytes());
    }
    h.extend_from_slice(b"fact");
    h.extend_from_slice(&4u32.to_le_bytes());
    h.extend_from_slice(&(frames as u32).to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    assemble(&h, samples, &[])
}

/// Integer PCM WAV (16/24-bit) — used only to feed FLAC in benchmarks.
pub fn write_int_pcm(sample_rate: u32, channels: u16, bits: u16, ints: &[i32]) -> Vec<u8> {
    let bps = (bits / 8) as usize;
    let data_len = (ints.len() * bps) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * (bps * channels as usize) as u32).to_le_bytes());
    out.extend_from_slice(&((bps * channels as usize) as u16).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &v in ints {
        out.extend_from_slice(&v.to_le_bytes()[..bps]);
    }
    out
}

/// Parse an integer PCM WAV (16/24-bit) produced by an external decoder.
pub fn parse_int_pcm(bytes: &[u8]) -> Result<(u16, Vec<i32>)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::InvalidInput("not a RIFF/WAVE file".into()));
    }
    let mut pos = 12;
    let mut bits = 0u16;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(bytes, pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            bits = u16le(bytes, body + 14);
        } else if id == b"data" {
            let bps = (bits / 8) as usize;
            if bps == 0 {
                return Err(Error::InvalidInput("missing fmt".into()));
            }
            let end = (body + size).min(bytes.len());
            let shift = 32 - bits as u32;
            let v = bytes[body..end]
                .chunks_exact(bps)
                .map(|c| {
                    let mut b = [0u8; 4];
                    b[..bps].copy_from_slice(c);
                    (i32::from_le_bytes(b) << shift) >> shift
                })
                .collect();
            return Ok((bits, v));
        }
        pos = body + size + (size & 1);
    }
    Err(Error::InvalidInput("no data chunk".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_roundtrip_is_byte_exact() {
        for ch in [1u16, 2, 6] {
            let s: Vec<u32> = (0..ch as u32 * 100).map(|i| i.wrapping_mul(0x9E3779B9)).collect();
            let bytes = write_float32(96_000, ch, &s);
            let w = parse(&bytes).unwrap();
            assert_eq!((w.sample_rate, w.channels), (96_000, ch));
            assert_eq!(w.samples, s);
            assert_eq!(assemble(&w.prefix, &w.samples, &w.suffix), bytes);
        }
    }

    #[test]
    fn rejects_integer_pcm() {
        let b = write_int_pcm(48000, 2, 24, &[1, 2, 3, 4]);
        assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
        assert_eq!(parse_int_pcm(&b).unwrap(), (24, vec![1, 2, 3, 4]));
    }

    #[test]
    fn preserves_extra_chunks_and_odd_tail() {
        let mut bytes = write_float32(44_100, 2, &[1, 2, 3, 4]);
        bytes.extend_from_slice(b"LIST\x03\x00\x00\x00abc\x00");
        bytes.extend_from_slice(&[1, 2, 3]); // garbage after the RIFF
        let w = parse(&bytes).unwrap();
        assert_eq!(assemble(&w.prefix, &w.samples, &w.suffix), bytes);
    }
}

/// How a WAV file was brought into float32 form for encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Already IEEE float32: samples and header bytes are kept verbatim.
    Float32,
    /// Integer PCM with this many bits, converted *exactly* to float32
    /// (`k / 2^(bits-1)`; every 8/16/24-bit integer is representable). The
    /// decoded file is a float32 WAV with identical sample values.
    IntPcm(u16),
}

/// Parse a WAV for encoding: float32 as-is, or 8/16/24-bit integer PCM
/// converted exactly to float32. 32-bit integer and 64-bit float input are
/// rejected because float32 cannot represent all their values.
pub fn parse_for_encoding(bytes: &[u8]) -> Result<(WavData, Source)> {
    match parse(bytes) {
        Ok(w) => return Ok((w, Source::Float32)),
        Err(Error::Unsupported(_)) => {}
        Err(e) => return Err(e),
    }
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::InvalidInput("not a RIFF/WAVE file".into()));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(bytes, pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && size >= 16 && body + size <= bytes.len() {
            let f = &bytes[body..body + size];
            let mut tag = u16le(f, 0);
            if tag == WAVE_FORMAT_EXTENSIBLE && size >= 40 && f[26..40] == GUID_TAIL {
                tag = u16le(f, 24);
            }
            fmt = Some((tag, u16le(f, 2), u32le(f, 4), u16le(f, 14)));
        } else if id == b"data" {
            let (tag, channels, rate, bits) = fmt.ok_or(Error::InvalidInput("data chunk before fmt chunk".into()))?;
            if channels == 0 {
                return Err(Error::InvalidInput("zero channels".into()));
            }
            match (tag, bits) {
                (WAVE_FORMAT_PCM, 8 | 16 | 24) => {}
                (WAVE_FORMAT_PCM, 32) => {
                    return Err(Error::Unsupported(
                        "32-bit integer PCM cannot be represented exactly in float32".into(),
                    ))
                }
                (WAVE_FORMAT_IEEE_FLOAT, 64) => {
                    return Err(Error::Unsupported("64-bit float WAV cannot be represented exactly in float32".into()))
                }
                (t, b) => return Err(Error::Unsupported(format!("WAV format tag {t:#x}, {b} bits"))),
            }
            let bps = (bits / 8) as usize;
            let avail = (bytes.len() - body).min(size);
            let n = avail / (bps * channels as usize) * channels as usize;
            let data = &bytes[body..body + n * bps];
            let scale_exp = bits as i32 - 1;
            let samples = data
                .chunks_exact(bps)
                .map(|c| {
                    let v: i32 = match bps {
                        1 => c[0] as i32 - 128,
                        2 => i16::from_le_bytes([c[0], c[1]]) as i32,
                        _ => (i32::from_le_bytes([0, c[0], c[1], c[2]])) >> 8,
                    };
                    // exact: |v| < 2^24 and the scale is a power of two
                    ((v as f64) / f64::powi(2.0, scale_exp)) as f32
                })
                .map(f32::to_bits)
                .collect();
            return Ok((
                WavData { sample_rate: rate, channels, samples, prefix: Vec::new(), suffix: Vec::new() },
                Source::IntPcm(bits),
            ));
        }
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(Error::InvalidInput("no data chunk found".into()))
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    #[test]
    fn integer_pcm_converts_exactly() {
        let ints = vec![0, 1, -1, 8_388_607, -8_388_608, 12345, -54321, 4_000_000];
        let b = write_int_pcm(44_100, 2, 24, &ints);
        let (w, src) = parse_for_encoding(&b).unwrap();
        assert_eq!(src, Source::IntPcm(24));
        for (&i, &s) in ints.iter().zip(&w.samples) {
            // multiplying back by 2^23 is exact
            assert_eq!((f32::from_bits(s) as f64 * 8_388_608.0) as i32, i);
        }
        let b16 = write_int_pcm(48_000, 1, 16, &[-32768, 32767, 0, 7]);
        let (w, _) = parse_for_encoding(&b16).unwrap();
        assert_eq!(f32::from_bits(w.samples[0]), -1.0);
        let b32 = write_int_pcm(48_000, 1, 32, &[1, 2]);
        assert!(matches!(parse_for_encoding(&b32), Err(Error::Unsupported(_))));
        let f = write_float32(48_000, 2, &[1, 2, 3, 4]);
        assert_eq!(parse_for_encoding(&f).unwrap().1, Source::Float32);
    }
}
