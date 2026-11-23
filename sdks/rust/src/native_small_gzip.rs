//! Bounded, connection-local fast path. `None` means replay the original frame
//! through the ordinary decoder, including its error and trailer handling.
use crate::websocket::WsError;
use libdeflater::Decompressor;
use spacetimedb_client_api_messages::websocket::{common::SERVER_MSG_COMPRESSION_TAG_GZIP, v2::ServerMessage};

#[derive(Default)]
pub(crate) struct SmallGzip(Option<Decompressor>);

impl SmallGzip {
    pub(crate) fn try_decode(&mut self, wire: &[u8]) -> Option<Result<ServerMessage, WsError>> {
        // Only the simple gzip header (in particular, no unverified FHCRC) and
        // a single final dynamic block. Fixed codes include reserved symbols
        // that libdeflate and zlib don't handle identically.
        let [SERVER_MSG_COMPRESSION_TAG_GZIP, data @ ..] = wire else {
            return None;
        };
        if wire.len() > 4096 || data.len() < 20 || data[..4] != [0x1f, 0x8b, 8, 0] || data[10] & 7 != 5 {
            return None;
        }
        // A hint only: if there is trailing data this needn't be the first
        // member's ISIZE. Never use it to allocate, accept or reject a frame.
        if u32::from_le_bytes(data.last_chunk().copied()?) >= 8192 {
            return None;
        }
        // At exactly 8192 the existing path uses the streaming BSATN decoder,
        // whose errors differ from the slice decoder's on short fields.
        let mut scratch = [0; 8191];
        let n = self
            .0
            .get_or_insert_with(Decompressor::new)
            .gzip_decompress(data, &mut scratch)
            .ok()?;
        if !ordinary_trees(&data[10..])? {
            return None;
        }
        Some(
            spacetimedb_sats::bsatn::from_slice(&scratch[..n]).map_err(|source| WsError::DeserializeMessage { source }),
        )
    }
}

// libdeflate deliberately accepts more DEFLATE bitstreams than zlib does:
// reserved alphabet sizes and empty/singleton Huffman codes (both bit values
// can map to the same symbol). A valid gzip checksum alone cannot distinguish
// these. Only use a successful inflate if all three alphabets were complete,
// with no reserved symbols. We only read the bounded first block's tree
// description, not any payload symbols. The first block must also be the last.
fn ordinary_trees(block: &[u8]) -> Option<bool> {
    fn peek(block: &[u8], pos: usize) -> Option<u16> {
        let word = u16::from_le_bytes(*block.get(pos / 8..)?.first_chunk()?);
        Some(word >> (pos % 8))
    }
    let header = u32::from_le_bytes(*block.first_chunk()?);
    let literals = 257 + ((header >> 3) & 31) as usize;
    let distances = 1 + ((header >> 8) & 31) as usize;
    if literals > 286 || distances > 30 {
        return Some(false);
    }
    let count = 4 + ((header >> 13) & 15) as usize;
    let mut pos = 17;
    let mut lengths = [0u8; 19];
    let mut counts = [0u8; 8];
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    for &sym in &ORDER[..count] {
        let bits = (peek(block, pos)? & 7) as u8;
        pos += 3;
        lengths[sym] = bits;
        counts[usize::from(bits)] += 1;
    }
    // Canonical code -> 7-bit, least-significant-bit-first lookup.
    let mut next = [0u16; 8];
    for len in 2..=7 {
        next[len] = (next[len - 1] + u16::from(counts[len - 1])) * 2;
    }
    if next[7] + u16::from(counts[7]) != 128 {
        return Some(false);
    }
    let mut lookup = [(0u8, 0u8); 128];
    for (sym, &bits) in lengths.iter().enumerate().filter(|(_, bits)| **bits != 0) {
        let code = &mut next[usize::from(bits)];
        let start = usize::from(code.reverse_bits() >> (16 - bits));
        for i in (start..128).step_by(1 << bits) {
            lookup[i] = (sym as u8, bits);
        }
        *code += 1;
    }
    let mut end = 0;
    let mut prev = 0;
    let (mut lit_space, mut dist_space) = (0, 0);
    while end < literals + distances {
        let (sym, bits) = lookup[usize::from(peek(block, pos)? & 127)];
        pos += usize::from(bits);
        let (value, base, extra) = match sym {
            16 if end != 0 => (prev, 3, 2),
            17 => (0, 3, 3),
            18 => (0, 11, 7),
            0..=15 => (sym, 1, 0),
            _ => return None,
        };
        let run = base + usize::from(peek(block, pos)? & ((1 << extra) - 1));
        pos += extra;
        if value != 0 {
            let in_lit = run.min(literals.saturating_sub(end));
            lit_space += in_lit << (15 - value);
            dist_space += (run - in_lit) << (15 - value);
        }
        prev = value;
        end += run;
    }
    // Kraft equality: the codewords cover the complete 15-bit code space.
    // A successful final block also necessarily decoded end-of-block.
    Some(end == literals + distances && lit_space == 1 << 15 && dist_space == 1 << 15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming_decompression::decode;
    use spacetimedb_sats::bsatn;
    use std::io::Write as _;

    // A final dynamic block encoding the BSATN TransactionUpdate with zero query
    // sets: literal 4, literal 0, length 3 at distance 1, end-of-block.
    // Control the distance alphabet independently of the member CRC and size.
    fn empty_tx(distances: usize, singleton: bool, distance_bit: u8) -> Vec<u8> {
        member(4, distances, singleton, distance_bit)
    }

    fn member(tag: u8, distances: usize, singleton: bool, distance_bit: u8) -> Vec<u8> {
        let mut wire = vec![2, 31, 139, 8, 0, 0, 0, 0, 0, 0, 255];
        let mut pos = 0;
        let mut bits = |v: u8, n: u8| {
            for i in 0..n {
                if pos % 8 == 0 {
                    wire.push(0);
                }
                *wire.last_mut().unwrap() |= ((v >> i) & 1) << (pos % 8);
                pos += 1;
            }
        };
        bits(5, 3);
        bits(1, 5); // 258 literal/length symbols
        bits((distances - 1) as u8, 5);
        bits(14, 4); // 18 precode lengths; 0,1,2,18 each use two bits
        for sym in [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1] {
            bits(if matches!(sym, 0 | 1 | 2 | 18) { 2 } else { 0 }, 3);
        }
        for sym in 0..258 {
            bits(u8::from(matches!(sym, 0 | 256 | 257) || sym == usize::from(tag)), 2);
        }
        for sym in 0..distances {
            bits(if sym == 0 || (!singleton && sym == 1) { 2 } else { 0 }, 2);
        }
        bits(2, 2); // literal tag
        bits(0, 2); // literal 0
        bits(3, 2); // length 3
        bits(distance_bit, 1);
        bits(1, 2); // end
        let mut crc = flate2::Crc::new();
        crc.update(&[tag, 0, 0, 0, 0]);
        wire.extend(crc.sum().to_le_bytes());
        wire.extend(5u32.to_le_bytes());
        wire
    }

    fn parity(dec: &mut SmallGzip, wire: &[u8]) {
        let comparable = |r: Result<ServerMessage, WsError>| {
            r.map(|m| bsatn::to_vec(&m).unwrap()).map_err(|e| format!("{e:?}: {e}"))
        };
        let actual = dec.try_decode(wire).unwrap_or_else(|| decode(wire));
        assert_eq!(comparable(actual), comparable(decode(wire)));
    }

    #[test]
    fn permissive_native_success_is_not_sdk_success() {
        let dec = &mut SmallGzip::default();
        let valid = empty_tx(2, false, 0);
        assert!(dec.try_decode(&valid).unwrap().is_ok());
        let bad_bsatn = member(255, 2, false, 0);
        assert!(matches!(
            dec.try_decode(&bad_bsatn),
            Some(Err(WsError::DeserializeMessage { .. }))
        ));
        parity(dec, &bad_bsatn);
        for wire in [empty_tx(31, false, 0), empty_tx(2, true, 1), {
            let mut w = valid.clone();
            w[4] = 2; // FHCRC present but wrong
            w.splice(11..11, [0, 0]);
            w
        }] {
            assert_eq!(Decompressor::new().gzip_decompress(&wire[1..], &mut [0; 16]), Ok(5));
            assert!(matches!(decode(&wire), Err(WsError::Decompress { .. })));
            assert!(dec.try_decode(&wire).is_none());
            parity(dec, &wire);
            parity(dec, &valid);
        }
        // Valid singleton code; conservative fallback must still accept it.
        let wire = empty_tx(2, true, 0);
        assert!(decode(&wire).is_ok());
        parity(dec, &wire);
    }

    #[test]
    fn malformed_trailing_truncated_and_reused() {
        let dec = &mut SmallGzip::default();
        let good = empty_tx(2, false, 0);
        for end in 0..good.len() {
            parity(dec, &good[..end]);
        }
        for i in 1..good.len() {
            for bit in 0..8 {
                let mut bad = good.clone();
                bad[i] ^= 1 << bit;
                parity(dec, &bad);
            }
        }
        for tail in [&b"trailing data"[..], &good[1..], &[0; 4]] {
            let mut w = good.clone();
            w.extend(tail);
            assert!(decode(&w).is_ok());
            parity(dec, &w);
        }
        for len in [4095, 4096, 4097] {
            let mut padded = good.clone();
            padded.resize(len, 0);
            assert_eq!(dec.try_decode(&padded).is_some(), len <= 4096);
            parity(dec, &padded);
        }
        for n in [8191, 8192, 8193, 1024 * 1024] {
            for prefix in [4, 255] {
                let mut plain = vec![0; n];
                plain[0] = prefix;
                let mut encoder = flate2::write::GzEncoder::new(vec![2], flate2::Compression::fast());
                encoder.write_all(&plain).unwrap();
                let mut w = encoder.finish().unwrap();
                parity(dec, &w);
                // Force a small ISIZE hint even on a decompressed-size spill.
                // The fixed output limit and first-member semantics still hold.
                parity(dec, &[w.as_slice(), &[0; 4]].concat());
                let crc = w.len() - 8;
                w[crc] ^= 1;
                parity(dec, &w); // even with invalid BSATN, codec error wins
            }
        }
        parity(dec, &good);
    }
}
