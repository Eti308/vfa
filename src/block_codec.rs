//! Block payload coding: adaptive per-channel mode selection and stereo
//! decorrelation.
//!
//! Payload bit layout (SPEC.md §6):
//! ```text
//! [channels == 2] stereo_mode:2     0 independent, 1 left/side, 2 side/right, 3 mid/side
//! for each channel:  mode:3  phase-1 data
//! for each FloatSplit channel: phase-2 extras
//! zero padding to a byte boundary
//! ```
//! Phase-2 data is written after all phase-1 data so that stereo-coupled
//! coarse streams can be reconstructed before the per-channel extras (whose
//! lengths depend on the reconstructed coarse values) are parsed.

use crate::bitio::{BitReader, BitWriter};
use crate::multichannel as mc;
use crate::predictor::{self, Plan, PredictorSearch};
use crate::sample_codec::{self as sc, Mode, SplitChannel, MODE_BITS};
use crate::variable_integer::best_rice_param;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stereo {
    Independent = 0,
    LeftSide = 1,
    SideRight = 2,
    MidSide = 3,
}

impl Stereo {
    pub fn name(self) -> &'static str {
        match self {
            Stereo::Independent => "independent",
            Stereo::LeftSide => "left-side",
            Stereo::SideRight => "side-right",
            Stereo::MidSide => "mid-side",
        }
    }
}

/// Encoder configuration.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// 0 (fastest) ..= 8 (smallest)
    pub level: u8,
    /// Frames per block.
    pub block_frames: u32,
    /// Allowed sub-frame modes. `Raw` is always allowed as a fallback.
    pub modes: Vec<Mode>,
    pub stereo: bool,
    /// Force the XorChimp window sizes (`log2` values); `None` = automatic.
    pub xor_windows: Option<Vec<u32>>,
    /// Adaptive block sizes: `block_frames` is then the maximum (super-block)
    /// size, split recursively down to `min_block_frames`.
    pub adaptive_blocks: bool,
    pub min_block_frames: u32,
    /// Encoder level used to *evaluate* block splits (cheaper than `level`).
    pub block_search_level: u8,
    /// 12-byte block headers (format 1.1) instead of 20-byte ones.
    pub compact_headers: bool,
    /// Seek index flavour written to the footer.
    pub index: IndexKind,
    /// Residual entropy coders evaluated per coarse stream (Rice always).
    pub entropy: Vec<crate::entropy::Coder>,
}

/// Seek index written by the encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    /// Format 1.0: 16 bytes per block.
    Legacy,
    /// Format 1.1: LEB128 block sizes (+ frame counts), ~3-5 bytes per block.
    Compact,
    /// No index; decoders rebuild it by scanning block headers.
    None,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            level: 5,
            block_frames: 8192,
            modes: Mode::ALL.to_vec(),
            stereo: true,
            xor_windows: None,
            adaptive_blocks: true,
            min_block_frames: 256,
            block_search_level: 1,
            compact_headers: true,
            index: IndexKind::Compact,
            // Measured (RESEARCH.md stage 4): Huffman/rANS/range gain only
            // ~0.01 % of the file size at +30 % encode / +19 % decode time.
            entropy: vec![crate::entropy::Coder::Rice],
        }
    }
}

impl EncoderConfig {
    /// Preset for a compression level: levels 0–2 use fixed 2048-frame blocks
    /// (fast); levels 3–8 adaptive blocks of 256–8192 frames.
    pub fn preset(level: u8) -> Self {
        let level = level.min(8);
        let adaptive = level >= 3;
        EncoderConfig {
            level,
            adaptive_blocks: adaptive,
            block_frames: if adaptive { 8192 } else { 2048 },
            block_search_level: if level >= 7 { 2 } else { 1 },
            ..Default::default()
        }
    }

    /// Exactly the tool set of format 1.0 (the previous implementation), for
    /// stage-by-stage comparisons.
    pub fn v1_0() -> Self {
        EncoderConfig {
            modes: vec![Mode::Constant, Mode::Raw, Mode::XorLz, Mode::FieldVlc, Mode::FloatSplit],
            block_frames: 2048,
            adaptive_blocks: false,
            compact_headers: false,
            index: IndexKind::Legacy,
            entropy: vec![crate::entropy::Coder::Rice],
            ..Default::default()
        }
    }

    fn allows(&self, m: Mode) -> bool {
        m == Mode::Raw || self.modes.contains(&m)
    }

    fn grid_offsets(&self) -> &'static [i32] {
        match self.level {
            0..=1 => &[0],
            2..=4 => &[-4, 0, 4, 8],
            5..=6 => &[-8, -4, 0, 4, 8, 12],
            _ => &[-12, -8, -4, -2, 0, 2, 4, 8, 12, 16],
        }
    }

    /// Number of grid candidates that get a full predictor search.
    fn grid_full_trials(&self) -> usize {
        match self.level {
            0..=4 => 1,
            5..=6 => 2,
            _ => 3,
        }
    }
}

/// Per-channel statistics of an encoded block.
#[derive(Clone, Debug)]
pub struct ChannelStats {
    pub mode: Mode,
    pub bits: u64,
    pub grid: Option<i32>,
    pub predictor: Option<String>,
    pub exceptions: usize,
}

#[derive(Clone, Debug)]
pub struct BlockStats {
    pub stereo: Stereo,
    pub channels: Vec<ChannelStats>,
}

struct ChannelEnc {
    mode: Mode,
    phase1: BitWriter,
    phase2: BitWriter,
    split: Option<SplitChannel>,
    plan: Option<Plan>,
    predictor: Option<String>,
}

impl ChannelEnc {
    fn bits(&self) -> u64 {
        self.mode.wire_bits() + self.phase1.bit_len() + self.phase2.bit_len()
    }
}

fn simple(mode: Mode, f: impl FnOnce(&mut BitWriter)) -> ChannelEnc {
    let mut w = BitWriter::new();
    f(&mut w);
    ChannelEnc { mode, phase1: w, phase2: BitWriter::new(), split: None, plan: None, predictor: None }
}

/// Cheap estimate of a FloatSplit channel's size used to rank grid candidates.
fn quick_split_cost(split: &SplitChannel) -> u64 {
    quick_stream_cost(&split.coarse) + split.extras_bits + split.header_bits
}

/// Cheap estimate of a coarse stream's size (best of fixed orders 1 and 2,
/// single Rice parameter).
fn quick_stream_cost(c: &[i64]) -> u64 {
    let mut best = u64::MAX;
    for order in 1..=2usize {
        if c.len() <= order {
            break;
        }
        let res: Vec<u64> = (order..c.len())
            .map(|i| {
                let p = if order == 1 { c[i - 1] } else { c[i - 1].wrapping_mul(2).wrapping_sub(c[i - 2]) };
                crate::variable_integer::zigzag(c[i].wrapping_sub(p))
            })
            .collect();
        best = best.min(best_rice_param(&res).1);
    }
    if best == u64::MAX {
        best = 64 * c.len() as u64;
    }
    best
}

fn encode_floatsplit(s: &[u32], cfg: &EncoderConfig, search: &PredictorSearch) -> ChannelEnc {
    let mut cands: Vec<(u64, SplitChannel)> = sc::grid_candidates(s, cfg.grid_offsets())
        .into_iter()
        .map(|g| {
            let split = sc::split_channel(s, g);
            (quick_split_cost(&split), split)
        })
        .collect();
    cands.sort_by_key(|c| c.0);
    cands.truncate(cfg.grid_full_trials());
    let mut tried: Vec<(u64, SplitChannel, Plan)> = cands
        .into_iter()
        .map(|(_, split)| {
            let plan = predictor::plan(&split.coarse, search);
            (plan.bits + split.extras_bits + split.header_bits, split, plan)
        })
        .collect();
    // The size is nearly flat in g, but only the coarse part (not the raw
    // extra bits) can be decorrelated across channels. Among candidates
    // within 0.25 % of the smallest, prefer the finest grid.
    let min = tried.iter().map(|t| t.0).min().expect("at least one grid candidate");
    let pick = tried
        .iter()
        .enumerate()
        .filter(|(_, t)| t.0 * 400 <= min * 401)
        .min_by_key(|(_, t)| (t.1.g, t.0))
        .map(|(i, _)| i)
        .unwrap();
    let (_, split, plan) = tried.swap_remove(pick);
    build_split_channel(s, split, plan, search)
}

fn build_split_channel(s: &[u32], split: SplitChannel, plan: Plan, search: &PredictorSearch) -> ChannelEnc {
    let mut p1 = BitWriter::new();
    sc::write_split_header(&mut p1, &split);
    predictor::write_plan(&mut p1, &split.coarse, &plan, search);
    let mut p2 = BitWriter::new();
    sc::write_split_extras(&mut p2, s, &split);
    ChannelEnc {
        mode: Mode::FloatSplit,
        phase1: p1,
        phase2: p2,
        predictor: Some(plan.predictor.name()),
        split: Some(split),
        plan: Some(plan),
    }
}

/// Best independent encoding of one channel.
fn encode_channel(s: &[u32], cfg: &EncoderConfig, search: &PredictorSearch) -> ChannelEnc {
    if cfg.allows(Mode::Constant) && !s.is_empty() && s.iter().all(|&v| v == s[0]) {
        return simple(Mode::Constant, |w| sc::encode_constant(w, s[0]));
    }
    let raw_bits = 32 * s.len() as u64;
    let mut best: ChannelEnc = simple(Mode::Raw, |w| sc::encode_raw(w, s));
    let consider = |c: ChannelEnc, best: &mut ChannelEnc| {
        if c.bits() < best.bits() {
            *best = c;
        }
    };
    if cfg.allows(Mode::FloatSplit) {
        consider(encode_floatsplit(s, cfg, search), &mut best);
    }
    if cfg.allows(Mode::FieldVlc) && sc::fieldvlc_cost(s) < best.bits().min(raw_bits) {
        consider(simple(Mode::FieldVlc, |w| sc::encode_fieldvlc(w, s)), &mut best);
    }
    if cfg.allows(Mode::XorLz) {
        consider(simple(Mode::XorLz, |w| sc::encode_xorlz(w, s)), &mut best);
    }
    let profile = RepeatProfile::of(s);
    if cfg.allows(Mode::XorChimp) {
        let windows = cfg.xor_windows.clone().unwrap_or_else(|| profile.xor_windows(cfg.level));
        let (log2n, bits) = crate::xor_codec::best_window(s, &windows);
        if bits + (MODE_BITS as u64) < best.bits() {
            consider(simple(Mode::XorChimp, |w| crate::xor_codec::encode(w, s, log2n)), &mut best);
        }
    }
    // RLE needs exact repetitions; skip the LZ parse when there are almost none
    if cfg.allows(Mode::Rle) && profile.worth_rle(s.len()) {
        if let Some(w) = crate::rle_codec::encode(s, cfg.level) {
            consider(ChannelEnc { mode: Mode::Rle, phase1: w, ..simple(Mode::Rle, |_| {}) }, &mut best);
        }
    }
    best
}

/// Cheap repetition statistics of a channel (direct-mapped table of recent
/// values), used to skip modes that cannot pay off.
struct RepeatProfile {
    /// samples equal to their predecessor
    adjacent: usize,
    /// samples equal to a sample 2..=128 positions earlier (approximate)
    windowed: usize,
}

impl RepeatProfile {
    fn of(s: &[u32]) -> Self {
        let bits = (usize::BITS - (2 * s.len()).max(256).leading_zeros()).min(16);
        let mask = (1usize << bits) - 1;
        let mut table = vec![(u32::MAX, usize::MAX); mask + 1];
        let (mut adjacent, mut windowed) = (0, 0);
        for (i, &v) in s.iter().enumerate() {
            if i > 0 && s[i - 1] == v {
                adjacent += 1;
            }
            let h = (v.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as usize & mask;
            let (pv, pi) = table[h];
            if pv == v && pi != usize::MAX && i - pi > 1 && i - pi <= 128 {
                windowed += 1;
            }
            table[h] = (v, i);
        }
        RepeatProfile { adjacent, windowed }
    }

    fn worth_rle(&self, n: usize) -> bool {
        // >= 1 % repeated samples (adjacent or in a window) or any long run
        (self.adjacent + self.windowed) * 100 >= n.max(1)
    }

    fn xor_windows(&self, level: u8) -> Vec<u32> {
        let wide = self.windowed * 20 > 0 && self.windowed * 20 > self.adjacent.max(1);
        match (wide, level) {
            (false, _) => vec![0],
            (true, 0..=2) => vec![0, 7],
            (true, 3..=6) => vec![0, 3, 5, 7],
            (true, _) => (0..=7).collect(),
        }
    }
}

fn mid(l: i64, r: i64) -> i64 {
    (l + r) >> 1
}

/// Try stereo-coupled FloatSplit encodings; returns `(stereo, [ch0, ch1])`
/// when one beats `independent_bits`.
fn try_stereo(
    l: &[u32],
    r: &[u32],
    grids: &[i32],
    known: [&ChannelEnc; 2],
    independent_bits: u64,
    search: &PredictorSearch,
) -> Option<(Stereo, ChannelEnc, ChannelEnc)> {
    let mut best: Option<(u64, Stereo, i32)> = None;
    let mut cache = None;
    // reuse the independent split + plan of a channel when it used grid `g`
    let reuse = |e: &ChannelEnc, s: &[u32], g: i32| -> (SplitChannel, Plan) {
        match (&e.split, &e.plan) {
            (Some(sp), Some(p)) if sp.g == g => (sp.clone(), p.clone()),
            _ => {
                let sp = sc::split_channel(s, g);
                let p = predictor::plan(&sp.coarse, search);
                (sp, p)
            }
        }
    };
    for &g in grids {
        let (sl, pl) = reuse(known[0], l, g);
        let (sr, pr) = reuse(known[1], r, g);
        let side: Vec<i64> = sl.coarse.iter().zip(&sr.coarse).map(|(a, b)| a - b).collect();
        let midv: Vec<i64> = sl.coarse.iter().zip(&sr.coarse).map(|(a, b)| mid(*a, *b)).collect();
        // rank with cheap estimates; full LPC search only where it can win
        let (ql, qr, qs, qm) = (
            quick_stream_cost(&sl.coarse),
            quick_stream_cost(&sr.coarse),
            quick_stream_cost(&side),
            quick_stream_cost(&midv),
        );
        if qs + ql.min(qr).min(qm) >= ql + qr {
            continue;
        }
        let ps = predictor::plan(&side, search);
        let pm =
            if qm < ql.min(qr) { predictor::plan(&midv, search) } else { Plan { bits: u64::MAX / 4, ..ps.clone() } };
        let fixed = 2 * MODE_BITS as u64 + sl.extras_bits + sl.header_bits + sr.extras_bits + sr.header_bits;
        for (mode, bits) in [
            (Stereo::LeftSide, pl.bits + ps.bits),
            (Stereo::SideRight, ps.bits + pr.bits),
            (Stereo::MidSide, pm.bits + ps.bits),
        ] {
            let total = fixed + bits;
            if total < independent_bits && best.is_none_or(|b| total < b.0) {
                best = Some((total, mode, g));
                cache = Some((
                    sl.clone(),
                    sr.clone(),
                    side.clone(),
                    midv.clone(),
                    [pl.clone(), pr.clone(), ps.clone(), pm.clone()],
                ));
            }
        }
    }
    let (_, mode, _) = best?;
    let (sl, sr, side, midv, [pl, pr, ps, pm]) = cache?;
    let (sa, pa, sb, pb): (&[i64], &Plan, &[i64], &Plan) = match mode {
        Stereo::LeftSide => (&sl.coarse, &pl, &side, &ps),
        Stereo::SideRight => (&side, &ps, &sr.coarse, &pr),
        Stereo::MidSide => (&midv, &pm, &side, &ps),
        Stereo::Independent => unreachable!(),
    };
    let make = |samples: &[u32], split: &SplitChannel, stream: &[i64], plan: &Plan| {
        let mut p1 = BitWriter::new();
        sc::write_split_header(&mut p1, split);
        predictor::write_plan(&mut p1, stream, plan, search);
        let mut p2 = BitWriter::new();
        sc::write_split_extras(&mut p2, samples, split);
        ChannelEnc {
            mode: Mode::FloatSplit,
            phase1: p1,
            phase2: p2,
            predictor: Some(plan.predictor.name()),
            split: Some(split.clone()),
            plan: None,
        }
    };
    let a = make(l, &sl, sa, pa);
    let b = make(r, &sr, sb, pb);
    Some((mode, a, b))
}

/// Encode one block. `channels[c]` holds the raw bit patterns of channel `c`
/// (all the same length, at least 1).
pub fn encode_block(channels: &[Vec<u32>], cfg: &EncoderConfig) -> (Vec<u8>, BlockStats) {
    let mut search = PredictorSearch::for_level(cfg.level);
    search.coders = cfg.entropy.clone();
    let mut encs: Vec<ChannelEnc> = channels.iter().map(|s| encode_channel(s, cfg, &search)).collect();
    if cfg.allows(Mode::FloatSplitRef) || cfg.allows(Mode::XorRef) {
        for k in 1..channels.len() {
            if encs[k].mode == Mode::Constant {
                continue;
            }
            if let Some(e) = try_interchannel(k, channels, &encs, cfg, &search) {
                if e.bits() < encs[k].bits() {
                    encs[k] = e;
                }
            }
        }
    }
    let mut stereo = Stereo::Independent;
    if channels.len() == 2 && cfg.stereo && cfg.allows(Mode::FloatSplit) && cfg.level >= 1 {
        let both_coded = encs.iter().all(|e| e.mode != Mode::Constant);
        if both_coded {
            let mut grids: Vec<i32> = encs.iter().filter_map(|e| e.split.as_ref().map(|s| s.g)).collect();
            if grids.is_empty() {
                grids = sc::grid_candidates(&channels[0], &[0]);
            }
            // The total size is nearly flat in g (bits move between coarse
            // and extras), so a single shared grid suffices at normal levels:
            // the coarser one, which keeps both channels' extras valid.
            grids.sort_unstable();
            grids.dedup();
            if cfg.level < 7 {
                grids = vec![*grids.last().unwrap()];
            }
            let indep = encs[0].bits() + encs[1].bits();
            if let Some((mode, a, b)) =
                try_stereo(&channels[0], &channels[1], &grids, [&encs[0], &encs[1]], indep, &search)
            {
                stereo = mode;
                encs = vec![a, b];
            }
        }
    }
    let mut w = BitWriter::with_capacity(channels.iter().map(|c| c.len() * 4).sum());
    if channels.len() == 2 {
        w.write_bits(stereo as u64, 2);
    }
    for e in &encs {
        e.mode.write_id(&mut w);
        w.append(&e.phase1);
    }
    for e in &encs {
        w.append(&e.phase2);
    }
    let stats = BlockStats {
        stereo,
        channels: encs
            .iter()
            .map(|e| ChannelStats {
                mode: e.mode,
                bits: e.bits(),
                grid: e.split.as_ref().map(|s| s.g),
                predictor: e.predictor.clone(),
                exceptions: e.split.as_ref().map_or(0, |s| s.exceptions.len()),
            })
            .collect(),
    };
    (w.into_bytes(), stats)
}

/// Reusable decoder buffers.
#[derive(Default)]
pub struct DecodeScratch {
    coarse: Vec<Vec<i64>>,
    residuals: Vec<u64>,
}

/// Decode a block payload into `out[c][..frames]`.
pub fn decode_block(payload: &[u8], frames: usize, out: &mut [Vec<u32>], scratch: &mut DecodeScratch) -> Result<()> {
    let nch = out.len();
    let mut r = BitReader::new(payload);
    let stereo = if nch == 2 {
        match r.read_bits(2)? {
            0 => Stereo::Independent,
            1 => Stereo::LeftSide,
            2 => Stereo::SideRight,
            _ => Stereo::MidSide,
        }
    } else {
        Stereo::Independent
    };
    for o in out.iter_mut() {
        o.clear();
        o.resize(frames, 0);
    }
    scratch.coarse.resize_with(nch, Vec::new);
    let mut headers: Vec<Option<sc::SplitHeader>> = Vec::with_capacity(nch);
    let mut modes: Vec<Mode> = Vec::with_capacity(nch);
    let mut xor_refs: Vec<(usize, usize)> = Vec::new();
    #[allow(clippy::needless_range_loop)]
    for ch in 0..nch {
        let mode = Mode::read_id(&mut r)?;
        let o = &mut out[ch];
        let mut header = None;
        match mode {
            Mode::FloatSplitRef => {
                if stereo != Stereo::Independent {
                    return Err(Error::Corrupt("inter-channel reference inside a stereo-coupled block"));
                }
                let refs = mc::Refs::read(&mut r, ch)?;
                let h = sc::read_split_header(&mut r, frames)?;
                for &(j, _) in &refs.0 {
                    if !headers[j].as_ref().is_some_and(|hj: &sc::SplitHeader| hj.g == h.g) {
                        return Err(Error::Corrupt("inter-channel reference to a channel with another grid"));
                    }
                }
                let c = &mut scratch.coarse[ch];
                c.clear();
                c.resize(frames, 0);
                predictor::decode_stream(&mut r, c, &mut scratch.residuals)?;
                mc::reconstruct(&refs, ch, &mut scratch.coarse);
                header = Some(h);
            }
            Mode::XorRef => {
                let d = crate::variable_integer::read_gamma(&mut r)?;
                if d == 0 || d > ch as u64 {
                    return Err(Error::Corrupt("xor reference out of range"));
                }
                let inner = Mode::from_id(r.read_bits(MODE_BITS)?)?;
                decode_simple(inner, &mut r, o)?;
                xor_refs.push((ch, ch - d as usize));
            }
            Mode::Constant => sc::decode_constant(&mut r, o)?,
            Mode::Raw => sc::decode_raw(&mut r, o)?,
            Mode::XorLz => sc::decode_xorlz(&mut r, o)?,
            Mode::FieldVlc => sc::decode_fieldvlc(&mut r, o)?,
            Mode::XorChimp => crate::xor_codec::decode(&mut r, o)?,
            Mode::Rle => crate::rle_codec::decode(&mut r, o)?,
            Mode::FloatSplit => {
                header = Some(sc::read_split_header(&mut r, frames)?);
                let c = &mut scratch.coarse[ch];
                c.clear();
                c.resize(frames, 0);
                predictor::decode_stream(&mut r, c, &mut scratch.residuals)?;
            }
        }
        headers.push(header);
        modes.push(mode);
    }
    if stereo != Stereo::Independent {
        match (&headers[0], &headers[1]) {
            (Some(a), Some(b)) if a.g == b.g && modes[0] == Mode::FloatSplit && modes[1] == Mode::FloatSplit => {}
            _ => return Err(Error::Corrupt("stereo coupling requires two FloatSplit channels with equal grid")),
        }
        let (c0, c1) = scratch.coarse.split_at_mut(1);
        let (a, b) = (&mut c0[0], &mut c1[0]);
        for i in 0..frames {
            let (x, y) = (a[i], b[i]);
            let (l, rr) = match stereo {
                Stereo::LeftSide => (x, x.wrapping_sub(y)),
                Stereo::SideRight => (x.wrapping_add(y), y),
                Stereo::MidSide => {
                    let sum = (x.wrapping_shl(1)) | (y & 1);
                    (sum.wrapping_add(y) >> 1, sum.wrapping_sub(y) >> 1)
                }
                Stereo::Independent => unreachable!(),
            };
            a[i] = l;
            b[i] = rr;
        }
    }
    for ch in 0..nch {
        if let Some(h) = &headers[ch] {
            sc::read_split_extras(&mut r, &scratch.coarse[ch], h, &mut out[ch])?;
        }
    }
    // phase 3: XOR references, in channel order (a reference is always final)
    for (k, j) in xor_refs {
        let (a, b) = out.split_at_mut(k);
        for (x, &y) in b[0].iter_mut().zip(a[j].iter()) {
            *x ^= y;
        }
    }
    Ok(())
}

/// Decode one of the simple (self-contained, single-phase) modes.
fn decode_simple(mode: Mode, r: &mut BitReader, o: &mut [u32]) -> Result<()> {
    match mode {
        Mode::Constant => sc::decode_constant(r, o),
        Mode::Raw => sc::decode_raw(r, o),
        Mode::XorLz => sc::decode_xorlz(r, o),
        Mode::FieldVlc => sc::decode_fieldvlc(r, o),
        Mode::XorChimp => crate::xor_codec::decode(r, o),
        Mode::Rle => crate::rle_codec::decode(r, o),
        _ => Err(Error::Corrupt("mode not allowed here")),
    }
}

// ================================================================ inter-channel search

/// Cheapest simple-mode encoding of `x` (used for XorRef payloads).
fn best_simple(x: &[u32], cfg: &EncoderConfig) -> (Mode, BitWriter) {
    if !x.is_empty() && x.iter().all(|&v| v == x[0]) {
        let mut w = BitWriter::new();
        sc::encode_constant(&mut w, x[0]);
        return (Mode::Constant, w);
    }
    let mut best = (Mode::Raw, {
        let mut w = BitWriter::new();
        sc::encode_raw(&mut w, x);
        w
    });
    if sc::fieldvlc_cost(x) < best.1.bit_len() {
        let mut w = BitWriter::new();
        sc::encode_fieldvlc(&mut w, x);
        best = (Mode::FieldVlc, w);
    }
    let (log2n, bits) = crate::xor_codec::best_window(x, &[0]);
    if bits < best.1.bit_len() {
        let mut w = BitWriter::new();
        crate::xor_codec::encode(&mut w, x, log2n);
        best = (Mode::XorChimp, w);
    }
    if let Some(w) = crate::rle_codec::encode(x, cfg.level) {
        if w.bit_len() < best.1.bit_len() {
            best = (Mode::Rle, w);
        }
    }
    best
}

/// Best inter-channel encoding of channel `k` given the already chosen
/// encodings of channels `0..k`, or `None`.
fn try_interchannel(
    k: usize,
    channels: &[Vec<u32>],
    encs: &[ChannelEnc],
    cfg: &EncoderConfig,
    search: &PredictorSearch,
) -> Option<ChannelEnc> {
    let s = &channels[k];
    let lo = k.saturating_sub(mc::MAX_REF_DISTANCE);
    let mut best: Option<ChannelEnc> = None;
    let mut consider = |c: ChannelEnc| {
        if best.as_ref().is_none_or(|b| c.bits() < b.bits()) {
            best = Some(c);
        }
    };
    let current = encs[k].bits();
    if cfg.allows(Mode::XorRef) {
        #[allow(clippy::needless_range_loop)]
        for j in lo..k {
            let x: Vec<u32> = s.iter().zip(&channels[j]).map(|(a, b)| a ^ b).collect();
            // FieldVlc of the XOR is a cheap exact cost; when even that and a
            // cheap repetition check show no chance, skip the full search
            let all_same = x.iter().all(|&v| v == x[0]);
            let promising = all_same || sc::fieldvlc_cost(&x) < current || RepeatProfile::of(&x).adjacent * 4 > x.len();
            if !promising {
                continue;
            }
            let (inner, payload) = best_simple(&x, cfg);
            let mut p1 = BitWriter::new();
            crate::variable_integer::write_gamma(&mut p1, (k - j) as u64);
            p1.write_bits(inner as u64, MODE_BITS);
            p1.append(&payload);
            consider(ChannelEnc { mode: Mode::XorRef, phase1: p1, ..simple(Mode::XorRef, |_| {}) });
        }
    }
    if cfg.allows(Mode::FloatSplitRef) && cfg.allows(Mode::FloatSplit) {
        // coarse values of the reference candidates, as the decoder will see them
        let coarse_of = |j: usize| encs[j].split.as_ref();
        let coarse_all = all_coarse(encs, k);
        let mut splits: Vec<SplitChannel> = Vec::new();
        let mut cands: Vec<(u64, mc::Refs, usize)> = Vec::new(); // (quick cost, refs, split idx)
        for j in lo..k {
            let Some(sj) = coarse_of(j) else { continue };
            let si = match splits.iter().position(|sp| sp.g == sj.g) {
                Some(i) => i,
                None => {
                    splits.push(sc::split_channel(s, sj.g));
                    splits.len() - 1
                }
            };
            let ck = &splits[si].coarse;
            let w = mc::ls_weight(ck, &sj.coarse);
            if w == 0 {
                continue;
            }
            let refs = mc::Refs(vec![(j, w)]);
            let d = mc::residual_stream(&refs, ck, &coarse_all);
            cands.push((quick_stream_cost(&d), refs, si));
        }
        cands.sort_by_key(|c| c.0);
        // a second reference at the same grid
        if cands.len() >= 2 && cands[0].2 == cands[1].2 {
            let (j1, j2) = (cands[0].1 .0[0].0, cands[1].1 .0[0].0);
            let ck = &splits[cands[0].2].coarse;
            let (a, b) = (&coarse_of(j1).unwrap().coarse, &coarse_of(j2).unwrap().coarse);
            if let Some((w1, w2)) = mc::ls_weights2(ck, a, b) {
                let refs = mc::Refs(vec![(j1, w1), (j2, w2)]);
                let d = mc::residual_stream(&refs, ck, &coarse_all);
                let q = quick_stream_cost(&d);
                if q < cands[0].0 {
                    let si = cands[0].2;
                    cands.insert(0, (q, refs, si));
                }
            }
        }
        if let Some((_, refs, si)) = cands.into_iter().next() {
            let split = splits.swap_remove(si);
            let d = mc::residual_stream(&refs, &split.coarse, &coarse_all);
            let plan = predictor::plan(&d, search);
            let mut p1 = BitWriter::new();
            refs.write(&mut p1, k);
            sc::write_split_header(&mut p1, &split);
            predictor::write_plan(&mut p1, &d, &plan, search);
            let mut p2 = BitWriter::new();
            sc::write_split_extras(&mut p2, s, &split);
            consider(ChannelEnc {
                mode: Mode::FloatSplitRef,
                phase1: p1,
                phase2: p2,
                predictor: Some(format!("{}+ref", plan.predictor.name())),
                split: Some(split),
                plan: None,
            });
        }
    }
    best
}

/// Coarse values of channels `0..k` (empty for channels without FloatSplit).
fn all_coarse(encs: &[ChannelEnc], k: usize) -> Vec<Vec<i64>> {
    encs[..k].iter().map(|e| e.split.as_ref().map_or_else(Vec::new, |s| s.coarse.clone())).collect()
}

// ================================================================ adaptive block sizes

/// One encoded block.
pub struct EncodedBlock {
    pub frames: usize,
    pub payload: Vec<u8>,
    pub stats: BlockStats,
}

fn slice_channels(chans: &[Vec<u32>], lo: usize, hi: usize) -> Vec<Vec<u32>> {
    chans.iter().map(|c| c[lo..hi].to_vec()).collect()
}

struct Leaf {
    lo: usize,
    hi: usize,
    enc: (Vec<u8>, BlockStats),
}

/// Recursive binary split search. Cost = payload bytes + `overhead` (block
/// header + index entry) per block, so smaller blocks must pay for their
/// headers.
fn split_search(chans: &[Vec<u32>], lo: usize, hi: usize, cfg: &EncoderConfig, overhead: usize) -> (usize, Vec<Leaf>) {
    let sub = slice_channels(chans, lo, hi);
    let enc = encode_block(&sub, cfg);
    let whole = enc.0.len() + overhead;
    let all_constant = enc.1.channels.iter().all(|c| c.mode == Mode::Constant);
    let min = cfg.min_block_frames.max(1) as usize;
    if !all_constant && hi - lo >= 2 * min {
        let mid = lo + (hi - lo) / 2;
        let (lc, mut ll) = split_search(chans, lo, mid, cfg, overhead);
        if lc < whole {
            let (rc, rl) = split_search(chans, mid, hi, cfg, overhead);
            if lc + rc < whole {
                ll.extend(rl);
                return (lc + rc, ll);
            }
        }
    }
    (whole, vec![Leaf { lo, hi, enc }])
}

/// Encode one super-block (`chans[c].len()` frames). With adaptive block sizes
/// the super-block is split where that makes the complete representation
/// (payloads + per-block overhead) smaller; otherwise it is one block.
pub fn encode_superblock(chans: &[Vec<u32>], cfg: &EncoderConfig, overhead: usize) -> Vec<EncodedBlock> {
    let n = chans.first().map_or(0, |c| c.len());
    if !cfg.adaptive_blocks || n < 2 * cfg.min_block_frames.max(1) as usize {
        let (payload, stats) = encode_block(chans, cfg);
        return vec![EncodedBlock { frames: n, payload, stats }];
    }
    // Split decisions use a cheaper encoder: lower level and no inter-channel
    // or stereo trials (those shrink parent and child blocks alike). The final
    // encode of the chosen blocks uses the full configuration.
    let search_cfg = EncoderConfig {
        level: cfg.block_search_level.min(cfg.level),
        stereo: false,
        modes: cfg
            .modes
            .iter()
            .copied()
            .filter(|m| !matches!(m, Mode::FloatSplitRef | Mode::XorRef | Mode::XorLz))
            .collect(),
        ..cfg.clone()
    };
    let same = search_cfg.level == cfg.level && search_cfg.stereo == cfg.stereo && search_cfg.modes == cfg.modes;
    let (_, leaves) = split_search(chans, 0, n, &search_cfg, overhead);
    if leaves.len() == 1 && !same {
        let (payload, stats) = encode_block(chans, cfg);
        return vec![EncodedBlock { frames: n, payload, stats }];
    }
    leaves
        .into_iter()
        .map(|l| {
            let (payload, stats) = if same { l.enc } else { encode_block(&slice_channels(chans, l.lo, l.hi), cfg) };
            EncodedBlock { frames: l.hi - l.lo, payload, stats }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(channels: Vec<Vec<u32>>, cfg: &EncoderConfig) -> BlockStats {
        let (payload, stats) = encode_block(&channels, cfg);
        let mut out = vec![Vec::new(); channels.len()];
        decode_block(&payload, channels[0].len(), &mut out, &mut DecodeScratch::default()).unwrap();
        assert_eq!(out, channels);
        let total: u64 = stats.channels.iter().map(|c| c.bits).sum::<u64>() + if channels.len() == 2 { 2 } else { 0 };
        assert_eq!(payload.len() as u64, total.div_ceil(8), "stats must match payload size");
        stats
    }

    fn sine(n: usize, f: f64, amp: f64, phase: f64) -> Vec<u32> {
        (0..n).map(|i| (((i as f64 * f + phase).sin() * amp) as f32).to_bits()).collect()
    }

    #[test]
    fn stereo_sine_all_levels() {
        for level in [0u8, 3, 5, 8] {
            let cfg = EncoderConfig { level, ..Default::default() };
            let l = sine(4096, 0.031, 0.5, 0.0);
            let r = sine(4096, 0.031, 0.49, 0.01);
            let st = check(vec![l, r], &cfg);
            assert!(st.channels.iter().all(|c| matches!(c.mode, Mode::FloatSplit | Mode::FloatSplitRef)), "{st:?}");
        }
    }

    #[test]
    fn mid_side_inverse_extremes() {
        let big = (1i64 << 59) - 1;
        for (l, r) in [(big, -big), (-big, big), (big, big), (-big, -big), (1, 0), (0, -1), (-3, 4)] {
            let m = mid(l, r);
            let s = l - r;
            let sum = (m << 1) | (s & 1);
            assert_eq!(((sum + s) >> 1, (sum - s) >> 1), (l, r));
        }
    }

    #[test]
    fn specials_and_noise_fall_back() {
        let mut x = 0xDEADBEEFu32;
        let noise: Vec<u32> = (0..1000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            })
            .collect();
        let st = check(vec![noise.clone()], &EncoderConfig::default());
        assert_eq!(st.channels[0].mode, Mode::Raw);
        check(vec![noise.clone(), noise], &EncoderConfig::default());
        let specials = vec![0x8000_0000, 0, 0x7FC0_1234, 0xFF80_0000, 1, 0x8000_0001, 0x7F7F_FFFF];
        check(vec![specials.clone(), specials.iter().rev().cloned().collect()], &EncoderConfig::default());
        check(vec![vec![0x8000_0000; 64]; 3], &EncoderConfig::default());
    }

    #[test]
    fn forced_modes_roundtrip() {
        let l = sine(777, 0.2, 1e10, 0.0);
        for m in Mode::ALL {
            let cfg = EncoderConfig { modes: vec![m], ..Default::default() };
            let st = check(vec![l.clone()], &cfg);
            assert!(st.channels[0].mode == m || st.channels[0].mode == Mode::Raw);
        }
    }
}
