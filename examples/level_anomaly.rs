//! Diagnose non-monotone levels on multichannel material.
use vfa::block_codec::EncoderConfig;
use vfa::container::encode_to_vec;
use vfa::sample_codec::Mode;
fn main() {
    let mats = vfa::signals::all_materials(5.0, 48000);
    let m = mats.iter().find(|m| m.name == "18_music_5.1_96k").unwrap();
    let norefs: Vec<Mode> =
        Mode::ALL.iter().copied().filter(|m| !matches!(m, Mode::FloatSplitRef | Mode::XorRef)).collect();
    for level in [3u8, 5, 6, 7, 8] {
        for refs in [false, true] {
            let mut cfg = EncoderConfig { adaptive_blocks: false, block_frames: 2048, ..EncoderConfig::preset(level) };
            if !refs {
                cfg.modes = norefs.clone();
            }
            let (nx, s) = encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], &cfg, 0).unwrap();
            println!(
                "level {level} refs {refs:5}: {:.3} bits/sample  {}",
                nx.len() as f64 * 8.0 / m.samples.len() as f64,
                s.mode_share()
            );
        }
    }
}
