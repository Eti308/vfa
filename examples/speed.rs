//! Encoder time breakdown (single thread) by feature, on selected materials.
use vfa::block_codec::EncoderConfig;
use vfa::container::encode_to_vec;
use vfa::sample_codec::Mode;
fn main() {
    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let mats = vfa::signals::all_materials(secs, 48000);
    let d = EncoderConfig::default();
    let no = |ms: &[Mode]| Mode::ALL.iter().copied().filter(|m| !ms.contains(m)).collect::<Vec<_>>();
    let variants = [
        ("v1.0", EncoderConfig::v1_0()),
        ("default", d.clone()),
        ("fixed blocks", EncoderConfig { adaptive_blocks: false, block_frames: 2048, ..d.clone() }),
        ("no refs", EncoderConfig { modes: no(&[Mode::FloatSplitRef, Mode::XorRef]), ..d.clone() }),
        ("no rle/xorchimp", EncoderConfig { modes: no(&[Mode::Rle, Mode::XorChimp]), ..d.clone() }),
        ("preset level 2", EncoderConfig::preset(2)),
        ("preset level 0", EncoderConfig::preset(0)),
        ("preset level 8", EncoderConfig::preset(8)),
    ];
    for name in ["02_sine_1k", "05_music_dynamic", "18_music_5.1_96k"] {
        let m = mats.iter().find(|m| m.name == name).unwrap();
        for (label, cfg) in &variants {
            let t = std::time::Instant::now();
            let (nx, _) = encode_to_vec(m.channels, m.sample_rate, &m.samples, vec![], cfg, 1).unwrap();
            println!(
                "{name:18} {label:16} {:.3} bits/sample {:6.1}x RT",
                nx.len() as f64 * 8.0 / m.samples.len() as f64,
                secs / t.elapsed().as_secs_f64()
            );
        }
    }
}
