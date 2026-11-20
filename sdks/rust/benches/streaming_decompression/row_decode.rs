//! Compare release binaries before/after a decoder change; use --no-default-features for cacheless SDK.
mod fixture;

use spacetimedb_client_api_messages::websocket::common::*;
use spacetimedb_sats::bsatn;
use spacetimedb_sdk::{__codegen::parse_row_list_as_inserts, unstable::streaming_decompression as sdk};
use std::{hint::black_box, time::Instant};

fn measure<T>(name: &str, size: usize, iterations: usize, mut decode: impl FnMut() -> T) {
    drop(black_box(decode()));
    let start = Instant::now();
    for _ in 0..iterations {
        drop(black_box(decode()));
    }
    println!(
        "{name} {size} {:.1}",
        start.elapsed().as_secs_f64() * 1e9 / iterations as f64
    );
}

fn main() {
    for (size, iterations) in [(1, 300_000), (256, 8_000), (16_384, 100)] {
        let mut fixed = Vec::new();
        let mut variable = Vec::new();
        let mut offsets = Vec::new();
        for i in 0..size {
            fixed.extend(bsatn::to_vec(&(i as u64, i as u32)).unwrap());
            offsets.push(variable.len() as RowOffset);
            variable.extend(bsatn::to_vec(&(i as u64, "x".repeat(1 + i % 23))).unwrap());
        }
        let fixed = BsatnRowList::new(RowSizeHint::FixedSize(12), fixed.into());
        let variable = BsatnRowList::new(RowSizeHint::RowOffsets(offsets.into()), variable.into());
        measure("typed-fixed", size, iterations, || {
            parse_row_list_as_inserts::<(u64, u32)>(black_box(fixed.clone())).unwrap()
        });
        measure("typed-variable", size, iterations, || {
            parse_row_list_as_inserts::<(u64, String)>(black_box(variable.clone())).unwrap()
        });
    }
    for (size, iterations) in [(256, 50_000), (8 * 1024, 12_000), (128 * 1024, 1000)] {
        let raw = bsatn::to_vec(&fixture::message(size, false)).unwrap();
        let gzip = fixture::wire(size, false, SERVER_MSG_COMPRESSION_TAG_GZIP);
        measure("envelope-reader", size, iterations, || {
            sdk::from_reader(black_box(&raw[..]), "bench").unwrap()
        });
        measure("envelope-gzip", size, iterations / 4, || {
            sdk::streaming(black_box(&gzip)).unwrap()
        });
    }
}
