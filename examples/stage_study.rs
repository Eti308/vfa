//! Stage-by-stage studies for the optimization work (see RESEARCH.md).
//! Usage: cargo run --release --example stage_study -- <xor|rle|blocks|entropy|multichannel> [seconds]
use vfa::benchmark;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let which = args.get(1).map(String::as_str).unwrap_or("xor");
    let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let mats = vfa::signals::all_materials(secs, 48_000);
    let out = match which {
        "xor" => benchmark::xor_study(&mats),
        "rle" => benchmark::rle_study(&mats),
        "blocks" => benchmark::blocks_study(&mats),
        "entropy" => benchmark::entropy_study(&mats),
        "multichannel" => benchmark::multichannel_study(&mats),
        "ablation" => benchmark::ablation_study(&mats),
        other => panic!("unknown study {other}"),
    };
    println!("{out}");
}
