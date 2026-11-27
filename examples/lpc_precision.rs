//! Effect of LPC coefficient precision / order on very predictable signals.
use vfa::predictor::{plan, PredictorSearch};
use vfa::sample_codec as sc;
fn main() {
    let mats = vfa::signals::all_materials(2.0, 48000);
    for name in ["02_sine_1k", "05_music_dynamic"] {
        let m = mats.iter().find(|m| m.name == name).unwrap();
        let l: Vec<u32> = m.samples.iter().step_by(2).cloned().collect();
        for (prec, orders) in [
            (14u32, vec![4, 8, 12, 16]),
            (15, vec![4, 8, 12, 16]),
            (15, vec![4, 8, 12, 16, 24, 32]),
            (14, vec![4, 8, 12, 16, 24, 32]),
        ] {
            let search = PredictorSearch {
                max_lpc_order: *orders.last().unwrap(),
                lpc_precisions: vec![prec],
                lpc_orders: orders.clone(),
                max_partition_order: 8,
                coders: vec![vfa::entropy::Coder::Rice],
            };
            let mut bits = 0u64;
            for blk in l.chunks(2048) {
                let g = *sc::grid_candidates(blk, &[0]).last().unwrap();
                let sp = sc::split_channel(blk, g);
                bits += plan(&sp.coarse, &search).bits + sp.extras_bits;
            }
            println!("{name} precision {prec} orders {orders:?}: {:.3} bits/sample", bits as f64 / l.len() as f64);
        }
    }
}
