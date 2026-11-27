//! End-to-end and property-based tests: bit-exact round trips through the
//! complete file format for arbitrary bit patterns, channel counts, sample
//! rates and block sizes; random access; streaming; determinism; and
//! robustness against corrupted files.

use proptest::prelude::*;
use vfa::block_codec::{EncoderConfig, IndexKind};
use vfa::container::{encode_to_vec, tags};
use vfa::decoder::{NxFile, StreamDecoder};
use vfa::entropy::Coder;
use vfa::sample_codec::Mode;
use vfa::{signals, wav};

fn roundtrip(ch: u16, rate: u32, samples: &[u32], cfg: &EncoderConfig) -> Vec<u8> {
    let (nx, _) = encode_to_vec(ch, rate, samples, vec![], cfg, 0).expect("encode");
    let f = NxFile::open(&nx).expect("open");
    assert_eq!(f.info.channels, ch);
    assert_eq!(f.info.sample_rate, rate);
    assert_eq!(f.info.total_frames as usize, samples.len() / ch as usize);
    let out = f.decode_all(0).expect("decode");
    assert!(out == samples, "bit mismatch");
    f.verify_sample_crc(&out).expect("sample crc");
    // streaming decoder
    let mut sd = StreamDecoder::new(std::io::Cursor::new(&nx)).unwrap();
    let mut streamed = Vec::new();
    while let Some(b) = sd.next_block().unwrap() {
        streamed.extend_from_slice(&b);
    }
    assert!(streamed == samples, "stream mismatch");
    nx
}

/// Float-like signals: a sine at a random magnitude exponent with random
/// special values injected — exercises the FloatSplit paths far more than
/// uniformly random patterns (which mostly fall back to Raw).
fn floatish() -> impl Strategy<Value = (u16, Vec<u32>)> {
    (
        1u16..=3,
        1usize..700,
        -140i32..127,
        0.0f64..1.0,
        prop::collection::vec((0usize..2000, any::<u32>()), 0..20),
        any::<bool>(),
    )
        .prop_map(|(ch, frames, exp, freq, specials, quantize)| {
            let n = frames * ch as usize;
            let amp = 2f64.powi(exp);
            let mut s: Vec<u32> = (0..n)
                .map(|i| {
                    let mut v = (i as f64 * freq * 0.3).sin() * amp;
                    if quantize {
                        let q = 2f64.powi(exp - 15);
                        v = (v / q).round() * q;
                    }
                    (v as f32).to_bits()
                })
                .collect();
            for (pos, bits) in specials {
                if n > 0 {
                    s[pos % n] = bits;
                }
            }
            (ch, s)
        })
}

fn any_cfg() -> impl Strategy<Value = EncoderConfig> {
    (
        prop::sample::select(vec![0u8, 1, 3, 5, 8]),
        1u32..600,
        any::<bool>(),
        any::<bool>(),
        1u32..80,
        prop::sample::select(vec![IndexKind::Compact, IndexKind::Legacy, IndexKind::None]),
        any::<bool>(),
        prop::sample::select(vec![
            vec![Coder::Rice],
            Coder::ALL.to_vec(),
            vec![Coder::Huffman],
            vec![Coder::Rans],
            vec![Coder::Range],
        ]),
    )
        .prop_map(|(level, block_frames, stereo, adaptive, min_block, index, compact, entropy)| {
            // the legacy index cannot describe variable blocks
            let index = if adaptive && index == IndexKind::Legacy { IndexKind::Compact } else { index };
            EncoderConfig {
                level,
                block_frames,
                stereo,
                adaptive_blocks: adaptive,
                min_block_frames: min_block,
                index,
                compact_headers: compact,
                entropy,
                ..Default::default()
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bit_patterns(ch in 1u16..=4, frames in 0usize..500, seed in any::<u64>(), cfg in any_cfg()) {
        let mut rng = signals::Rng::new(seed);
        let samples: Vec<u32> = (0..frames * ch as usize).map(|_| rng.next_u64() as u32).collect();
        roundtrip(ch, 48_000, &samples, &cfg);
    }

    #[test]
    fn float_like_signals((ch, samples) in floatish(), cfg in any_cfg()) {
        roundtrip(ch, 96_000, &samples, &cfg);
    }

    #[test]
    fn every_forced_mode((ch, samples) in floatish(), mode in prop::sample::select(Mode::ALL.to_vec())) {
        let cfg = EncoderConfig { modes: vec![mode], block_frames: 256, ..Default::default() };
        roundtrip(ch, 44_100, &samples, &cfg);
    }

    #[test]
    fn random_access((ch, samples) in floatish(), a in 0usize..2000, b in 0usize..2000) {
        let cfg = EncoderConfig { block_frames: 97, ..Default::default() };
        let (nx, _) = encode_to_vec(ch, 48_000, &samples, vec![], &cfg, 1).unwrap();
        let f = NxFile::open(&nx).unwrap();
        let frames = samples.len() / ch as usize;
        let (lo, hi) = if frames == 0 { (0, 0) } else { let (x, y) = (a % (frames + 1), b % (frames + 1)); (x.min(y), x.max(y)) };
        let got = f.decode_range(lo as u64, (hi - lo) as u64).unwrap();
        prop_assert!(got == samples[lo * ch as usize..hi * ch as usize]);
    }

    /// Corrupted input must never panic, only return errors (or, if the
    /// damage hit a non-checked byte, decode successfully).
    #[test]
    fn corruption_never_panics((ch, samples) in floatish(), edits in prop::collection::vec((any::<usize>(), any::<u8>()), 1..8), truncate in any::<Option<usize>>(), fix_crc in any::<bool>()) {
        let cfg = EncoderConfig { block_frames: 512, min_block_frames: 64, entropy: Coder::ALL.to_vec(), ..Default::default() };
        let (mut nx, _) = encode_to_vec(ch, 48_000, &samples, vec![], &cfg, 1).unwrap();
        for (pos, val) in edits {
            let p = pos % nx.len();
            nx[p] = val;
        }
        if let Some(t) = truncate { nx.truncate(t % (nx.len() + 1)); }
        if fix_crc { recompute_block_crcs(&mut nx); }
        if let Ok(f) = NxFile::open(&nx) {
            let _ = f.decode_all(1);
            let _ = f.decode_range(0, f.info.total_frames.min(10));
        }
        if let Ok(mut sd) = StreamDecoder::new(std::io::Cursor::new(&nx)) {
            while let Ok(Some(_)) = sd.next_block() {}
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(data in prop::collection::vec(any::<u8>(), 0..400)) {
        let mut d = data;
        if d.len() >= 36 && d[0] & 1 == 0 {
            // give it a valid header so deeper code paths are reached
            let mut info = vfa::container::StreamInfo::fixed(2, 48_000, 50, 16);
            info.variable_blocks = d[1] & 1 == 1;
            info.compact_headers = d[1] & 2 == 2;
            let h = info.to_header_bytes();
            d[..36].copy_from_slice(&h);
        }
        if let Ok(f) = NxFile::open(&d) { let _ = f.decode_all(1); }
    }
}

/// Recompute block CRCs so that mutations reach the payload decoder.
fn recompute_block_crcs(d: &mut [u8]) {
    vfa::container::recompute_block_crcs(d)
}

#[test]
fn all_benchmark_materials_roundtrip() {
    for m in signals::all_materials(0.25, 48_000) {
        for level in [0u8, 5, 8] {
            let cfg = EncoderConfig { level, ..Default::default() };
            roundtrip(m.channels, m.sample_rate, &m.samples, &cfg);
        }
    }
}

#[test]
fn sample_rates_and_channel_counts_preserved() {
    for rate in [8_000u32, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 384_000, 768_000] {
        for ch in [1u16, 2, 3, 6, 8, 16] {
            let s: Vec<u32> = (0..ch as usize * 1000).map(|i| ((i as f32 * 0.01).sin() * 3.0).to_bits()).collect();
            roundtrip(ch, rate, &s, &EncoderConfig::default());
        }
    }
}

#[test]
fn deterministic_and_thread_independent() {
    let m = &signals::all_materials(0.5, 48_000)[4];
    let cfg = EncoderConfig::default();
    let a = encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], &cfg, 1).unwrap().0;
    let b = encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], &cfg, 4).unwrap().0;
    let c = encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], &cfg, 0).unwrap().0;
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn wav_file_is_reproduced_byte_for_byte() {
    let s: Vec<u32> = (0..2000).map(|i| ((i as f32 * 0.05).cos() * 1e10).to_bits()).collect();
    let mut bytes = wav::write_float32(192_000, 2, &s);
    bytes.extend_from_slice(b"LIST\x05\x00\x00\x00hello\x00"); // trailing chunk + pad byte
    let w = wav::parse(&bytes).unwrap();
    let meta = vec![(tags::WAV_PREFIX, w.prefix.clone()), (tags::WAV_SUFFIX, w.suffix.clone())];
    let (nx, _) = encode_to_vec(w.channels, w.sample_rate, &w.samples, meta, &EncoderConfig::default(), 0).unwrap();
    let f = NxFile::open(&nx).unwrap();
    let out = f.decode_all(0).unwrap();
    let rebuilt = wav::assemble(f.meta(tags::WAV_PREFIX).unwrap(), &out, f.meta(tags::WAV_SUFFIX).unwrap());
    assert_eq!(rebuilt, bytes);
}

/// The prototype's definition of done, as executable checks.
#[test]
fn definition_of_done() {
    let mats = signals::all_materials(1.0, 48_000);
    let get = |n: &str| mats.iter().find(|m| m.name == n).unwrap();
    // +200 dBFS (1e10) survives exactly and compresses
    let hot = get("11_plus200dBFS");
    assert!(hot.samples.iter().any(|&v| f32::from_bits(v).abs() > 5e9));
    let nx = roundtrip(2, 48_000, &hot.samples, &EncoderConfig::default());
    assert!(nx.len() < hot.samples.len() * 4 * 3 / 4, "+200 dBFS should compress below 75%");
    // incompressible data: fallback overhead below 0.3 % (block header 20 B,
    // mode bits, 16 B seek-index entry per 2048-frame block, file header/trailer)
    let rnd = get("17_random_bits");
    let nx = roundtrip(2, 48_000, &rnd.samples, &EncoderConfig::default());
    let raw = rnd.samples.len() * 4;
    assert!((nx.len() as f64) < raw as f64 * 1.003, "fallback overhead too large: {} vs {raw}", nx.len());
    // special values survive
    let sp = get("16_special_values");
    roundtrip(2, 48_000, &sp.samples, &EncoderConfig::default());
}

#[test]
fn damaged_footer_falls_back_to_scanning() {
    let m = &signals::all_materials(0.2, 48_000)[4];
    let (mut nx, _) =
        encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], &EncoderConfig::default(), 0).unwrap();
    let n = nx.len();
    nx[n - 1] ^= 0xFF; // break trailer magic
    let f = NxFile::open(&nx).unwrap();
    assert_eq!(f.index_source, vfa::decoder::IndexSource::Recovered);
    assert_eq!(f.decode_all(0).unwrap(), m.samples);
}

#[test]
fn corrupted_payload_is_detected_by_crc() {
    let m = &signals::all_materials(0.2, 48_000)[4];
    let (mut nx, _) =
        encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], &EncoderConfig::default(), 0).unwrap();
    nx[200] ^= 0x10;
    let f = NxFile::open(&nx).unwrap();
    assert!(matches!(f.decode_all(1), Err(vfa::Error::ChecksumMismatch { .. })));
}

/// Files written by the format 1.0 encoder (tests/fixtures/v1_0) must keep
/// decoding bit-exactly.
#[test]
fn format_1_0_files_still_decode() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1_0");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "nx") {
            let nx = std::fs::read(&p).unwrap();
            assert_eq!(nx[5], 0, "fixture must be format 1.0");
            let wav_bytes = std::fs::read(p.with_extension("wav")).unwrap();
            let w = wav::parse(&wav_bytes).unwrap();
            let f = NxFile::open(&nx).unwrap();
            assert_eq!(f.index_source, vfa::decoder::IndexSource::Footer);
            let out = f.decode_all(0).unwrap();
            assert!(out == w.samples, "{}", p.display());
            let rebuilt = wav::assemble(f.meta(tags::WAV_PREFIX).unwrap(), &out, f.meta(tags::WAV_SUFFIX).unwrap());
            assert_eq!(rebuilt, wav_bytes);
            n += 1;
        }
    }
    assert_eq!(n, 5);
}

#[test]
fn index_kinds_and_time_seeking() {
    let m = &signals::all_materials(1.0, 48_000)[4];
    for index in [IndexKind::Compact, IndexKind::Legacy, IndexKind::None] {
        for adaptive in [false, true] {
            if adaptive && index == IndexKind::Legacy {
                continue;
            }
            let cfg = EncoderConfig { index, adaptive_blocks: adaptive, block_frames: 4096, ..Default::default() };
            let (nx, _) = encode_to_vec(2, 48_000, &m.samples, vec![(tags::ENCODER, b"x".to_vec())], &cfg, 0).unwrap();
            let f = NxFile::open(&nx).unwrap();
            let expect = if index == IndexKind::None {
                vfa::decoder::IndexSource::Scanned
            } else {
                vfa::decoder::IndexSource::Footer
            };
            assert_eq!(f.index_source, expect);
            assert_eq!(f.meta(tags::ENCODER), Some(&b"x"[..]), "metadata kept without index");
            // 0.25 s .. 0.5 s
            let got = f.decode_time_range(0.25, 0.25).unwrap();
            assert!(got == m.samples[12_000 * 2..24_000 * 2]);
            assert_eq!(f.block_of_frame(0), Some(0));
            assert_eq!(f.block_of_frame(47_999), Some(f.block_count() - 1));
        }
    }
}

/// Correlated multichannel material: copies, sign flips, scaled versions,
/// sums and partially identical channels, with specials injected. Exercises
/// FloatSplitRef / XorRef / stereo coupling; every result must be bit exact.
fn correlated() -> impl Strategy<Value = (u16, Vec<u32>)> {
    (2u16..=6, 64usize..1500, -100i32..100, prop::collection::vec((0usize..9000, any::<u32>()), 0..12), any::<u64>())
        .prop_map(|(ch, frames, exp, specials, seed)| {
            let mut rng = signals::Rng::new(seed);
            let amp = 2f64.powi(exp);
            let a: Vec<f64> = (0..frames).map(|i| ((i as f64 * 0.05).sin() + 0.1 * rng.gauss()) * amp).collect();
            let b: Vec<f64> = (0..frames).map(|i| ((i as f64 * 0.013).cos() + 0.1 * rng.gauss()) * amp).collect();
            let mut s = Vec::with_capacity(frames * ch as usize);
            for i in 0..frames {
                for c in 0..ch {
                    let v: u32 = match c {
                        0 => (a[i] as f32).to_bits(),
                        1 => (b[i] as f32).to_bits(),
                        2 => ((a[i] + b[i]) as f32 * 0.5).to_bits(),
                        3 => (a[i] as f32).to_bits() ^ 0x8000_0000,
                        4 => ((a[i] * 0.7) as f32).to_bits(),
                        _ => {
                            if i % 97 == 0 {
                                0x7FC0_0000 | i as u32
                            } else {
                                (b[i] as f32).to_bits()
                            }
                        }
                    };
                    s.push(v);
                }
            }
            let n = s.len();
            for (p, v) in specials {
                s[p % n] = v;
            }
            (ch, s)
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, ..ProptestConfig::default() })]

    #[test]
    fn multichannel_transforms_bit_exact((ch, samples) in correlated(), stereo in any::<bool>(), refs in 0u8..4, level in prop::sample::select(vec![1u8, 5, 8])) {
        let mut modes = vec![Mode::Constant, Mode::Raw, Mode::FloatSplit, Mode::FieldVlc, Mode::XorChimp, Mode::Rle];
        if refs & 1 == 1 { modes.push(Mode::FloatSplitRef); }
        if refs & 2 == 2 { modes.push(Mode::XorRef); }
        let cfg = EncoderConfig { modes, stereo, level, block_frames: 700, min_block_frames: 128, ..Default::default() };
        roundtrip(ch, 48_000, &samples, &cfg);
    }
}

#[test]
fn dual_mono_and_polarity_use_xorref() {
    let m: Vec<u32> = (0..4096)
        .map(|i| (((i as f64 * 0.01).sin() * 0.3 + (i as f64 * 0.27).sin() * 0.01) as f32).to_bits())
        .collect();
    for f in [0u32, 0x8000_0000] {
        let s: Vec<u32> = m.iter().flat_map(|&v| [v, v ^ f]).collect();
        let (nx, summary) = encode_to_vec(2, 48_000, &s, vec![], &EncoderConfig::default(), 0).unwrap();
        assert!(summary.modes.contains_key(&Mode::XorRef), "{:?}", summary.modes);
        let f2 = NxFile::open(&nx).unwrap();
        assert!(f2.decode_all(0).unwrap() == s);
    }
}
