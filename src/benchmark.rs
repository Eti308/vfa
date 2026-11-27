//! Compression and performance comparison.
//!
//! For every material: VFA size / ratio / encode & decode speed / bit-exact
//! check, WavPack (float lossless) and FLAC (only when the material is
//! exactly representable as 16- or 24-bit integers) if their command-line
//! tools are installed. Additional studies: forced single modes (answers
//! "is sample-wise coding enough?"), block size, and compression level.

use crate::block_codec::EncoderConfig;
use crate::container::{encode_to_vec, tags, EncodeSummary};
use crate::decoder::NxFile;
use crate::ieee754::{split_at_grid, Split};
use crate::sample_codec::Mode;
use crate::signals::Material;
use crate::wav;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ExternalResult {
    pub bytes: u64,
    pub enc: Duration,
    pub dec: Duration,
    pub bit_exact: bool,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub material: String,
    pub channels: u16,
    pub rate: u32,
    pub frames: usize,
    pub wav_bytes: u64,
    pub nx_bytes: u64,
    pub enc_1t: Duration,
    pub enc_mt: Duration,
    pub dec_1t: Duration,
    pub dec_2t: Duration,
    pub dec_mt: Duration,
    /// mean time to decode a random 10 ms window (random access)
    pub seek: Duration,
    pub peak_enc: Option<usize>,
    pub peak_dec: Option<usize>,
    pub bit_exact: bool,
    pub wav_byte_exact: bool,
    pub summary: EncodeSummary,
    pub wavpack: Option<ExternalResult>,
    pub flac: Option<(u16, ExternalResult)>,
}

impl Row {
    pub fn ratio(&self) -> f64 {
        self.nx_bytes as f64 / self.wav_bytes as f64
    }
    fn audio_secs(&self) -> f64 {
        self.frames as f64 / self.rate as f64
    }
    pub fn enc_speed(&self) -> f64 {
        self.audio_secs() / self.enc_1t.as_secs_f64()
    }
    pub fn dec_speed(&self) -> f64 {
        self.audio_secs() / self.dec_1t.as_secs_f64()
    }
    pub fn dec_speed_mt(&self) -> f64 {
        self.audio_secs() / self.dec_mt.as_secs_f64()
    }
}

fn best_of<T>(reps: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut out = None;
    for _ in 0..reps.max(1) {
        let t = Instant::now();
        let v = f();
        best = best.min(t.elapsed());
        out = Some(v);
    }
    (best, out.unwrap())
}

pub fn tool_available(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok() || Command::new(name).arg("--help").output().is_ok()
}

/// If every sample is finite and exactly `k / 2^(bits-1)` with
/// `-2^(bits-1) <= k < 2^(bits-1)`, return the integers. Pure bit logic.
pub fn as_integers(samples: &[u32], bits: u32) -> Option<Vec<i32>> {
    let g = -(bits as i32 - 1);
    let lim = 1i64 << (bits - 1);
    samples
        .iter()
        .map(|&v| match split_at_grid(v, g) {
            Split::Zero => Some(0),
            Split::Coarse { c, extra: 0, .. } if c >= -lim && c < lim => Some(c as i32),
            _ => None,
        })
        .collect()
}

fn run_wavpack(dir: &Path, m: &Material, wav_path: &Path) -> Option<ExternalResult> {
    let wv = dir.join(format!("{}.wv", m.name));
    let back = dir.join(format!("{}.wv.wav", m.name));
    let t = Instant::now();
    let ok = Command::new("wavpack")
        .args(["-q", "-y", "-hh", "-x"])
        .arg(wav_path)
        .arg("-o")
        .arg(&wv)
        .status()
        .ok()?
        .success();
    let enc = t.elapsed();
    if !ok {
        return None;
    }
    let t = Instant::now();
    let ok = Command::new("wvunpack").args(["-q", "-y"]).arg(&wv).arg("-o").arg(&back).status().ok()?.success();
    let dec = t.elapsed();
    let bytes = std::fs::metadata(&wv).ok()?.len();
    let bit_exact =
        ok && std::fs::read(&back).ok().and_then(|b| wav::parse(&b).ok()).is_some_and(|w| w.samples == m.samples);
    let _ = std::fs::remove_file(&back);
    Some(ExternalResult { bytes, enc, dec, bit_exact })
}

fn run_flac(dir: &Path, m: &Material) -> Option<(u16, ExternalResult)> {
    let (bits, ints) = [16u16, 24].iter().find_map(|&b| as_integers(&m.samples, b as u32).map(|i| (b, i)))?;
    let src = dir.join(format!("{}.int{bits}.wav", m.name));
    std::fs::write(&src, wav::write_int_pcm(m.sample_rate, m.channels, bits, &ints)).ok()?;
    let fl = dir.join(format!("{}.flac", m.name));
    let back = dir.join(format!("{}.flac.wav", m.name));
    let t = Instant::now();
    let ok = Command::new("flac").args(["-8", "-s", "-f", "-o"]).arg(&fl).arg(&src).status().ok()?.success();
    let enc = t.elapsed();
    if !ok {
        return None;
    }
    let t = Instant::now();
    let ok = Command::new("flac").args(["-d", "-s", "-f", "-o"]).arg(&back).arg(&fl).status().ok()?.success();
    let dec = t.elapsed();
    let bytes = std::fs::metadata(&fl).ok()?.len();
    let bit_exact =
        ok && std::fs::read(&back).ok().and_then(|b| wav::parse_int_pcm(&b).ok()).is_some_and(|(_, v)| v == ints);
    let _ = std::fs::remove_file(&back);
    let _ = std::fs::remove_file(&src);
    Some((bits, ExternalResult { bytes, enc, dec, bit_exact }))
}

pub struct Options {
    pub out_dir: PathBuf,
    pub cfg: EncoderConfig,
    pub reps: usize,
    pub external: bool,
}

pub fn run_material(m: &Material, opts: &Options) -> Row {
    let wav_bytes_v = wav::write_float32(m.sample_rate, m.channels, &m.samples);
    let wav_path = opts.out_dir.join(format!("{}.wav", m.name));
    let _ = std::fs::write(&wav_path, &wav_bytes_v);
    let parsed = wav::parse(&wav_bytes_v).expect("own WAV parses");
    let meta = vec![(tags::WAV_PREFIX, parsed.prefix.clone()), (tags::WAV_SUFFIX, parsed.suffix.clone())];
    let (enc_1t, (nx, summary)) = best_of(opts.reps, || {
        encode_to_vec(m.channels, m.sample_rate, &m.samples, meta.clone(), &opts.cfg, 1).expect("encode")
    });
    let (enc_mt, _) = best_of(opts.reps, || {
        encode_to_vec(m.channels, m.sample_rate, &m.samples, meta.clone(), &opts.cfg, 0).expect("encode")
    });
    let (_, peak_enc) = crate::memory::measure_peak(|| {
        encode_to_vec(m.channels, m.sample_rate, &m.samples, meta.clone(), &opts.cfg, 0).expect("encode")
    });
    let _ = std::fs::write(opts.out_dir.join(format!("{}.nx", m.name)), &nx);
    let file = NxFile::open(&nx).expect("open");
    let (dec_1t, decoded) = best_of(opts.reps, || file.decode_all(1).expect("decode"));
    let (dec_2t, decoded_2t) = best_of(opts.reps, || file.decode_all(2).expect("decode"));
    let (dec_mt, decoded_mt) = best_of(opts.reps, || file.decode_all(0).expect("decode"));
    let (_, peak_dec) = crate::memory::measure_peak(|| file.decode_all(1).expect("decode"));
    // random access: 200 random 10 ms windows, each checked against the source
    let ch = m.channels as usize;
    let win = (m.sample_rate as u64 / 100).max(1).min(m.frames() as u64);
    let mut rng = crate::signals::Rng::new(99);
    let mut seek_ok = true;
    let t = Instant::now();
    for _ in 0..200 {
        let start = rng.next_u64() % (m.frames() as u64 - win + 1);
        let got = file.decode_range(start, win).expect("range");
        seek_ok &= got[..] == m.samples[start as usize * ch..(start + win) as usize * ch];
    }
    let seek = t.elapsed() / 200;
    let bit_exact = decoded == m.samples
        && decoded_2t == m.samples
        && decoded_mt == m.samples
        && seek_ok
        && file.verify_sample_crc(&decoded).is_ok();
    let rebuilt = wav::assemble(
        file.meta(tags::WAV_PREFIX).unwrap_or_default(),
        &decoded,
        file.meta(tags::WAV_SUFFIX).unwrap_or_default(),
    );
    let (wavpack, flac) = if opts.external {
        (
            if tool_available("wavpack") { run_wavpack(&opts.out_dir, m, &wav_path) } else { None },
            if tool_available("flac") { run_flac(&opts.out_dir, m) } else { None },
        )
    } else {
        (None, None)
    };
    Row {
        material: m.name.to_string(),
        channels: m.channels,
        rate: m.sample_rate,
        frames: m.frames(),
        wav_bytes: wav_bytes_v.len() as u64,
        nx_bytes: nx.len() as u64,
        enc_1t,
        enc_mt,
        dec_1t,
        dec_2t,
        dec_mt,
        seek,
        peak_enc,
        peak_dec,
        bit_exact,
        wav_byte_exact: rebuilt == wav_bytes_v,
        summary,
        wavpack,
        flac,
    }
}

fn pct(a: u64, b: u64) -> String {
    format!("{:.1}%", 100.0 * a as f64 / b as f64)
}

pub fn main_table(rows: &[Row]) -> String {
    let mut s = String::new();
    s.push_str("| material | ch / rate | WAV f32 | VFA | VFA % | WavPack % | FLAC % | enc ×RT | dec ×RT (1T / MT) | bit-exact | modes |\n");
    s.push_str("|---|---|---:|---:|---:|---:|---:|---:|---:|:-:|---|\n");
    for r in rows {
        let wvp = r
            .wavpack
            .as_ref()
            .map_or("–".into(), |w| format!("{}{}", pct(w.bytes, r.wav_bytes), if w.bit_exact { "" } else { " ✗" }));
        let fl = r.flac.as_ref().map_or("n/a".into(), |(b, f)| {
            format!("{} ({b}-bit){}", pct(f.bytes, r.wav_bytes), if f.bit_exact { "" } else { " ✗" })
        });
        s.push_str(&format!(
            "| {} | {} / {} | {} | {} | **{}** | {} | {} | {:.0} | {:.0} / {:.0} | {} | {} |\n",
            r.material,
            r.channels,
            r.rate,
            r.wav_bytes,
            r.nx_bytes,
            pct(r.nx_bytes, r.wav_bytes),
            wvp,
            fl,
            r.enc_speed(),
            r.dec_speed(),
            r.dec_speed_mt(),
            if r.bit_exact && r.wav_byte_exact { "✓" } else { "✗" },
            r.summary.mode_share(),
        ));
    }
    let (w, n): (u64, u64) = rows.iter().fold((0, 0), |a, r| (a.0 + r.wav_bytes, a.1 + r.nx_bytes));
    s.push_str(&format!("| **total** | | {w} | {n} | **{}** | | | | | | |\n", pct(n, w)));
    s
}

pub fn timing_table(rows: &[Row]) -> String {
    let mut s = String::from("| material | VFA enc ms (1T) | VFA dec ms (1T) | VFA dec ms (MT) | WavPack enc / dec ms | FLAC enc / dec ms |\n|---|---:|---:|---:|---:|---:|\n");
    let ms = |d: Duration| format!("{:.1}", d.as_secs_f64() * 1e3);
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.material,
            ms(r.enc_1t),
            ms(r.dec_1t),
            ms(r.dec_mt),
            r.wavpack.as_ref().map_or("–".into(), |w| format!("{} / {}", ms(w.enc), ms(w.dec))),
            r.flac.as_ref().map_or("–".into(), |(_, f)| format!("{} / {}", ms(f.enc), ms(f.dec))),
        ));
    }
    s
}

/// Bits per sample (complete file size incl. all headers, index and
/// metadata) for each named configuration; every encode is verified bit
/// exact. Also reports single-thread encode speed of the last column group.
pub fn config_table(materials: &[Material], variants: &[(&str, EncoderConfig)]) -> String {
    let mut s = String::from("| material |");
    for (n, _) in variants {
        s.push_str(&format!(" {n} |"));
    }
    s.push_str("\n|---|");
    s.push_str(&"---:|".repeat(variants.len()));
    s.push('\n');
    let mut totals = vec![(0u64, 0u64); variants.len()];
    let mut times = vec![Duration::ZERO; variants.len()];
    let mut dtimes = vec![Duration::ZERO; variants.len()];
    for m in materials {
        s.push_str(&format!("| {} |", m.name));
        for (k, (_, cfg)) in variants.iter().enumerate() {
            let t = Instant::now();
            let (nx, _) = encode_to_vec(m.channels, m.sample_rate, &m.samples, Vec::new(), cfg, 0).unwrap();
            times[k] += t.elapsed();
            let file = NxFile::open(&nx).unwrap();
            let t = Instant::now();
            let out = file.decode_all(1).unwrap();
            dtimes[k] += t.elapsed();
            assert!(out == m.samples, "config study roundtrip {} {}", m.name, variants[k].0);
            totals[k].0 += nx.len() as u64;
            totals[k].1 += m.samples.len() as u64 * 4;
            s.push_str(&format!(" {:.2} |", nx.len() as f64 * 8.0 / m.samples.len() as f64));
        }
        s.push('\n');
    }
    s.push_str("| **total size vs WAV** |");
    for (b, raw) in totals {
        s.push_str(&format!(" **{:.2}%** |", 100.0 * b as f64 / raw as f64));
    }
    s.push_str("\n| encode time, all materials (s, all threads) |");
    for t in times {
        s.push_str(&format!(" {:.2} |", t.as_secs_f64()));
    }
    s.push_str("\n| decode time, all materials (s, 1 thread) |");
    for t in dtimes {
        s.push_str(&format!(" {:.3} |", t.as_secs_f64()));
    }
    s.push('\n');
    s
}

/// Bits per sample when the encoder is restricted to a single mode.
pub fn mode_study(materials: &[Material], base: &EncoderConfig) -> String {
    let variants: Vec<(&str, EncoderConfig)> = vec![
        ("raw", EncoderConfig { modes: vec![Mode::Raw], ..base.clone() }),
        ("xorlz", EncoderConfig { modes: vec![Mode::XorLz], ..base.clone() }),
        ("fieldvlc", EncoderConfig { modes: vec![Mode::FieldVlc], ..base.clone() }),
        ("floatsplit (no stereo)", EncoderConfig { modes: vec![Mode::FloatSplit], stereo: false, ..base.clone() }),
        ("floatsplit + stereo", EncoderConfig { modes: vec![Mode::FloatSplit], ..base.clone() }),
        ("adaptive (all)", base.clone()),
    ];
    let mut s = String::from("| material |");
    for (n, _) in &variants {
        s.push_str(&format!(" {n} |"));
    }
    s.push_str("\n|---|");
    s.push_str(&"---:|".repeat(variants.len()));
    s.push('\n');
    for m in materials {
        s.push_str(&format!("| {} |", m.name));
        for (_, cfg) in &variants {
            let (nx, _) = encode_to_vec(m.channels, m.sample_rate, &m.samples, Vec::new(), cfg, 0).unwrap();
            let file = NxFile::open(&nx).unwrap();
            assert_eq!(file.decode_all(0).unwrap(), m.samples, "mode study roundtrip");
            s.push_str(&format!(" {:.2} |", nx.len() as f64 * 8.0 / m.samples.len() as f64));
        }
        s.push('\n');
    }
    s
}

pub fn block_size_study(materials: &[&Material], base: &EncoderConfig, sizes: &[u32]) -> String {
    let mut s = String::from("| material |");
    for b in sizes {
        s.push_str(&format!(" {b} |"));
    }
    s.push_str("\n|---|");
    s.push_str(&"---:|".repeat(sizes.len()));
    s.push('\n');
    for m in materials {
        s.push_str(&format!("| {} (bits/sample, dec ×RT) |", m.name));
        for &b in sizes {
            let cfg = EncoderConfig { block_frames: b, adaptive_blocks: false, ..base.clone() };
            let (nx, _) = encode_to_vec(m.channels, m.sample_rate, &m.samples, Vec::new(), &cfg, 0).unwrap();
            let f = NxFile::open(&nx).unwrap();
            let (d, out) = best_of(3, || f.decode_all(1).unwrap());
            assert_eq!(out, m.samples);
            let rt = m.frames() as f64 / m.sample_rate as f64 / d.as_secs_f64();
            s.push_str(&format!(" {:.2}, {:.0}× |", nx.len() as f64 * 8.0 / m.samples.len() as f64, rt));
        }
        s.push('\n');
    }
    s
}

pub fn level_study(materials: &[&Material], base: &EncoderConfig, levels: &[u8]) -> String {
    let mut s = String::from("| material |");
    for l in levels {
        s.push_str(&format!(" level {l} |"));
    }
    s.push_str("\n|---|");
    s.push_str(&"---:|".repeat(levels.len()));
    s.push('\n');
    for m in materials {
        s.push_str(&format!("| {} (bits/sample, enc ×RT 1T) |", m.name));
        for &l in levels {
            let cfg = EncoderConfig { level: l, ..base.clone() };
            let (d, (nx, _)) =
                best_of(1, || encode_to_vec(m.channels, m.sample_rate, &m.samples, Vec::new(), &cfg, 1).unwrap());
            let rt = m.frames() as f64 / m.sample_rate as f64 / d.as_secs_f64();
            s.push_str(&format!(" {:.2}, {:.0}× |", nx.len() as f64 * 8.0 / m.samples.len() as f64, rt));
        }
        s.push('\n');
    }
    s
}

/// Zero-order entropy (bits/sample) of the sign, exponent and mantissa
/// fields, and of the exponent delta — what a memoryless field coder could
/// at best achieve.
pub fn field_entropy(samples: &[u32]) -> (f64, f64, f64, f64) {
    fn h(counts: &std::collections::HashMap<u32, u64>, n: f64) -> f64 {
        counts
            .values()
            .map(|&c| {
                let p = c as f64 / n;
                -p * p.log2()
            })
            .sum()
    }
    let n = samples.len() as f64;
    let mut s = std::collections::HashMap::new();
    let mut e = std::collections::HashMap::new();
    let mut de = std::collections::HashMap::new();
    let mut m_low = std::collections::HashMap::new();
    let mut m_high = std::collections::HashMap::new();
    let mut prev = 0u32;
    for &v in samples {
        *s.entry(v >> 31).or_default() += 1;
        let ex = (v >> 23) & 0xFF;
        *e.entry(ex).or_default() += 1;
        *de.entry(ex.wrapping_sub(prev)).or_default() += 1;
        prev = ex;
        // mantissa entropy estimated as two independent ~12-bit halves
        *m_high.entry((v >> 12) & 0x7FF).or_default() += 1;
        *m_low.entry(v & 0xFFF).or_default() += 1;
    }
    (h(&s, n), h(&e, n), h(&de, n), h(&m_high, n) + h(&m_low, n))
}

/// The cumulative stage ladder of the implementation plan:
/// 0 = format 1.0 tools, 1 = + XorChimp, 2 = + RLE, 3 = + container 1.1 and
/// adaptive blocks, 4 = + Huffman/rANS/range coders, 5 = + inter-channel modes.
pub fn stage_config(n: u8) -> EncoderConfig {
    use crate::block_codec::IndexKind;
    use crate::entropy::Coder;
    let mut c = EncoderConfig::v1_0();
    if n >= 1 {
        c.modes.push(Mode::XorChimp);
    }
    if n >= 2 {
        c.modes.push(Mode::Rle);
    }
    if n >= 3 {
        c.compact_headers = true;
        c.index = IndexKind::Compact;
        c.adaptive_blocks = true;
        c.block_frames = 8192;
        c.min_block_frames = 256;
    }
    if n >= 4 {
        c.entropy = Coder::ALL.to_vec();
    }
    if n >= 5 {
        c.modes.extend([Mode::FloatSplitRef, Mode::XorRef]);
    }
    c
}

/// Stage 1 study: XOR bit-pattern coding variants (each with raw fallback),
/// on top of stage 0.
pub fn xor_study(materials: &[Material]) -> String {
    let only = |modes: Vec<Mode>, w: Option<Vec<u32>>| EncoderConfig { modes, xor_windows: w, ..stage_config(0) };
    config_table(
        materials,
        &[
            ("raw", only(vec![Mode::Raw], None)),
            ("xorlz (Gorilla window)", only(vec![Mode::XorLz], None)),
            ("xorchimp N=1", only(vec![Mode::XorChimp], Some(vec![0]))),
            ("xorchimp N=16", only(vec![Mode::XorChimp], Some(vec![4]))),
            ("xorchimp N=128", only(vec![Mode::XorChimp], Some(vec![7]))),
            ("xorchimp auto", only(vec![Mode::XorChimp], None)),
            ("stage 0 (v1.0)", stage_config(0)),
            ("stage 1 (+ xorchimp)", stage_config(1)),
        ],
    )
}

/// Stage 2 study: run-length / repeated-sequence coding.
pub fn rle_study(materials: &[Material]) -> String {
    config_table(
        materials,
        &[
            ("rle only (+raw)", EncoderConfig { modes: vec![Mode::Rle], ..stage_config(0) }),
            ("stage 0 (v1.0)", stage_config(0)),
            ("stage 1", stage_config(1)),
            ("stage 2 (+ rle)", stage_config(2)),
        ],
    )
}

/// Stage 3 study: fixed vs adaptive block sizes (all overhead included).
pub fn blocks_study(materials: &[Material]) -> String {
    let s2 = stage_config(2);
    let compact =
        |c: EncoderConfig| EncoderConfig { compact_headers: true, index: crate::block_codec::IndexKind::Compact, ..c };
    let fixed = |b: u32| compact(EncoderConfig { block_frames: b, ..s2.clone() });
    let adaptive = |max: u32, min: u32, search: u8| EncoderConfig {
        block_frames: max,
        min_block_frames: min,
        adaptive_blocks: true,
        block_search_level: search,
        ..compact(s2.clone())
    };
    config_table(
        materials,
        &[
            ("stage 2 (fixed 2048, 1.0 container)", s2.clone()),
            ("fixed 512", fixed(512)),
            ("fixed 1024", fixed(1024)),
            ("fixed 2048", fixed(2048)),
            ("fixed 4096", fixed(4096)),
            ("adaptive 256-8192 search L0", adaptive(8192, 256, 0)),
            ("adaptive 256-8192 search L1 (stage 3)", adaptive(8192, 256, 1)),
            ("adaptive 256-8192 search L5", adaptive(8192, 256, 5)),
            ("adaptive 128-16384 search L1", adaptive(16384, 128, 1)),
        ],
    )
}

/// Stage 4 study: residual entropy coders (each alone vs Rice, and combined).
pub fn entropy_study(materials: &[Material]) -> String {
    use crate::entropy::Coder;
    let with = |c: Vec<Coder>| EncoderConfig { entropy: c, ..stage_config(3) };
    config_table(
        materials,
        &[
            ("rice (stage 3)", with(vec![Coder::Rice])),
            ("rice + huffman", with(vec![Coder::Rice, Coder::Huffman])),
            ("rice + rans", with(vec![Coder::Rice, Coder::Rans])),
            ("rice + range", with(vec![Coder::Rice, Coder::Range])),
            ("all coders (stage 4)", with(Coder::ALL.to_vec())),
        ],
    )
}

/// Stage 5 study: multichannel decorrelation (all transforms integer or
/// bitwise; complete sizes; bit-exact verified). Base: stage 3 (Rice only).
pub fn multichannel_study(materials: &[Material]) -> String {
    let with = |extra: &[Mode], stereo: bool| {
        let mut c = stage_config(3);
        c.modes.extend_from_slice(extra);
        c.stereo = stereo;
        c
    };
    config_table(
        materials,
        &[
            ("independent channels", with(&[], false)),
            ("stereo L/S, S/R, M/S", with(&[], true)),
            ("+ xorref", with(&[Mode::XorRef], true)),
            ("+ floatsplitref", with(&[Mode::FloatSplitRef], true)),
            ("+ both", with(&[Mode::XorRef, Mode::FloatSplitRef], true)),
            ("refs only, no stereo", with(&[Mode::XorRef, Mode::FloatSplitRef], false)),
        ],
    )
}

/// Ablation: every optimization alone on top of format 1.0, then all stages
/// cumulatively (the order of the implementation plan), plus the shipped
/// defaults and presets.
pub fn ablation_study(materials: &[Material]) -> String {
    use crate::entropy::Coder;
    let v10 = stage_config(0);
    let add_modes = |ms: &[Mode]| {
        let mut c = v10.clone();
        c.modes.extend_from_slice(ms);
        c
    };
    let container11 =
        EncoderConfig { compact_headers: true, index: crate::block_codec::IndexKind::Compact, ..v10.clone() };
    let adaptive =
        EncoderConfig { adaptive_blocks: true, block_frames: 8192, min_block_frames: 256, ..container11.clone() };
    config_table(
        materials,
        &[
            ("stage 0 = v1.0", v10.clone()),
            ("only +xorchimp", add_modes(&[Mode::XorChimp])),
            ("only +rle", add_modes(&[Mode::Rle])),
            ("only +container 1.1", container11),
            ("only +adaptive blocks", adaptive),
            ("only +entropy coders", EncoderConfig { entropy: Coder::ALL.to_vec(), ..v10.clone() }),
            ("only +inter-channel", add_modes(&[Mode::FloatSplitRef, Mode::XorRef])),
            ("stage 1", stage_config(1)),
            ("stages 1-2", stage_config(2)),
            ("stages 1-3", stage_config(3)),
            ("stages 1-4", stage_config(4)),
            ("stages 1-5", stage_config(5)),
            ("default (level 5)", EncoderConfig::preset(5)),
            ("preset level 2", EncoderConfig::preset(2)),
            ("preset level 8", EncoderConfig::preset(8)),
        ],
    )
}

/// Performance table: speeds (× real time), parallel scaling, random
/// access latency, peak heap use and index overhead.
pub fn performance_table(rows: &[Row]) -> String {
    let mut s = String::from(
        "| material | enc ×RT 1T / MT | dec ×RT 1T / 2T / MT | random access (µs per 10 ms read) | peak heap enc / dec (MB, × input) | blocks | index bytes (% of file) |\n|---|---:|---:|---:|---:|---:|---:|\n",
    );
    let mb = |b: Option<usize>, input: u64| {
        b.map_or("n/a".into(), |b| format!("{:.1} ({:.2}×)", b as f64 / 1e6, b as f64 / input as f64))
    };
    for r in rows {
        let secs = r.frames as f64 / r.rate as f64;
        let rt = |d: Duration| secs / d.as_secs_f64();
        s.push_str(&format!(
            "| {} | {:.0} / {:.0} | {:.0} / {:.0} / {:.0} | {:.0} | {} / {} | {} | {} ({:.3}%) |\n",
            r.material,
            rt(r.enc_1t),
            rt(r.enc_mt),
            rt(r.dec_1t),
            rt(r.dec_2t),
            rt(r.dec_mt),
            r.seek.as_secs_f64() * 1e6,
            mb(r.peak_enc, r.wav_bytes),
            mb(r.peak_dec, r.wav_bytes),
            r.summary.blocks,
            r.summary.index_bytes,
            100.0 * r.summary.index_bytes as f64 / r.nx_bytes as f64,
        ));
    }
    s
}
