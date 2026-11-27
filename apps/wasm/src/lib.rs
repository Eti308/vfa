//! WebAssembly interface of the VFA codec for the desktop app.
//!
//! Plain C ABI, no bindings generator. Protocol: the host allocates input
//! buffers with `vfa_alloc`, calls an operation, which returns 0 (ok) or 1
//! (error). Binary output is then at `vfa_result_ptr/len`; JSON or text
//! output (or the error message) at `vfa_text_ptr/len`. `vfa_clear` frees
//! both. Long operations report progress through the imported
//! `env.vfa_progress(stage, done, total)`.
//!
//! Everything is single-threaded (wasm32 without threads).

use std::cell::RefCell;
use vfa::block_codec::DecodeScratch;
use vfa::container::{tags, EncodeSummary, NxEncoder};
use vfa::decoder::NxFile;
use vfa::options::{config_json, json_str, tokenize, Args, OPTIONS_HELP};
use vfa::wav::{self, Source};
use vfa::Result;

#[link(wasm_import_module = "env")]
extern "C" {
    fn vfa_progress(stage: u32, done: f64, total: f64);
}

const STAGE_ENCODE: u32 = 1;
const STAGE_DECODE: u32 = 2;
const STAGE_VERIFY: u32 = 3;

fn progress(stage: u32, done: u64, total: u64) {
    unsafe { vfa_progress(stage, done as f64, total as f64) }
}

thread_local! {
    static RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_result(v: Vec<u8>) {
    RESULT.with(|r| *r.borrow_mut() = v);
}
fn set_text(s: String) {
    TEXT.with(|t| *t.borrow_mut() = s);
}

fn finish(r: Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            set_result(Vec::new());
            set_text(e.to_string());
            1
        }
    }
}

unsafe fn slice<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if n == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(p, n)
    }
}

unsafe fn string(p: *const u8, n: usize) -> String {
    String::from_utf8_lossy(slice(p, n)).into_owned()
}

// ---------------------------------------------------------------- memory

#[no_mangle]
pub extern "C" fn vfa_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `p` must come from `vfa_alloc(n)`.
#[no_mangle]
pub unsafe extern "C" fn vfa_free(p: *mut u8, n: usize) {
    drop(Vec::from_raw_parts(p, 0, n.max(1)));
}

#[no_mangle]
pub extern "C" fn vfa_result_ptr() -> *const u8 {
    RESULT.with(|r| r.borrow().as_ptr())
}
#[no_mangle]
pub extern "C" fn vfa_result_len() -> usize {
    RESULT.with(|r| r.borrow().len())
}
#[no_mangle]
pub extern "C" fn vfa_text_ptr() -> *const u8 {
    TEXT.with(|t| t.borrow().as_ptr())
}
#[no_mangle]
pub extern "C" fn vfa_text_len() -> usize {
    TEXT.with(|t| t.borrow().len())
}
#[no_mangle]
pub extern "C" fn vfa_clear() {
    set_result(Vec::new());
    set_text(String::new());
}

// ---------------------------------------------------------------- helpers

fn parse_args(line: &str) -> Result<Args> {
    let a = Args::parse(tokenize(line));
    a.check()?;
    Ok(a)
}

fn source_name(s: &Source) -> String {
    match s {
        Source::Float32 => "float32".into(),
        Source::IntPcm(b) => format!("int{b}"),
    }
}

fn is_nx(b: &[u8]) -> bool {
    b.len() >= 4 && b[0..4] == vfa::container::MAGIC
}

/// Decode every block in order with progress (single thread).
fn decode_with_progress(f: &NxFile, stage: u32) -> Result<Vec<u32>> {
    let ch = f.info.channels as usize;
    let mut out = Vec::with_capacity(f.info.total_frames as usize * ch);
    let (mut planar, mut scratch) = (Vec::new(), DecodeScratch::default());
    let n = f.block_count();
    for b in 0..n {
        f.decode_block_interleaved(b, &mut out, &mut planar, &mut scratch)?;
        if b % 16 == 0 || b + 1 == n {
            progress(stage, (out.len() / ch) as u64, f.info.total_frames);
        }
    }
    Ok(out)
}

fn summary_json(s: &EncodeSummary) -> String {
    let total: u64 = s.modes.values().map(|v| v.0).sum();
    let modes: Vec<String> = s
        .modes
        .iter()
        .map(|(m, (n, bits))| {
            format!(
                "{{\"mode\":\"{}\",\"share\":{:.4},\"bytes\":{}}}",
                m.name(),
                *n as f64 / total.max(1) as f64,
                bits / 8
            )
        })
        .collect();
    format!("{{\"blocks\":{},\"index_bytes\":{},\"modes\":[{}]}}", s.blocks, s.index_bytes, modes.join(","))
}

// ---------------------------------------------------------------- operations

/// Validate an option line; text = configuration JSON.
///
/// # Safety
/// Pointer/length pairs must describe valid memory.
#[no_mangle]
pub unsafe extern "C" fn vfa_parse(args_p: *const u8, args_n: usize) -> i32 {
    let line = string(args_p, args_n);
    finish((|| {
        let a = parse_args(&line)?;
        let cfg = a.config()?;
        let mut extra = Vec::new();
        for k in ["--start", "--frames", "--start-time", "--duration"] {
            if let Some(v) = a.get(k) {
                extra.push(format!("{}:{}", json_str(k), json_str(v)));
            }
        }
        set_text(format!(
            "{{\"config\":{},\"decode\":{{{}}},\"positional\":[{}]}}",
            config_json(&cfg, !a.has("--no-wav-header")),
            extra.join(","),
            a.pos.iter().map(|p| json_str(p)).collect::<Vec<_>>().join(",")
        ));
        Ok(())
    })())
}

/// Encode a WAV file; result = .nx bytes, text = summary JSON.
///
/// # Safety
/// Pointer/length pairs must describe valid memory.
#[no_mangle]
pub unsafe extern "C" fn vfa_encode(wav_p: *const u8, wav_n: usize, args_p: *const u8, args_n: usize) -> i32 {
    let bytes = slice(wav_p, wav_n);
    let line = string(args_p, args_n);
    finish((|| {
        let a = parse_args(&line)?;
        let cfg = a.config()?;
        let (w, src) = wav::parse_for_encoding(bytes)?;
        let keep_header = src == Source::Float32 && !a.has("--no-wav-header");
        let mut meta = Vec::new();
        if keep_header {
            meta.push((tags::WAV_PREFIX, w.prefix.clone()));
            meta.push((tags::WAV_SUFFIX, w.suffix.clone()));
        }
        meta.push((tags::ENCODER, format!("vfa app {} | {}", env!("CARGO_PKG_VERSION"), line.trim()).into_bytes()));
        let frames = w.frames() as u64;
        let info = vfa::container::stream_info_for(w.channels, w.sample_rate, frames, &cfg);
        let mut enc = NxEncoder::new(Vec::with_capacity(bytes.len() / 2), info, cfg.clone(), 1)?;
        let ch = w.channels as usize;
        let step = (cfg.block_frames as usize * 4).max(16_384) * ch;
        let mut done = 0usize;
        progress(STAGE_ENCODE, 0, frames);
        for chunk in w.samples.chunks(step) {
            enc.write_interleaved(chunk)?;
            done += chunk.len() / ch;
            progress(STAGE_ENCODE, done as u64, frames);
        }
        let (nx, summary) = enc.finish(meta)?;
        let json = format!(
            "{{\"input_bytes\":{},\"output_bytes\":{},\"channels\":{},\"sample_rate\":{},\"frames\":{},\"source\":{},\"byte_exact_wav\":{},\"bits_per_sample\":{:.4},\"summary\":{},\"config\":{}}}",
            bytes.len(),
            nx.len(),
            w.channels,
            w.sample_rate,
            frames,
            json_str(&source_name(&src)),
            keep_header,
            nx.len() as f64 * 8.0 / (w.samples.len().max(1) as f64),
            summary_json(&summary),
            config_json(&cfg, keep_header)
        );
        set_result(nx);
        set_text(json);
        Ok(())
    })())
}

/// Decode a .nx file to WAV (byte-identical to the original when its header
/// was stored), or an excerpt with --start/--frames or --start-time/--duration.
///
/// # Safety
/// Pointer/length pairs must describe valid memory.
#[no_mangle]
pub unsafe extern "C" fn vfa_decode(nx_p: *const u8, nx_n: usize, args_p: *const u8, args_n: usize) -> i32 {
    let data = slice(nx_p, nx_n);
    let line = string(args_p, args_n);
    finish((|| {
        let a = parse_args(&line)?;
        let f = NxFile::open(data)?;
        let (ch, rate) = (f.info.channels, f.info.sample_rate);
        let excerpt = a.has("--start-time") || a.has("--duration") || a.has("--start") || a.has("--frames");
        let (wav_bytes, exact) = if excerpt {
            let s = if a.has("--start-time") || a.has("--duration") {
                let st = a.num("--start-time", 0.0f64)?;
                let du = a.num("--duration", 1e12f64)?;
                f.decode_time_range(st, du.min(1e12))?
            } else {
                let st = a.num("--start", 0u64)?;
                let n = a.num("--frames", f.info.total_frames.saturating_sub(st))?;
                f.decode_range(st, n)?
            };
            (wav::write_float32(rate, ch, &s), false)
        } else {
            let s = decode_with_progress(&f, STAGE_DECODE)?;
            f.verify_sample_crc(&s)?;
            match (f.meta(tags::WAV_PREFIX), f.meta(tags::WAV_SUFFIX)) {
                (Some(p), Some(x)) => (wav::assemble(p, &s, x), true),
                _ => (wav::write_float32(rate, ch, &s), false),
            }
        };
        set_text(format!(
            "{{\"output_bytes\":{},\"original_header\":{},\"excerpt\":{},\"channels\":{},\"sample_rate\":{}}}",
            wav_bytes.len(),
            exact,
            excerpt,
            ch,
            rate
        ));
        set_result(wav_bytes);
        Ok(())
    })())
}

/// Describe a .nx or .wav file; text = JSON.
///
/// # Safety
/// Pointer/length pairs must describe valid memory.
#[no_mangle]
pub unsafe extern "C" fn vfa_info(p: *const u8, n: usize) -> i32 {
    let data = slice(p, n);
    finish((|| {
        if is_nx(data) {
            let f = NxFile::open(data)?;
            let i = &f.info;
            let samples = i.total_frames * i.channels as u64;
            let header = f.meta(tags::WAV_PREFIX).map_or(0, |p| p.len()) + f.meta(tags::WAV_SUFFIX).map_or(0, |s| s.len());
            let wav_bytes = samples * 4 + if header > 0 { header as u64 } else { 58 };
            let encoder = f.meta(tags::ENCODER).map(|e| String::from_utf8_lossy(e).into_owned()).unwrap_or_default();
            set_text(format!(
                "{{\"kind\":\"nx\",\"version\":\"1.{}\",\"channels\":{},\"sample_rate\":{},\"frames\":{},\"blocks\":{},\"variable_blocks\":{},\"compact_headers\":{},\"max_block\":{},\"index\":\"{:?}\",\"bytes\":{},\"wav_bytes\":{},\"bits_per_sample\":{:.4},\"wav_header\":{},\"encoder\":{}}}",
                data[5],
                i.channels,
                i.sample_rate,
                i.total_frames,
                f.block_count(),
                i.variable_blocks,
                i.compact_headers,
                i.block_frames,
                f.index_source,
                data.len(),
                wav_bytes,
                data.len() as f64 * 8.0 / samples.max(1) as f64,
                header > 0,
                json_str(&encoder)
            ));
        } else {
            let (w, src) = wav::parse_for_encoding(data)?;
            set_text(format!(
                "{{\"kind\":\"wav\",\"format\":{},\"channels\":{},\"sample_rate\":{},\"frames\":{},\"bytes\":{}}}",
                json_str(&source_name(&src)),
                w.channels,
                w.sample_rate,
                w.frames(),
                data.len()
            ));
        }
        Ok(())
    })())
}

/// Samples for playback from a .nx or .wav file: result = interleaved
/// float32 bit patterns (little endian), text = JSON info.
///
/// # Safety
/// Pointer/length pairs must describe valid memory.
#[no_mangle]
pub unsafe extern "C" fn vfa_pcm(p: *const u8, n: usize) -> i32 {
    let data = slice(p, n);
    finish((|| {
        let (samples, ch, rate) = if is_nx(data) {
            let f = NxFile::open(data)?;
            (decode_with_progress(&f, STAGE_DECODE)?, f.info.channels, f.info.sample_rate)
        } else {
            let (w, _) = wav::parse_for_encoding(data)?;
            (w.samples, w.channels, w.sample_rate)
        };
        set_text(format!("{{\"channels\":{ch},\"sample_rate\":{rate},\"frames\":{}}}", samples.len() / ch as usize));
        let mut out = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        set_result(out);
        Ok(())
    })())
}

/// Decode `nx` and compare every bit with `wav`; text = JSON.
///
/// # Safety
/// Pointer/length pairs must describe valid memory.
#[no_mangle]
pub unsafe extern "C" fn vfa_verify(wav_p: *const u8, wav_n: usize, nx_p: *const u8, nx_n: usize) -> i32 {
    let wav_bytes = slice(wav_p, wav_n);
    let nx = slice(nx_p, nx_n);
    finish((|| {
        let (w, _) = wav::parse_for_encoding(wav_bytes)?;
        let f = NxFile::open(nx)?;
        let s = decode_with_progress(&f, STAGE_VERIFY)?;
        let mismatches = s.iter().zip(&w.samples).filter(|(a, b)| a != b).count() + s.len().abs_diff(w.samples.len());
        let file_identical = match (f.meta(tags::WAV_PREFIX), f.meta(tags::WAV_SUFFIX)) {
            (Some(p), Some(x)) => Some(wav::assemble(p, &s, x) == wav_bytes),
            _ => None,
        };
        let same_format = f.info.channels == w.channels && f.info.sample_rate == w.sample_rate;
        set_text(format!(
            "{{\"samples\":{},\"mismatches\":{},\"identical\":{},\"same_format\":{},\"wav_file_identical\":{}}}",
            s.len(),
            mismatches,
            mismatches == 0 && same_format,
            same_format,
            file_identical.map_or("null".to_string(), |b| b.to_string())
        ));
        Ok(())
    })())
}

/// Option help text.
#[no_mangle]
pub extern "C" fn vfa_help() -> i32 {
    set_text(OPTIONS_HELP.to_string());
    0
}
