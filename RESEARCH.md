# VFA research notes — answers to the project brief's questions (§ 13)

All numbers come from `vfa bench --seconds 10 --studies` (full tables in
`BENCHMARK.md`, reproducible) and `cargo run --release --example entropy_study`.
"bits/sample" is the total file size × 8 divided by the number of samples
(all channels); uncompressed float32 is 32.

Test material is synthesized in `f64` and rounded once to `f32`, like a DAW
float render, so it carries full 24-bit significand precision. Exceptions are
the materials that explicitly model a 24/16-bit integer source. The "music" is
synthetic (harmonic voices, drums with noise components, room delay, ~30 dB
dynamics). It is noisier than many real recordings, so absolute ratios on real
material will differ. The *relative* findings are what matter here.

---

## Q1. How many bits does an arbitrary float32 pattern need when coded independently?

**32.** There are 2^32 distinct patterns. By the pigeonhole principle any
lossless code that makes some patterns shorter must make others longer, so no
code is shorter than 32 bits on average over uniformly random patterns. This
holds for any scheme, sample-wise or not.

Measured: `17_random_bits` costs 32.07 bits/sample (100.2 %) in VFA, where every
block falls back to `Raw`. The 0.07 bits are block headers, mode bits and the
seek index. WavPack expands the same data to 124.6 %.

So compression can only come from *redundancy in real signals*:

* unused exponent range (the exponent of audio samples varies little)
* known-zero low bits (integer-origin material)
* correlation between neighbouring samples (prediction)
* correlation between channels (stereo decorrelation)

## Q2. Which patterns benefit from leading/trailing-zero coding?

Measured directly with the two pure "sample-wise variable length" modes
(single-mode study):

| material | XorLz (XOR + leading/trailing zeros) | FieldVlc (sign + Δexp + trailing-zero coded mantissa) | FloatSplit (prediction) |
|---|---:|---:|---:|
| silence | 1.09 | 8.10 | 0.09 |
| transients (silence between hits) | 6.78 | 11.71 | 4.49 |
| 16-bit-origin music | 14.38 | 14.69 | 7.21 |
| 24-bit-origin music | 22.52 | 21.58 | 15.13 |
| float-rendered music (any level, −80 … +769 dBFS) | 32.05 | 25.59 | 21.56 |
| sine | 32.07 | 27.14 | 9.50 |
| white noise | 32.07 | 27.13 | 26.21 |

* **Trailing zeros** only exist when the signal came from an integer source,
  or contains exact zeros or values with short mantissas. For float-rendered
  audio the mantissa is essentially full: its zero-order entropy is
  22.97 of 23 bits for every music level. That is why per-sample
  trailing-zero coding cannot shrink float renders much.
* **Leading zeros of the XOR with the previous sample** (Gorilla-style, as used
  in time-series databases) work only for repeated or near-identical values:
  silence, sparse transients. For continuously changing audio the XOR has
  almost no leading zeros.
* **Exponent coding** is where per-sample coding genuinely saves bits. The
  exponent field has 2–3.4 bits of zero-order entropy instead of 8. FieldVlc
  turns that into ~25.6 bits/sample on float music (−20 %).

Conclusion: leading/trailing-zero coding is valuable as a *block-level*
property, not per sample. VFA uses it in two places:

* the **precision floor** (lowest set bit over a block), which removes
  known-zero bits for integer-origin material;
* **Constant** blocks and all-zero Rice partitions, which code silence in
  ~0 bits.

## Q3. How large is the overhead of signalling variable sample lengths?

In VFA the per-sample length is **implicit**: once the coarse integer `c` is
decoded, its bit length fixes the sample's exponent, and the exponent fixes how
many extra significand bits follow (`n = g − lsb(msb(c))`). No length field is
transmitted. The remaining signalling cost is:

| item | cost |
|---|---|
| block header | 20 bytes per block |
| seek index entry | 16 bytes per block |
| mode / stereo fields | 3 bits per channel + 2 bits per block |
| FloatSplit header (grid, floor, flags) | ≈ 12–20 bits per channel and block |
| small samples (`|x| < 2^g`) | 1 flag bit + γ-coded exponent per small sample |
| exceptions (NaN/Inf) | γ(Δindex) + 32 bits each |

At the default 2048-frame stereo block this totals ≈ 0.23 % of the raw size
(measured: 100.2 % on random data). In explicit-length schemes like FieldVlc
the length information (Rice-coded Δexponent and trailing-zero count) costs
≈ 2.5–4 bits per sample. That cost is the main reason such schemes stay far
behind prediction.

## Q4. Is sample-wise coding more efficient than block floating point?

Neither alone is best. The combination is.

* **Pure block floating point** (one shared exponent, integer mantissas) is
  `FloatSplit` with the grid at the block's finest set bit. On float renders the
  finest bit belongs to the smallest sample, so the integers become 40–60 bits
  wide. At grid −40 for a music block: 30.8 bits/sample even with prediction.
  It is optimal only for integer-origin data, where the floor equals the source
  resolution (24-bit origin: 15.1 bits/sample, on par with FLAC on the integer
  data: 46.8 % vs VFA 47.3 %).
* **Pure sample-wise coding** without prediction (FieldVlc) reaches ~25.6 bits
  on float music.
* **FloatSplit** = block-level grid exponent + per-sample implicit extra bits +
  prediction: 21.56 bits/sample. Measured on one block of float music, the
  total is almost constant over a wide range of grid exponents
  (−27 … −15: 20.14 bits): bits move between the predicted coarse part and the
  raw extra bits. This shows the extra bits are genuine, incompressible
  information (rounding residue of a float render). It also means the encoder
  does not need an exhaustive grid search.

## Q5. How to optimise for values with different exponents?

* The exponent is never coded explicitly for samples with `|x| ≥ 2^g`. It is
  implied by the magnitude of the predicted coarse integer, so a smooth signal
  crossing exponent boundaries costs nothing extra.
* The grid exponent `g` is chosen per block and channel from the loudest sample
  (`g = msb_max − 23 − k`, a few `k` evaluated). The result is **level
  invariant**: the same music at −80, +24, +100, +200 dBFS and at 3·10^38
  (≈ +769.5 dBFS) compresses to 21.56 bits/sample in every case. In other words
  there is no penalty above 0 dBFS, and no 0 dBFS ceiling.
* Samples far below the grid (near zero crossings, `|x| < 2^g`) are escaped
  with an Elias-γ exponent relative to the grid plus the mantissa.
* Smaller blocks track exponent and statistics changes better. Block-size study
  (float music): 256 → 22.10, 1024 → 21.51, 2048 → 21.56, 4096 → 21.75,
  16384 → 22.34 bits/sample. The default is 2048: within 0.05 bits of the
  optimum, and lower per-block overhead than 1024 on incompressible data.

## Q6. How are NaN and subnormal values handled?

* **NaN / ±Inf**: FloatSplit stores them as *exceptions*: index (γ-coded
  delta) + verbatim 32-bit pattern, so every NaN payload and both quiet and
  signalling NaNs survive. A placeholder coarse value keeps the predictor
  undisturbed. When a block is dominated by specials, the adaptive selector
  picks `FieldVlc`, which codes any pattern natively (`16_special_values`:
  95.5 % vs WavPack 116 %).
* **Subnormals** are handled by the same exact integer arithmetic. Their
  significand has no implicit bit and `lsb = −149`. `15_subnormal_range`
  (music peaking at 10^−36, subnormal near zero crossings) compresses to
  20.73 bits/sample.
* **−0.0** is preserved: it is a "small" sample with sign bit 1.
* Verified exhaustively: the grid split/join is bit exact for **all 2^32
  patterns** at 7 grid exponents (−149, −60, −23, 0, 23, 100, 127), and
  XorLz/FieldVlc round-trip all 2^32 patterns (`cargo test --release -- --ignored`).

## Q7. Which entropy coder gives the best compression/speed trade-off?

Measured on the actual FloatSplit prediction residuals (bits/sample of the
coarse stream):

| material | partitioned Rice (VFA) | adaptive Rice (WavPack/Monkey style) | ideal adaptive arithmetic / rANS (Laplace model) |
|---|---:|---:|---:|
| sine | 5.655 | 5.861 | 5.663 |
| pink noise | 22.737 | 22.885 | 22.705 |
| float music | 19.543 | 19.610 | 19.468 |
| compressed music | 19.796 | 19.863 | 19.720 |
| 24-bit-origin music | 15.442 | 15.510 | 15.368 |
| 16-bit-origin music | 7.477 | 7.558 | 7.390 |

* **Partitioned Rice** (per-partition optimal parameter, all-zero and raw
  partitions) is within **0.03–0.09 bits/sample (≤ 0.5 %)** of an ideal
  adaptive arithmetic coder under a Laplace model.
* A range coder or rANS would add complexity and cost decode speed for under
  0.5 % gain. Not worth it at this stage.
* Huffman has no advantage over Rice for geometric-like residuals.
* Golomb codes with non-power-of-two divisors would gain < 0.03 bits.
* The extra bits of float renders are uniform random, so they are stored raw.
  That is optimal.

Decode speed with partitioned Rice: ~450–550× real time on one core for
stereo 48 kHz (≈ 85–100 MB/s of float32 per core), ~1200–1700× with 4 threads.

## Q8. Can the strategy be chosen adaptively?

Yes, and that is what VFA does. Per block and channel the encoder computes the
exact size of every mode and keeps the smallest:

* Constant and Raw (fallback)
* XorLz and FieldVlc
* FloatSplit at several grids, each with its best predictor (verbatim,
  fixed 1–4, LPC 4–32 with several coefficient precisions)
* for stereo, additionally left/side, side/right and mid/side coupling

What gets selected: FloatSplit for all real-audio material, Constant for
silence (68 % of the transient material's blocks), FieldVlc for
special-value-heavy data, Raw for noise-like bit patterns. Because Raw is
always a candidate, **no block can ever cost more than raw + header**.

---

## Comparison with WavPack and FLAC

WavPack 5.6 `-hhx` (its strongest float-lossless mode) and FLAC 1.4 `-8` (only
for material that is exactly representable as 16/24-bit integers):

| material | VFA | WavPack -hhx | FLAC -8 |
|---|---:|---:|---:|
| sine 1 kHz | **29.7 %** | 49.5 % | – |
| white noise | **81.9 %** | 82.2 % | – |
| pink noise | 80.7 % | **80.4 %** | – |
| float music (all levels) | 67.4 % | **65.8 %** | – |
| 24-bit-origin music | 47.3 % | **45.7 %** | 46.8 % |
| 16-bit-origin music | 22.5 % | **21.3 %** | 22.2 % |
| special values (NaN/Inf/−0/subnormal) | **95.5 %** | 116.0 % | – |
| random bit patterns | **100.2 %** | 124.6 % | – |
| 5.1 / 96 kHz | **63.0 %** | 63.5 % | – |

Timing (10 s stereo float music): VFA decodes in 21 ms on one core (8.5 ms
with 4 threads), WavPack in 52 ms. VFA encodes in 425 ms at level 5, WavPack in
114 ms. **Caveat:** the WavPack/FLAC times are wall-clock times of the
command-line tools, including process start and file I/O, whereas the VFA
times are in-memory library calls. So decoding is *at least* competitive, but
the 2.5× figure overstates the codec difference. VFA encoding at the default
level is clearly slower (~4×). Level 2 encodes at ~40× real time for
+0.01 bits/sample.

### Where the remaining 1.5 points vs WavPack on music come from

Not from entropy coding (Q7 shows ≤ 0.5 % headroom). WavPack's `-x` mode runs
*sample-adaptive* decorrelation filters (cascaded sign-LMS) that follow the
signal continuously and exploit cross-channel correlation per sample. VFA's
LPC is fixed per block. Raising the VFA level from 5 to 8 (32-tap LPC, more
grids) gains only 0.02 bits, so a better *block* predictor will not close the
gap. An adaptive filter stage would.

## Next steps (proposed)

1. **Adaptive residual stage**: an NLMS/sign-LMS cascade after the block LPC,
   including the other channel's previous samples. This is the measured gap to
   WavPack (~1.5 points).
2. **Float64** (`sample_format = 2`): same structure with 53-bit significands
   and `i128` or 2-limb coarse values.
3. **Faster encoder**: reuse autocorrelation across grid candidates (LPC
   coefficients barely depend on `g`). Ranking-only estimates for
   precision/order selection.
4. **Real-world corpus**: validate on real 32-bit-float recordings, DAW stems
   and mastering renders, not only synthetic material.
5. RF64 / BW64 input for files > 4 GiB, and a memory-mapped reader.

---

# Part 2 — Format 1.1 optimizations

Implemented incrementally in the order of the project plan. After each stage
the full dataset was benchmarked against the previous stage. Every number below:

* is the **complete file size**, including file header, block headers, mode
  fields, coding parameters, tables, metadata and seek index;
* comes from an encode that was **decoded and compared bit for bit** with the
  input.

Dataset: the 18 original materials plus 8 new ones (constant DC, ±0 runs, ±Inf
bursts, NaN payloads, bit-exact periodic wavetable, dual mono, polarity
inversion, sparse events). Stage tables use 5 s per material and are in
`docs/stages/*.md` (reproduce with `cargo run --release --example stage_study -- <stage>`).
The final benchmark uses 10 s per material (`BENCHMARK.md`). Timings are on 4 cores.

## Summary (ablation, `docs/stages/ablation.md`)

| configuration | total vs WAV | encode s (4 threads) | decode s (1 thread) |
|---|---:|---:|---:|
| stage 0 = format 1.0 tools | 53.29 % | 1.56 | 0.271 |
| only + XorChimp | 51.69 % | 1.86 | 0.250 |
| only + RLE | 50.95 % | 1.72 | 0.235 |
| only + container 1.1 (compact headers / index) | 53.17 % | 1.53 | 0.256 |
| only + adaptive block sizes | 52.61 % | 4.01 | 0.277 |
| only + Huffman / rANS / range coders | 53.09 % | 2.01 | 0.295 |
| only + inter-channel transforms | 50.96 % | 1.96 | 0.257 |
| stage 1 (XOR) | 51.69 % | 1.84 | 0.237 |
| stages 1–2 (+ RLE) | 49.96 % | 1.93 | 0.234 |
| stages 1–3 (+ adaptive blocks, container 1.1) | 49.30 % | 6.04 | 0.244 |
| stages 1–4 (+ entropy coders) | 49.29 % | 7.83 | 0.253 |
| stages 1–5 (+ multichannel) | 47.18 % | 8.28 | 0.243 |
| **default = level 5** (stages 1–5, Rice only) | **47.22 %** | 6.35 | 0.244 |
| level 2 preset (fixed blocks, all modes) | 48.09 % | **1.40** | **0.214** |
| level 8 preset | 47.22 % | 14.00 | 0.249 |

Strict before/after with the **frozen format-1.0 binary** on the identical
10-second WAV files (`docs/stages/v1_0_binary_comparison.md`):

* total size: **53.39 % → 47.34 %** (−11.3 %), and 48.18 % at level 2;
* all 26 format-1.0 files still decode with the 1.1 decoder into
  byte-identical WAV files.

Level 2 is both smaller and faster than the previous implementation.

## Stage 1 — XOR-based bit-pattern coding (`docs/stages/xor.md`)

Three ways of reusing previous bit-window information were compared, each as
the only mode plus the Raw fallback:

| variant | total | float music | NaN payloads | periodic wavetable |
|---|---:|---:|---:|---:|
| raw | 100.20 % | 32.07 | 32.07 | 32.07 |
| XorLz: Gorilla, exact previous window | 82.07 % | 32.06 | 30.73 | 32.07 |
| XorChimp N=1: bucketed leading zeros, reuse of previous bucket | 71.76 % | 26.71 | 24.44 | 27.50 |
| XorChimp N=16 references | 71.37 % | 26.55 | 15.70 | 27.04 |
| XorChimp N=128 references | 70.16 % | 26.58 | 17.47 | 9.71 |
| XorChimp, window chosen per block | 66.54 % | 26.71 | 15.64 | 9.71 |

Findings:

* **Exact-window reuse (Gorilla) fails on audio.** Consecutive float samples
  rarely share an exact leading/trailing-zero window, so nearly every sample
  pays the 12-bit window header.
* **Bucketing leading zeros to 8 classes** and reusing the previous bucket is
  what makes XOR coding work on audio (−17 % vs Gorilla).
* **Multi-reference windows** pay off for repeated values (NaN payload
  patterns, ±Inf runs) and periodic signals. A cheap repetition profile
  decides whether wide windows are evaluated at all.
* **XOR coding never beats prediction on regular audio** (26.7 vs 21.5
  bits/sample on music), so the adaptive selector uses it only where it wins:
  NaN payloads 25.10 → 15.64, special values 30.56 → 25.80.
* **Total effect:** 53.29 % → 51.69 %.

Exactness: XorChimp round-trips all 2^32 patterns, both in sequence and
bijectively scrambled (`tests/exhaustive.rs`).

## Stage 2 — Run-length / repeated-sequence coding (`docs/stages/rle.md`)

The parse uses three tokens over raw `u32` patterns: `LIT`, `RUN` (repeat the
previous sample) and `COPY(dist, len)` (LZ77-style). Literals are collected
into one stream coded by the best of Raw / XorLz / FieldVlc / XorChimp, which
is how RLE and XOR coding are *combined*. +0.0 and −0.0 never merge, because
the comparison is on bit patterns.

* **Total:** 51.69 % → **49.96 %**.
* **Periodic wavetable:** 9.71 → 1.16 bits/sample.
* **1 kHz sine:** 9.48 → 1.78. At 48 kHz the period is exactly 48 samples and
  the float32 rounding repeats bit-identically. This is a property of
  synthetic test tones, not of recordings.
* **Signed-zero runs:** 1.13 → 0.60.
* **Music:** no change. RLE is selected only where the complete encoding is
  smaller.

Long digital silence in an otherwise active block was already cheap before
this stage: FloatSplit has all-zero Rice partitions and zero extra bits for
+0.0. RLE adds silence made of −0.0, constant non-zero values and sequence
repetition.

## Stage 3 — Adaptive block sizes (`docs/stages/blocks.md`)

Format changes:

* header flag *variable block sizes* (`block_frames` becomes the maximum);
* compact 12-byte block headers instead of 20 bytes;
* compact LEB128 seek index.

The encoder splits each 8192-frame super-block recursively in halves down to
256 frames. A split is kept only if the two halves, *including their block
headers and index entries*, are smaller than the parent.

| configuration | total | encode s |
|---|---:|---:|
| stage 2 (fixed 2048, 1.0 container) | 49.96 % | 1.93 |
| fixed 512 / 1024 / 2048 / 4096 (compact container) | 50.52 / 49.94 / 49.85 / 50.09 % | ~2.0 |
| adaptive 256–8192, decisions at level 0 | 49.40 % | 5.55 |
| **adaptive 256–8192, decisions at level 1 (stage 3)** | **49.30 %** | 5.93 |
| adaptive 256–8192, decisions at level 5 (exhaustive) | 49.29 % | 10.24 |
| adaptive 128–16384, level 1 | 49.23 % | 7.08 |

* **Search cost:** evaluating splits with a cheap encoder gets 99 % of the
  exhaustive search's gain at 58 % of its cost. The search also skips
  inter-channel and stereo trials, which shrink parent and child blocks alike;
  the final encode uses everything.
* **Which sizes get chosen:** transient material gets small blocks
  (`21_infinities`: 1101 blocks in 10 s, isolating the Inf bursts).
  Predictable and periodic material gets the maximum size (wavetable: 59
  blocks; 1.12 → 0.29 bits/sample). Music averages ~1500 frames per block.
* **Cost:** encoding is ~3× slower, so levels 0–2 keep fixed blocks.

## Stage 4 — Entropy coders and adaptive selection (`docs/stages/entropy.md`)

Huffman (canonical, length-limited), static rANS (12-bit frequencies) and an
adaptive LZMA-style binary range coder (context = previous symbol class) are
implemented for prediction residuals. They share a 136-symbol alphabet with
raw low bits. Per coarse stream the encoder takes the smallest *exact* size,
including code tables, frequency tables and flush bytes. Cheap exact or
lower-bound estimates skip coders that cannot win.

| coders | total | encode s | decode s |
|---|---:|---:|---:|
| Rice (stage 3) | 49.30 % | 6.00 | 0.241 |
| + Huffman | 49.30 % | 6.08 | 0.239 |
| + rANS | 49.30 % | 6.21 | 0.237 |
| + range coder | 49.30 % | 7.05 | 0.238 |
| all | 49.29 % | 7.58 | 0.246 |

This confirms the Part 1 analysis (research question 7): partitioned Rice is
within ~0.5 % of an ideal adaptive coder on these residuals. The largest
per-material gain is 0.05 bits/sample. All coders remain selectable
(`--entropy`); the default is Rice only, since +26 % encode time for 0.01
points is not justified.

## Stage 5 — Multichannel decorrelation (`docs/stages/multichannel.md`)

Ordinary float addition and subtraction are not reversible: `(a + b) − b ≠ a`
in general, and −0.0 and NaN payloads would be lost. Every transform here is
therefore integer or bitwise:

* **stereo L/S, S/R, M/S** (existing): on the exact integer coarse values;
* **FloatSplitRef** (new): `d = c_k − ((w₁·c_j₁ + w₂·c_j₂) >> 6)` over the
  coarse integers of up to two earlier channels at the same grid. The 10-bit
  weights are chosen by least squares; the decoder adds back the identical
  integer prediction. It generalises L/S (w = 64) to any channel count, scaled
  copies and sums (e.g. C = (L + R)/2).
* **XorRef** (new): XOR with an earlier channel's raw bit patterns, then any
  simple mode. Dual mono becomes Constant(0) and polarity inversion
  Constant(0x80000000).

| configuration | total | 5.1 / 96 kHz | dual mono | polarity inverted | transients |
|---|---:|---:|---:|---:|---:|
| independent channels | 51.52 % | 19.93 | 21.40 | 21.40 | 4.75 |
| stereo L/S, S/R, M/S | 49.30 % | 19.93 | 11.68 | 12.19 | 4.55 |
| + XorRef | 49.05 % | 19.93 | 10.74 | 10.74 | 4.55 |
| + FloatSplitRef | 47.43 % | 17.00 | 11.68 | 11.69 | 4.07 |
| + both (stage 5) | **47.22 %** | **17.00** | **10.74** | **10.74** | **4.07** |

Dual mono and polarity inversion reach exactly half of the independent size:
the second channel costs a few bits per block.

**Finding: levels were not monotone with inter-channel prediction** (fixed
in this stage). A channel's size is almost flat across grid exponents, so a
more thorough independent search picked a high grid more or less at random.
That moves bits from the predicted coarse part into raw per-channel extra
bits, which no inter-channel transform can reach. On 5.1, levels 5–8 got
*worse* (17.34 → 17.47). The fix: among grid candidates within 0.25 % of the
best, prefer the finest grid. Levels are monotone again (17.20 / 17.17 /
17.17 for levels 3 / 5 / 8), and independent coding is unchanged.

Safety: a dedicated property test encodes correlated multichannel material
(copies, sign flips, scaled copies, sums, injected NaN/Inf/±0) with all
transform combinations and verifies every bit (`tests/roundtrip.rs`).

## Stage 6 — Indexing, random access, parallelism, memory (`BENCHMARK.md`)

* **Index overhead** (compact): 0.05 % of the file for music (~4 bytes per
  block), 0.02 % for 5.1. It is relatively large only for nearly empty files:
  183 bytes in a 1.5 KB silence file. With `--index none` the decoder scans
  the block headers instead and keeps all metadata.
* **Seeking** by sample (binary search over the index) or by time
  (`decode_time_range`, CLI `--start-time/--duration`) decodes only the
  overlapping blocks. A random 10 ms read costs ~170 µs for stereo music and
  ~600 µs for 6-channel 96 kHz.
* **Parallel decoding** (blocks are independent), music, × real time:
  491 (1 thread) → 779 (2) → 1644 (4 threads); 5.1/96 kHz: 76 → 151 → 292.
  Encoding: 8 → 28× (music), 1 → 4× (5.1/96 kHz).
* **Memory:** decoding now writes blocks in parallel straight into the output
  buffer. Peak heap is 1.02–1.09× the decoded size, down from 2.0×. Encoding
  peaks at ~3.3× the input (input copy + output + one batch of super-blocks).

## Compliance with the safety rules

| rule | how it is guaranteed | how it is checked |
|---|---|---|
| 1 no lossy compression · 8 only exact methods | every mode is an exact bijection on bit patterns; the decoder rejects any stream that does not map back to a valid pattern | round trips of all materials, property tests, all-2^32-pattern tests, fuzzing |
| 2 no quantization/rounding · 3 no normalization · 4 no clipping | no floating-point arithmetic on samples anywhere (floats only *choose* LPC/LS weights; the transforms are integer) | +24/+100/+200 dBFS and near-`f32::MAX` materials round-trip |
| 5 signed zero · 6 NaN payloads | comparisons and transforms on raw `u32`; −0 is an escaped "small" sample, NaN/Inf are exceptions or bit-coded | `20_signed_zeros`, `22_nan_payloads`, `16_special_values`, exhaustive tests |
| 7 no lossy integer PCM conversion | coarse integers are exact images of the float bits at a grid; extra bits below the grid are kept | `ieee754` split/join verified for all 2^32 patterns at 7 grids |
| 9 raw fallback per block | Raw is always a candidate for every channel of every block | random data: 100.0 % (+0.03 % container overhead) |
| 10 decisions include all overhead | mode, block-split, coder and transform choices compare exact complete sizes (headers, parameters, tables, index entries) | `stats must match payload size` unit checks |

## Encoder speed

The new search costs encode time. Default level 5 encodes stereo music at ~8×
real time on one core (~28× on 4), versus ~23× for format 1.0. The level 2
preset encodes faster than 1.0 (1.40 s vs 1.56 s for the dataset) and still
gives most of the gain (48.09 % vs 53.29 %). Level 8 is not worth using: it
equals level 5 at twice the time. Decoding speed is unchanged by all stages
(0.21–0.25 s for the dataset on one thread).

## Caveats

* All material is synthetic. The very large gains on the sine and the
  wavetable come from exact bit-level periodicity that real recordings do not
  have. Music, noise and multichannel results are the relevant ones for real
  use, and those need confirming on a real-world corpus.
* Stage 0 in the ablation (53.29 %) differs from the frozen-binary number
  (53.39 % for 10 s files, 53.33 % for 5 s files). Two reasons: the grid
  tie-break from stage 5 also applies to the 1.0 tool set, and the material
  lengths differ.
