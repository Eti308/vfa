//! `vfa` command-line tool.

#[global_allocator]
static ALLOC: vfa::memory::CountingAlloc = vfa::memory::CountingAlloc;

use std::path::PathBuf;
use vfa::options::Args;
use std::time::Instant;
use vfa::block_codec::EncoderConfig;
use vfa::container::{encode_to_vec, tags, StreamInfo};
use vfa::decoder::{NxFile, StreamDecoder};
use vfa::sample_codec::Mode;
use vfa::{benchmark, ieee754, signals, wav, Error, Result};

const USAGE: &str = "\
vfa — lossless, bit-exact float32 audio codec (VFA / .nx)

USAGE:
  vfa encode <in.wav> <out.nx> [--level 0-8] [--block N] [--fixed-blocks|--adaptive] [--min-block N]
                               [--modes m1,m2] [--no-stereo] [--index compact|legacy|none]
                               [--entropy rice,huffman,rans,range]
                               [--no-wav-header] [--threads N]
  vfa decode <in.nx> <out.wav> [--start FRAME --frames N | --start-time SEC --duration SEC]
                               [--threads N] [--stream]
  vfa verify <in.wav> [encode options]     encode + decode in memory, compare bits
  vfa info   <in.nx> [--blocks]            stream parameters, metadata, block modes
  vfa analyze <in.wav>                     float field statistics of a WAV file
  vfa gen    <dir> [--seconds S]           write the benchmark test signals as WAV
  vfa bench  [--out DIR] [--seconds S] [--level L] [--block N] [--reps R]
             [--no-external] [--studies]   compression / speed comparison (Markdown)
  vfa fuzz   [--iters N] [--seed S]        mutate encoded files, decoder must never panic

modes: constant, raw, xorlz, fieldvlc, floatsplit, xorchimp, rle
blocks: levels 3-8 adaptive (--block = maximum, default 8192; --min-block = minimum, default 256),
        levels 0-2 fixed 2048; override with --fixed-blocks / --adaptive
";

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() || raw[0] == "-h" || raw[0] == "--help" {
        print!("{USAGE}");
        return;
    }
    let cmd = raw[0].clone();
    let args = Args::parse(raw[1..].to_vec());
    if let Err(e) = args.check() {
        eprintln!("error: {e}\n\n{USAGE}");
        std::process::exit(2);
    }
    let r = match cmd.as_str() {
        "encode" => cmd_encode(&args),
        "decode" => cmd_decode(&args),
        "verify" => cmd_verify(&args),
        "info" => cmd_info(&args),
        "analyze" => cmd_analyze(&args),
        "gen" => cmd_gen(&args),
        "bench" => cmd_bench(&args),
        "fuzz" => cmd_fuzz(&args),
        _ => Err(Error::InvalidInput(format!("unknown command {cmd}\n\n{USAGE}"))),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn mb_s(bytes: usize, secs: f64) -> f64 {
    bytes as f64 / 1e6 / secs
}

fn cmd_encode(a: &Args) -> Result<()> {
    let (inp, out) = (a.path(0, "input WAV")?, a.path(1, "output .nx")?);
    let cfg = a.config()?;
    let threads = a.num("--threads", 0usize)?;
    let bytes = std::fs::read(&inp)?;
    let w = wav::parse(&bytes)?;
    let meta = if a.has("--no-wav-header") {
        vec![]
    } else {
        vec![(tags::WAV_PREFIX, w.prefix.clone()), (tags::WAV_SUFFIX, w.suffix.clone())]
    };
    let mut meta = meta;
    meta.push((tags::ENCODER, format!("vfa {} level {}", env!("CARGO_PKG_VERSION"), cfg.level).into_bytes()));
    let t = Instant::now();
    let (nx, summary) = encode_to_vec(w.channels, w.sample_rate, &w.samples, meta, &cfg, threads)?;
    let secs = t.elapsed().as_secs_f64();
    std::fs::write(&out, &nx)?;
    println!(
        "{} -> {}: {} ch, {} Hz, {} frames | {} -> {} bytes ({:.2}%, {:.2} bits/sample) | {:.2} s ({:.0} MB/s)",
        inp.display(),
        out.display(),
        w.channels,
        w.sample_rate,
        w.frames(),
        bytes.len(),
        nx.len(),
        100.0 * nx.len() as f64 / bytes.len() as f64,
        nx.len() as f64 * 8.0 / w.samples.len().max(1) as f64,
        secs,
        mb_s(bytes.len(), secs)
    );
    println!("modes: {}", summary.mode_share());
    Ok(())
}

fn cmd_decode(a: &Args) -> Result<()> {
    let (inp, out) = (a.path(0, "input .nx")?, a.path(1, "output WAV")?);
    let threads = a.num("--threads", 0usize)?;
    let data = std::fs::read(&inp)?;
    let t = Instant::now();
    if a.has("--stream") {
        let mut dec = StreamDecoder::new(std::io::Cursor::new(&data))?;
        let mut samples = Vec::new();
        while let Some(b) = dec.next_block()? {
            samples.extend_from_slice(&b);
        }
        std::fs::write(&out, wav::write_float32(dec.info.sample_rate, dec.info.channels, &samples))?;
        println!(
            "streamed {} frames in {:.3} s",
            samples.len() / dec.info.channels as usize,
            t.elapsed().as_secs_f64()
        );
        return Ok(());
    }
    let f = NxFile::open(&data)?;
    match f.index_source {
        vfa::decoder::IndexSource::Footer => {}
        vfa::decoder::IndexSource::Scanned => eprintln!("note: file has no seek index; index rebuilt by scanning"),
        vfa::decoder::IndexSource::Recovered => {
            eprintln!("warning: footer damaged or missing; block index rebuilt by scanning, metadata lost")
        }
    }
    let wav_bytes = if a.has("--start-time") || a.has("--duration") {
        let start = a.num("--start-time", 0.0f64)?;
        let dur = a.num("--duration", f64::INFINITY)?;
        let s = f.decode_time_range(start, dur.min(1e12))?;
        wav::write_float32(f.info.sample_rate, f.info.channels, &s)
    } else if a.has("--start") || a.has("--frames") {
        let start = a.num("--start", 0u64)?;
        let frames = a.num("--frames", f.info.total_frames.saturating_sub(start))?;
        let s = f.decode_range(start, frames)?;
        wav::write_float32(f.info.sample_rate, f.info.channels, &s)
    } else {
        let s = f.decode_all(threads)?;
        f.verify_sample_crc(&s)?;
        match (f.meta(tags::WAV_PREFIX), f.meta(tags::WAV_SUFFIX)) {
            (Some(p), Some(sfx)) => wav::assemble(p, &s, sfx),
            _ => wav::write_float32(f.info.sample_rate, f.info.channels, &s),
        }
    };
    let secs = t.elapsed().as_secs_f64();
    std::fs::write(&out, &wav_bytes)?;
    println!(
        "{} -> {}: {} bytes in {:.3} s ({:.0} MB/s)",
        inp.display(),
        out.display(),
        wav_bytes.len(),
        secs,
        mb_s(wav_bytes.len(), secs)
    );
    Ok(())
}

fn cmd_verify(a: &Args) -> Result<()> {
    let inp = a.path(0, "input WAV")?;
    let cfg = a.config()?;
    let bytes = std::fs::read(&inp)?;
    let w = wav::parse(&bytes)?;
    let meta = vec![(tags::WAV_PREFIX, w.prefix.clone()), (tags::WAV_SUFFIX, w.suffix.clone())];
    let t = Instant::now();
    let (nx, summary) = encode_to_vec(w.channels, w.sample_rate, &w.samples, meta, &cfg, 0)?;
    let te = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let f = NxFile::open(&nx)?;
    let s = f.decode_all(0)?;
    let td = t.elapsed().as_secs_f64();
    let mismatches = s.iter().zip(&w.samples).filter(|(a, b)| a != b).count() + s.len().abs_diff(w.samples.len());
    let rebuilt = wav::assemble(f.meta(tags::WAV_PREFIX).unwrap(), &s, f.meta(tags::WAV_SUFFIX).unwrap());
    println!("file:          {}", inp.display());
    println!("format:        {} ch, {} Hz, {} frames", f.info.channels, f.info.sample_rate, f.info.total_frames);
    println!(
        "size:          {} -> {} bytes ({:.2}%)",
        bytes.len(),
        nx.len(),
        100.0 * nx.len() as f64 / bytes.len() as f64
    );
    println!("encode/decode: {te:.3} s / {td:.3} s");
    println!("modes:         {}", summary.mode_share());
    println!(
        "sample bits:   {}",
        if mismatches == 0 { "IDENTICAL".to_string() } else { format!("{mismatches} MISMATCHES") }
    );
    println!("WAV file:      {}", if rebuilt == bytes { "byte-identical" } else { "DIFFERS" });
    if mismatches != 0 || rebuilt != bytes {
        return Err(Error::Corrupt("verification failed"));
    }
    Ok(())
}

fn cmd_info(a: &Args) -> Result<()> {
    let data = std::fs::read(a.path(0, "input .nx")?)?;
    let f = NxFile::open(&data)?;
    let StreamInfo { channels, sample_rate, total_frames, block_frames, variable_blocks, compact_headers } = f.info;
    println!(
        "VFA v1.{} | float32 | {channels} ch | {sample_rate} Hz | {total_frames} frames ({:.3} s)",
        data[5],
        total_frames as f64 / sample_rate as f64
    );
    println!(
        "blocks: {} ({}, max {block_frames} frames, {} headers) | index: {:?} | file {} bytes | {:.3} bits/sample",
        f.block_count(),
        if variable_blocks { "variable size" } else { "fixed size" },
        if compact_headers { "compact" } else { "long" },
        f.index_source,
        data.len(),
        data.len() as f64 * 8.0 / (total_frames as f64 * channels as f64)
    );
    for (tag, d) in &f.metadata {
        let t = String::from_utf8_lossy(tag);
        if tag == &tags::ENCODER {
            println!("meta {t}: {}", String::from_utf8_lossy(d));
        } else {
            println!("meta {t}: {} bytes", d.len());
        }
    }
    if a.has("--blocks") {
        for b in 0..f.block_count() {
            let (h, p) = f.block_payload(b)?;
            let mut r = vfa::bitio::BitReader::new(p);
            let stereo = if channels == 2 { r.read_bits(2)? } else { 0 };
            let first_mode = Mode::read_id(&mut r)?;
            println!(
                "block {b:6}: {} frames, {} bytes, stereo {stereo}, ch0 {}",
                h.frames,
                h.payload_len,
                first_mode.name()
            );
        }
    }
    Ok(())
}

fn cmd_analyze(a: &Args) -> Result<()> {
    let w = wav::parse(&std::fs::read(a.path(0, "input WAV")?)?)?;
    let mut classes = std::collections::BTreeMap::new();
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    let mut tz_hist = [0u64; 24];
    for &s in &w.samples {
        *classes.entry(format!("{:?}", ieee754::classify(s))).or_insert(0u64) += 1;
        if let Some(d) = ieee754::dbfs(s) {
            lo = lo.min(d);
            hi = hi.max(d);
        }
        let m = s & ieee754::MANT_MASK;
        tz_hist[if m == 0 { 23 } else { m.trailing_zeros() as usize }] += 1;
    }
    let (hs, he, hde, hm) = benchmark::field_entropy(&w.samples);
    println!("{} ch, {} Hz, {} frames", w.channels, w.sample_rate, w.frames());
    println!("classes: {classes:?}");
    println!("magnitude range: {lo:.1} .. {hi:.1} dBFS");
    println!(
        "zero-order field entropy (bits/sample): sign {hs:.2}, exponent {he:.2} (Δexp {hde:.2}), mantissa ≈{hm:.2}"
    );
    println!("  => memoryless field coding bound ≈ {:.2} bits/sample", hs + he.min(hde) + hm);
    let n = w.samples.len() as f64;
    let mut cum = 0.0;
    print!("mantissa trailing zeros (cumulative share ≥ k):");
    for k in (0..24).rev() {
        cum += tz_hist[k] as f64;
        if [0, 1, 4, 8, 12, 16, 23].contains(&k) {
            print!(" ≥{k}: {:.1}%", 100.0 * cum / n);
        }
    }
    println!();
    for bits in [16u32, 24] {
        let ok = benchmark::as_integers(&w.samples, bits).is_some();
        println!("exactly representable as {bits}-bit PCM: {ok}");
    }
    Ok(())
}

fn cmd_gen(a: &Args) -> Result<()> {
    let dir = a.path(0, "output directory")?;
    std::fs::create_dir_all(&dir)?;
    for m in signals::all_materials(a.num("--seconds", 10.0)?, 48_000) {
        let p = dir.join(format!("{}.wav", m.name));
        std::fs::write(&p, wav::write_float32(m.sample_rate, m.channels, &m.samples))?;
        println!("{}  ({})", p.display(), m.description);
    }
    Ok(())
}

fn cmd_bench(a: &Args) -> Result<()> {
    let out_dir = PathBuf::from(a.get("--out").unwrap_or("bench_out"));
    std::fs::create_dir_all(&out_dir)?;
    let seconds = a.num("--seconds", 10.0f64)?;
    let cfg = a.config()?;
    let opts = benchmark::Options {
        out_dir: out_dir.clone(),
        cfg: cfg.clone(),
        reps: a.num("--reps", 3usize)?,
        external: !a.has("--no-external"),
    };
    eprintln!("generating materials ({seconds} s each)...");
    let mats = signals::all_materials(seconds, 48_000);
    let mut rows = Vec::new();
    for m in &mats {
        eprintln!("  {}", m.name);
        rows.push(benchmark::run_material(m, &opts));
    }
    let mut md = String::new();
    md.push_str(&format!(
        "# VFA benchmark\n\n{seconds} s per material, level {}, block {} frames, {} threads available. \
         `×RT` = times faster than real time (1T = one thread, MT = all threads). Sizes in bytes; percentages relative to the float32 WAV.\n\n",
        cfg.level,
        cfg.block_frames,
        vfa::parallel::default_threads()
    ));
    md.push_str("## Compression\n\n");
    md.push_str(&benchmark::main_table(&rows));
    md.push_str("\n## Timing\n\n");
    md.push_str(&benchmark::timing_table(&rows));
    md.push_str("\n## Performance (speed, parallel scaling, random access, memory, index)\n\n");
    md.push_str(&benchmark::performance_table(&rows));
    if a.has("--studies") {
        eprintln!("mode study...");
        md.push_str("\n## Single-mode study (bits per sample, lower is better)\n\n");
        md.push_str(&benchmark::mode_study(&mats, &cfg));
        let pick: Vec<&signals::Material> = mats
            .iter()
            .filter(|m| {
                ["05_music_dynamic", "06_music_compressed", "13_music_24bit_origin", "11_plus200dBFS"].contains(&m.name)
            })
            .collect();
        eprintln!("block size study...");
        md.push_str("\n## Block size study (bits/sample, single-thread decode speed)\n\n");
        md.push_str(&benchmark::block_size_study(&pick, &cfg, &[256, 512, 1024, 2048, 4096, 8192, 16384]));
        eprintln!("level study...");
        md.push_str("\n## Level study (bits/sample, single-thread encode speed)\n\n");
        md.push_str(&benchmark::level_study(&pick, &cfg, &[0, 2, 5, 8]));
        md.push_str("\n## Field entropy (zero-order, bits/sample)\n\n| material | sign | exponent | Δexponent | mantissa≈ |\n|---|---:|---:|---:|---:|\n");
        for m in &mats {
            let (s, e, de, mm) = benchmark::field_entropy(&m.samples);
            md.push_str(&format!("| {} | {s:.2} | {e:.2} | {de:.2} | {mm:.2} |\n", m.name));
        }
    }
    let path = out_dir.join("results.md");
    std::fs::write(&path, &md)?;
    println!("{md}");
    eprintln!("written to {}", path.display());
    Ok(())
}

fn cmd_fuzz(a: &Args) -> Result<()> {
    let iters = a.num("--iters", 20_000u64)?;
    let mut rng = signals::Rng::new(a.num("--seed", 1u64)?);
    let mats = signals::all_materials(0.05, 48_000);
    // seed files cover every container option and every coder
    let cfgs = [
        EncoderConfig { block_frames: 512, min_block_frames: 64, ..Default::default() },
        EncoderConfig {
            block_frames: 512,
            entropy: vfa::entropy::Coder::ALL.to_vec(),
            index: vfa::block_codec::IndexKind::None,
            ..Default::default()
        },
        EncoderConfig { entropy: vec![vfa::entropy::Coder::Huffman], block_frames: 300, ..EncoderConfig::preset(1) },
        EncoderConfig {
            entropy: vec![vfa::entropy::Coder::Rans],
            block_frames: 700,
            min_block_frames: 100,
            ..Default::default()
        },
        EncoderConfig { entropy: vec![vfa::entropy::Coder::Range], block_frames: 512, ..Default::default() },
        EncoderConfig { block_frames: 512, ..EncoderConfig::v1_0() },
    ];
    let mut seeds: Vec<Vec<u8>> = Vec::new();
    for (i, m) in mats.iter().enumerate() {
        for (k, cfg) in cfgs.iter().enumerate() {
            if (i + k) % 2 == 0 {
                seeds.push(encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], cfg, 1).unwrap().0);
            }
        }
    }
    let (mut ok, mut err) = (0u64, 0u64);
    let t = Instant::now();
    for i in 0..iters {
        let mut d = seeds[(rng.next_u64() % seeds.len() as u64) as usize].clone();
        let edits = 1 + rng.next_u64() % 8;
        for _ in 0..edits {
            let pos = (rng.next_u64() % d.len() as u64) as usize;
            match rng.next_u64() % 4 {
                0 => d[pos] ^= 1 << (rng.next_u64() % 8),
                1 => d[pos] = rng.next_u64() as u8,
                2 => d.truncate(pos.max(1)),
                _ => {
                    // bypass CRC checks by re-computing block CRCs: exercises the payload decoder
                    d[pos] = rng.next_u64() as u8;
                    fix_crcs(&mut d);
                }
            }
        }
        let res = std::panic::catch_unwind(|| match NxFile::open(&d) {
            Ok(f) => f.decode_all(1).map(|_| ()),
            Err(e) => Err(e),
        });
        match res {
            Ok(Ok(())) => ok += 1,
            Ok(Err(_)) => err += 1,
            Err(_) => {
                let p = format!("fuzz_crash_{i}.nx");
                std::fs::write(&p, &d)?;
                return Err(Error::Corrupt("decoder panicked; input saved"));
            }
        }
    }
    println!(
        "{iters} mutated files in {:.1} s: {ok} decoded, {err} rejected with an error, 0 panics",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Recompute the payload CRC of every block whose header is intact.
fn fix_crcs(d: &mut [u8]) {
    vfa::container::recompute_block_crcs(d)
}
