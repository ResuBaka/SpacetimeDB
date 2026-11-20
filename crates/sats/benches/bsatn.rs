use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use spacetimedb_sats::{bsatn, de::Deserialize, ser::Serialize};
use std::hint::black_box;

#[derive(Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats)]
#[repr(C)]
struct Row {
    id: u64,
    x: f32,
    y: f32,
    timestamp: i64,
}

#[derive(Clone, Copy, bytemuck::Zeroable, bytemuck::Pod, Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats, bsatn_pod)]
#[repr(C)]
struct PodRow {
    id: u64,
    x: f32,
    y: f32,
    timestamp: i64,
}

#[derive(Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats)]
struct Mixed {
    id: u64,
    label: String,
    tag: Option<i32>,
}

// Generated SDK rows ordinarily have Rust layout. This one has wire width
// 13 even on targets where the native struct has padding.
#[derive(Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats)]
struct Position {
    layer: u8,
    id: u64,
    x: i32,
}

fn bench<T: Serialize + for<'de> Deserialize<'de>>(c: &mut Criterion, name: &str, value: T) {
    let wire = bsatn::to_vec(&value).unwrap();
    let mut group = c.benchmark_group(name);
    group.throughput(Throughput::Bytes(wire.len() as u64));
    group.bench_function("encode", |b| b.iter(|| bsatn::to_vec(black_box(&value)).unwrap()));
    group.bench_function("decode", |b| {
        b.iter(|| bsatn::from_slice::<T>(black_box(&wire)).unwrap())
    });
    group.finish();
}

fn codec(c: &mut Criterion) {
    for n in [1, 256, 16_384] {
        bench(c, &format!("u32/{n}"), (0..n as u32).collect::<Vec<_>>());
        bench(c, &format!("u64/{n}"), (0..n as u64).collect::<Vec<_>>());
    }
    let rows = (0..1024)
        .map(|id| Row {
            id,
            x: id as f32,
            y: -1.5,
            timestamp: -17,
        })
        .collect::<Vec<_>>();
    let pods = rows
        .iter()
        .map(|r| PodRow {
            id: r.id,
            x: r.x,
            y: r.y,
            timestamp: r.timestamp,
        })
        .collect::<Vec<_>>();
    bench(c, "fixed-rows/1024", rows);
    bench(c, "pod-rows/1024", pods);
    bench(
        c,
        "rust-layout-rows/1024",
        (0..1024).map(|id| Position { layer: 1, id, x: -7 }).collect::<Vec<_>>(),
    );
    let mixed = (23u64, String::from("subscription"), Some(-9i32));
    let wire = bsatn::to_vec(&mixed).unwrap();
    c.bench_function("mixed-row/reused-writer", |b| {
        let mut buf = Vec::with_capacity(wire.len());
        b.iter(|| {
            buf.clear();
            bsatn::to_writer(&mut buf, black_box(&mixed)).unwrap();
            black_box(buf.as_slice());
        })
    });
    c.bench_function("mixed-row/borrowed-reader", |b| {
        b.iter(|| bsatn::from_slice::<(u64, &str, Option<i32>)>(black_box(&wire)).unwrap())
    });
    bench(c, "mixed-row", mixed);
    for (name, label) in [
        ("short-ascii", "subscription".to_owned()),
        ("ascii-4k", "a".repeat(4096)),
        ("unicode-64", "🦀".repeat(16)),
        ("unicode-4k", "αβγδ".repeat(512)),
    ] {
        bench(
            c,
            &format!("mixed-struct/{name}"),
            Mixed {
                id: 23,
                label,
                tag: Some(-9),
            },
        );
    }
}

criterion_group!(benches, codec);
criterion_main!(benches);
