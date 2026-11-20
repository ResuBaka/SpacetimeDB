//! Isolate variable row-offset decoding, then include the real gzip envelope.
use spacetimedb_client_api_messages::websocket::{common::*, v2::*};
use spacetimedb_sats::bsatn;
use spacetimedb_sdk::unstable::streaming_decompression as sdk;
use std::{hint::black_box, io::Write, time::Instant};

fn message(n: usize) -> ServerMessage {
    let mut offsets = Vec::with_capacity(n);
    let mut data = Vec::new();
    for i in 0..n {
        offsets.push(data.len() as RowOffset);
        data.resize(data.len() + 1 + i % 7, i as u8);
    }
    SubscribeApplied {
        request_id: 1,
        query_set_id: QuerySetId::new(2),
        rows: QueryRows {
            tables: vec![SingleTableRows {
                table: "test".into(),
                rows: BsatnRowList::new(RowSizeHint::RowOffsets(offsets.into()), data.into()),
            }]
            .into(),
        },
    }
    .into()
}

fn measure(n: usize, path: &str, iterations: usize, decode: impl Fn() -> ServerMessage) {
    drop(black_box(decode()));
    let start = Instant::now();
    for _ in 0..iterations {
        drop(black_box(decode()));
    }
    println!(
        "{n} {path} {:.1}",
        start.elapsed().as_secs_f64() * 1e9 / iterations as f64
    );
}

fn main() {
    println!("offsets path ns/envelope");
    for (n, iterations) in [(0, 100_000), (3, 100_000), (256, 15_000), (16384, 400)] {
        let raw = bsatn::to_vec(&message(n)).unwrap();
        let mut gzip =
            flate2::write::GzEncoder::new(vec![SERVER_MSG_COMPRESSION_TAG_GZIP], flate2::Compression::fast());
        gzip.write_all(&raw).unwrap();
        let gzip = gzip.finish().unwrap();
        measure(n, "slice", iterations, || bsatn::from_slice(black_box(&raw)).unwrap());
        measure(n, "reader", iterations, || {
            sdk::from_reader(black_box(&raw[..]), "bench").unwrap()
        });
        measure(n, "gzip", iterations / 4, || sdk::streaming(black_box(&gzip)).unwrap());
    }
}
