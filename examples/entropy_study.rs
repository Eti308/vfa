//! Research question 7: how much could a stronger entropy coder gain on
//! FloatSplit coarse residuals? Compares partitioned Rice (implemented) with
//! adaptive Rice and an ideal adaptive-Laplace code length (≈ what an
//! adaptive arithmetic / rANS coder would achieve).
use vfa::predictor::{plan, PredictorSearch};
use vfa::sample_codec as sc;
use vfa::variable_integer::{rice_bits, unzigzag};

fn adaptive_rice(res: &[u64]) -> f64 {
    let mut mean = res.iter().map(|&u| u as f64).sum::<f64>() / res.len().max(1) as f64;
    let mut bits = 0u64;
    for &u in res {
        let k = (mean * std::f64::consts::LN_2).max(1.0).log2().floor() as u32;
        bits += rice_bits(u, k.min(58));
        mean = mean * 0.94 + u as f64 * 0.06;
    }
    bits as f64
}

fn ideal_adaptive_laplace(res: &[u64]) -> f64 {
    // two-sided geometric P(r) = (1-t)/(1+t) * t^|r|, t fitted to an
    // exponentially weighted mean of |r|; ideal (arithmetic-coding) length
    let mut m = res.iter().map(|&u| unzigzag(u).unsigned_abs() as f64).sum::<f64>() / res.len().max(1) as f64;
    let mut bits = 0.0;
    for &u in res {
        let r = unzigzag(u).unsigned_abs() as f64;
        let mm = m.max(1e-3);
        let t = ((1.0 + mm * mm).sqrt() - 1.0) / mm;
        bits += -((1.0 - t) / (1.0 + t)).log2() - r * t.log2();
        m = m * 0.94 + r * 0.06;
    }
    bits
}

fn main() {
    let mats = vfa::signals::all_materials(10.0, 48000);
    let search = PredictorSearch::for_level(5);
    println!("| material | partitioned Rice | adaptive Rice | ideal adaptive Laplace |\n|---|---:|---:|---:|");
    for name in [
        "02_sine_1k",
        "04_pink_noise",
        "05_music_dynamic",
        "06_music_compressed",
        "13_music_24bit_origin",
        "14_music_16bit_origin",
    ] {
        let m = mats.iter().find(|m| m.name == name).unwrap();
        let l: Vec<u32> = m.samples.iter().step_by(2).cloned().collect();
        let (mut a, mut b, mut c, mut n) = (0.0, 0.0, 0.0, 0usize);
        for blk in l.chunks(4096) {
            let g = sc::grid_candidates(blk, &[0])[0].max(*sc::grid_candidates(blk, &[0]).last().unwrap());
            let sp = sc::split_channel(blk, g);
            let p = plan(&sp.coarse, &search);
            a += vfa::variable_integer::partitioned_cost(&p.residuals, 8) as f64;
            b += adaptive_rice(&p.residuals);
            c += ideal_adaptive_laplace(&p.residuals);
            n += p.residuals.len();
        }
        let n = n as f64;
        println!("| {name} | {:.3} | {:.3} | {:.3} |", a / n, b / n, c / n);
    }
}
