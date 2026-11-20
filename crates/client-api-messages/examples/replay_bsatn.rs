//! Offline codec replay. Run in release mode with STDB_CAPTURE_PATH set to a native SDK capture.
#![allow(
    clippy::disallowed_macros,
    reason = "CLI benchmark reports aggregate results to stdout"
)]

use spacetimedb_client_api_messages::websocket::v2::ServerMessage;
use spacetimedb_sats::bsatn;
use std::{env, fs, hint::black_box, io, time::Instant};

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn frames(file: &[u8]) -> io::Result<Vec<&[u8]>> {
    let mut rest = file
        .strip_prefix(b"STDBCAP1")
        .ok_or_else(|| invalid("not an SDK v1 capture"))?;
    let mut frames = Vec::new();
    while !rest.is_empty() {
        let (header, body) = rest
            .split_first_chunk::<25>()
            .ok_or_else(|| invalid("partial header"))?;
        let len = usize::try_from(u64::from_le_bytes(header[16..24].try_into().unwrap()))
            .map_err(|_| invalid("frame length overflow"))?;
        let (payload, tail) = body.split_at_checked(len).ok_or_else(|| invalid("partial payload"))?;
        frames.push(payload);
        rest = tail;
    }
    Ok(frames)
}

fn measure(label: &str, mut pass: impl FnMut()) {
    pass();
    let mut samples = [0.0; 5];
    for sample in &mut samples {
        let start = Instant::now();
        pass();
        *sample = start.elapsed().as_secs_f64() * 1000.;
    }
    samples.sort_by(f64::total_cmp);
    println!("{label}: {:.3} ms per capture (median of 5)", samples[2]);
}

fn main() -> io::Result<()> {
    let path = env::var_os("STDB_CAPTURE_PATH").ok_or_else(|| invalid("set STDB_CAPTURE_PATH"))?;
    let file = fs::read(path)?;
    let frames = frames(&file)?;
    let mut messages = Vec::with_capacity(frames.len());
    for (i, &payload) in frames.iter().enumerate() {
        let mut reader = payload;
        let message: ServerMessage = bsatn::from_reader(&mut reader)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("invalid BSATN in record {i}")))?;
        if !reader.is_empty() || bsatn::to_vec(&message).map_err(|_| invalid("encode failed"))? != payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("round-trip mismatch in record {i}"),
            ));
        }
        messages.push(message);
    }
    println!(
        "verified {} frames, {} BSATN bytes",
        frames.len(),
        frames.iter().map(|f| f.len()).sum::<usize>()
    );
    measure("decode owned envelope", || {
        for &raw in &frames {
            drop(black_box(bsatn::from_slice::<ServerMessage>(black_box(raw)).unwrap()));
        }
    });
    measure("encode fresh envelope", || {
        for msg in &messages {
            drop(black_box(bsatn::to_vec(black_box(msg)).unwrap()));
        }
    });
    let mut buf = Vec::new();
    measure("encode reused envelope", || {
        for msg in &messages {
            buf.clear();
            bsatn::to_writer(&mut buf, black_box(msg)).unwrap();
            black_box(buf.as_slice());
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::frames;

    #[test]
    fn distinguishes_complete_records_from_a_partial_tail() {
        let mut file = b"STDBCAP1".to_vec();
        let mut header = [0; 25];
        header[16..24].copy_from_slice(&2u64.to_le_bytes());
        for _ in 0..2 {
            let start = file.len();
            file.extend(header);
            file.extend([7, 8]);
            for end in start + 1..file.len() {
                assert!(frames(&file[..end]).is_err());
            }
        }
        assert_eq!(frames(&file).unwrap(), [&[7, 8], &[7, 8]]);
    }
}
