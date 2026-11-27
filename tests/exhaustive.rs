//! Exhaustive checks over all 2^32 float32 bit patterns. Slow (a few minutes
//! on 4 cores); run with `cargo test --release -- --ignored`.

use vfa::bitio::{BitReader, BitWriter};
use vfa::ieee754::{extra_bits_for, join_from_grid, split_at_grid, Split, SIGN_MASK};
use vfa::parallel::par_map;
use vfa::sample_codec::{decode_fieldvlc, decode_xorlz, encode_fieldvlc, encode_xorlz};

const CHUNK: u64 = 1 << 20;

#[test]
#[ignore]
fn grid_split_join_all_patterns() {
    for g in [-149, -60, -23, 0, 23, 100, 127] {
        let bad: Vec<u64> = par_map((1u64 << 32).div_ceil(CHUNK) as usize, 0, |c| {
            let mut bad = 0u64;
            for v in (c as u64 * CHUNK)..((c as u64 + 1) * CHUNK) {
                let v = v as u32;
                match split_at_grid(v, g) {
                    Split::Coarse { c, n_extra, extra } => {
                        if extra_bits_for(c, g) != Ok(n_extra) || join_from_grid(c, g, n_extra, extra) != v {
                            bad += 1;
                        }
                    }
                    Split::Zero => bad += (v != 0) as u64,
                    Split::Small | Split::Exception => {}
                }
                if v == SIGN_MASK {
                    bad += (split_at_grid(v, g) != Split::Small) as u64;
                }
            }
            bad
        });
        assert_eq!(bad.iter().sum::<u64>(), 0, "grid {g}");
    }
}

#[test]
#[ignore]
fn per_sample_codecs_all_patterns() {
    let bad: Vec<bool> = par_map((1u64 << 32).div_ceil(CHUNK) as usize, 0, |c| {
        let s: Vec<u32> = ((c as u64 * CHUNK)..((c as u64 + 1) * CHUNK)).map(|v| v as u32).collect();
        let mut ok = true;
        for (enc, dec) in [
            (
                encode_fieldvlc as fn(&mut BitWriter, &[u32]),
                decode_fieldvlc as fn(&mut BitReader, &mut [u32]) -> vfa::Result<()>,
            ),
            (encode_xorlz, decode_xorlz),
        ] {
            let mut w = BitWriter::new();
            enc(&mut w, &s);
            let b = w.into_bytes();
            let mut out = vec![0; s.len()];
            ok &= dec(&mut BitReader::new(&b), &mut out).is_ok() && out == s;
        }
        // XorChimp (window 8) on the same chunk and on a bijective scramble of
        // it, so that every pattern is also seen next to unrelated neighbours
        let scrambled: Vec<u32> = s.iter().map(|v| v.wrapping_mul(0x9E37_79B1)).collect();
        for seq in [&s, &scrambled] {
            let mut w = BitWriter::new();
            vfa::xor_codec::encode(&mut w, seq, 3);
            let b = w.into_bytes();
            let mut out = vec![0; seq.len()];
            ok &= vfa::xor_codec::decode(&mut BitReader::new(&b), &mut out).is_ok() && &out == seq;
        }
        !ok
    });
    assert!(bad.iter().all(|b| !b));
}
