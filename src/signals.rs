//! Deterministic test material (the 12 categories of the project brief plus
//! extra robustness cases). Signals are synthesized in `f64` and rounded once
//! to `f32` — the way a DAW renders a float mix — so the float32 test
//! material carries full 24-bit significand precision unless a material
//! explicitly models an integer source (24/16-bit origin).

use std::f64::consts::PI;

pub struct Material {
    pub name: &'static str,
    pub description: &'static str,
    pub channels: u16,
    pub sample_rate: u32,
    /// Interleaved raw float32 bit patterns.
    pub samples: Vec<u32>,
}

impl Material {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels as usize
    }
}

/// SplitMix64 — tiny, deterministic, good enough for test signals.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn gauss(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
}

fn to_bits(x: &[f64]) -> Vec<u32> {
    x.iter().map(|&v| (v as f32).to_bits()).collect()
}

fn db(d: f64) -> f64 {
    10f64.powf(d / 20.0)
}

fn peak(x: &[f64]) -> f64 {
    x.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

fn normalize(x: &mut [f64], target_peak: f64) {
    let p = peak(x);
    if p > 0.0 {
        let g = target_peak / p;
        x.iter_mut().for_each(|v| *v *= g);
    }
}

/// Synthesized stereo "music": chords with harmonic voices and ADSR, bass,
/// kick/snare/hi-hat, a feedback-delay "room", and a section-level dynamic
/// envelope spanning ~30 dB. Returns interleaved stereo at `rate`.
pub fn synth_music(seconds: f64, rate: u32, seed: u64) -> Vec<f64> {
    let n = (seconds * rate as f64) as usize;
    let sr = rate as f64;
    let mut rng = Rng::new(seed);
    let mut l = vec![0.0f64; n];
    let mut r = vec![0.0f64; n];
    let beat = 0.5; // 120 bpm
    let chords: [[f64; 4]; 4] = [
        [220.0, 277.18, 329.63, 440.0],  // A
        [174.61, 220.0, 261.63, 349.23], // F
        [261.63, 329.63, 392.0, 523.25], // C
        [196.0, 246.94, 293.66, 392.0],  // G
    ];
    let bass = [110.0, 87.31, 130.81, 98.0];
    let total_beats = (seconds / beat).ceil() as usize;
    for b in 0..total_beats {
        let t0 = b as f64 * beat;
        let bar = (b / 4) % 4;
        let start = (t0 * sr) as usize;
        // pad chord: every 4 beats, 2 s long
        if b % 4 == 0 {
            for (vi, &f) in chords[bar].iter().enumerate() {
                let pan = 0.2 + 0.2 * vi as f64;
                let detune = 1.0 + (rng.uniform() - 0.5) * 0.004;
                let len = (2.2 * sr) as usize;
                for i in 0..len.min(n.saturating_sub(start)) {
                    let t = i as f64 / sr;
                    let env = (t / 0.08).min(1.0) * (-t * 1.2).exp();
                    let mut v = 0.0;
                    for h in 1..=8 {
                        v += (2.0 * PI * f * detune * h as f64 * t).sin() / (h as f64).powf(1.3);
                    }
                    v *= 0.08 * env;
                    l[start + i] += v * (1.0 - pan);
                    r[start + i] += v * pan;
                }
            }
        }
        // bass on every beat
        let fb = bass[bar];
        for i in 0..((0.45 * sr) as usize).min(n.saturating_sub(start)) {
            let t = i as f64 / sr;
            let env = (t / 0.005).min(1.0) * (-t * 4.0).exp();
            let v = 0.25 * env * ((2.0 * PI * fb * t).sin() + 0.3 * (4.0 * PI * fb * t).sin());
            l[start + i] += v;
            r[start + i] += v;
        }
        // kick on 1 and 3, snare on 2 and 4
        let len = (0.3 * sr) as usize;
        for i in 0..len.min(n.saturating_sub(start)) {
            let t = i as f64 / sr;
            let v = if b % 2 == 0 {
                let f = 45.0 + 80.0 * (-t * 30.0).exp();
                0.6 * (-t * 12.0).exp() * (2.0 * PI * f * t).sin()
            } else {
                0.25 * (-t * 25.0).exp() * (rng.gauss() * 0.7 + (2.0 * PI * 185.0 * t).sin())
            };
            l[start + i] += v;
            r[start + i] += v * 0.95;
        }
        // hi-hats on eighths: high-passed noise burst
        for half in 0..2 {
            let s = start + (half as f64 * beat * 0.5 * sr) as usize;
            let mut lp = 0.0;
            for i in 0..((0.06 * sr) as usize).min(n.saturating_sub(s)) {
                let t = i as f64 / sr;
                let w = rng.gauss();
                lp = lp * 0.6 + w * 0.4;
                let v = 0.08 * (-t * 70.0).exp() * (w - lp);
                l[s + i] += v * 0.7;
                r[s + i] += v;
            }
        }
    }
    // stereo feedback delay "room"
    let (dl, dr) = ((0.0371 * sr) as usize, (0.0433 * sr) as usize);
    for i in 0..n {
        if i >= dl {
            l[i] += 0.35 * r[i - dl];
        }
        if i >= dr {
            r[i] += 0.35 * l[i - dr];
        }
    }
    // dynamic envelope: quiet intro, crescendo, loud section, breakdown
    for i in 0..n {
        let t = i as f64 / n as f64;
        let d = if t < 0.2 {
            -30.0 + 10.0 * (t / 0.2)
        } else if t < 0.5 {
            -20.0 + 20.0 * ((t - 0.2) / 0.3)
        } else if t < 0.8 {
            0.0
        } else {
            -12.0
        };
        let g = db(d);
        l[i] *= g;
        r[i] *= g;
    }
    let mut out = Vec::with_capacity(2 * n);
    for i in 0..n {
        out.push(l[i]);
        out.push(r[i]);
    }
    normalize(&mut out, db(-1.0));
    out
}

/// Heavy bus compression + soft clipping ("loudness war" master).
pub fn compress(x: &[f64], rate: u32) -> Vec<f64> {
    let att = (-1.0 / (0.001 * rate as f64)).exp();
    let rel = (-1.0 / (0.080 * rate as f64)).exp();
    let (thr, ratio) = (db(-30.0), 10.0);
    let mut env = 0.0f64;
    let mut out = Vec::with_capacity(x.len());
    for fr in x.chunks(2) {
        let lvl = fr.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        env = if lvl > env { att * env + (1.0 - att) * lvl } else { rel * env + (1.0 - rel) * lvl };
        let g = if env > thr { (thr * (env / thr).powf(1.0 / ratio)) / env } else { 1.0 };
        for &v in fr {
            out.push((v * g * 12.0).tanh());
        }
    }
    normalize(&mut out, db(-0.1));
    out
}

fn quantize_to_int(x: &[f64], bits: u32) -> Vec<u32> {
    let scale = (1u64 << (bits - 1)) as f64;
    x.iter()
        .map(|&v| {
            // `+ 0.0` turns -0.0 into +0.0: an integer source cannot produce -0
            let q = (v * scale).round().clamp(-scale, scale - 1.0) + 0.0;
            ((q / scale) as f32).to_bits() // exact: q has <= 24 significant bits
        })
        .collect()
}

/// All benchmark materials. `seconds` per material at `rate` (stereo unless
/// stated otherwise).
pub fn all_materials(seconds: f64, rate: u32) -> Vec<Material> {
    let n = (seconds * rate as f64) as usize;
    let sr = rate as f64;
    let music = synth_music(seconds, rate, 1);
    let mut m = Vec::new();
    let mut push = |name, description, channels, sample_rate, samples| {
        m.push(Material { name, description, channels, sample_rate, samples })
    };

    push("01_silence", "digital silence (+0.0)", 2, rate, vec![0u32; 2 * n]);

    let sine: Vec<f64> = (0..n)
        .flat_map(|i| {
            let t = i as f64 / sr;
            [db(-3.0) * (2.0 * PI * 1000.0 * t).sin(), db(-3.0) * (2.0 * PI * 1000.0 * t + 0.5).sin()]
        })
        .collect();
    push("02_sine_1k", "1 kHz sine, -3 dBFS", 2, rate, to_bits(&sine));

    let mut rng = Rng::new(2);
    let white: Vec<f64> = (0..2 * n).map(|_| (rng.uniform() * 2.0 - 1.0) * db(-1.0)).collect();
    push("03_white_noise", "uniform white noise, -1 dBFS peak", 2, rate, to_bits(&white));

    let mut rng = Rng::new(3);
    let mut pink = Vec::with_capacity(2 * n);
    let mut st = [[0.0f64; 7]; 2];
    for _ in 0..n {
        for b in st.iter_mut() {
            let w = rng.gauss();
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.1538520;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.0168980;
            let p = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
            b[6] = w * 0.115926;
            pink.push(p);
        }
    }
    normalize(&mut pink, db(-1.0));
    push("04_pink_noise", "pink noise (Kellet filter), -1 dBFS peak", 2, rate, to_bits(&pink));

    push("05_music_dynamic", "synth mix, ~30 dB dynamics, float render", 2, rate, to_bits(&music));
    let comp = compress(&music, rate);
    push("06_music_compressed", "heavily compressed + soft-clipped master", 2, rate, to_bits(&comp));

    let mut rng = Rng::new(7);
    let mut tr = vec![0.0f64; 2 * n];
    let mut t = 0usize;
    while t < n {
        let amp = db(-(rng.uniform() * 30.0));
        let f = 200.0 + rng.uniform() * 3000.0;
        for i in 0..((0.05 * sr) as usize).min(n - t) {
            let tt = i as f64 / sr;
            let v = amp * (-tt * 120.0).exp() * ((2.0 * PI * f * tt).sin() + 0.5 * rng.gauss());
            tr[2 * (t + i)] = v;
            tr[2 * (t + i) + 1] = v * 0.8;
        }
        t += (0.2 * sr) as usize + (rng.uniform() * 0.2 * sr) as usize;
    }
    push("07_transients", "percussive hits separated by exact silence", 2, rate, to_bits(&tr));

    let scaled = |g: f64| -> Vec<u32> { music.iter().map(|&v| ((v * g) as f32).to_bits()).collect() };
    push("08_very_quiet", "music at -80 dB", 2, rate, scaled(db(-80.0)));
    push("09_plus24dBFS", "music at +24 dBFS", 2, rate, scaled(db(24.0)));
    push("10_plus100dBFS", "music at +100 dBFS", 2, rate, scaled(db(100.0)));
    push("11_plus200dBFS", "music at +200 dBFS (1e10)", 2, rate, scaled(db(200.0)));
    push("12_near_f32_max", "music peaking at 3.0e38 (~+769.5 dBFS)", 2, rate, scaled(3.0e38 / peak(&music)));

    // ---- additional cases
    push(
        "13_music_24bit_origin",
        "music quantized to 24-bit PCM, stored as float",
        2,
        rate,
        quantize_to_int(&music, 24),
    );
    push(
        "14_music_16bit_origin",
        "music quantized to 16-bit PCM, stored as float",
        2,
        rate,
        quantize_to_int(&music, 16),
    );
    push(
        "15_subnormal_range",
        "music peaking at 1e-36 (subnormals near zero crossings)",
        2,
        rate,
        scaled(1e-36 / peak(&music)),
    );

    let mut rng = Rng::new(16);
    let specials: Vec<u32> = (0..2 * n)
        .map(|i| match rng.next_u64() % 8 {
            0 => 0x7FC0_0000 | (rng.next_u64() as u32 & 0x3F_FFFF), // quiet NaN with payload
            1 => 0x7F80_0001 | (rng.next_u64() as u32 & 0x3F_FFFF), // signalling NaN
            2 => [f32::INFINITY.to_bits(), f32::NEG_INFINITY.to_bits()][i % 2],
            3 => 0x8000_0000,                         // -0.0
            4 => rng.next_u64() as u32 & 0x807F_FFFF, // subnormal
            _ => music[i].to_bits_f32(),
        })
        .collect();
    push("16_special_values", "music mixed with NaN payloads, ±Inf, -0, subnormals", 2, rate, specials);

    let mut rng = Rng::new(17);
    push(
        "17_random_bits",
        "uniformly random 32-bit patterns (incompressible)",
        2,
        rate,
        (0..2 * n).map(|_| rng.next_u64() as u32).collect(),
    );

    // multichannel high-rate material
    let r96 = 96_000u32;
    let n96 = (seconds * r96 as f64) as usize;
    let m96 = synth_music(seconds, r96, 5);
    let mut mc = Vec::with_capacity(6 * n96);
    let mut lfe = 0.0f64;
    for i in 0..n96 {
        let (l, r) = (m96[2 * i], m96[2 * i + 1]);
        lfe = lfe * 0.995 + 0.005 * (l + r);
        let d = i.saturating_sub(1500);
        let (sl, sr2) = (m96[2 * d] * 0.4, m96[2 * d + 1] * 0.4);
        mc.extend_from_slice(&[l, r, 0.5 * (l + r), lfe * 4.0, sl, sr2]);
    }
    push("18_music_5.1_96k", "6-channel 96 kHz mix (L R C LFE Ls Rs)", 6, r96, to_bits(&mc));

    // ---- additional datasets for the optimization stages (format 1.1)
    let music_bits = to_bits(&music);
    push(
        "19_constant_dc",
        "constant signals: L = 0.25, R = -1e10",
        2,
        rate,
        (0..2 * n).map(|i| if i % 2 == 0 { 0.25f32.to_bits() } else { (-1e10f32).to_bits() }).collect(),
    );

    let mut rng = Rng::new(20);
    let mut sz = Vec::with_capacity(2 * n);
    while sz.len() < 2 * n {
        let v = if rng.next_u64().is_multiple_of(2) { 0u32 } else { 0x8000_0000 };
        let len = 1 + (rng.next_u64() % 100) as usize;
        sz.extend(std::iter::repeat_n(v, len));
    }
    sz.truncate(2 * n);
    push("20_signed_zeros", "runs of +0.0 and -0.0 (e.g. gated float stems)", 2, rate, sz);

    let mut rng = Rng::new(21);
    let mut inf = music_bits.clone();
    let mut i = 0;
    while i < inf.len() {
        if rng.next_u64().is_multiple_of(400) {
            let len = 1 + (rng.next_u64() % 64) as usize;
            let v =
                if rng.next_u64().is_multiple_of(2) { f32::INFINITY.to_bits() } else { f32::NEG_INFINITY.to_bits() };
            for s in inf.iter_mut().skip(i).take(len) {
                *s = v;
            }
            i += len;
        }
        i += 1;
    }
    push("21_infinities", "music with bursts of +Inf / -Inf (overflowed processing)", 2, rate, inf);

    let mut rng = Rng::new(22);
    let nans: Vec<u32> = (0..2 * n)
        .map(|i| {
            let payload = if i % 3 == 0 { rng.next_u64() as u32 & 0x003F_FFFF } else { (i as u32 / 64) & 0x3F_FFFF };
            let quiet = if rng.next_u64().is_multiple_of(4) { 0 } else { 0x0040_0000 };
            let sign = (rng.next_u64() as u32 & 1) << 31;
            sign | 0x7F80_0000 | quiet | payload | if quiet == 0 && payload == 0 { 1 } else { 0 }
        })
        .collect();
    push("22_nan_payloads", "quiet and signalling NaNs with diverse payloads and signs", 2, rate, nans);

    // exact-period wavetable: 96-sample cycle (500 Hz @ 48 kHz), repeated bit-exactly
    let cycle: Vec<f64> = (0..96)
        .map(|i| {
            let p = 2.0 * PI * i as f64 / 96.0;
            0.5 * p.sin() + 0.2 * (3.0 * p).sin() + 0.1 * (5.0 * p).cos()
        })
        .collect();
    let wt: Vec<f64> = (0..n).flat_map(|i| [cycle[i % 96], cycle[(i + 24) % 96]]).collect();
    push("23_periodic_wavetable", "bit-exactly repeating 96-sample wavetable cycle", 2, rate, to_bits(&wt));

    let dual: Vec<u32> = (0..n).flat_map(|i| [music_bits[2 * i], music_bits[2 * i]]).collect();
    push("24_dual_mono", "identical left and right channels", 2, rate, dual);
    let inv: Vec<u32> = (0..n).flat_map(|i| [music_bits[2 * i], music_bits[2 * i] ^ 0x8000_0000]).collect();
    push("25_polarity_inverted", "right = -left (sign bit flipped)", 2, rate, inv);

    let mut rng = Rng::new(26);
    let mut sp = vec![0.0f64; 2 * n];
    let mut t = 0usize;
    while t < n {
        t += (rng.uniform() * 3.0 * sr) as usize;
        for k in 0..((0.004 * sr) as usize).min(n.saturating_sub(t)) {
            let v = (-(k as f64) / 30.0).exp() * rng.gauss() * 0.5;
            sp[2 * (t + k)] = v;
            sp[2 * (t + k) + 1] = v * 0.7;
        }
    }
    push("26_sparse_events", "digital silence with rare 4 ms clicks (~0.2 % active)", 2, rate, to_bits(&sp));
    m
}

trait F64Bits {
    fn to_bits_f32(self) -> u32;
}
impl F64Bits for f64 {
    fn to_bits_f32(self) -> u32 {
        (self as f32).to_bits()
    }
}
