//! Prediction of coarse integer streams.
//!
//! All prediction arithmetic uses *wrapping* `i64` operations, identically in
//! encoder and decoder. Overflow therefore can never cause a mismatch or a
//! panic: an overflowing predictor merely produces large residuals.
//! Floating point is used only in the encoder to *choose* LPC coefficients;
//! the chosen coefficients are quantized integers stored in the stream, so
//! reconstruction is purely integer and bit exact.
//!
//! Stream layout (see SPEC.md §6.3):
//! ```text
//! pred_type:3          0 = verbatim, 1..4 = fixed polynomial order, 5 = LPC
//! [LPC] order-1:5 precision-1:4 shift:5 coef[order]:precision (two's complement)
//! [order>0] warmup_width:7 warmup[order]:warmup_width (zigzag)
//! residuals            partitioned Rice over n-order zigzag residuals
//! ```

use crate::bitio::{BitReader, BitWriter};
use crate::entropy::{self, Coder};
use crate::variable_integer::{best_rice_param, bit_len, unzigzag, write_partitioned, zigzag};
use crate::{Error, Result};

pub const MAX_LPC_ORDER: u32 = 32;
pub const MAX_FIXED_ORDER: u32 = 4;
const PRED_VERBATIM: u64 = 0;
const PRED_LPC: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predictor {
    /// residual = sample
    Verbatim,
    /// Fixed polynomial predictor of order 1..=4 (as in Shorten / FLAC).
    Fixed(u32),
    /// Quantized linear predictor: `pred = (Σ coef[j]·x[i-1-j]) >> shift`.
    Lpc { coefs: Vec<i32>, precision: u32, shift: u32 },
}

impl Predictor {
    pub fn order(&self) -> usize {
        match self {
            Predictor::Verbatim => 0,
            Predictor::Fixed(o) => *o as usize,
            Predictor::Lpc { coefs, .. } => coefs.len(),
        }
    }

    fn header_bits(&self) -> u64 {
        match self {
            Predictor::Lpc { coefs, precision, .. } => 3 + 5 + 4 + 5 + coefs.len() as u64 * *precision as u64,
            _ => 3,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Predictor::Verbatim => "verbatim".into(),
            Predictor::Fixed(o) => format!("fixed{o}"),
            Predictor::Lpc { coefs, .. } => format!("lpc{}", coefs.len()),
        }
    }
}

/// Encoder-side search effort.
#[derive(Clone, Debug)]
pub struct PredictorSearch {
    pub max_lpc_order: u32,
    /// Coefficient precisions tried (bits incl. sign, each in 2..=16).
    pub lpc_precisions: Vec<u32>,
    /// LPC orders tried (each `<= max_lpc_order`)
    pub lpc_orders: Vec<u32>,
    pub max_partition_order: u32,
    /// Residual coders evaluated (Rice is always available).
    pub coders: Vec<Coder>,
}

impl PredictorSearch {
    pub fn for_level(level: u8) -> Self {
        let (max, prec, orders): (u32, &[u32], &[u32]) = match level {
            0 => (0, &[12], &[]),
            1 => (4, &[14], &[4]),
            2 => (8, &[14], &[4, 8]),
            3 => (8, &[14, 16], &[2, 4, 6, 8]),
            4 => (12, &[14, 16], &[4, 8, 12]),
            5 => (16, &[13, 16], &[4, 8, 12, 16]),
            6 => (24, &[13, 15, 16], &[4, 8, 12, 16, 20, 24]),
            7 => (32, &[12, 14, 15, 16], &[4, 8, 12, 16, 20, 24, 32]),
            _ => (32, &[12, 13, 14, 15, 16], &[2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32]),
        };
        PredictorSearch {
            max_lpc_order: max,
            lpc_precisions: prec.to_vec(),
            lpc_orders: orders.to_vec(),
            max_partition_order: if level == 0 { 4 } else { 8 },
            coders: vec![Coder::Rice],
        }
    }
}

// ------------------------------------------------------------ residuals

/// Zigzag residuals of `x[order..]` under `pred`.
pub fn compute_residuals(pred: &Predictor, x: &[i64], out: &mut Vec<u64>) {
    out.clear();
    let o = pred.order();
    if x.len() <= o {
        return;
    }
    out.reserve(x.len() - o);
    match pred {
        Predictor::Verbatim => out.extend(x.iter().map(|&v| zigzag(v))),
        Predictor::Fixed(order) => {
            for i in o..x.len() {
                let p = fixed_prediction(*order, x, i);
                out.push(zigzag(x[i].wrapping_sub(p)));
            }
        }
        Predictor::Lpc { coefs, shift, .. } => {
            for i in o..x.len() {
                let p = lpc_prediction(coefs, *shift, x, i);
                out.push(zigzag(x[i].wrapping_sub(p)));
            }
        }
    }
}

#[inline(always)]
fn fixed_prediction(order: u32, x: &[i64], i: usize) -> i64 {
    match order {
        1 => x[i - 1],
        2 => x[i - 1].wrapping_mul(2).wrapping_sub(x[i - 2]),
        3 => x[i - 1].wrapping_sub(x[i - 2]).wrapping_mul(3).wrapping_add(x[i - 3]),
        _ => x[i - 1]
            .wrapping_add(x[i - 3])
            .wrapping_mul(4)
            .wrapping_sub(x[i - 2].wrapping_mul(6))
            .wrapping_sub(x[i - 4]),
    }
}

#[inline(always)]
fn lpc_prediction(coefs: &[i32], shift: u32, x: &[i64], i: usize) -> i64 {
    let mut acc = 0i64;
    for (j, &c) in coefs.iter().enumerate() {
        acc = acc.wrapping_add((c as i64).wrapping_mul(x[i - 1 - j]));
    }
    acc >> shift
}

// ------------------------------------------------------------ LPC design

fn autocorrelation(x: &[i64], max_lag: usize) -> Vec<f64> {
    let n = x.len();
    // Tukey(0.5) window
    let taper = (n / 4).max(1);
    let w: Vec<f64> = (0..n)
        .map(|i| {
            let v = x[i] as f64;
            let d = i.min(n - 1 - i);
            if d < taper {
                v * 0.5 * (1.0 - (std::f64::consts::PI * d as f64 / taper as f64).cos())
            } else {
                v
            }
        })
        .collect();
    (0..=max_lag)
        .map(|lag| if lag >= n { 0.0 } else { w[lag..].iter().zip(&w[..n - lag]).map(|(a, b)| a * b).sum() })
        .collect()
}

/// Levinson–Durbin recursion. Returns coefficient sets for orders 1..=max,
/// with `x[i] ≈ Σ a[j]·x[i-1-j]`.
fn levinson(r: &[f64], max_order: usize) -> Vec<Vec<f64>> {
    let mut sets = Vec::with_capacity(max_order);
    if r.is_empty() || r[0] <= 0.0 || !r[0].is_finite() {
        return sets;
    }
    let mut a: Vec<f64> = Vec::with_capacity(max_order);
    let mut err = r[0] * (1.0 + 1e-10); // slight white-noise correction
    for i in 0..max_order {
        let mut acc = r[i + 1];
        for j in 0..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / err;
        if !k.is_finite() {
            break;
        }
        let prev = a.clone();
        a.push(k);
        for j in 0..i {
            a[j] = prev[j] - k * prev[i - 1 - j];
        }
        err *= 1.0 - k * k;
        sets.push(a.clone());
        if err <= 0.0 {
            break;
        }
    }
    sets
}

fn quantize(coefs: &[f64], precision: u32) -> Option<(Vec<i32>, u32)> {
    let cmax = coefs.iter().fold(0.0f64, |m, &c| m.max(c.abs()));
    if cmax <= 0.0 || !cmax.is_finite() {
        return None;
    }
    // smallest e with cmax < 2^e
    let e = cmax.log2().floor() as i32 + 1;
    let shift = (precision as i32 - 1 - e).clamp(0, 31) as u32;
    let qmax = (1i64 << (precision - 1)) - 1;
    let qmin = -(1i64 << (precision - 1));
    let scale = (1u64 << shift) as f64;
    let mut q = Vec::with_capacity(coefs.len());
    let mut carry = 0.0;
    for &c in coefs {
        let v = c * scale + carry;
        let r = v.round().clamp(qmin as f64, qmax as f64);
        carry = v - r;
        q.push(r as i32);
    }
    if q.iter().all(|&c| c == 0) {
        return None;
    }
    Some((q, shift))
}

// ------------------------------------------------------------ selection

/// Chosen predictor plus estimated cost.
#[derive(Clone)]
pub struct Plan {
    pub predictor: Predictor,
    pub residuals: Vec<u64>,
    pub warmup_width: u32,
    pub bits: u64,
    /// Residual coder and, for non-Rice coders, the pre-encoded section.
    pub coder: Coder,
    pub coded: Option<BitWriter>,
}

fn warmup_width(x: &[i64], order: usize) -> u32 {
    x[..order.min(x.len())].iter().map(|&v| bit_len(zigzag(v))).max().unwrap_or(0)
}

fn quick_cost(pred: &Predictor, x: &[i64], scratch: &mut Vec<u64>) -> u64 {
    compute_residuals(pred, x, scratch);
    let o = pred.order();
    let ww = warmup_width(x, o);
    pred.header_bits() + if o > 0 { 7 + ww as u64 * o as u64 } else { 0 } + best_rice_param(scratch).1 + 4 + 6
}

/// Choose the best predictor for `x`.
pub fn plan(x: &[i64], search: &PredictorSearch) -> Plan {
    let n = x.len();
    let mut scratch = Vec::with_capacity(n);
    let mut best = (Predictor::Verbatim, quick_cost(&Predictor::Verbatim, x, &mut scratch));
    for order in 1..=MAX_FIXED_ORDER {
        if (order as usize) >= n {
            break;
        }
        let p = Predictor::Fixed(order);
        let c = quick_cost(&p, x, &mut scratch);
        if c < best.1 {
            best = (p, c);
        }
    }
    let max_lpc = (search.max_lpc_order as usize).min(n.saturating_sub(1)).min(MAX_LPC_ORDER as usize);
    if max_lpc > 0 && !search.lpc_orders.is_empty() && n >= 32 {
        let r = autocorrelation(x, max_lpc);
        let sets = levinson(&r, max_lpc);
        for &order in &search.lpc_orders {
            let order = order as usize;
            if order == 0 || order > sets.len() {
                continue;
            }
            for &precision in &search.lpc_precisions {
                if let Some((coefs, shift)) = quantize(&sets[order - 1], precision) {
                    let p = Predictor::Lpc { coefs, precision, shift };
                    let c = quick_cost(&p, x, &mut scratch);
                    if c < best.1 {
                        best = (p, c);
                    }
                }
            }
        }
    }
    let predictor = best.0;
    compute_residuals(&predictor, x, &mut scratch);
    let o = predictor.order();
    let ww = warmup_width(x, o);
    let (coder, res_bits, coded) = if scratch.is_empty() {
        (Coder::Rice, 0, None)
    } else {
        entropy::choose(&scratch, &search.coders, search.max_partition_order)
    };
    let bits = predictor.header_bits() + if o > 0 { 7 + ww as u64 * o as u64 } else { 0 } + res_bits;
    Plan { predictor, residuals: scratch, warmup_width: ww, bits, coder, coded }
}

/// Write a planned coarse stream.
pub fn write_plan(w: &mut BitWriter, x: &[i64], plan: &Plan, search: &PredictorSearch) {
    match &plan.predictor {
        Predictor::Verbatim => w.write_bits(PRED_VERBATIM, 3),
        Predictor::Fixed(o) => w.write_bits(*o as u64, 3),
        Predictor::Lpc { coefs, precision, shift } => {
            w.write_bits(PRED_LPC, 3);
            w.write_bits(coefs.len() as u64 - 1, 5);
            w.write_bits(*precision as u64 - 1, 4);
            w.write_bits(*shift as u64, 5);
            for &c in coefs {
                w.write_bits(c as u32 as u64, *precision);
            }
        }
    }
    let o = plan.predictor.order();
    if o > 0 {
        w.write_bits(plan.warmup_width as u64, 7);
        for &v in &x[..o] {
            w.write_bits(zigzag(v), plan.warmup_width);
        }
    }
    if !plan.residuals.is_empty() {
        match &plan.coded {
            None => write_partitioned(w, &plan.residuals, search.max_partition_order),
            Some(enc) => {
                entropy::write_escape(w, plan.coder);
                w.append(enc);
            }
        }
    }
}

/// Decode a coarse stream of `out.len()` values.
pub fn decode_stream(r: &mut BitReader, out: &mut [i64], scratch: &mut Vec<u64>) -> Result<()> {
    let n = out.len();
    let kind = r.read_bits(3)?;
    let pred = match kind {
        PRED_VERBATIM => Predictor::Verbatim,
        1..=4 => Predictor::Fixed(kind as u32),
        PRED_LPC => {
            let order = r.read_bits(5)? as usize + 1;
            let precision = r.read_bits(4)? as u32 + 1;
            let shift = r.read_bits(5)? as u32;
            let mut coefs = Vec::with_capacity(order);
            for _ in 0..order {
                let raw = r.read_bits(precision)? as u32;
                // sign-extend from `precision` bits
                let sh = 32 - precision;
                coefs.push(((raw << sh) as i32) >> sh);
            }
            Predictor::Lpc { coefs, precision, shift }
        }
        _ => return Err(Error::Corrupt("invalid predictor type")),
    };
    let o = pred.order();
    if o > n {
        return Err(Error::Corrupt("predictor order exceeds block length"));
    }
    if o > 0 {
        let ww = r.read_bits(7)? as u32;
        if ww > 64 {
            return Err(Error::Corrupt("warmup width > 64"));
        }
        for v in out[..o].iter_mut() {
            *v = unzigzag(r.read_bits(ww)?);
        }
    }
    if n == o {
        return Ok(());
    }
    scratch.clear();
    scratch.resize(n - o, 0);
    entropy::read_residuals(r, scratch)?;
    match &pred {
        Predictor::Verbatim => {
            for (v, &u) in out.iter_mut().zip(scratch.iter()) {
                *v = unzigzag(u);
            }
        }
        Predictor::Fixed(order) => {
            for i in o..n {
                let p = fixed_prediction(*order, out, i);
                out[i] = unzigzag(scratch[i - o]).wrapping_add(p);
            }
        }
        Predictor::Lpc { coefs, shift, .. } => {
            for i in o..n {
                let p = lpc_prediction(coefs, *shift, out, i);
                out[i] = unzigzag(scratch[i - o]).wrapping_add(p);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(x: &[i64], level: u8) -> (Predictor, u64) {
        let search = PredictorSearch::for_level(level);
        let p = plan(x, &search);
        let mut w = BitWriter::new();
        write_plan(&mut w, x, &p, &search);
        assert_eq!(w.bit_len(), p.bits, "cost estimate must be exact");
        let bits = w.bit_len();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let mut out = vec![0i64; x.len()];
        decode_stream(&mut r, &mut out, &mut Vec::new()).unwrap();
        assert_eq!(out, x);
        (p.predictor, bits)
    }

    #[test]
    fn sine_uses_lpc_and_compresses() {
        let x: Vec<i64> = (0..4096)
            .map(|i| ((i as f64 * 0.05).sin() * 8_000_000.0 + (i as f64 * 0.31).sin() * 1000.0) as i64)
            .collect();
        let (p, bits) = roundtrip(&x, 5);
        assert!(matches!(p, Predictor::Lpc { .. } | Predictor::Fixed(_)), "{p:?}");
        assert!(bits < 4096 * 12, "bits {bits}");
    }

    #[test]
    fn extreme_values_wrap_safely() {
        let x: Vec<i64> = (0..300).map(|i| if i % 2 == 0 { i64::MAX - i } else { i64::MIN + i }).collect();
        for level in [0, 5, 8] {
            roundtrip(&x, level);
        }
    }

    #[test]
    fn tiny_inputs() {
        for n in 0..10 {
            let x: Vec<i64> = (0..n).map(|i| i * 3 - 7).collect();
            roundtrip(&x, 8);
        }
    }
}
