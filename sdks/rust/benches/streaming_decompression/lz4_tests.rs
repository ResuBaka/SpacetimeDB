#[path = "lz4.rs"]
mod experimental_lz4;
mod fixture;

use experimental_lz4::{buffered, compress, streaming};
use spacetimedb_sats::bsatn;

fn equivalent(wire: &[u8]) {
    match (buffered(wire), streaming(wire)) {
        (Ok(a), Ok(b)) => assert_eq!(bsatn::to_vec(&a).unwrap(), bsatn::to_vec(&b).unwrap()),
        (Err(_), Err(_)) => (),
        (a, b) => panic!("LZ4 paths disagree: {a:?} vs {b:?}"),
    }
}

#[test]
fn lz4_roundtrip_and_truncation() {
    // Exercise the shared fixture's existing compression path as well.
    let _ = fixture::wire(64, false, 0);
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
        assert!(
            streaming(&wire[..end]).is_err(),
            "accepted truncation {end}/{}",
            wire.len()
        );
    }
    assert!(spacetimedb_sdk::unstable::streaming_decompression::streaming(&wire).is_err());
}

#[test]
fn lz4_checksum_errors_after_decoding() {
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
    let bad = compress(&[255]);
    equivalent(&bad);
    assert!(streaming(&bad).is_err());

    // The experiment explicitly accepts exactly one frame, with no suffix.
    let mut suffix = wire.clone();
    suffix.extend_from_slice(b"garbage");
    equivalent(&suffix);
    assert!(streaming(&suffix).is_err());
    let mut joined = wire.clone();
    joined.extend_from_slice(&wire[1..]);
    equivalent(&joined);
    assert!(streaming(&joined).is_err());
}
