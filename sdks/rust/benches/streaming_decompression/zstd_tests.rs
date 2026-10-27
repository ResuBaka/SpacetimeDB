#[path = "zstd.rs"]
mod experimental_zstd;
mod fixture;

use experimental_zstd::{buffered, compress, streaming};
use spacetimedb_sats::bsatn;

fn equivalent(wire: &[u8]) {
    match (buffered(wire), streaming(wire)) {
        (Ok(a), Ok(b)) => assert_eq!(bsatn::to_vec(&a).unwrap(), bsatn::to_vec(&b).unwrap()),
        (Err(_), Err(_)) => (),
        (a, b) => panic!("zstd paths disagree: {a:?} vs {b:?}"),
    }
}

#[test]
fn zstd_roundtrip_and_truncation() {
    for tag in 0..=2 {
        equivalent(&fixture::wire(256, false, tag));
    }
    for snapshot in [false, true] {
        for size in [0, 256, 1024 * 1024] {
            let bytes = bsatn::to_vec(&fixture::message(size, snapshot)).unwrap();
            let wire = compress(&bytes);
            assert_eq!(bsatn::to_vec(&streaming(&wire).unwrap()).unwrap(), bytes);
            equivalent(&wire);
        }
    }
    let wire = compress(&bsatn::to_vec(&fixture::message(256, false)).unwrap());
    for end in 1..wire.len() {
        equivalent(&wire[..end]);
        assert!(streaming(&wire[..end]).is_err());
    }
    // No protocol change: real SDK entry points must still reject tag 3.
    assert!(spacetimedb_sdk::unstable::streaming_decompression::streaming(&wire).is_err());
}

#[test]
fn zstd_codec_errors_after_bsatn_decode() {
    let mut bytes = bsatn::to_vec(&fixture::message(256, false)).unwrap();
    bytes.extend_from_slice(&[42; 100_000]);
    let wire = compress(&bytes);
    equivalent(&wire);
    assert!(streaming(&wire).is_ok());
    for offset in 1..=4 {
        let mut bad = wire.clone();
        let index = bad.len() - offset;
        bad[index] ^= 0x80;
        equivalent(&bad);
        assert!(streaming(&bad).unwrap_err().contains("decompress"));
    }
    equivalent(&compress(&[255]));
    assert!(streaming(&compress(&[255])).is_err());

    // A bad second frame must not be hidden by decoding a valid first message.
    let mut joined = wire.clone();
    joined.extend_from_slice(&compress(&[42; 100_000])[1..]);
    equivalent(&joined);
    assert!(streaming(&joined).is_ok());
    joined.pop();
    equivalent(&joined);
    assert!(streaming(&joined).is_err());

    let mut suffix = wire;
    suffix.extend_from_slice(b"garbage");
    equivalent(&suffix);
    assert!(streaming(&suffix).is_err());
}
