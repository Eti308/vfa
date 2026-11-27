//! # VFA — Variable Float Audio
//!
//! A lossless, *bit-exact* codec for IEEE-754 float32 audio.
//!
//! Guarantee: for every input sample bit pattern `b` (including ±0, subnormals,
//! ±Inf and every NaN payload) `decode(encode(b)) == b`. The codec never
//! performs floating-point arithmetic on sample values; every transformation
//! is an integer / bit operation on the raw `u32` patterns.
//!
//! Module map (see `SPEC.md` for the normative format description):
//!
//! | module             | purpose                                                     |
//! |--------------------|-------------------------------------------------------------|
//! | [`ieee754`]        | float32 bit-pattern analysis, exact grid split / join        |
//! | [`bitio`]          | MSB-first bit writer / bounds-checked bit reader             |
//! | [`variable_integer`]| zigzag, LEB128, Elias γ/δ, Rice (with escape), partitioned Rice |
//! | [`predictor`]      | fixed polynomial + LPC predictors on wrapping `i64`          |
//! | [`sample_codec`]   | per-channel sub-frame modes (Raw, Constant, XorLz, FieldVlc, FloatSplit) |
//! | [`block_codec`]    | block layout, adaptive mode selection, stereo decorrelation  |
//! | [`container`]      | `.nx` file format: header, blocks, metadata, seek index      |
//! | [`decoder`]        | file parsing, parallel / random-access / streaming decoding  |
//! | [`wav`]            | float32 WAV reader / writer with byte-exact header preservation |
//! | [`signals`]        | deterministic test-signal generator                          |
//! | [`benchmark`]      | compression & performance comparison                         |

pub mod benchmark;
pub mod bitio;
pub mod block_codec;
pub mod container;
pub mod decoder;
pub mod entropy;
pub mod ieee754;
pub mod memory;
pub mod multichannel;
pub mod options;
pub mod predictor;
pub mod rle_codec;
pub mod sample_codec;
pub mod signals;
pub mod variable_integer;
pub mod wav;
pub mod xor_codec;

mod error;
pub mod parallel;

pub use error::{Error, Result};
