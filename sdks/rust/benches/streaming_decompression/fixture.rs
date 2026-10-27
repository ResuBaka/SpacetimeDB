use spacetimedb_client_api_messages::websocket::{common::*, v2::*};
use spacetimedb_sats::bsatn;
use std::io::Write;

pub fn message(size: usize, snapshot: bool) -> ServerMessage {
    // Fixed-size rows: id, repeated category, pseudo-random value, padding.
    // Synthetic but neither all-zero nor incompressible; stable across runs.
    let mut data = Vec::new();
    let mut state = 12345u64;
    for id in 0..size.div_ceil(64) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&(id as u64).to_le_bytes());
        data.extend_from_slice(&((id % 16) as u64).to_le_bytes());
        data.extend_from_slice(&state.to_le_bytes());
        data.extend_from_slice(&[0; 40]);
    }
    let rows = BsatnRowList::new(RowSizeHint::FixedSize(64), data.into());
    if snapshot {
        SubscribeApplied {
            request_id: 7,
            query_set_id: QuerySetId::new(3),
            rows: QueryRows {
                tables: vec![SingleTableRows {
                    table: "inventory".into(),
                    rows,
                }]
                .into(),
            },
        }
        .into()
    } else {
        TransactionUpdate {
            query_sets: vec![QuerySetUpdate {
                query_set_id: QuerySetId::new(3),
                tables: vec![TableUpdate {
                    table_name: "inventory".into(),
                    rows: vec![TableUpdateRows::PersistentTable(PersistentTableRows {
                        inserts: rows,
                        deletes: BsatnRowList::default(),
                    })]
                    .into(),
                }]
                .into(),
            }]
            .into(),
        }
        .into()
    }
}

pub fn compress(bytes: &[u8], tag: u8) -> Vec<u8> {
    let mut out = vec![tag];
    match tag {
        SERVER_MSG_COMPRESSION_TAG_NONE => out.extend_from_slice(bytes),
        SERVER_MSG_COMPRESSION_TAG_BROTLI => {
            let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 4, 22);
            writer.write_all(bytes).unwrap();
        }
        SERVER_MSG_COMPRESSION_TAG_GZIP => {
            let mut writer = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            writer.write_all(bytes).unwrap();
            writer.finish().unwrap();
        }
        _ => panic!("invalid fixture codec"),
    }
    out
}

pub fn wire(size: usize, snapshot: bool, tag: u8) -> Vec<u8> {
    compress(&bsatn::to_vec(&message(size, snapshot)).unwrap(), tag)
}
