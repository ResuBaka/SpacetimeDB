//! Codec compression speed/size sweep. No allocation instrumentation.
#[path = "lz4.rs"]
#[allow(dead_code)] // SDK decode helpers are used by the other benchmark binaries.
mod experimental_lz4;
mod fixture;

use std::{
    hint::black_box,
    io::{self, Read, Write},
    time::Instant,
};

#[derive(Clone, Copy, Debug)]
enum Codec {
    Brotli,
    Gzip,
    Zstd,
    Lz4,
}

#[derive(Clone, Copy)]
enum LevelSet {
    Full,
    Practical,
    SweetSpot,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::Brotli => "brotli",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
        }
    }

    fn levels(self) -> &'static [i32] {
        match self {
            Self::Brotli => &[1, 4, 6, 9, 11],
            Self::Gzip => &[1, 3, 6, 9],
            Self::Zstd => &[1, 3, 6, 9, 15, 19],
            Self::Lz4 => &[0],
        }
    }

    fn initial_response_levels(self) -> &'static [i32] {
        match self {
            Self::Brotli => &[4],
            Self::Gzip => &[1],
            Self::Zstd => &[1, 3],
            Self::Lz4 => &[0],
        }
    }

    fn sweet_spot_levels(self) -> &'static [i32] {
        match self {
            Self::Brotli => &[1, 2, 3, 4],
            Self::Gzip => &[1],
            // zstd CLI --fast=N corresponds to compression level -N.
            Self::Zstd => &[-32, -16, -8, -4, -2, -1, 1, 2, 3, 4],
            Self::Lz4 => &[0],
        }
    }

    fn compress(self, input: &[u8], level: i32) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            Self::Brotli => {
                // Dropping the writer finalizes the stream. Writes target Vec,
                // so finalization cannot encounter an underlying I/O failure.
                let mut writer = brotli::CompressorWriter::new(
                    &mut out,
                    4096,
                    level.try_into().expect("negative Brotli quality"),
                    22,
                );
                writer.write_all(input)?;
            }
            Self::Gzip => {
                let mut writer = flate2::write::GzEncoder::new(
                    &mut out,
                    flate2::Compression::new(level.try_into().expect("negative gzip level")),
                );
                writer.write_all(input)?;
                writer.finish()?;
            }
            Self::Zstd => {
                let mut writer = zstd::stream::write::Encoder::new(&mut out, level)?;
                writer.include_checksum(true)?;
                writer.write_all(input)?;
                writer.finish()?;
            }
            Self::Lz4 => experimental_lz4::write_payload(input, &mut out)?,
        }
        Ok(out)
    }

    fn decompress(self, input: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            Self::Brotli => {
                brotli::Decompressor::new(input, 4096).read_to_end(&mut out)?;
            }
            Self::Gzip => {
                flate2::read::GzDecoder::new(input).read_to_end(&mut out)?;
            }
            Self::Zstd => {
                zstd::stream::read::Decoder::new(input)?.read_to_end(&mut out)?;
            }
            Self::Lz4 => {
                experimental_lz4::decoder(input)?.read_to_end(&mut out)?;
            }
        }
        Ok(out)
    }
}

struct Measurement {
    codec: Codec,
    level: i32,
    bytes: usize,
    encode_us: f64,
    decode_us: f64,
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let mid = samples.len() / 2;
    if samples.len() % 2 == 0 {
        (samples[mid - 1] + samples[mid]) / 2.0
    } else {
        samples[mid]
    }
}

fn measure(codec: Codec, level: i32, input: &[u8], iterations: usize) -> Measurement {
    // Warm up and verify every codec/level before measuring. Keep a compressed
    // sample for separate decompression measurements; no BSATN decoding here.
    let encoded = codec.compress(input, level).unwrap();
    assert_eq!(codec.decompress(&encoded).unwrap(), input);
    let mut encode = Vec::with_capacity(iterations);
    let mut decode = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let output = codec.compress(black_box(input), level).unwrap();
        encode.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        assert_eq!(black_box(output.len()), encoded.len());
        drop(output);
    }
    for _ in 0..iterations {
        let start = Instant::now();
        let output = codec.decompress(black_box(&encoded)).unwrap();
        decode.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        assert_eq!(black_box(output.len()), input.len());
        drop(output);
    }
    Measurement {
        codec,
        level,
        bytes: encoded.len(),
        encode_us: median(&mut encode),
        decode_us: median(&mut decode),
    }
}

fn benchmark(name: &str, input: &[u8], iterations: usize, level_set: LevelSet) {
    let mut results = Vec::new();
    for codec in [Codec::Brotli, Codec::Gzip, Codec::Zstd, Codec::Lz4] {
        let levels = match level_set {
            LevelSet::Practical => codec.initial_response_levels(),
            LevelSet::Full => codec.levels(),
            LevelSet::SweetSpot => codec.sweet_spot_levels(),
        };
        for &level in levels {
            eprintln!(
                "{name}: {} level {level}, {} bytes, {iterations} iterations",
                codec.name(),
                input.len()
            );
            results.push(measure(codec, level, input, iterations));
        }
    }
    for result in &results {
        // A point is dominated if another tested setting is no slower AND no
        // larger, with at least one strict improvement. Flags are descriptive,
        // not statistically significant winners, especially on tiny payloads.
        let pareto = !results.iter().any(|other| {
            other.bytes <= result.bytes
                && other.encode_us <= result.encode_us
                && (other.bytes < result.bytes || other.encode_us < result.encode_us)
        });
        let ratio = if input.is_empty() {
            0.0
        } else {
            result.bytes as f64 / input.len() as f64
        };
        let mib_per_sec = input.len() as f64 / 1048576.0 / (result.encode_us / 1_000_000.0);
        println!(
            "{name},{},{},{},{},{:.6},{:.3},{:.3},{:.3},{pareto}",
            result.codec.name(),
            result.level,
            input.len(),
            result.bytes,
            ratio,
            result.encode_us,
            mib_per_sec,
            result.decode_us
        );
    }
}

fn help() {
    eprintln!(
        "Usage: compression-tradeoff-bench [--snapshot-mib N] [--iterations N]\n\
        Or: compression-tradeoff-bench --file RAW_PAYLOAD [--iterations N]\n\
        Or: compression-tradeoff-bench --large-initial-response [--iterations N] [--all-levels]\n\
        --large-initial-response: 500 MiB rows, Brotli 4 / gzip 1 / zstd 1,3; skips live case.\n\
        Add --all-levels to sweep all 16 settings (potentially very slow).\n\
        LZ4 framed default (level 0 label, content checksum enabled) is included in every set.\n\
        Add --sweet-spot to any case: Brotli 1-4, gzip 1, zstd 1-4 and fast 1,2,4,8,16,32.\n\
        Default: 256-byte live update (100 iterations), 4 MiB snapshot (3 iterations).\n\
        --file expects uncompressed bytes, without a compression tag.\n\
        CSV goes to stdout; progress goes to stderr. High levels may take a while."
    );
}

fn main() {
    let mut file = None;
    let mut snapshot_mib = None;
    let mut iterations = None;
    let mut large_initial_response = false;
    let mut all_levels = false;
    let mut sweet_spot = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--file" => file = Some(args.next().expect("--file needs a path")),
            "--snapshot-mib" => {
                snapshot_mib = Some(args.next().expect("missing MiB").parse::<usize>().expect("invalid MiB"))
            }
            "--large-initial-response" => large_initial_response = true,
            "--all-levels" => all_levels = true,
            "--sweet-spot" => sweet_spot = true,
            "--iterations" => {
                let n: usize = args
                    .next()
                    .expect("missing iterations")
                    .parse()
                    .expect("invalid iterations");
                assert!(n > 0, "iterations must be positive");
                iterations = Some(n);
            }
            "--help" | "-h" => {
                help();
                return;
            }
            _ => {
                help();
                panic!("unknown argument {arg}");
            }
        }
    }
    assert!(
        !large_initial_response || (file.is_none() && snapshot_mib.is_none()),
        "--large-initial-response cannot be combined with --file or --snapshot-mib"
    );
    assert!(
        !all_levels || large_initial_response,
        "--all-levels is only needed with --large-initial-response"
    );
    assert!(
        !(all_levels && sweet_spot),
        "choose either --all-levels or --sweet-spot"
    );
    let level_set = if sweet_spot {
        LevelSet::SweetSpot
    } else if large_initial_response && !all_levels {
        LevelSet::Practical
    } else {
        LevelSet::Full
    };
    println!("case,codec,level,input_bytes,compressed_bytes,compressed_fraction,encode_median_us,encode_mib_per_sec,decode_median_us,encode_size_pareto");
    if let Some(path) = file {
        benchmark(
            "file",
            &std::fs::read(path).expect("cannot read input"),
            iterations.unwrap_or(3),
            level_set,
        );
    } else if large_initial_response {
        eprintln!("500 MiB initial response: expect multi-GiB transient memory usage.");
        let snapshot = fixture::wire(500 * 1048576, true, 0);
        benchmark("initial_500mib", &snapshot[1..], iterations.unwrap_or(3), level_set);
    } else {
        // Reuse exact live/snapshot message construction from the decode bench.
        let live = fixture::wire(256, false, 0);
        benchmark("live", &live[1..], iterations.unwrap_or(100), level_set);
        let size = snapshot_mib
            .unwrap_or(4)
            .checked_mul(1048576)
            .expect("snapshot size overflow");
        let snapshot = fixture::wire(size, true, 0);
        benchmark("snapshot", &snapshot[1..], iterations.unwrap_or(3), level_set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_levels_roundtrip() {
        for codec in [Codec::Brotli, Codec::Gzip, Codec::Zstd, Codec::Lz4] {
            for &level in codec.levels() {
                for input in [&b""[..], &b"repeated repeated repeated"[..]] {
                    let compressed = codec.compress(input, level).unwrap();
                    assert_eq!(codec.decompress(&compressed).unwrap(), input);
                }
            }
        }
    }

    #[test]
    fn medians() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn initial_response_levels_roundtrip() {
        // Verify preset settings cheaply; the 500 MiB allocation is opt-in via CLI.
        let wire = fixture::wire(1024, true, 0);
        for codec in [Codec::Brotli, Codec::Gzip, Codec::Zstd, Codec::Lz4] {
            for &level in codec.initial_response_levels() {
                let compressed = codec.compress(&wire[1..], level).unwrap();
                assert_eq!(codec.decompress(&compressed).unwrap(), &wire[1..]);
            }
        }
    }

    #[test]
    fn sweet_spot_levels_roundtrip() {
        let wire = fixture::wire(4096, true, 0);
        for codec in [Codec::Brotli, Codec::Gzip, Codec::Zstd, Codec::Lz4] {
            for &level in codec.sweet_spot_levels() {
                for input in [&wire[1..], &b""[..]] {
                    let compressed = codec.compress(input, level).unwrap();
                    assert_eq!(codec.decompress(&compressed).unwrap(), input);
                }
            }
        }
        assert!(Codec::Zstd.sweet_spot_levels().contains(&-32));
        assert!(Codec::Zstd.sweet_spot_levels().contains(&4));
    }
}
