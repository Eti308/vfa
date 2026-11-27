# VFA — Variable Float Audio (prototype)

Lossless, **bit-exact** compression of IEEE-754 float32 audio, written in
Rust.

**⬇ Mac app: [download VFA-mac.zip](https://github.com/Eti308/vfa/raw/main/download/VFA-mac.zip)**
(encoder, decoder and player; unzip and open *VFA Encoder.app* or *VFA.html*,
see [Desktop app](#desktop-app-macos)). File extension `.nx`; format version 1.1, which also reads 1.0 files.

* Every one of the 2^32 float32 bit patterns round-trips exactly: ±0,
  subnormals, ±Inf and every NaN payload.
* Sample rate, channel count (1–1024), channel order and sample order are
  preserved.
* No level ceiling: +24, +100, +200 dBFS and values near `f32::MAX`
  (≈ +769.5 dBFS) compress as well as 0 dBFS material.
* The original WAV file is reproduced **byte for byte**, including all
  non-audio chunks.
* No floating-point arithmetic touches sample values. All transforms,
  including the inter-channel ones, are integer or bitwise.
* Blocks are independent, CRC-protected and **adaptively sized** (256–8192
  frames). That gives random access by sample or time, streaming, parallel
  decoding and error isolation.
* Every block of every channel picks the smallest *complete* representation,
  and raw storage is always a candidate. Incompressible data costs +0.03 %.

## Results (10 s per material, default level 5; full tables in [BENCHMARK.md](BENCHMARK.md))

| material | VFA 1.1 | VFA 1.0 | WavPack -hhx | FLAC -8 |
|---|---:|---:|---:|---:|
| float-rendered music (−80 … +769 dBFS, same at every level) | 66.6 % | 67.4 % | 65.8 % | – |
| heavily compressed master | 66.5 % | 67.2 % | 65.8 % | – |
| 24-bit-origin music as float | 46.5 % | 47.3 % | 45.7 % | 46.8 % (int24) |
| 16-bit-origin music as float | 21.9 % | 22.5 % | 21.3 % | 22.2 % (int16) |
| 5.1 / 96 kHz | **53.7 %** | 63.0 % | 63.5 % | – |
| dual mono / polarity-inverted stereo | **33.6 %** / **33.6 %** | 37.7 / 39.6 % | 33.3 / 44.5 % | – |
| transients | **12.4 %** | 14.0 % | 12.9 % | – |
| NaN payloads | **48.7 %** | 78.4 % | 82.5 % | – |
| NaN / Inf / −0 / subnormal mix | **80.4 %** | 95.5 % | 116.0 % | – |
| ±0 runs | **1.3 %** | 3.5 % | 6.3 % | – |
| random 32-bit patterns | **100.0 %** | 100.2 % | 124.6 % | – |
| **all 26 materials** | **47.3 %** | 53.4 % | | |

Exactly periodic synthetic signals (1 kHz sine, wavetable) drop to 1–4 %
because their bit patterns repeat exactly. Real recordings won't show such
gains.

All materials decode bit-exact; percentages are relative to the float32 WAV.

**Speed:**

* Decoding (stereo/48 kHz): ~450–500× real time on one core, ~1600× on 4.
* Encoding at the default level: ~8× real time on one core, ~28× on 4.
* Level 2 encodes faster than format 1.0 did and still gets most of the gain:
  48.1 % total.

Details:

* [RESEARCH.md](RESEARCH.md): research questions (Part 1) and the format 1.1
  optimization stages with ablations (Part 2).
* [SPEC.md](SPEC.md): normative format.
* `docs/stages/`: per-stage benchmark tables.

## Desktop app (macOS)

[`download/VFA-mac.zip`](https://github.com/Eti308/vfa/raw/main/download/VFA-mac.zip) (built from `apps/web/` into `apps/web/dist/`) contains **VFA Encoder.app** and a standalone
**VFA.html**. Unzip it on a Mac and double-click either one. The app:

* encodes WAV (float32, or exactly converted 8/16/24-bit) → .nx with level
  and option controls, and verifies every result bit for bit;
* decodes .nx → WAV (byte-identical to the original file), optionally as an
  excerpt;
* plays both formats with a QuickTime-style player: waveform scrubbing,
  ±10 s, loop, volume, space / arrow keys;
* has a command line that accepts the same options as the `vfa` CLI
  (`--level 8 --no-stereo`, `encode`, `decode --start-time 30 --duration 10`,
  `help`), kept in sync with the controls.

The codec is compiled to WebAssembly and embedded in the page. Everything
runs locally, and nothing needs to be installed.

* The `.app` opens the page in a chromeless window if Chrome/Edge/Brave is
  installed, otherwise in Safari.
* It is not signed. The first time, use right-click → Open, or System
  Settings → Privacy & Security → Open Anyway. VFA.html works without any
  prompt.

Rebuild it:

```bash
rustup target add wasm32-unknown-unknown
```

```bash
python3 apps/web/build.py
```

End-to-end UI test (headless Chromium, needs Playwright):

```bash
node apps/web/test_ui.js some_float32.wav target/release/vfa /tmp/ui-test
```

## How it works (short)

Each block is coded per channel with whichever mode is smallest:

| mode | idea |
|---|---|
| **FloatSplit** | Choose a grid exponent `g` per block. Split every float exactly into a coarse integer `c = trunc(x / 2^g)` and the significand bits below `2^g`. The coarse integers are predicted (fixed polynomial or LPC, optional stereo coupling) and the residuals entropy coded. The number of extra bits follows from `c` itself, so per-sample lengths are variable *without* any length field. Bits below the block's lowest set bit are never stored. |
| FloatSplitRef *(1.1)* | FloatSplit whose coarse integers are predicted from 1–2 other channels with integer weights |
| XorRef *(1.1)* | XOR with another channel's bit patterns (dual mono → 0, polarity inversion → sign bit) |
| XorChimp *(1.1)* | XOR with one of the last 1–128 samples, bucketed leading zeros |
| Rle *(1.1)* | runs and repeated sequences (LZ-style copies); literals coded by another mode |
| FieldVlc | sign + Rice(Δexponent) + Rice(trailing zeros) + significant mantissa bits |
| XorLz | Gorilla-style XOR with the previous sample |
| Constant / Raw | identical samples / verbatim 32 bits (the guaranteed fallback) |

Residuals can use partitioned Rice (default) or Huffman, rANS or an adaptive
range coder (`--entropy`).

## Usage

```bash
cargo build --release
```

```bash
./target/release/vfa encode input.wav output.nx
```

```bash
./target/release/vfa decode output.nx restored.wav
```

```bash
./target/release/vfa decode output.nx excerpt.wav --start-time 12.5 --duration 2
```

```bash
./target/release/vfa verify input.wav
```

Encoder options:

* `--level 0-8` (default 5; levels 0–2 use fixed blocks and are fast)
* `--block N` and `--min-block N` (maximum and minimum block size)
* `--fixed-blocks` / `--adaptive`
* `--modes floatsplit,rle,…` (restrict the allowed modes)
* `--entropy rice,huffman,rans,range`
* `--index compact|legacy|none`
* `--no-stereo`
* `--threads N`

Decoder options:

* `--start F --frames N` (random access by sample)
* `--start-time S --duration S` (random access by time)
* `--stream` (sequential decoding without the index)

Other commands: `info`, `analyze`, `gen`, `bench`, `fuzz`.

Benchmarks (`wavpack` and `flac` on `PATH` for the comparison columns):

```bash
./target/release/vfa bench --seconds 10 --out bench_out
```

```bash
cargo run --release --example stage_study -- ablation 5
```

## Tests

```bash
cargo test --release
```

```bash
cargo test --release -- --ignored
```

The first command runs:

* unit tests;
* property-based tests: random bit patterns, float-like signals with injected
  specials, correlated multichannel material through every transform, every
  mode, every entropy coder, adaptive blocks, all index kinds, random access,
  time seeking, streaming;
* corruption tests;
* format-1.0 compatibility fixtures (`tests/fixtures/v1_0`).

The second runs the exhaustive checks over all 2^32 float32 patterns (~2 min
on 4 cores).

Fuzzing:

```bash
./target/release/vfa fuzz --iters 300000
```

```bash
cd fuzz && cargo +nightly fuzz run decode
```

## Code map

| file | content |
|---|---|
| `src/ieee754.rs` | float32 bit analysis, exact grid split / join |
| `src/variable_integer.rs` | zigzag, LEB128, Elias γ/δ, Rice with escape, partitioned Rice |
| `src/entropy.rs` | Huffman, rANS, adaptive range coder for residuals |
| `src/bitio.rs` | bit writer / bounds-checked bit reader |
| `src/predictor.rs` | fixed + LPC prediction on wrapping `i64` |
| `src/sample_codec.rs` | sub-frame modes (Constant, Raw, XorLz, FieldVlc, FloatSplit) |
| `src/xor_codec.rs` | XorChimp multi-reference XOR coding |
| `src/rle_codec.rs` | run-length / repeated-sequence coding |
| `src/multichannel.rs` | integer and bitwise inter-channel transforms |
| `src/block_codec.rs` | block layout, adaptive mode selection, stereo, adaptive block splitting |
| `src/container.rs` | `.nx` file format 1.1, streaming encoder |
| `src/decoder.rs` | parallel / random-access / time-seeking / streaming decoding |
| `src/wav.rs` | float32 WAV I/O with byte-exact header preservation |
| `src/signals.rs` | deterministic test signals (26 materials) |
| `src/benchmark.rs`, `src/memory.rs` | comparisons, stage studies, heap measurement |
| `examples/` | research experiments and stage studies |
| `src/options.rs` | option parsing shared by the CLI and the app |
| `apps/wasm/` | WebAssembly build of the codec (C ABI) |
| `apps/web/` | desktop app (UI, player, worker), `build.py`, UI test |

## Limitations of this prototype

* float32 only (float64 is reserved in the format).
* WAV input up to 4 GiB (no RF64 yet). The CLI holds the whole file in
  memory; the library encoder (`NxEncoder`) and `StreamDecoder` stream.
* Test material is synthetic. Validation on real float recordings is the next
  step.
* About 0.8 points behind WavPack's slowest mode on stereo music (WavPack uses
  sample-adaptive filters; see RESEARCH.md).
