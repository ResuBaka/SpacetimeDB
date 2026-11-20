//! Standalone benchmark template; `prepare.py` supplies schema.rs next to this file.
use fearless_simd::{dispatch, prelude::*, u32x8, Level};
use spacetimedb_client_api_messages::websocket::v2::ServerMessage;
use spacetimedb_sats::{bsatn, raw_identifier::RawIdentifier};
use std::{error::Error, hint::black_box, time::Instant};

include!("schema.rs");

fn names_from_capture() -> Result<Vec<RawIdentifier>, Box<dyn Error>> {
    let path = std::env::var_os("STDB_CAPTURE_PATH").ok_or("set STDB_CAPTURE_PATH")?;
    let file = std::fs::read(path)?;
    let mut rest = file.strip_prefix(b"STDBCAP1").ok_or("not an SDK capture")?;
    let mut names = Vec::new();
    while !rest.is_empty() {
        let (h, body) = rest.split_first_chunk::<25>().ok_or("partial header")?;
        let len = usize::try_from(u64::from_le_bytes(h[16..24].try_into()?))?;
        let (mut frame, next) = body.split_at_checked(len).ok_or("partial frame")?;
        match bsatn::from_reader(&mut frame).map_err(|_| "invalid envelope")? {
            ServerMessage::SubscribeApplied(s) => names.extend(s.rows.tables.into_iter().map(|t| t.table)),
            ServerMessage::TransactionUpdate(tx) => {
                for q in tx.query_sets {
                    names.extend(q.tables.into_iter().map(|t| t.table_name));
                }
            }
            _ => (),
        }
        if !frame.is_empty() {
            return Err("trailing bytes in envelope".into());
        }
        rest = next;
    }
    if names.is_empty() {
        return Err("capture has no subscription or transaction table names".into());
    }
    Ok(names)
}

#[inline(always)]
fn bucket(n: &str) -> Option<(usize, usize, u32)> {
    let b = n.as_bytes();
    let len = b.len();
    let &(offset, count) = OFF.get(len)?;
    if count == 0 {
        return None;
    }
    Some((
        offset,
        count,
        u32::from_le_bytes([b[0], b[len / 2], b[len / 3], b[len.saturating_sub(2)]]),
    ))
}

fn scalar(n: &str) -> Option<u16> {
    let (offset, count, tag) = bucket(n)?;
    (offset..offset + count)
        .find(|&i| TAGS[i] == tag && NAMES[IDS[i] as usize] == n)
        .map(|i| IDS[i])
}

#[inline(always)]
fn vector<S: Simd>(s: S, n: &str) -> Option<u16> {
    let (offset, count, tag) = bucket(n)?;
    for i in (offset..offset + count).step_by(8) {
        let mut hits = u32x8::from_slice(s, &TAGS[i..i + 8]).simd_eq(tag).to_bitmask();
        while hits != 0 {
            let id = IDS[i + hits.trailing_zeros() as usize];
            if id != u16::MAX && NAMES[id as usize] == n {
                return Some(id);
            }
            hits &= hits - 1;
        }
    }
    None
}

fn sweep(names: &[RawIdentifier], lookup: impl Fn(&str) -> Option<u16>) -> u64 {
    names
        .iter()
        .map(|n| u64::from(lookup(black_box(n)).unwrap_or(u16::MAX)))
        .sum()
}

fn main() -> Result<(), Box<dyn Error>> {
    let names = names_from_capture()?;
    let level = Level::new();
    let check = |n: &str| {
        let want = generated(n);
        assert!(
            scalar(n) == want && dispatch!(level, s => vector(s, n)) == want,
            "dispatch mismatch"
        );
    };
    for key in NAMES {
        check(key);
        let mut miss = key.as_bytes().to_vec();
        // Prefer a miss that preserves the entire fingerprint, so the exact
        // comparison is exercised even if none of the schema keys collide.
        let len = miss.len();
        let pos = (0..len)
            .find(|i| ![0, len / 2, len / 3, len.saturating_sub(2)].contains(i))
            .unwrap_or(0);
        miss[pos] = b'?';
        check(std::str::from_utf8(&miss)?);
    }
    check("");
    for n in &names {
        check(n);
        if generated(n).is_none() {
            return Err("capture and generated table schema do not match".into());
        }
    }
    type Pass = fn(&[RawIdentifier]) -> u64;
    let passes: &[(&str, Pass)] = &[
        ("generated match", |n| sweep(n, generated)),
        ("scalar filter", |n| sweep(n, scalar)),
        ("SIMD dispatch per name", |n| {
            sweep(n, |k| dispatch!(Level::new(), s => vector(s, k)))
        }),
        (
            "SIMD dispatch per sweep",
            |n| dispatch!(Level::new(), s => sweep(n, |k| vector(s, k))),
        ),
    ];
    let expected = sweep(&names, generated);
    println!("{} names, {} schema tables, {level:?}", names.len(), NAMES.len());
    let mut samples = vec![Vec::new(); passes.len()];
    for r in 0..=16 {
        for j in 0..passes.len() {
            let i = (r + j) % passes.len();
            let start = Instant::now();
            let result = passes[i].1(black_box(&names));
            let elapsed = start.elapsed();
            assert_eq!(result, expected);
            if r > 0 {
                samples[i].push(elapsed.as_secs_f64() * 1000.);
            }
        }
    }
    for ((label, _), times) in passes.iter().zip(&mut samples) {
        times.sort_by(f64::total_cmp);
        println!("{label}: {:.3} ms per capture", (times[7] + times[8]) / 2.);
    }
    Ok(())
}
