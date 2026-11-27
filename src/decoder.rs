//! Bit-exact decoding of `.nx` files: whole-file (parallel), random access by
//! frame range or time, block iteration, and a streaming decoder over
//! `io::Read`. Reads format 1.0 and 1.1.
//!
//! The decoder treats its input as untrusted: every length and index is
//! validated, every block payload is CRC checked, and all failures are
//! reported as [`Error`] values — never panics.

use crate::block_codec::{decode_block, DecodeScratch};
use crate::container::{
    decode_index, decode_metadata, max_payload_bytes, tags, BlockHeader, Metadata, SeekEntry, StreamInfo, HEADER_SIZE,
    TRAILER_MAGIC, TRAILER_SIZE,
};
use crate::{Error, Result};
use std::io::Read;

/// How the block index was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexSource {
    /// Read from the footer's seek index.
    Footer,
    /// The file has no index; it was rebuilt by scanning block headers.
    Scanned,
    /// Footer missing or damaged: index rebuilt by scanning, metadata lost.
    Recovered,
}

/// A parsed `.nx` file held in memory.
pub struct NxFile<'a> {
    data: &'a [u8],
    pub info: StreamInfo,
    pub metadata: Metadata,
    pub index: Vec<SeekEntry>,
    pub index_source: IndexSource,
}

impl<'a> NxFile<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self> {
        let info = StreamInfo::from_header_bytes(data)?;
        if info.total_frames > data.len() as u64 * 8 * info.block_frames as u64 {
            return Err(Error::Corrupt("declared frame count impossible for file size"));
        }
        match Self::read_footer(data, &info) {
            Ok((metadata, meta_off, index)) => {
                if !index.is_empty() && Self::validate_index(&index, &info, meta_off).is_ok() {
                    return Ok(NxFile { data, info, metadata, index, index_source: IndexSource::Footer });
                }
                let index = Self::scan(data, &info, meta_off as usize)?;
                Ok(NxFile { data, info, metadata, index, index_source: IndexSource::Scanned })
            }
            Err(_) => {
                let index = Self::scan(data, &info, data.len())?;
                Ok(NxFile { data, info, metadata: Vec::new(), index, index_source: IndexSource::Recovered })
            }
        }
    }

    /// True when the footer was unusable (kept for API compatibility).
    pub fn index_rebuilt(&self) -> bool {
        self.index_source != IndexSource::Footer
    }

    fn read_footer(data: &[u8], info: &StreamInfo) -> Result<(Metadata, u64, Vec<SeekEntry>)> {
        if data.len() < HEADER_SIZE + TRAILER_SIZE {
            return Err(Error::UnexpectedEof);
        }
        let t = &data[data.len() - TRAILER_SIZE..];
        if t[20..24] != TRAILER_MAGIC {
            return Err(Error::Corrupt("missing trailer"));
        }
        let meta_off = u64::from_le_bytes(t[0..8].try_into().unwrap());
        let index_off = u64::from_le_bytes(t[8..16].try_into().unwrap());
        let crc = u32::from_le_bytes(t[16..20].try_into().unwrap());
        let end = (data.len() - TRAILER_SIZE) as u64;
        if !(HEADER_SIZE as u64 <= meta_off && meta_off <= index_off && index_off <= end) {
            return Err(Error::Corrupt("trailer offsets"));
        }
        let footer = &data[meta_off as usize..end as usize];
        let actual = crc32fast::hash(footer);
        if actual != crc {
            return Err(Error::ChecksumMismatch { what: "footer", expected: crc, actual });
        }
        let metadata = decode_metadata(&data[meta_off as usize..index_off as usize])?;
        let index = decode_index(&data[index_off as usize..end as usize], info)?;
        Ok((metadata, meta_off, index))
    }

    fn validate_index(index: &[SeekEntry], info: &StreamInfo, meta_off: u64) -> Result<()> {
        let mut frame = 0u64;
        let mut last_off = 0u64;
        for (i, e) in index.iter().enumerate() {
            let expected = if info.variable_blocks { None } else { Some(info.fixed_block_frames(i as u64)) };
            let frames_ok = e.frames >= 1
                && e.frames <= info.block_frames
                && expected.is_none_or(|x| x == e.frames as u64)
                && frame < info.total_frames;
            if !frames_ok || e.first_frame != frame || e.offset < last_off.max(HEADER_SIZE as u64) {
                return Err(Error::Corrupt("seek index entry"));
            }
            if e.offset + info.header_len() as u64 > meta_off {
                return Err(Error::Corrupt("seek index offset"));
            }
            frame += e.frames as u64;
            last_off = e.offset + 1;
        }
        if frame != info.total_frames {
            return Err(Error::Corrupt("seek index frame total"));
        }
        Ok(())
    }

    /// Rebuild the block index by walking block headers from the start.
    fn scan(data: &[u8], info: &StreamInfo, end: usize) -> Result<Vec<SeekEntry>> {
        let mut index = Vec::new();
        let mut pos = HEADER_SIZE;
        let mut frame = 0u64;
        let hl = info.header_len();
        while frame < info.total_frames {
            let b = index.len() as u64;
            let h = BlockHeader::parse(data.get(pos..end).ok_or(Error::UnexpectedEof)?, info, b)?;
            let expected = if info.variable_blocks { None } else { Some(info.fixed_block_frames(b)) };
            if h.frames == 0
                || h.frames > info.block_frames
                || expected.is_some_and(|x| x != h.frames as u64)
                || frame + h.frames as u64 > info.total_frames
            {
                return Err(Error::Corrupt("block frame count"));
            }
            index.push(SeekEntry { offset: pos as u64, first_frame: frame, frames: h.frames });
            frame += h.frames as u64;
            pos = pos.checked_add(hl + h.payload_len as usize).filter(|&p| p <= end).ok_or(Error::UnexpectedEof)?;
        }
        Ok(index)
    }

    pub fn meta(&self, tag: [u8; 4]) -> Option<&[u8]> {
        self.metadata.iter().find(|(t, _)| *t == tag).map(|(_, d)| d.as_slice())
    }

    pub fn block_count(&self) -> usize {
        self.index.len()
    }

    /// Frames in block `b`.
    pub fn block_frames(&self, b: usize) -> usize {
        self.index.get(b).map_or(0, |e| e.frames as usize)
    }

    /// Index of the block containing `frame` (binary search over the index).
    pub fn block_of_frame(&self, frame: u64) -> Option<usize> {
        if frame >= self.info.total_frames {
            return None;
        }
        Some(self.index.partition_point(|e| e.first_frame <= frame) - 1)
    }

    /// Validated header and payload of block `b`.
    pub fn block_payload(&self, b: usize) -> Result<(BlockHeader, &'a [u8])> {
        let e = self.index.get(b).ok_or(Error::InvalidInput("block index out of range".into()))?;
        let off = e.offset as usize;
        let h = BlockHeader::parse(self.data.get(off..).ok_or(Error::UnexpectedEof)?, &self.info, b as u64)?;
        if h.frames != e.frames {
            return Err(Error::Corrupt("block frame count mismatch"));
        }
        if h.payload_len as u64 > max_payload_bytes(h.frames as u64, self.info.channels) {
            return Err(Error::Corrupt("block payload too large"));
        }
        let start = off + self.info.header_len();
        let payload = self
            .data
            .get(start..start.checked_add(h.payload_len as usize).ok_or(Error::Corrupt("payload length"))?)
            .ok_or(Error::UnexpectedEof)?;
        let actual = crc32fast::hash(payload);
        if actual != h.payload_crc {
            return Err(Error::ChecksumMismatch { what: "block payload", expected: h.payload_crc, actual });
        }
        Ok((h, payload))
    }

    /// Decode block `b` into per-channel planar buffers.
    pub fn decode_block_planar(&self, b: usize, out: &mut Vec<Vec<u32>>, scratch: &mut DecodeScratch) -> Result<usize> {
        let (h, payload) = self.block_payload(b)?;
        out.resize_with(self.info.channels as usize, Vec::new);
        decode_block(payload, h.frames as usize, out, scratch)?;
        Ok(h.frames as usize)
    }

    /// Decode block `b` and append interleaved samples to `dst`.
    pub fn decode_block_interleaved(
        &self,
        b: usize,
        dst: &mut Vec<u32>,
        planar: &mut Vec<Vec<u32>>,
        scratch: &mut DecodeScratch,
    ) -> Result<()> {
        let frames = self.decode_block_planar(b, planar, scratch)?;
        interleave(planar, frames, dst);
        Ok(())
    }

    /// Decode the whole file (interleaved). `threads == 0` uses all cores.
    /// Blocks are decoded in parallel directly into one output buffer, so
    /// peak memory is the output plus small per-thread scratch buffers.
    pub fn decode_all(&self, threads: usize) -> Result<Vec<u32>> {
        let ch = self.info.channels as usize;
        let mut all = vec![0u32; self.info.total_frames as usize * ch];
        let mut jobs: Vec<(usize, &mut [u32])> = Vec::with_capacity(self.block_count());
        let mut rest = &mut all[..];
        for b in 0..self.block_count() {
            let (dst, r) = std::mem::take(&mut rest).split_at_mut(self.block_frames(b) * ch);
            jobs.push((b, dst));
            rest = r;
        }
        let threads =
            if threads == 0 { crate::parallel::default_threads() } else { threads }.clamp(1, jobs.len().max(1));
        let queue = std::sync::Mutex::new(jobs.into_iter());
        let error: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);
        let work = || {
            let (mut planar, mut scratch) = (Vec::new(), DecodeScratch::default());
            loop {
                let job = queue.lock().unwrap().next();
                let Some((b, dst)) = job else { break };
                let res = self.decode_block_planar(b, &mut planar, &mut scratch).and_then(|frames| {
                    if frames * ch != dst.len() {
                        return Err(Error::Corrupt("block frame count mismatch"));
                    }
                    for (c, p) in planar.iter().enumerate() {
                        for (f, &v) in p[..frames].iter().enumerate() {
                            dst[f * ch + c] = v;
                        }
                    }
                    Ok(())
                });
                if let Err(e) = res {
                    error.lock().unwrap().get_or_insert(e);
                    break;
                }
            }
        };
        if threads <= 1 {
            work();
        } else {
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(work);
                }
            });
        }
        match error.into_inner().unwrap() {
            Some(e) => Err(e),
            None => Ok(all),
        }
    }

    /// Random access: decode `count` frames starting at frame `start`.
    /// Only the blocks overlapping the range are decoded.
    pub fn decode_range(&self, start: u64, count: u64) -> Result<Vec<u32>> {
        let end = start.checked_add(count).filter(|&e| e <= self.info.total_frames);
        let end = end.ok_or_else(|| Error::InvalidInput("frame range beyond end of stream".into()))?;
        let ch = self.info.channels as usize;
        let mut out = Vec::with_capacity(count as usize * ch);
        if count == 0 {
            return Ok(out);
        }
        let first = self.block_of_frame(start).ok_or(Error::Corrupt("frame not covered by index"))?;
        let last = self.block_of_frame(end - 1).ok_or(Error::Corrupt("frame not covered by index"))?;
        let (mut planar, mut scratch, mut tmp) = (Vec::new(), DecodeScratch::default(), Vec::new());
        for b in first..=last {
            tmp.clear();
            self.decode_block_interleaved(b, &mut tmp, &mut planar, &mut scratch)?;
            let e = self.index[b];
            let b0 = e.first_frame;
            let lo = (start.max(b0) - b0) as usize * ch;
            let hi = (end.min(b0 + e.frames as u64) - b0) as usize * ch;
            out.extend_from_slice(&tmp[lo..hi]);
        }
        Ok(out)
    }

    /// Frame position of a time in seconds (rounded down, clamped to the
    /// stream). Only positions use floating point — never sample values.
    pub fn frame_at_time(&self, seconds: f64) -> u64 {
        if seconds.is_nan() || seconds <= 0.0 {
            return 0;
        }
        ((seconds * self.info.sample_rate as f64).floor() as u64).min(self.info.total_frames)
    }

    /// Random access by time: decode `duration` seconds starting at `start` seconds.
    pub fn decode_time_range(&self, start: f64, duration: f64) -> Result<Vec<u32>> {
        let a = self.frame_at_time(start);
        let b = self.frame_at_time(start + duration.max(0.0)).max(a);
        self.decode_range(a, b - a)
    }

    /// Check the stored whole-stream sample CRC against decoded samples.
    pub fn verify_sample_crc(&self, samples: &[u32]) -> Result<()> {
        let Some(stored) = self.meta(tags::SAMPLE_CRC) else { return Ok(()) };
        let stored: [u8; 4] = stored.try_into().map_err(|_| Error::Corrupt("SCRC length"))?;
        let expected = u32::from_le_bytes(stored);
        let actual = sample_crc(samples);
        if expected != actual {
            return Err(Error::ChecksumMismatch { what: "decoded samples", expected, actual });
        }
        Ok(())
    }
}

pub fn sample_crc(samples: &[u32]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    let mut buf = Vec::with_capacity(4096 * 4);
    for chunk in samples.chunks(4096) {
        buf.clear();
        for &s in chunk {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        h.update(&buf);
    }
    h.finalize()
}

fn interleave(planar: &[Vec<u32>], frames: usize, dst: &mut Vec<u32>) {
    let ch = planar.len();
    let base = dst.len();
    dst.resize(base + frames * ch, 0);
    for (c, p) in planar.iter().enumerate() {
        for (f, &v) in p[..frames].iter().enumerate() {
            dst[base + f * ch + c] = v;
        }
    }
}

/// Sequential decoder over any `io::Read` (pipes, sockets): it needs neither
/// seeking nor the footer, so playback can start after the first block.
pub struct StreamDecoder<R: Read> {
    src: R,
    pub info: StreamInfo,
    next_block: u64,
    frames_done: u64,
    planar: Vec<Vec<u32>>,
    scratch: DecodeScratch,
    payload: Vec<u8>,
}

impl<R: Read> StreamDecoder<R> {
    pub fn new(mut src: R) -> Result<Self> {
        let mut h = [0u8; HEADER_SIZE];
        src.read_exact(&mut h).map_err(eof)?;
        let info = StreamInfo::from_header_bytes(&h)?;
        Ok(StreamDecoder {
            src,
            info,
            next_block: 0,
            frames_done: 0,
            planar: Vec::new(),
            scratch: DecodeScratch::default(),
            payload: Vec::new(),
        })
    }

    /// Next block as interleaved samples, or `None` at the end of the stream.
    pub fn next_block(&mut self) -> Result<Option<Vec<u32>>> {
        if self.frames_done >= self.info.total_frames {
            return Ok(None);
        }
        let mut hb = [0u8; 20];
        let hb = &mut hb[..self.info.header_len()];
        self.src.read_exact(hb).map_err(eof)?;
        let h = BlockHeader::parse(hb, &self.info, self.next_block)?;
        let remaining = self.info.total_frames - self.frames_done;
        let ok = if self.info.variable_blocks {
            h.frames >= 1 && h.frames <= self.info.block_frames && h.frames as u64 <= remaining
        } else {
            h.frames as u64 == remaining.min(self.info.block_frames as u64)
        };
        if !ok {
            return Err(Error::Corrupt("unexpected block frame count"));
        }
        if h.payload_len as u64 > max_payload_bytes(h.frames as u64, self.info.channels) {
            return Err(Error::Corrupt("block payload too large"));
        }
        self.payload.resize(h.payload_len as usize, 0);
        self.src.read_exact(&mut self.payload).map_err(eof)?;
        let actual = crc32fast::hash(&self.payload);
        if actual != h.payload_crc {
            return Err(Error::ChecksumMismatch { what: "block payload", expected: h.payload_crc, actual });
        }
        self.planar.resize_with(self.info.channels as usize, Vec::new);
        decode_block(&self.payload, h.frames as usize, &mut self.planar, &mut self.scratch)?;
        let mut out = Vec::new();
        interleave(&self.planar, h.frames as usize, &mut out);
        self.next_block += 1;
        self.frames_done += h.frames as u64;
        Ok(Some(out))
    }
}

fn eof(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::UnexpectedEof
    } else {
        Error::Io(e)
    }
}
