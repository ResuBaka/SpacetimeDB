//! Native-only benchmark experiment; tag 3 is private to this runner, NOT a
//! server protocol tag. The SDK continues rejecting it on real connections.
use spacetimedb_client_api_messages::websocket::v2::ServerMessage;
use spacetimedb_sats::bsatn;
use spacetimedb_sdk::unstable::streaming_decompression as sdk;
use std::io::{Read, Write};

pub const EXPERIMENTAL_TAG: u8 = 3;

pub fn compress(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![EXPERIMENTAL_TAG];
    // Explicit level 3, checksum enabled so post-decode validation is tested.
    let mut encoder = zstd::stream::write::Encoder::new(&mut out, 3).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap();
    out
}

pub fn buffered(wire: &[u8]) -> Result<ServerMessage, String> {
    if wire.first() != Some(&EXPERIMENTAL_TAG) {
        return sdk::buffered(wire);
    }
    let mut decoder = zstd::stream::read::Decoder::new(&wire[1..]).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    bsatn::from_slice(&bytes).map_err(|e| e.to_string())
}

pub fn streaming(wire: &[u8]) -> Result<ServerMessage, String> {
    if wire.first() != Some(&EXPERIMENTAL_TAG) {
        return sdk::streaming(wire);
    }
    let decoder = zstd::stream::read::Decoder::new(&wire[1..]).map_err(|e| e.to_string())?;
    // The zstd Read decoder validates checksums and, by default, concatenated
    // frames. Both comparison paths use exactly the same codec configuration.
    sdk::from_reader(decoder, "experimental zstd")
}
