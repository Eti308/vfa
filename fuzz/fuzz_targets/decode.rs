//! Arbitrary bytes into the decoder: must never panic.
//! Run: cargo +nightly fuzz run decode
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(f) = vfa::decoder::NxFile::open(data) {
        let _ = f.decode_all(1);
    }
    if let Ok(mut s) = vfa::decoder::StreamDecoder::new(data) {
        while let Ok(Some(_)) = s.next_block() {}
    }
    // payload decoder directly, bypassing CRCs
    let mut out = vec![Vec::new(); 2];
    let _ = vfa::block_codec::decode_block(data, 64, &mut out, &mut Default::default());
});
