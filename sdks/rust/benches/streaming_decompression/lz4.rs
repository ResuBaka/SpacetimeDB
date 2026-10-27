//! Benchmark-only LZ4 frame codec. Tag 4 is private to the runner, not the SDK.
use spacetimedb_client_api_messages::websocket::v2::ServerMessage;
use spacetimedb_sats::bsatn;
use spacetimedb_sdk::unstable::streaming_decompression as sdk;
use std::io::{self, Read, Write};

pub const EXPERIMENTAL_TAG: u8 = 4;

/// Strict single-frame adapter for the exact format emitted below. lz4_flex
/// 0.12.2 accepts EOF in place of a block header/end marker, bypassing the final
/// checksum. Walk compressed block boundaries (without decoding/copying them)
/// first so a missing footer cannot masquerade as successful stream completion.
/// The actual decoder still verifies header/content checksums while reading.
pub fn decoder(bytes: &[u8]) -> io::Result<lz4_flex::frame::FrameDecoder<&[u8]>> {
    let truncated = || io::Error::new(io::ErrorKind::UnexpectedEof, "truncated LZ4 frame");
    if bytes.len() < 7 {
        return Err(truncated());
    }
    // Modern frame, independent blocks, content checksum, no size/dictionary or
    // block checksums. This is deliberately not a general-purpose LZ4 API.
    if bytes[..4] != [0x04, 0x22, 0x4d, 0x18] || bytes[4] != 0x64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported experimental LZ4 frame header",
        ));
    }
    let mut pos = 7;
    loop {
        let header = bytes.get(pos..pos + 4).ok_or_else(truncated)?;
        let block = u32::from_le_bytes(header.try_into().unwrap());
        pos += 4;
        if block == 0 {
            let end = pos.checked_add(4).ok_or_else(truncated)?;
            bytes.get(pos..end).ok_or_else(truncated)?;
            if end != bytes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "trailing data/frames after LZ4 frame",
                ));
            }
            break;
        }
        let len = (block & 0x7fff_ffff) as usize;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty LZ4 block"));
        }
        let end = pos.checked_add(len).ok_or_else(truncated)?;
        bytes.get(pos..end).ok_or_else(truncated)?;
        pos = end;
    }
    Ok(lz4_flex::frame::FrameDecoder::new(bytes))
}

pub fn write_payload(bytes: &[u8], output: impl Write) -> io::Result<()> {
    let info = lz4_flex::frame::FrameInfo::new().content_checksum(true);
    let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(info, output);
    encoder.write_all(bytes)?;
    encoder.finish()?;
    Ok(())
}

pub fn compress(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![EXPERIMENTAL_TAG];
    write_payload(bytes, &mut out).unwrap();
    out
}

pub fn buffered(wire: &[u8]) -> Result<ServerMessage, String> {
    if wire.first() != Some(&EXPERIMENTAL_TAG) {
        return Err("expected experimental LZ4 tag".into());
    }
    let mut decoder = decoder(&wire[1..]).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    bsatn::from_slice(&bytes).map_err(|e| e.to_string())
}

pub fn streaming(wire: &[u8]) -> Result<ServerMessage, String> {
    if wire.first() != Some(&EXPERIMENTAL_TAG) {
        return Err("expected experimental LZ4 tag".into());
    }
    sdk::from_reader(decoder(&wire[1..]).map_err(|e| e.to_string())?, "experimental lz4")
}
