//! The `.nx` container (VFA file format 1.1; reads 1.0). Normative
//! description: SPEC.md. All multi-byte integers are little endian.
//!
//! ```text
//! FileHeader (36 bytes)
//! Block 0: BlockHeader (20 bytes long form / 12 bytes compact form) + payload
//! Block 1 ...
//! Metadata section     count:u32 { tag:[4] len:u32 bytes }*
//! Seek index           legacy: count:u32 { offset:u64 first_frame:u64 }*
//!                      compact: 0xFFFFFFFF LEB128(count) LEB128(first_offset) { LEB128(block_bytes) [LEB128(frames)] }*
//!                      none: count = 0 (decoders rebuild the index by scanning)
//! Trailer (24 bytes)   meta_offset:u64 index_offset:u64 footer_crc:u32 "VFAE"
//! ```

use crate::block_codec::{encode_superblock, BlockStats, EncoderConfig, IndexKind, Stereo};
use crate::parallel::par_map;
use crate::sample_codec::Mode;
use crate::variable_integer::{read_leb128, write_leb128};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::io::Write;

pub const MAGIC: [u8; 4] = *b"VFA\x1A";
pub const VERSION_MAJOR: u8 = 1;
/// Minor version written by this encoder (1.1); 1.0 files are still read.
pub const VERSION_MINOR: u8 = 1;
pub const HEADER_SIZE: usize = 36;
/// Sync of the 20-byte (format 1.0) block header.
pub const BLOCK_SYNC: [u8; 2] = [0xFA, 0xB1];
/// Sync of the 12-byte compact (format 1.1) block header.
pub const BLOCK_SYNC_COMPACT: [u8; 2] = [0xFA, 0xB2];
pub const BLOCK_HEADER_SIZE: usize = 20;
pub const BLOCK_HEADER_SIZE_COMPACT: usize = 12;
pub const TRAILER_MAGIC: [u8; 4] = *b"VFAE";
pub const TRAILER_SIZE: usize = 24;
pub const SAMPLE_FORMAT_F32: u8 = 1;
pub const FLAG_BLOCK_CRC: u8 = 1;
/// Blocks may have any frame count in 1..=block_frames (format 1.1).
pub const FLAG_VARIABLE_BLOCKS: u8 = 2;
/// Block headers use the 12-byte compact form (format 1.1).
pub const FLAG_COMPACT_HEADERS: u8 = 4;
const KNOWN_FLAGS: u8 = FLAG_BLOCK_CRC | FLAG_VARIABLE_BLOCKS | FLAG_COMPACT_HEADERS;
pub const MAX_BLOCK_FRAMES: u32 = 1 << 20;
/// Compact headers store `frames - 1` in 16 bits and the payload length in 24 bits.
pub const MAX_COMPACT_BLOCK_FRAMES: u32 = 1 << 16;
pub const MAX_CHANNELS: u16 = 1024;
const COMPACT_INDEX_MARKER: u32 = 0xFFFF_FFFF;

/// Upper bound for a block payload (any mode is at most raw size plus small
/// headers); used to validate lengths and to decide whether compact headers fit.
pub fn max_payload_bytes(frames: u64, channels: u16) -> u64 {
    frames * channels as u64 * 13 + 64
}

/// Metadata tags.
pub mod tags {
    /// Original WAV bytes preceding the sample data.
    pub const WAV_PREFIX: [u8; 4] = *b"WAVP";
    /// Original WAV bytes following the sample data.
    pub const WAV_SUFFIX: [u8; 4] = *b"WAVS";
    /// CRC-32 of all sample bytes (interleaved, little endian), u32 LE.
    pub const SAMPLE_CRC: [u8; 4] = *b"SCRC";
    /// UTF-8 encoder description.
    pub const ENCODER: [u8; 4] = *b"ENCR";
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub total_frames: u64,
    /// Frames per block (fixed) or the maximum frames per block (variable).
    pub block_frames: u32,
    pub variable_blocks: bool,
    pub compact_headers: bool,
}

impl StreamInfo {
    /// Fixed block size, long headers (the format 1.0 layout).
    pub fn fixed(channels: u16, sample_rate: u32, total_frames: u64, block_frames: u32) -> Self {
        StreamInfo { channels, sample_rate, total_frames, block_frames, variable_blocks: false, compact_headers: false }
    }

    pub fn validate(&self) -> Result<()> {
        if self.channels == 0 || self.channels > MAX_CHANNELS {
            return Err(Error::InvalidInput(format!("channel count {} not in 1..={MAX_CHANNELS}", self.channels)));
        }
        if self.sample_rate == 0 {
            return Err(Error::InvalidInput("sample rate must be > 0".into()));
        }
        if self.block_frames == 0 || self.block_frames > MAX_BLOCK_FRAMES {
            return Err(Error::InvalidInput(format!("block size must be in 1..={MAX_BLOCK_FRAMES}")));
        }
        if self.compact_headers && !compact_headers_fit(self.block_frames, self.channels) {
            return Err(Error::InvalidInput("blocks too large for compact headers".into()));
        }
        Ok(())
    }

    /// Number of blocks for fixed block sizes.
    pub fn fixed_block_count(&self) -> u64 {
        self.total_frames.div_ceil(self.block_frames as u64)
    }

    /// Frame count of block `b` for fixed block sizes.
    pub fn fixed_block_frames(&self, b: u64) -> u64 {
        (self.total_frames - b * self.block_frames as u64).min(self.block_frames as u64)
    }

    pub fn header_len(&self) -> usize {
        if self.compact_headers {
            BLOCK_HEADER_SIZE_COMPACT
        } else {
            BLOCK_HEADER_SIZE
        }
    }

    fn flags(&self) -> u8 {
        FLAG_BLOCK_CRC
            | if self.variable_blocks { FLAG_VARIABLE_BLOCKS } else { 0 }
            | if self.compact_headers { FLAG_COMPACT_HEADERS } else { 0 }
    }

    pub fn to_header_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut h = [0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(&MAGIC);
        h[4] = VERSION_MAJOR;
        h[5] = VERSION_MINOR;
        h[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        h[8] = SAMPLE_FORMAT_F32;
        h[9] = self.flags();
        h[10..12].copy_from_slice(&self.channels.to_le_bytes());
        h[12..16].copy_from_slice(&self.sample_rate.to_le_bytes());
        h[16..24].copy_from_slice(&self.total_frames.to_le_bytes());
        h[24..28].copy_from_slice(&self.block_frames.to_le_bytes());
        let crc = crc32fast::hash(&h[..32]);
        h[32..36].copy_from_slice(&crc.to_le_bytes());
        h
    }

    pub fn from_header_bytes(h: &[u8]) -> Result<Self> {
        if h.len() < HEADER_SIZE {
            return Err(Error::UnexpectedEof);
        }
        if h[0..4] != MAGIC {
            return Err(Error::Corrupt("not a VFA file (bad magic)"));
        }
        if h[4] != VERSION_MAJOR {
            return Err(Error::Unsupported(format!("format version {}.{}", h[4], h[5])));
        }
        let crc = u32::from_le_bytes(h[32..36].try_into().unwrap());
        let actual = crc32fast::hash(&h[..32]);
        if crc != actual {
            return Err(Error::ChecksumMismatch { what: "file header", expected: crc, actual });
        }
        if u16::from_le_bytes([h[6], h[7]]) as usize != HEADER_SIZE {
            return Err(Error::Unsupported("header size".into()));
        }
        if h[8] != SAMPLE_FORMAT_F32 {
            return Err(Error::Unsupported(format!("sample format {}", h[8])));
        }
        let flags = h[9];
        if flags & !KNOWN_FLAGS != 0 || flags & FLAG_BLOCK_CRC == 0 {
            return Err(Error::Unsupported(format!("header flags {flags:#04x}")));
        }
        if h[5] == 0 && flags != FLAG_BLOCK_CRC {
            return Err(Error::Corrupt("format 1.0 file with 1.1 flags"));
        }
        let info = StreamInfo {
            channels: u16::from_le_bytes([h[10], h[11]]),
            sample_rate: u32::from_le_bytes(h[12..16].try_into().unwrap()),
            total_frames: u64::from_le_bytes(h[16..24].try_into().unwrap()),
            block_frames: u32::from_le_bytes(h[24..28].try_into().unwrap()),
            variable_blocks: flags & FLAG_VARIABLE_BLOCKS != 0,
            compact_headers: flags & FLAG_COMPACT_HEADERS != 0,
        };
        info.validate().map_err(|_| Error::Corrupt("invalid stream parameters"))?;
        Ok(info)
    }
}

pub fn compact_headers_fit(block_frames: u32, channels: u16) -> bool {
    block_frames <= MAX_COMPACT_BLOCK_FRAMES && max_payload_bytes(block_frames as u64, channels) < (1 << 24)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    /// Block index (compact headers only carry its low 8 bits).
    pub index: u32,
    pub frames: u32,
    pub payload_len: u32,
    pub payload_crc: u32,
}

impl BlockHeader {
    /// Long form (format 1.0): sync FA B1, flags, reserved, index:u32,
    /// frames:u32, payload_len:u32, crc:u32.
    pub fn to_bytes(&self) -> [u8; BLOCK_HEADER_SIZE] {
        let mut b = [0u8; BLOCK_HEADER_SIZE];
        b[0..2].copy_from_slice(&BLOCK_SYNC);
        b[4..8].copy_from_slice(&self.index.to_le_bytes());
        b[8..12].copy_from_slice(&self.frames.to_le_bytes());
        b[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        b[16..20].copy_from_slice(&self.payload_crc.to_le_bytes());
        b
    }

    /// Compact form (format 1.1): sync FA B2, index_lo:u8, (frames-1):u16,
    /// payload_len:u24, crc:u32.
    pub fn to_compact_bytes(&self) -> [u8; BLOCK_HEADER_SIZE_COMPACT] {
        debug_assert!(self.frames >= 1 && self.frames <= MAX_COMPACT_BLOCK_FRAMES && self.payload_len < 1 << 24);
        let mut b = [0u8; BLOCK_HEADER_SIZE_COMPACT];
        b[0..2].copy_from_slice(&BLOCK_SYNC_COMPACT);
        b[2] = self.index as u8;
        b[3..5].copy_from_slice(&((self.frames - 1) as u16).to_le_bytes());
        b[5..8].copy_from_slice(&self.payload_len.to_le_bytes()[..3]);
        b[8..12].copy_from_slice(&self.payload_crc.to_le_bytes());
        b
    }

    pub fn write_to(&self, compact: bool, out: &mut Vec<u8>) {
        if compact {
            out.extend_from_slice(&self.to_compact_bytes());
        } else {
            out.extend_from_slice(&self.to_bytes());
        }
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() < BLOCK_HEADER_SIZE {
            return Err(Error::UnexpectedEof);
        }
        if b[0..2] != BLOCK_SYNC {
            return Err(Error::Corrupt("missing block sync"));
        }
        if b[2] != 0 || b[3] != 0 {
            return Err(Error::Unsupported("block flags".into()));
        }
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Ok(BlockHeader { index: u(4), frames: u(8), payload_len: u(12), payload_crc: u(16) })
    }

    pub fn from_compact_bytes(b: &[u8]) -> Result<Self> {
        if b.len() < BLOCK_HEADER_SIZE_COMPACT {
            return Err(Error::UnexpectedEof);
        }
        if b[0..2] != BLOCK_SYNC_COMPACT {
            return Err(Error::Corrupt("missing block sync"));
        }
        Ok(BlockHeader {
            index: b[2] as u32,
            frames: u16::from_le_bytes([b[3], b[4]]) as u32 + 1,
            payload_len: u32::from_le_bytes([b[5], b[6], b[7], 0]),
            payload_crc: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        })
    }

    /// Parse the header form used by `info`; checks the (partial) index.
    pub fn parse(b: &[u8], info: &StreamInfo, expected_index: u64) -> Result<Self> {
        let h = if info.compact_headers { Self::from_compact_bytes(b)? } else { Self::from_bytes(b)? };
        let ok = if info.compact_headers {
            h.index == expected_index as u8 as u32
        } else {
            h.index as u64 == expected_index
        };
        if !ok {
            return Err(Error::Corrupt("block index out of sequence"));
        }
        Ok(h)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekEntry {
    pub offset: u64,
    pub first_frame: u64,
    pub frames: u32,
}

pub type Metadata = Vec<([u8; 4], Vec<u8>)>;

pub fn encode_metadata(meta: &Metadata) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    for (tag, data) in meta {
        out.extend_from_slice(tag);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

pub fn decode_metadata(b: &[u8]) -> Result<Metadata> {
    let rd = |o: usize| -> Result<u32> {
        b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or(Error::UnexpectedEof)
    };
    let count = rd(0)? as usize;
    let mut pos = 4;
    let mut meta = Vec::new();
    for _ in 0..count {
        let tag: [u8; 4] = b.get(pos..pos + 4).ok_or(Error::UnexpectedEof)?.try_into().unwrap();
        let len = rd(pos + 4)? as usize;
        pos += 8;
        let data =
            b.get(pos..pos.checked_add(len).ok_or(Error::Corrupt("metadata length"))?).ok_or(Error::UnexpectedEof)?;
        meta.push((tag, data.to_vec()));
        pos += len;
    }
    if pos != b.len() {
        return Err(Error::Corrupt("trailing bytes in metadata section"));
    }
    Ok(meta)
}

pub fn encode_index(index: &[SeekEntry], kind: IndexKind, info: &StreamInfo, end_of_blocks: u64) -> Vec<u8> {
    match kind {
        IndexKind::None => 0u32.to_le_bytes().to_vec(),
        IndexKind::Legacy => {
            let mut out = Vec::with_capacity(4 + index.len() * 16);
            out.extend_from_slice(&(index.len() as u32).to_le_bytes());
            for e in index {
                out.extend_from_slice(&e.offset.to_le_bytes());
                out.extend_from_slice(&e.first_frame.to_le_bytes());
            }
            out
        }
        IndexKind::Compact => {
            let mut out = COMPACT_INDEX_MARKER.to_le_bytes().to_vec();
            write_leb128(&mut out, index.len() as u64);
            write_leb128(&mut out, index.first().map_or(HEADER_SIZE as u64, |e| e.offset));
            for (i, e) in index.iter().enumerate() {
                let next = index.get(i + 1).map_or(end_of_blocks, |n| n.offset);
                write_leb128(&mut out, next - e.offset);
                if info.variable_blocks {
                    write_leb128(&mut out, e.frames as u64);
                }
            }
            out
        }
    }
}

/// Decode a seek index section (any flavour). An empty result means "no
/// index"; the caller then scans. Entries are validated by the caller.
pub fn decode_index(b: &[u8], info: &StreamInfo) -> Result<Vec<SeekEntry>> {
    let first = u32::from_le_bytes(b.get(0..4).ok_or(Error::UnexpectedEof)?.try_into().unwrap());
    if first != COMPACT_INDEX_MARKER {
        let count = first as usize;
        if b.len() != 4 + count * 16 {
            return Err(Error::Corrupt("seek index size"));
        }
        if info.variable_blocks && count > 0 {
            return Err(Error::Corrupt("legacy index cannot describe variable blocks"));
        }
        return Ok((0..count)
            .map(|i| {
                let o = 4 + i * 16;
                SeekEntry {
                    offset: u64::from_le_bytes(b[o..o + 8].try_into().unwrap()),
                    first_frame: u64::from_le_bytes(b[o + 8..o + 16].try_into().unwrap()),
                    frames: if (i as u64) < info.fixed_block_count() {
                        info.fixed_block_frames(i as u64) as u32
                    } else {
                        0
                    },
                }
            })
            .collect());
    }
    let mut pos = 4;
    let next = |pos: &mut usize| -> Result<u64> {
        let (v, n) = read_leb128(b.get(*pos..).unwrap_or(&[]))?;
        *pos += n;
        Ok(v)
    };
    let count = next(&mut pos)?;
    if count > (b.len() as u64) {
        return Err(Error::Corrupt("seek index count"));
    }
    let mut offset = next(&mut pos)?;
    let mut frame = 0u64;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let size = next(&mut pos)?;
        let frames = if info.variable_blocks {
            next(&mut pos)?
        } else if i < info.fixed_block_count() {
            info.fixed_block_frames(i)
        } else {
            0
        };
        if frames > u32::MAX as u64 {
            return Err(Error::Corrupt("seek index frames"));
        }
        out.push(SeekEntry { offset, first_frame: frame, frames: frames as u32 });
        offset = offset.checked_add(size).ok_or(Error::Corrupt("seek index offset"))?;
        frame = frame.saturating_add(frames);
    }
    if pos != b.len() {
        return Err(Error::Corrupt("trailing bytes in seek index"));
    }
    Ok(out)
}

// ------------------------------------------------------------ encoder

/// Aggregated statistics of an encode run.
#[derive(Clone, Debug, Default)]
pub struct EncodeSummary {
    pub blocks: u64,
    pub file_bytes: u64,
    pub payload_bytes: u64,
    /// (mode) -> (channel sub-frames, bits)
    pub modes: BTreeMap<Mode, (u64, u64)>,
    /// stereo mode name -> blocks
    pub stereo: BTreeMap<&'static str, u64>,
    pub predictors: BTreeMap<String, u64>,
    pub exceptions: u64,
    /// block frame count -> number of blocks
    pub block_sizes: BTreeMap<u32, u64>,
    pub index_bytes: u64,
}

impl EncodeSummary {
    fn add(&mut self, s: &BlockStats) {
        self.blocks += 1;
        if s.channels.len() == 2 {
            *self.stereo.entry(s.stereo.name()).or_default() += 1;
        } else {
            *self.stereo.entry(Stereo::Independent.name()).or_default() += 1;
        }
        for c in &s.channels {
            let e = self.modes.entry(c.mode).or_default();
            e.0 += 1;
            e.1 += c.bits;
            if let Some(p) = &c.predictor {
                *self.predictors.entry(p.clone()).or_default() += 1;
            }
            self.exceptions += c.exceptions as u64;
        }
    }

    /// e.g. "floatsplit 97% / raw 3%" by sub-frame count.
    pub fn mode_share(&self) -> String {
        let total: u64 = self.modes.values().map(|v| v.0).sum();
        let mut v: Vec<_> = self.modes.iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1 .0));
        v.iter()
            .map(|(m, (n, _))| format!("{} {:.0}%", m.name(), 100.0 * *n as f64 / total.max(1) as f64))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Streaming encoder. Samples are given as interleaved raw bit patterns.
/// Super-blocks (`block_frames` frames) are encoded in parallel batches;
/// memory use is bounded by the batch size.
pub struct NxEncoder<W: Write> {
    out: W,
    info: StreamInfo,
    cfg: EncoderConfig,
    threads: usize,
    pending: Vec<u32>,
    next_block: u32,
    frames_in: u64,
    frames_out: u64,
    offset: u64,
    index: Vec<SeekEntry>,
    crc: crc32fast::Hasher,
    pub summary: EncodeSummary,
}

impl<W: Write> NxEncoder<W> {
    /// `info.total_frames` must be the exact number of frames that will be
    /// written. `threads == 0` uses all cores.
    pub fn new(mut out: W, info: StreamInfo, cfg: EncoderConfig, threads: usize) -> Result<Self> {
        info.validate()?;
        if cfg.block_frames != info.block_frames {
            return Err(Error::InvalidInput("config block size differs from stream info".into()));
        }
        if cfg.adaptive_blocks && !info.variable_blocks {
            return Err(Error::InvalidInput("adaptive block sizes need a variable-block stream".into()));
        }
        out.write_all(&info.to_header_bytes())?;
        Ok(NxEncoder {
            out,
            info,
            cfg,
            threads,
            pending: Vec::new(),
            next_block: 0,
            frames_in: 0,
            frames_out: 0,
            offset: HEADER_SIZE as u64,
            index: Vec::new(),
            crc: crc32fast::Hasher::new(),
            summary: EncodeSummary::default(),
        })
    }

    fn batch_frames(&self) -> usize {
        let t = if self.threads == 0 { crate::parallel::default_threads() } else { self.threads };
        self.info.block_frames as usize * (4 * t).max(1)
    }

    pub fn write_interleaved(&mut self, samples: &[u32]) -> Result<()> {
        let ch = self.info.channels as usize;
        if !samples.len().is_multiple_of(ch) {
            return Err(Error::InvalidInput("sample count is not a multiple of the channel count".into()));
        }
        self.frames_in += (samples.len() / ch) as u64;
        if self.frames_in > self.info.total_frames {
            return Err(Error::InvalidInput("more frames than declared".into()));
        }
        for &s in samples {
            self.crc.update(&s.to_le_bytes());
        }
        self.pending.extend_from_slice(samples);
        let batch = self.batch_frames() * ch;
        while self.pending.len() >= batch {
            let rest = self.pending.split_off(batch);
            let chunk = std::mem::replace(&mut self.pending, rest);
            self.encode_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Per-block overhead charged by the adaptive block search: header plus
    /// an estimated compact index entry.
    fn block_overhead(&self) -> usize {
        self.info.header_len()
            + match self.cfg.index {
                IndexKind::Compact => 5,
                IndexKind::Legacy => 16,
                IndexKind::None => 0,
            }
    }

    fn encode_chunk(&mut self, interleaved: &[u32]) -> Result<()> {
        let ch = self.info.channels as usize;
        let frames = interleaved.len() / ch;
        let bf = self.info.block_frames as usize;
        let nsuper = frames.div_ceil(bf);
        let cfg = &self.cfg;
        let overhead = self.block_overhead();
        let encoded = par_map(nsuper, self.threads, |b| {
            let start = b * bf;
            let n = bf.min(frames - start);
            let mut chans = vec![Vec::with_capacity(n); ch];
            for f in start..start + n {
                for (c, chan) in chans.iter_mut().enumerate() {
                    chan.push(interleaved[f * ch + c]);
                }
            }
            encode_superblock(&chans, cfg, overhead)
        });
        let mut buf = Vec::new();
        for blocks in encoded {
            for blk in blocks {
                let hdr = BlockHeader {
                    index: self.next_block,
                    frames: blk.frames as u32,
                    payload_len: blk.payload.len() as u32,
                    payload_crc: crc32fast::hash(&blk.payload),
                };
                self.index.push(SeekEntry {
                    offset: self.offset,
                    first_frame: self.frames_out,
                    frames: blk.frames as u32,
                });
                buf.clear();
                hdr.write_to(self.info.compact_headers, &mut buf);
                self.out.write_all(&buf)?;
                self.out.write_all(&blk.payload)?;
                self.offset += (buf.len() + blk.payload.len()) as u64;
                self.frames_out += blk.frames as u64;
                self.summary.payload_bytes += blk.payload.len() as u64;
                self.summary.add(&blk.stats);
                *self.summary.block_sizes.entry(blk.frames as u32).or_default() += 1;
                self.next_block += 1;
            }
        }
        Ok(())
    }

    /// Flush remaining samples and write metadata, seek index and trailer.
    pub fn finish(mut self, mut metadata: Metadata) -> Result<(W, EncodeSummary)> {
        if self.frames_in != self.info.total_frames {
            return Err(Error::InvalidInput(format!(
                "declared {} frames but received {}",
                self.info.total_frames, self.frames_in
            )));
        }
        let rest = std::mem::take(&mut self.pending);
        if !rest.is_empty() {
            self.encode_chunk(&rest)?;
        }
        metadata.retain(|(t, _)| *t != tags::SAMPLE_CRC);
        metadata.push((tags::SAMPLE_CRC, self.crc.clone().finalize().to_le_bytes().to_vec()));
        let meta = encode_metadata(&metadata);
        let index = encode_index(&self.index, self.cfg.index, &self.info, self.offset);
        let meta_offset = self.offset;
        let index_offset = meta_offset + meta.len() as u64;
        let mut h = crc32fast::Hasher::new();
        h.update(&meta);
        h.update(&index);
        let mut trailer = [0u8; TRAILER_SIZE];
        trailer[0..8].copy_from_slice(&meta_offset.to_le_bytes());
        trailer[8..16].copy_from_slice(&index_offset.to_le_bytes());
        trailer[16..20].copy_from_slice(&h.finalize().to_le_bytes());
        trailer[20..24].copy_from_slice(&TRAILER_MAGIC);
        self.out.write_all(&meta)?;
        self.out.write_all(&index)?;
        self.out.write_all(&trailer)?;
        self.out.flush()?;
        self.summary.index_bytes = index.len() as u64;
        self.summary.file_bytes = index_offset + index.len() as u64 + TRAILER_SIZE as u64;
        Ok((self.out, self.summary))
    }
}

/// Stream parameters implied by an encoder configuration.
pub fn stream_info_for(channels: u16, sample_rate: u32, total_frames: u64, cfg: &EncoderConfig) -> StreamInfo {
    StreamInfo {
        channels,
        sample_rate,
        total_frames,
        block_frames: cfg.block_frames,
        variable_blocks: cfg.adaptive_blocks,
        compact_headers: cfg.compact_headers && compact_headers_fit(cfg.block_frames, channels),
    }
}

/// Encode a whole interleaved signal into memory.
pub fn encode_to_vec(
    channels: u16,
    sample_rate: u32,
    interleaved: &[u32],
    metadata: Metadata,
    cfg: &EncoderConfig,
    threads: usize,
) -> Result<(Vec<u8>, EncodeSummary)> {
    if channels == 0 {
        return Err(Error::InvalidInput("zero channels".into()));
    }
    let info = stream_info_for(channels, sample_rate, (interleaved.len() / channels as usize) as u64, cfg);
    let mut enc = NxEncoder::new(Vec::with_capacity(interleaved.len() * 3), info, cfg.clone(), threads)?;
    enc.write_interleaved(interleaved)?;
    enc.finish(metadata)
}

/// Testing/fuzzing helper: recompute the payload CRC of every block whose
/// header is intact, so that mutations reach the payload decoder.
#[doc(hidden)]
pub fn recompute_block_crcs(d: &mut [u8]) {
    let Ok(info) = StreamInfo::from_header_bytes(d) else { return };
    let hl = info.header_len();
    let mut pos = HEADER_SIZE;
    let mut b = 0u64;
    while let Ok(mut h) = BlockHeader::parse(d.get(pos..).unwrap_or(&[]), &info, b) {
        let start = pos + hl;
        let Some(end) = start.checked_add(h.payload_len as usize).filter(|&e| e <= d.len()) else { break };
        h.payload_crc = crc32fast::hash(&d[start..end]);
        let mut hb = Vec::new();
        h.index = b as u32;
        h.write_to(info.compact_headers, &mut hb);
        d[pos..start].copy_from_slice(&hb);
        pos = end;
        b += 1;
    }
}
