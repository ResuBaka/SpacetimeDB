//! Sequential compression -> SDK decompression + owned BSATN decode benchmark.
//! Each codec/level/mode runs in a fresh worker, without allocation counters.
#[path = "lz4.rs"]
#[allow(dead_code)] // The standalone fixture encoder is used by the decode benchmark.
mod experimental_lz4;
#[path = "zstd.rs"]
#[allow(dead_code)] // Its fixture encoder is only used by the other benchmark binaries.
mod experimental_zstd;
mod fixture;

use spacetimedb_client_api_messages::websocket::{common::*, v2::*};
use spacetimedb_sats::bsatn;
use std::{
    borrow::Cow,
    hint::black_box,
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::Instant,
};

// Mirrors crates/core/src/subscription/websocket_building.rs::decide_compression.
// Deliberately benchmark-local to avoid depending on the entire server crate.
const SERVER_COMPRESSION_THRESHOLD: usize = 1024;

struct TempInput(std::path::PathBuf);

impl Drop for TempInput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    None,
    Brotli,
    Gzip,
    Zstd,
    Lz4,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Brotli => "brotli",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
        }
    }

    fn from_name(name: &str) -> Self {
        match name {
            "none" => Self::None,
            "brotli" => Self::Brotli,
            "gzip" => Self::Gzip,
            "zstd" => Self::Zstd,
            "lz4" => Self::Lz4,
            _ => panic!("unknown codec {name}"),
        }
    }
}

#[derive(Clone, Copy)]
struct Setting {
    codec: Codec,
    level: i32,
    zstd_workers: u32,
    // 0 = original server encoder; 1/2/4 = compress_multi lanes including caller.
    brotli_threads: usize,
}

fn settings(sweet_spot: bool) -> Vec<Setting> {
    let mut out = vec![Setting {
        codec: Codec::None,
        level: 0,
        zstd_workers: 0,
        brotli_threads: 0,
    }];
    for (codec, levels) in [
        (Codec::Brotli, if sweet_spot { &[1, 2, 3, 4][..] } else { &[1][..] }),
        (Codec::Gzip, &[1][..]),
        (Codec::Lz4, &[0][..]),
        (
            Codec::Zstd,
            if sweet_spot {
                &[-32, -16, -8, -4, -2, -1, 1, 2, 3, 4][..]
            } else {
                &[-2, -1, 1, 3][..]
            },
        ),
    ] {
        out.extend(levels.iter().map(|&level| Setting {
            codec,
            level,
            zstd_workers: 0,
            brotli_threads: 0,
        }));
    }
    out
}

fn zstd_mt_settings(sweet_spot: bool) -> Vec<Setting> {
    settings(sweet_spot)
        .into_iter()
        .filter(|s| s.codec == Codec::Zstd)
        .flat_map(|setting| {
            [0, 2, 4].map(|zstd_workers| Setting {
                zstd_workers,
                ..setting
            })
        })
        .collect()
}

fn brotli_mt_settings() -> Vec<Setting> {
    [1, 2, 3, 4]
        .into_iter()
        .flat_map(|level| {
            [0, 1, 2, 4].map(move |brotli_threads| Setting {
                codec: Codec::Brotli,
                level,
                zstd_workers: 0,
                brotli_threads,
            })
        })
        .collect()
}

fn mt_size_settings() -> Vec<Setting> {
    // Focus on the promising levels so the size sweep doesn't repeat every
    // codec/quality. Include the original Brotli API and the one-lane MT control.
    let mut out: Vec<_> = brotli_mt_settings().into_iter().filter(|s| s.level == 2).collect();
    out.extend(
        zstd_mt_settings(false)
            .into_iter()
            .filter(|s| s.level == -2 || s.level == 1),
    );
    out
}

const MT_SIZE_KIB: &[usize] = &[4, 16, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 65536];

fn size_sweep_iterations(kib: usize) -> usize {
    if kib <= 512 {
        100
    } else if kib <= 4096 {
        10
    } else {
        5
    }
}

struct SharedBrotliInput(Arc<Vec<u8>>);

impl brotli::SliceWrapper<u8> for SharedBrotliInput {
    fn slice(&self) -> &[u8] {
        &self.0[1..]
    }
}

fn compress_brotli_multi(
    input: &Arc<Vec<u8>>,
    params: &brotli::enc::BrotliEncoderParams,
    threads: usize,
    frame: &mut Vec<u8>,
) -> io::Result<()> {
    use brotli::enc::{
        compress_multi, BrotliEncoderMaxCompressedSizeMulti, Owned, SendAlloc, StandardAlloc, UnionHasher,
    };
    let size = input.len() - 1;
    // Share already-owned data; no 500 MiB clone to satisfy the 'static bound.
    let mut input = Owned::new(SharedBrotliInput(Arc::clone(input)));
    let bound = BrotliEncoderMaxCompressedSizeMulti(size, threads);
    let output_len = bound
        .checked_add(1)
        .ok_or_else(|| io::Error::other("Brotli output size overflow"))?;
    frame
        .try_reserve_exact(bound)
        .map_err(|e| io::Error::other(e.to_string()))?;
    frame.resize(output_len, 0);
    let mut allocators: Vec<_> = (0..threads)
        .map(|_| SendAlloc::new(StandardAlloc::default(), UnionHasher::Uninit))
        .collect();
    let written = compress_multi(params, &mut input, &mut frame[1..], &mut allocators)
        .map_err(|e| io::Error::other(format!("Brotli MT: {e:?}")))?;
    frame.truncate(written + 1);
    Ok(())
}

/// Input already has the uncompressed tag, just like the server's serialization
/// buffer. Below threshold (or with compression disabled), reuse it without a copy.
fn encode_frame(uncompressed_frame: &Arc<Vec<u8>>, setting: Setting) -> io::Result<Cow<'_, [u8]>> {
    assert_eq!(uncompressed_frame[0], SERVER_MSG_COMPRESSION_TAG_NONE);
    let input = &uncompressed_frame[1..];
    let codec = if input.len() > SERVER_COMPRESSION_THRESHOLD {
        setting.codec
    } else {
        Codec::None
    };
    if codec == Codec::None {
        return Ok(Cow::Borrowed(uncompressed_frame.as_slice()));
    }
    let tag = match codec {
        Codec::None => SERVER_MSG_COMPRESSION_TAG_NONE,
        Codec::Brotli => SERVER_MSG_COMPRESSION_TAG_BROTLI,
        Codec::Gzip => SERVER_MSG_COMPRESSION_TAG_GZIP,
        Codec::Zstd => experimental_zstd::EXPERIMENTAL_TAG,
        Codec::Lz4 => experimental_lz4::EXPERIMENTAL_TAG,
    };
    // Write the tag first, like the server; no extra compressed-payload copy.
    let mut frame = vec![tag];
    match codec {
        Codec::None => unreachable!(),
        Codec::Brotli => {
            // Same encoder entry point/default parameters as brotli_compress on
            // the server. Its production quality is 1; other levels are probes.
            let params = brotli::enc::BrotliEncoderParams {
                quality: setting.level,
                ..Default::default()
            };
            if setting.brotli_threads == 0 {
                brotli::BrotliCompress(&mut &input[..], &mut frame, &params)?;
            } else {
                compress_brotli_multi(uncompressed_frame, &params, setting.brotli_threads, &mut frame)?;
            }
        }
        Codec::Gzip => {
            let mut writer = flate2::write::GzEncoder::new(
                &mut frame,
                flate2::Compression::new(setting.level.try_into().expect("negative gzip level")),
            );
            writer.write_all(input)?;
            writer.finish()?;
        }
        Codec::Zstd => {
            let mut writer = zstd::stream::write::Encoder::new(&mut frame, setting.level)?;
            writer.multithread(setting.zstd_workers)?;
            writer.include_checksum(true)?;
            writer.write_all(input)?;
            writer.finish()?;
        }
        Codec::Lz4 => experimental_lz4::write_payload(input, &mut frame)?,
    }
    Ok(Cow::Owned(frame))
}

fn decode_frame(frame: &[u8], mode: &str) -> Result<ServerMessage, String> {
    if frame.first() == Some(&experimental_lz4::EXPERIMENTAL_TAG) {
        return match mode {
            "buffered" => experimental_lz4::buffered(frame),
            "streaming" => experimental_lz4::streaming(frame),
            _ => panic!("unknown decode mode {mode}"),
        };
    }
    match mode {
        "buffered" => experimental_zstd::buffered(frame),
        "streaming" => experimental_zstd::streaming(frame),
        _ => panic!("unknown decode mode {mode}"),
    }
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

#[cfg(unix)]
fn process_cpu_seconds() -> Option<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // RUSAGE_SELF includes all threads, including zstd's native workers.
    // getrusage initializes the structure iff it returns success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    Some(
        usage.ru_utime.tv_sec as f64
            + usage.ru_utime.tv_usec as f64 / 1e6
            + usage.ru_stime.tv_sec as f64
            + usage.ru_stime.tv_usec as f64 / 1e6,
    )
}

#[cfg(not(unix))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

fn worker(args: &[String]) {
    assert_eq!(args.len(), 10, "invalid worker arguments");
    let mut input = std::fs::read(&args[2]).expect("cannot read raw BSATN input");
    // Model the server's already-tagged serialization buffer; this one-time
    // setup is outside timing. Uncompressed sends must not pay an extra copy.
    input.reserve_exact(1);
    input.insert(0, SERVER_MSG_COMPRESSION_TAG_NONE);
    let input = Arc::new(input);
    let name = &args[3];
    let setting = Setting {
        codec: Codec::from_name(&args[4]),
        level: args[5].parse().unwrap(),
        zstd_workers: args[6].parse().unwrap(),
        brotli_threads: args[7].parse().unwrap(),
    };
    let mode = &args[8];
    let iterations: usize = args[9].parse().unwrap();
    assert!(iterations > 0);
    let warm = encode_frame(&input, setting).unwrap();
    let wire_bytes = warm.len();
    let compressed = warm[0] != SERVER_MSG_COMPRESSION_TAG_NONE;
    let message = decode_frame(&warm, mode).expect("warmup SDK decode failed");
    assert_eq!(bsatn::to_vec(&message).unwrap(), input[1..], "warmup roundtrip failed");
    drop(message);
    drop(warm);

    let mut compression = Vec::with_capacity(iterations);
    let mut sdk_decode = Vec::with_capacity(iterations);
    let mut total = Vec::with_capacity(iterations);
    let mut cpu = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let cpu_start = process_cpu_seconds();
        let start = Instant::now();
        let frame = encode_frame(black_box(&input), setting).unwrap();
        let encoded = Instant::now();
        let message = decode_frame(black_box(&frame), mode).expect("SDK decode failed");
        let decoded = Instant::now();
        drop(black_box(message));
        drop(black_box(frame));
        let finished = Instant::now();
        if let (Some(start), Some(end)) = (cpu_start, process_cpu_seconds()) {
            cpu.push((end - start) * 1e6);
        }
        compression.push(encoded.duration_since(start).as_secs_f64() * 1e6);
        sdk_decode.push(decoded.duration_since(encoded).as_secs_f64() * 1e6);
        total.push(finished.duration_since(start).as_secs_f64() * 1e6);
    }
    let cpu_median = if cpu.len() == iterations {
        format!("{:.3}", median(&mut cpu))
    } else {
        String::new()
    };
    println!(
        "{name},{},{},{},{},{mode},{},{wire_bytes},{compressed},{iterations},{:.3},{:.3},{:.3},{cpu_median}",
        setting.codec.name(),
        setting.level,
        setting.zstd_workers,
        setting.brotli_threads,
        input.len() - 1,
        median(&mut compression),
        median(&mut sdk_decode),
        median(&mut total)
    );
}

fn threshold_payload(size: usize) -> Vec<u8> {
    let message = |error: String| {
        ServerMessage::SubscriptionError(SubscriptionError {
            request_id: None,
            query_set_id: QuerySetId::new(0),
            error: error.into(),
        })
    };
    let overhead = bsatn::to_vec(&message(String::new())).unwrap().len();
    let payload = bsatn::to_vec(&message("x".repeat(size.checked_sub(overhead).unwrap()))).unwrap();
    assert_eq!(payload.len(), size);
    payload
}

fn run_case(path: &Path, name: &str, input: Vec<u8>, iterations: usize, settings: &[Setting]) {
    std::fs::write(path, &input).unwrap();
    drop(input); // Do not retain the large parent fixture while workers run.
    for (index, setting) in settings.iter().enumerate() {
        // Reverse pair order for alternating settings to reduce systematic bias.
        let modes = if index % 2 == 0 {
            ["buffered", "streaming"]
        } else {
            ["streaming", "buffered"]
        };
        for mode in modes {
            eprintln!(
                "{name}: {} {}, {} zstd workers, {} Brotli threads, {mode}, {iterations} samples",
                setting.codec.name(),
                setting.level,
                setting.zstd_workers,
                setting.brotli_threads
            );
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--worker")
                .arg(path)
                .arg(name)
                .arg(setting.codec.name())
                .arg(setting.level.to_string())
                .arg(setting.zstd_workers.to_string())
                .arg(setting.brotli_threads.to_string())
                .arg(mode)
                .arg(iterations.to_string())
                .status()
                .unwrap()
                .success());
        }
    }
}

fn help() {
    println!(
        "Usage: compression-pipeline-bench [--sweet-spot] [--iterations N] [--snapshot-mib N]\n\
        Or: compression-pipeline-bench --large-initial-response [--sweet-spot] [--iterations N]\n\
        Or: compression-pipeline-bench --file RAW_BSATN [--sweet-spot] [--iterations N]\n\
        Add --zstd-mt for a zstd-only sweep with 0 (synchronous), 2, and 4 worker threads.\n\
        Workers are in addition to the coordinating caller; these are not hard CPU caps.\n\
        Add --brotli-mt for Brotli qualities 1-4: original server API (0), compress_multi with 1/2/4 threads.\n\
        Brotli thread counts include the caller. MT presets are mutually exclusive.\n\
        Add --mt-size-sweep for a focused 4 KiB-64 MiB sweep: Brotli 2, zstd -2/1, synchronous/2/4 threads.\n\
        Default: live update, 1023/1024/1025-byte threshold probes, 4 MiB snapshot.\n\
        Large preset: 500 MiB snapshot only. Inputs must be single v2 ServerMessages.\n\
        Server threshold: compress only when raw BSATN > 1024 bytes.\n\
        LZ4 framed default is included (CSV level 0 is a label, not a compression level).\n\
        CSV stdout, progress stderr. CPU pipeline only; no network or table/cache row decoding."
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--worker") {
        worker(&args);
        return;
    }
    let mut sweet_spot = false;
    let mut large = false;
    let mut file = None;
    let mut snapshot_mib = None;
    let mut iterations = None;
    let mut zstd_mt = false;
    let mut brotli_mt = false;
    let mut mt_size_sweep = false;
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sweet-spot" => sweet_spot = true,
            "--zstd-mt" => zstd_mt = true,
            "--brotli-mt" => brotli_mt = true,
            "--mt-size-sweep" => mt_size_sweep = true,
            "--large-initial-response" => large = true,
            "--file" => file = Some(args.next().expect("missing input path")),
            "--snapshot-mib" => {
                snapshot_mib = Some(
                    args.next()
                        .expect("missing size")
                        .parse::<usize>()
                        .expect("invalid size"),
                )
            }
            "--iterations" => {
                let n = args
                    .next()
                    .expect("missing iterations")
                    .parse::<usize>()
                    .expect("invalid iterations");
                assert!(n > 0);
                iterations = Some(n);
            }
            "--help" | "-h" => {
                help();
                return;
            }
            _ => panic!("unknown argument {arg}; use --help"),
        }
    }
    assert!(
        !large || (file.is_none() && snapshot_mib.is_none()),
        "large preset conflicts with file/size"
    );
    assert!(
        file.is_none() || snapshot_mib.is_none(),
        "file conflicts with snapshot size"
    );
    assert!(!(zstd_mt && brotli_mt), "choose one MT preset");
    assert!(
        !mt_size_sweep || (!zstd_mt && !brotli_mt && !sweet_spot && !large && file.is_none() && snapshot_mib.is_none()),
        "--mt-size-sweep cannot be combined with other input/codec presets"
    );
    let settings = if mt_size_sweep {
        mt_size_settings()
    } else if brotli_mt {
        brotli_mt_settings()
    } else if zstd_mt {
        zstd_mt_settings(sweet_spot)
    } else {
        settings(sweet_spot)
    };
    let path = std::env::temp_dir().join(format!("spacetimedb-pipeline-{}.bsatn", std::process::id()));
    // Fail rather than overwrite an existing fixture at this process-specific path.
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    let _cleanup = TempInput(path.clone());
    println!("case,codec,level,zstd_workers,brotli_threads,mode,input_bytes,wire_bytes,compressed,iterations,compression_median_us,sdk_decode_median_us,pipeline_median_us,process_cpu_median_us");
    if mt_size_sweep {
        for &kib in MT_SIZE_KIB {
            run_case(
                &path,
                &format!("size_{kib}kib"),
                bsatn::to_vec(&fixture::message(kib * 1024, true)).unwrap(),
                iterations.unwrap_or_else(|| size_sweep_iterations(kib)),
                &settings,
            );
        }
    } else if let Some(file) = file {
        run_case(
            &path,
            "file",
            std::fs::read(file).unwrap(),
            iterations.unwrap_or(3),
            &settings,
        );
    } else if large {
        run_case(
            &path,
            "initial_500mib",
            bsatn::to_vec(&fixture::message(500 * 1048576, true)).unwrap(),
            iterations.unwrap_or(3),
            &settings,
        );
    } else {
        // Uncompressed fixture path exercises the same message bytes as the other benches.
        let live = fixture::wire(256, false, 0);
        run_case(&path, "live", live[1..].to_vec(), iterations.unwrap_or(100), &settings);
        for size in [1023, 1024, 1025] {
            run_case(
                &path,
                &format!("threshold_{size}"),
                threshold_payload(size),
                iterations.unwrap_or(100),
                &settings,
            );
        }
        let size = snapshot_mib.unwrap_or(4).checked_mul(1048576).expect("size overflow");
        run_case(
            &path,
            "snapshot",
            bsatn::to_vec(&fixture::message(size, true)).unwrap(),
            iterations.unwrap_or(3),
            &settings,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_threshold_and_full_pipeline_roundtrips() {
        for size in [1023, 1024, 1025, 4096] {
            let input = threshold_payload(size);
            let mut tagged = vec![SERVER_MSG_COMPRESSION_TAG_NONE];
            tagged.extend_from_slice(&input);
            let tagged = Arc::new(tagged);
            for setting in settings(true) {
                let frame = encode_frame(&tagged, setting).unwrap();
                assert_eq!(
                    matches!(frame, Cow::Borrowed(_)),
                    size <= SERVER_COMPRESSION_THRESHOLD || setting.codec == Codec::None
                );
                assert_eq!(
                    frame[0] != 0,
                    size > SERVER_COMPRESSION_THRESHOLD && setting.codec != Codec::None
                );
                for mode in ["buffered", "streaming"] {
                    let decoded = decode_frame(&frame, mode).unwrap();
                    assert_eq!(bsatn::to_vec(&decoded).unwrap(), input);
                }
            }
        }
    }

    #[test]
    fn pipeline_snapshot_roundtrips() {
        let input = bsatn::to_vec(&fixture::message(128 * 1024, true)).unwrap();
        let mut tagged = vec![SERVER_MSG_COMPRESSION_TAG_NONE];
        tagged.extend_from_slice(&input);
        let tagged = Arc::new(tagged);
        for setting in settings(true) {
            let frame = encode_frame(&tagged, setting).unwrap();
            for mode in ["buffered", "streaming"] {
                assert_eq!(bsatn::to_vec(&decode_frame(&frame, mode).unwrap()).unwrap(), input);
            }
        }
    }

    #[test]
    fn zstd_multithreaded_roundtrips_and_threshold() {
        for size in [1024, 1025, 2 * 1024 * 1024] {
            let input = threshold_payload(size);
            let mut tagged = vec![SERVER_MSG_COMPRESSION_TAG_NONE];
            tagged.extend_from_slice(&input);
            let tagged = Arc::new(tagged);
            for setting in zstd_mt_settings(false) {
                let frame = encode_frame(&tagged, setting).unwrap();
                assert_eq!(frame[0] != 0, size > SERVER_COMPRESSION_THRESHOLD);
                for mode in ["buffered", "streaming"] {
                    assert_eq!(bsatn::to_vec(&decode_frame(&frame, mode).unwrap()).unwrap(), input);
                }
            }
        }
    }

    #[test]
    fn brotli_multithreaded_roundtrips_and_threshold() {
        let inputs = [
            threshold_payload(1024),
            threshold_payload(1025),
            bsatn::to_vec(&fixture::message(2 * 1024 * 1024, true)).unwrap(),
        ];
        for input in inputs {
            let mut tagged = vec![SERVER_MSG_COMPRESSION_TAG_NONE];
            tagged.extend_from_slice(&input);
            let tagged = Arc::new(tagged);
            for setting in brotli_mt_settings() {
                let frame = encode_frame(&tagged, setting).unwrap();
                assert_eq!(
                    matches!(frame, Cow::Borrowed(_)),
                    input.len() <= SERVER_COMPRESSION_THRESHOLD
                );
                assert_eq!(frame[0] != 0, input.len() > SERVER_COMPRESSION_THRESHOLD);
                for mode in ["buffered", "streaming"] {
                    assert_eq!(bsatn::to_vec(&decode_frame(&frame, mode).unwrap()).unwrap(), input);
                }
            }
            // MT input ownership must be restored/released, not leaked per call.
            assert_eq!(Arc::strong_count(&tagged), 1);
        }
    }

    #[test]
    fn mt_size_sweep_configuration() {
        let settings = mt_size_settings();
        assert_eq!(settings.len(), 10);
        assert_eq!(settings.iter().filter(|s| s.codec == Codec::Brotli).count(), 4);
        assert_eq!(settings.iter().filter(|s| s.codec == Codec::Zstd).count(), 6);
        assert!(MT_SIZE_KIB.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(size_sweep_iterations(64), 100);
        assert_eq!(size_sweep_iterations(1024), 10);
        assert_eq!(size_sweep_iterations(65536), 5);
    }
}
