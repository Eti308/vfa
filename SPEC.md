# VFA — Variable Float Audio — Format Specification

Version 1.1 (draft) · file extension `.nx` · sample format: IEEE-754 binary32

This document is normative and independent of the reference implementation
in `src/`. "MUST" requirements are checked by the reference decoder.

**Version 1.1** is a strict superset of 1.0: a 1.1 decoder decodes every 1.0
file unchanged (verified against stored 1.0 fixture files). Additions are
marked *(1.1)*: variable block sizes, compact block headers, compact /
optional seek index, sub-frame modes 5 (XorChimp), 6 (Rle) and 7 (extended:
FloatSplitRef, XorRef), and alternative residual entropy coders.

## 1. Principles

* **Bit exactness.** For every input sample bit pattern `b` (any of the
  2^32 patterns, including ±0, subnormals, ±Inf and all NaN payloads) the
  decoder outputs exactly `b`. Sample rate, channel count, channel order and
  sample order are preserved.
* **No floating-point arithmetic on samples.** All transforms are integer /
  bit operations, so results are identical on every platform.
* **Bounded worst case.** Every block can fall back to verbatim storage
  (mode Raw), so incompressible input costs at most
  `H + ⌈(2 + 3·C + 32·C·N)/8⌉` bytes per block of `N` frames and `C` channels
  (`H` = 20, or 12 with compact headers), plus the seek index (16 bytes per
  block legacy, ~3–5 bytes compact, 0 without) and ~100 bytes per file.
  With the reference encoder's defaults (compact headers and index, 8192-frame
  blocks for incompressible data) that is ≈ 0.03 % for stereo.
* **Never larger than necessary.** An encoder MUST choose, per block and
  channel, a representation no larger than Raw; every size comparison
  includes all headers, parameters and tables.
* **Independent blocks.** Each block decodes without any other block:
  random access, parallel decoding and error isolation.

## 2. Conventions

* Byte-level fields are **little endian**.
* Bit-level payloads are packed **MSB first**; the last byte of a payload is
  zero padded.
* `u(n)`: `n`-bit unsigned field. `s(n)`: `n`-bit two's complement.
* `γ(x)`: Elias gamma code of `x ≥ 1`: `⌊log2 x⌋` zero bits followed by `x`
  in binary (`⌊log2 x⌋+1` bits).
* `zigzag(v) = (v << 1) ^ (v >> 63)` on 64-bit integers; `unzigzag` its inverse.
* `Rice_k(u)`: let `q = u >> k`. If `q < 32`: `q` zero bits, a one bit, and
  the low `k` bits of `u`. Otherwise (escape): 32 zero bits, `u(7)` length
  `L = bitlen(u)` (MUST be ≤ 64), then `u` in `L` bits.
* All integer arithmetic in predictors and stereo reconstruction is
  **wrapping 64-bit two's complement**; `>>` on signed values is arithmetic.

## 3. File layout

```
FileHeader                36 bytes
Block[0] … Block[B-1]     fixed sizes: B = ceil(total_frames / block_frames)
                          variable sizes (1.1): frame counts sum to total_frames
MetadataSection
SeekIndex
Trailer                   24 bytes
```

### 3.1 FileHeader

| offset | size | field | value |
|---:|---:|---|---|
| 0 | 4 | magic | `56 46 41 1A` ("VFA", 0x1A) |
| 4 | 1 | version_major | 1 |
| 5 | 1 | version_minor | 0 or 1 (a 1.0 file MUST have flags = 1) |
| 6 | 2 | header_size | 36 |
| 8 | 1 | sample_format | 1 = IEEE-754 binary32 (2 = binary64, reserved) |
| 9 | 1 | flags | bit 0: block CRCs present (MUST be 1) · bit 1 *(1.1)*: variable block sizes · bit 2 *(1.1)*: compact block headers · other bits MUST be 0 |
| 10 | 2 | channels | 1 … 1024 |
| 12 | 4 | sample_rate | Hz, ≥ 1 (any rate: 44 100, 48 000, 96 000, 192 000, 384 000, …) |
| 16 | 8 | total_frames | frames per channel |
| 24 | 4 | block_frames | 1 … 2^20: frames per block (fixed), or the maximum (variable); ≤ 2^16 with compact headers |
| 28 | 4 | reserved | 0 |
| 32 | 4 | header_crc | CRC-32 (IEEE 802.3) of bytes 0 … 31 |

### 3.2 Block

Long form (flags bit 2 = 0):

| offset | size | field |
|---:|---:|---|
| 0 | 2 | sync `FA B1` |
| 2 | 1 | flags (0) |
| 3 | 1 | reserved (0) |
| 4 | 4 | block_index (0-based, sequential) |
| 8 | 4 | frame_count |
| 12 | 4 | payload_len (bytes) |
| 16 | 4 | payload_crc: CRC-32 of the payload |
| 20 | payload_len | payload (§ 5) |

Compact form *(1.1)* (flags bit 2 = 1), 12 bytes:

| offset | size | field |
|---:|---:|---|
| 0 | 2 | sync `FA B2` |
| 2 | 1 | block_index mod 256 |
| 3 | 2 | frame_count − 1 |
| 5 | 3 | payload_len (u24) |
| 8 | 4 | payload_crc |
| 12 | payload_len | payload |

`frame_count` MUST equal `min(block_frames, total_frames − index·block_frames)`
with fixed block sizes, and MUST be in `1 … block_frames` with variable sizes
(flags bit 1). `payload_len` MUST NOT exceed `13·frame_count·channels + 64`.
Blocks are independent: a block decodes without any other block.

### 3.3 MetadataSection

`count: u32`, then `count` entries `tag: [4]u8, len: u32, data: [len]u8`.
Defined tags (unknown tags MUST be ignored):

| tag | content |
|---|---|
| `WAVP` | original WAV bytes preceding the sample data |
| `WAVS` | original WAV bytes following the sample data |
| `SCRC` | CRC-32 of all samples, interleaved, as little-endian bytes (u32 LE) |
| `ENCR` | UTF-8 encoder description |

With `WAVP` + `WAVS` the decoder reproduces the original WAV file byte for
byte (`WAVP ‖ samples ‖ WAVS`).

### 3.4 SeekIndex

Three forms, distinguished by the first u32:

* **legacy** (1.0): `count: u32` (= B), then per block `offset: u64` (from file
  start), `first_frame: u64`. Not allowed with variable block sizes.
* **compact** *(1.1)*: `0xFFFFFFFF`, `LEB128(count)`, `LEB128(offset of block 0)`,
  then per block `LEB128(total block bytes incl. header)` and, with variable
  block sizes only, `LEB128(frame_count)`. Offsets and first frames are
  prefix sums. Typically 3–5 bytes per block.
* **none** *(1.1)*: `count = 0`. The decoder rebuilds the index by walking the
  block headers (metadata remains available).

An index MUST be consistent with the blocks (frame counts summing to
`total_frames`, increasing offsets); otherwise a decoder SHOULD fall back to
scanning. Seeking by sample position is a binary search over first frames;
seeking by time `t` uses frame `⌊t · sample_rate⌋` (positions only — sample
values never involve floating point).

### 3.5 Trailer (last 24 bytes)

`meta_offset: u64, index_offset: u64, footer_crc: u32, magic "VFAE"`.
`footer_crc` is the CRC-32 of the bytes from `meta_offset` up to the trailer.
If the trailer is missing or damaged, a decoder MAY rebuild the index by
walking the block headers from offset 36 (metadata is then lost). Streaming
decoders need neither index nor trailer.

## 4. Float32 decomposition at a grid exponent

For a finite non-zero pattern with sign `s`, biased exponent `e`, fraction `m`:

* `sig = m | 2^23`, `lsb = e − 150` if `e ≥ 1`; `sig = m`, `lsb = −149` if `e = 0`
* `|x| = sig · 2^lsb`, `msb = lsb + bitlen(sig) − 1`

At grid exponent `g` (−149 ≤ g ≤ 127) and precision floor `f ≤ g`:

| class | condition | coarse `c` | extra data |
|---|---|---|---|
| Zero | pattern `0x00000000` | 0 | — |
| Small | `msb < g`, or pattern `0x80000000` (−0) | 0 | escape (§ 6.4) |
| Coarse | `msb ≥ g` and `msb − g + 1 ≤ 60` | `±⌊|x| / 2^g⌋` | `n = max(0, g − lsb)` low bits of `sig` |
| Exception | `e = 255` (Inf/NaN), or `msb − g + 1 > 60` | placeholder | 32-bit verbatim (§ 6.2) |

**Decoder reconstruction of a Coarse sample** from `c ≠ 0`:

1. `L = bitlen(|c|)` (MUST be ≤ 60); `msb = g + L − 1` (MUST be ≤ 127)
2. `lsb = max(msb − 23, −149)`
3. if `lsb ≥ g`: the low `lsb − g` bits of `|c|` MUST be zero; `sig = |c| >> (lsb − g)`, `n = 0`
4. else `n = g − lsb`; `z = clamp(f − lsb, 0, n)`; read `n − z` bits `r`;
   `sig = (|c| << n) | (r << z)`
5. if `msb ≥ −126`: `e = msb + 127`, `m = sig & 0x7FFFFF`; else `e = 0`, `m = sig`
6. pattern = `(c < 0) << 31 | e << 23 | m`

The `z` bits below the precision floor are known zeros and not stored.

## 5. Block payload

```
[C == 2]  stereo_mode: u(2)       0 independent, 1 left/side, 2 side/right, 3 mid/side
for ch in 0..C:  mode: u(3) [mode == 7: ext: u(4)] ; phase-1 data of ch
for ch in 0..C:  if mode[ch] ∈ {FloatSplit, FloatSplitRef}: phase-2 data of ch
phase 3: for ch in 0..C with mode XorRef (ascending): s_ch[i] ^= s_ref[i]
zero padding to a byte boundary
```

`stereo_mode ≠ 0` requires both channels to be FloatSplit (mode 4, not 7)
with equal grid `g`; otherwise the payload is invalid. In that case the coarse streams decode
to `(a, b)` and the channel coarse values are

| stereo_mode | left | right |
|---|---|---|
| 1 left/side | `a` | `a − b` |
| 2 side/right | `a + b` | `b` |
| 3 mid/side | `(S + b) >> 1` | `(S − b) >> 1`, where `S = (a << 1) \| (b & 1)` |

Phase-2 data is read after the stereo reconstruction.

## 6. Sub-frame modes (per channel, `N` = frame_count)

### 6.0 Constant (mode 0)
`value: u(32)`; all `N` samples equal `value`.

### 6.1 Raw (mode 1)
`N × u(32)`.

### 6.2 XorLz (mode 2)
`first: u(32)`; then for each further sample, with `x = sample ^ previous`:
`0` → `x = 0`; `10` + `(32 − wlz − wtz)` bits → `x` within the current
window (MUST have been defined); `11 lz:u(5) (len−1):u(5) bits:u(len)` →
new window `(lz, 32 − lz − len)` (MUST satisfy `lz + len ≤ 32`).

### 6.3 FieldVlc (mode 3)
`k_e: u(4), k_t: u(3)`, then per sample:
`sign: u(1)`, `Rice_{k_e}(zigzag(e − e_prev))` (`e_prev` starts at 127,
result MUST be in 0 … 255), `t = Rice_{k_t}(…)` (MUST be ≤ 23),
and if `t < 23`: `(22 − t)` bits `h`, mantissa `m = ((h << 1) | 1) << t`;
if `t = 23`: `m = 0`.

### 6.4 FloatSplit (mode 4)

**Phase 1**

```
grid:      u(9)         g = grid − 149, MUST be in −149 … 127
floor:     γ(g − f + 1)
has_small: u(1)
exc_count: γ(count + 1)          count ≤ N
exceptions: count × { γ(index − previous_index) ; bits: u(32) }   (previous_index starts at −1; index < N)
coarse stream (§ 7)
```

**Phase 2** — for each sample index `i` in order that is not an exception:

* `c[i] ≠ 0`: the `n − z` extra bits of § 4.
* `c[i] = 0` and `has_small = 0`: the sample is `+0.0`; nothing is read.
* `c[i] = 0` and `has_small = 1`: `flag: u(1)`; `0` → `+0.0`; `1` → small
  sample: `sign: u(1)`, `d = γ(…) − 1` with `e = eb − d` (`eb = clamp(g + 126, 0, 254)`,
  MUST be `d ≤ eb`), then `23 − z` mantissa bits `r`, `m = r << z` where
  `z = clamp(f − lsb_e, 0, 23)`, `lsb_e = (e = 0) ? −149 : e − 150`.

Exception samples take their verbatim value; their coarse placeholder (any
value chosen by the encoder) only participates in prediction.

### 6.5 XorChimp (mode 5) *(1.1)*

`log2n: u(3)` (window `W = 2^log2n` previous samples), `first: u(32)`, then per
further sample `i` (with `LZ = [0, 8, 12, 16, 18, 20, 22, 24]` and `pb` the
previous leading-zero bucket, initially undefined):

| code | fields | sample |
|---|---|---|
| `00` | `r: u(log2n)` | `s[i−1−r]` (MUST have `r < i`) |
| `01` | `r: u(log2n)`, `b: u(3)`, `len−1: u(5)`, `v: u(len)` | `s[i−1−r] ^ (v << (32 − LZ[b] − len))`, MUST `LZ[b] + len ≤ 32`; sets `pb = b` |
| `10` | `v: u(32 − LZ[pb])` | `s[i−1] ^ v` (MUST have `pb` defined) |
| `11` | `b: u(3)`, `v: u(32 − LZ[b])` | `s[i−1] ^ v`; sets `pb = b` |

### 6.6 Rle (mode 6) *(1.1)*

`inner: u(3)` ∈ {1 Raw, 2 XorLz, 3 FieldVlc, 5 XorChimp}, then tokens until
`N` samples are covered (a token MUST NOT exceed the remaining count):

| token | fields | meaning |
|---|---|---|
| `00` | `γ(len)` | `len` literal samples (taken in order from the literal stream) |
| `01` | `γ(len)` | repeat the previous sample `len` times (MUST NOT be at position 0) |
| `10` | `γ(dist) γ(len)` | copy `len` samples from `dist` back, `1 ≤ dist ≤ position`, overlap allowed |
| `11` | — | invalid |

followed by the literal stream (all literal samples, concatenated) coded with
`inner` as a sub-frame of that mode. Comparisons are on raw bit patterns.

### 6.7 Extended modes (mode 7) *(1.1)*

`ext: u(4)`: 0 FloatSplitRef, 1 XorRef, others invalid.

**FloatSplitRef (7.0)** — integer inter-channel prediction for channel `k`:

```
nref−1: u(1)
nref × { γ(k − j) ; w: s(10) }        1 ≤ k − j ≤ k
FloatSplit phase 1 (§ 6.4) whose coarse stream decodes to d[i]
```

Every reference channel `j` MUST be FloatSplit or FloatSplitRef with the same
grid `g`, and the block MUST have `stereo_mode = 0`. Then
`c_k[i] = d[i] + ((Σ w · c_j[i]) >> 6)` (wrapping `i64`, arithmetic shift),
using the reference channels' reconstructed coarse values. Phase 2 is the
FloatSplit phase 2 of channel `k`.

**XorRef (7.1)** — bitwise: `γ(k − j)` (`1 ≤ k − j ≤ k`), `inner: u(3)` ∈
{0 Constant, 1 Raw, 2 XorLz, 3 FieldVlc, 5 XorChimp, 6 Rle}, then a sub-frame
of that mode decoding to `x[i]`. In phase 3 (after all other channels are
complete, in ascending channel order) `s_k[i] = x[i] ^ s_j[i]`.

Neither transform involves floating-point arithmetic: float addition is not
reversible, and these transforms operate only on exact integer images of the
patterns or on the patterns' bits.

## 7. Coarse stream (prediction + residuals)

```
pred_type: u(3)    0 verbatim · 1…4 fixed polynomial order · 5 LPC
[LPC]  order−1: u(5)  precision−1: u(4)  shift: u(5)  coef[order]: s(precision)
[order > 0]  warmup_width: u(7) (≤ 64) ; warmup[order]: zigzag in warmup_width bits
residuals (N − order values, if any): partitioned Rice (§ 7.1) or an alternative coder (§ 7.2)
```

Prediction for `i ≥ order` (wrapping arithmetic):

| type | prediction |
|---|---|
| verbatim | 0 |
| fixed 1 | `x[i−1]` |
| fixed 2 | `2x[i−1] − x[i−2]` |
| fixed 3 | `3(x[i−1] − x[i−2]) + x[i−3]` |
| fixed 4 | `4(x[i−1] + x[i−3]) − 6x[i−2] − x[i−4]` |
| LPC | `(Σ_{j<order} coef[j] · x[i−1−j]) >> shift` |

`x[i] = prediction + unzigzag(residual)`. `order ≤ N` MUST hold.

### 7.1 Partitioned Rice

`p: u(4)` (MUST be ≤ 8), then `2^p` partitions; partition `j` covers values
`[⌊jM/2^p⌋, ⌊(j+1)M/2^p⌋)` of the `M` values. Each partition starts with
`param: u(6)`:

| param | meaning |
|---|---|
| 0 … 58 | Rice parameter `k`; each value `Rice_k` |
| 61 | all values are 0; nothing follows |
| 62 | `width: u(7)` (≤ 64), then each value in `width` bits |
| other | invalid |

### 7.2 Alternative residual coders *(1.1)*

`p = 15` escapes: `coder: u(2)` (1 Huffman, 2 rANS, 3 adaptive range coder; 0
invalid). All three code the same alphabet of 136 symbols: a value `u < 16`
is symbol `u` with no raw bits; otherwise with `L = bitlen(u)` the symbol is
`16 + 2(L − 5) + ((u >> (L − 2)) & 1)` followed by the `L − 2` low bits of `u`
raw.

* **Huffman**: `lo: u(8) hi: u(8)`, code lengths `u(4)` for symbols `lo … hi`
  (0 = unused, ≤ 15, MUST satisfy the Kraft inequality); canonical codes
  (ordered by length, then symbol). Per value: code, then its raw bits.
* **rANS**: `lo: u(8) hi: u(8)`, frequencies `γ(f + 1)` for `lo … hi`, MUST
  sum to 4096; `γ(nbytes + 1)` and `nbytes` bytes; then the raw bits of all
  values in order. Decoding: state `x` = first 4 bytes big endian; per value
  `slot = x & 4095`, symbol `s` with `cum[s] ≤ slot < cum[s+1]`,
  `x = f[s]·(x >> 12) + slot − cum[s]`, then while `x < 2^23`:
  `x = (x << 8) | next byte`.
* **Range**: `γ(nbytes + 1)` and bytes of an LZMA-style binary range coder
  (11-bit probabilities initialised to 1024, adaptation shift 4, first byte
  MUST be 0), then the raw bits of all values. Each symbol is coded MSB first
  as 8 binary decisions in a bit tree (node `1`, child `2·node + bit`), with
  one tree per context `min(previous symbol / 8, 17)` (previous symbol starts
  at 0); decoded symbols ≥ 136 are invalid.

## 8. Error handling

A decoder MUST reject (not crash on) any violation of the MUST rules above,
CRC mismatches, truncated data and out-of-range lengths. The reference
decoder is fuzzed against these cases.

## 9. Reserved for future versions

* `sample_format = 2` (binary64): same structure with 53-bit significands.
* Extended mode ids 7.2 … 7.15 (e.g. adaptive prediction stages).
* Header flag bits 3 … 7.
