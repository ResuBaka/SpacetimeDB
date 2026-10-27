//! Run without arguments for a fresh-process comparison matrix.
#[path = "streaming_decompression/lz4.rs"]
mod experimental_lz4;
#[path = "streaming_decompression/zstd.rs"]
mod experimental_zstd;
#[path = "streaming_decompression/fixture.rs"]
mod fixture;

use spacetimedb_client_api_messages::websocket::v2::ServerMessage;

fn buffered(frame: &[u8]) -> Result<ServerMessage, String> {
    if frame.first() == Some(&experimental_lz4::EXPERIMENTAL_TAG) {
        experimental_lz4::buffered(frame)
    } else {
        experimental_zstd::buffered(frame)
    }
}

fn streaming(frame: &[u8]) -> Result<ServerMessage, String> {
    if frame.first() == Some(&experimental_lz4::EXPERIMENTAL_TAG) {
        experimental_lz4::streaming(frame)
    } else {
        experimental_zstd::streaming(frame)
    }
}
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
    time::Instant,
};

struct Measured;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);

fn allocated(size: usize) {
    CALLS.fetch_add(1, Relaxed);
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
}

// Requested bytes, not allocator usable size. Realloc's transient implementation
// overhead and allocator metadata are not included; RSS is reported separately.
unsafe impl GlobalAlloc for Measured {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, old: Layout, new: usize) -> *mut u8 {
        let p = unsafe { System.realloc(p, old, new) };
        if !p.is_null() {
            LIVE.fetch_sub(old.size(), Relaxed);
            allocated(new);
        }
        p
    }
}
#[global_allocator]
static ALLOC: Measured = Measured;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--worker") {
        let path = &args[2];
        let mode = &args[3];
        let iterations: usize = args[4].parse().unwrap();
        assert!(iterations > 0);
        // Fixture generation/compression happens in the parent, not in the RSS
        // measurement process. Retain compressed input, as the SDK does.
        let wire = std::fs::read(path).unwrap();
        let decode = match mode.as_str() {
            "buffered" => buffered,
            "streaming" => streaming,
            _ => panic!("bad mode"),
        };
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        CALLS.store(0, Relaxed);
        let start = Instant::now();
        for _ in 0..iterations {
            drop(black_box(decode(black_box(&wire)).unwrap()));
        }
        let elapsed = start.elapsed();
        let calls = CALLS.load(Relaxed);
        let peak = PEAK.load(Relaxed) - baseline;
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let rss = status
            .lines()
            .find(|l| l.starts_with("VmHWM:"))
            .unwrap_or("VmHWM: unavailable");
        println!(
            "{mode}: {:.3} us/message, {:.1} allocations/message, peak +{:.3} MiB, {rss}",
            elapsed.as_secs_f64() * 1_000_000.0 / iterations as f64,
            calls as f64 / iterations as f64,
            peak as f64 / 1048576.0
        );
        if wire.first() == Some(&experimental_zstd::EXPERIMENTAL_TAG) {
            println!("  zstd: heap/allocations above exclude native C allocations; RSS includes them");
        }
        return;
    }
    // Optional user-supplied, compression-tag-prefixed captured frame:
    // binary --file /path/to/frame [iterations]
    let compare = |path: &std::path::Path, iterations: usize| {
        for mode in ["buffered", "streaming"] {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--worker")
                .arg(path)
                .arg(mode)
                .arg(iterations.to_string())
                .status()
                .unwrap()
                .success());
        }
    };
    if args.get(1).map(String::as_str) == Some("--file") {
        compare(
            std::path::Path::new(&args[2]),
            args.get(3).map(|s| s.parse().unwrap()).unwrap_or(10),
        );
        return;
    }
    if args
        .get(1)
        .map(String::as_str)
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "Usage: streaming-decompression-bench [--large-initial-response [iterations]]\n\
            Or: streaming-decompression-bench --file TAGGED_FRAME [iterations]\n\
            Large preset: 500 MiB row snapshot, all five codecs, 3 iterations by default.\n\
            Expect multi-GiB transient memory and substantial temporary disk usage."
        );
        return;
    }
    let cases = if args.get(1).map(String::as_str) == Some("--large-initial-response") {
        let iterations = args.get(2).map(|s| s.parse().expect("invalid iterations")).unwrap_or(3);
        assert!(iterations > 0);
        vec![("initial_500mib", 500 * 1024 * 1024, true, iterations)]
    } else {
        assert!(args.len() == 1, "unknown arguments; use --help");
        vec![("live", 256, false, 5000), ("snapshot", 32 * 1024 * 1024, true, 10)]
    };
    let path = std::env::temp_dir().join(format!("spacetimedb-decode-{}.bin", std::process::id()));
    for (name, size, snapshot, iterations) in cases {
        for tag in 0..=experimental_lz4::EXPERIMENTAL_TAG {
            let wire = if tag == experimental_lz4::EXPERIMENTAL_TAG {
                experimental_lz4::compress(&spacetimedb_sats::bsatn::to_vec(&fixture::message(size, snapshot)).unwrap())
            } else if tag == experimental_zstd::EXPERIMENTAL_TAG {
                experimental_zstd::compress(
                    &spacetimedb_sats::bsatn::to_vec(&fixture::message(size, snapshot)).unwrap(),
                )
            } else {
                fixture::wire(size, snapshot, tag)
            };
            std::fs::write(&path, &wire).unwrap();
            let codec = [
                "none",
                "brotli q4",
                "gzip fast",
                "zstd level3 (experimental)",
                "lz4 framed (experimental)",
            ][tag as usize];
            println!("{name}, codec={codec}, row bytes={size}, wire bytes={}", wire.len());
            compare(&path, iterations);
        }
    }
    std::fs::remove_file(path).unwrap();
}
