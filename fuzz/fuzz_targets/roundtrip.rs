//! Arbitrary samples through encode + decode: must be bit exact.
//! Run: cargo +nightly fuzz run roundtrip
#![no_main]
use libfuzzer_sys::fuzz_target;
use vfa::block_codec::EncoderConfig;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let ch = (data[0] % 4 + 1) as u16;
    let cfg = EncoderConfig { level: data[1] % 9, block_frames: 1 + (data[1] as u32 * 7) % 300, ..Default::default() };
    let s: Vec<u32> = data[2..].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let s = &s[..s.len() - s.len() % ch as usize];
    let (nx, _) = vfa::container::encode_to_vec(ch, 48_000, s, vec![], &cfg, 1).unwrap();
    let out = vfa::decoder::NxFile::open(&nx).unwrap().decode_all(1).unwrap();
    assert_eq!(out, s);
});
