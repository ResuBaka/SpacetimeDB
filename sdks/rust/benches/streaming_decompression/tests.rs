mod fixture;

use spacetimedb_client_api_messages::websocket::{common::*, v2::*};
use spacetimedb_sats::bsatn;
use spacetimedb_sdk::unstable::streaming_decompression::{buffered, streaming};

fn equivalent(wire: &[u8]) {
    match (buffered(wire), streaming(wire)) {
        (Ok(a), Ok(b)) => assert_eq!(bsatn::to_vec(&a).unwrap(), bsatn::to_vec(&b).unwrap()),
        (Err(_), Err(_)) => (),
        (a, b) => panic!("baseline and streaming disagree: {a:?} vs {b:?}"),
    }
}

#[test]
fn owned_messages_roundtrip() {
    for tag in 0..=2 {
        for snapshot in [false, true] {
            for size in [0, 64, 256, 1024 * 1024 + 64] {
                let msg = fixture::message(size, snapshot);
                let bytes = bsatn::to_vec(&msg).unwrap();
                let wire = fixture::compress(&bytes, tag);
                assert_eq!(bsatn::to_vec(&streaming(&wire).unwrap()).unwrap(), bytes);
                equivalent(&wire);
            }
        }
        for msg in [
            ServerMessage::SubscriptionError(SubscriptionError {
                request_id: Some(8),
                query_set_id: QuerySetId::new(2),
                error: "a unicode error: 🦀".into(),
            }),
            ServerMessage::UnsubscribeApplied(UnsubscribeApplied {
                request_id: 1,
                query_set_id: QuerySetId::new(2),
                rows: None,
            }),
            ServerMessage::OneOffQueryResult(OneOffQueryResult {
                request_id: 1,
                result: Err("bad query".into()),
            }),
        ] {
            equivalent(&fixture::compress(&bsatn::to_vec(&msg).unwrap(), tag));
        }
    }
}

#[test]
fn truncated_and_malformed_frames() {
    equivalent(&[]);
    equivalent(&[255]);
    for tag in 0..=2 {
        let wire = fixture::wire(256, false, tag);
        for end in 0..wire.len() {
            equivalent(&wire[..end]);
            assert!(
                streaming(&wire[..end]).is_err(),
                "accepted truncation {end}/{} codec {tag}",
                wire.len()
            );
        }
        // Invalid sum tag; missing fields; impossible byte-field length.
        for bytes in [
            vec![255],
            vec![1],
            vec![0; 4],
            vec![3, 1, 0, 0, 0, 0, 255, 255, 255, 255],
        ] {
            let wire = fixture::compress(&bytes, tag);
            equivalent(&wire);
            assert!(streaming(&wire).is_err());
        }
        let msg = ServerMessage::SubscriptionError(SubscriptionError {
            request_id: None,
            query_set_id: QuerySetId::new(0),
            error: "x".into(),
        });
        let mut bytes = bsatn::to_vec(&msg).unwrap();
        *bytes.last_mut().unwrap() = 255;
        let wire = fixture::compress(&bytes, tag);
        equivalent(&wire);
        assert!(streaming(&wire).is_err());
        // Replace the string length with u32::MAX, with only one data byte.
        let start = bytes.len() - 5;
        bytes[start..start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let wire = fixture::compress(&bytes, tag);
        equivalent(&wire);
        assert!(streaming(&wire).is_err());
    }
}

#[test]
fn trailing_bytes_and_members_match_baseline() {
    let mut bytes = bsatn::to_vec(&fixture::message(256, false)).unwrap();
    bytes.extend_from_slice(&[42; 100_000]);
    for tag in 0..=2 {
        // Baseline allows trailing BSATN bytes.
        let wire = fixture::compress(&bytes, tag);
        assert!(buffered(&wire).is_ok());
        equivalent(&wire);
        let mut wire = fixture::wire(256, false, tag);
        wire.extend_from_slice(b"not another member");
        equivalent(&wire);
        let mut wire = fixture::wire(256, false, tag);
        wire.extend_from_slice(&fixture::wire(64, true, tag)[1..]);
        equivalent(&wire);
    }
}

#[test]
fn codec_errors_after_decoding_discard_message() {
    let mut bytes = bsatn::to_vec(&fixture::message(64, false)).unwrap();
    bytes.extend_from_slice(&[42; 100_000]);
    let gzip = fixture::compress(&bytes, SERVER_MSG_COMPRESSION_TAG_GZIP);
    for trailer_offset in [8, 4, 1] {
        let mut bad = gzip.clone();
        let index = bad.len() - trailer_offset;
        bad[index] ^= 0x80;
        equivalent(&bad);
        assert!(streaming(&bad).unwrap_err().contains("decompress"));
    }
    for tag in [SERVER_MSG_COMPRESSION_TAG_BROTLI, SERVER_MSG_COMPRESSION_TAG_GZIP] {
        let mut wire = fixture::compress(&bytes, tag);
        wire.pop();
        equivalent(&wire);
        assert!(streaming(&wire).is_err());
    }
}

#[test]
fn corrupted_frames_match_baseline() {
    for tag in 0..=2 {
        let wire = fixture::wire(256, false, tag);
        for index in 1..wire.len() {
            for bit in 0..8 {
                let mut corrupt = wire.clone();
                corrupt[index] ^= 1 << bit;
                equivalent(&corrupt);
            }
        }
    }
}

#[test]
fn variable_row_offsets_and_event_rows() {
    let rows = BsatnRowList::new(RowSizeHint::RowOffsets(vec![0, 3, 8].into()), vec![1u8; 12].into());
    let msg: ServerMessage = TransactionUpdate {
        query_sets: vec![QuerySetUpdate {
            query_set_id: QuerySetId::new(42),
            tables: vec![TableUpdate {
                table_name: "events".into(),
                rows: vec![TableUpdateRows::EventTable(EventTableRows { events: rows })].into(),
            }]
            .into(),
        }]
        .into(),
    }
    .into();
    for tag in 0..=2 {
        equivalent(&fixture::compress(&bsatn::to_vec(&msg).unwrap(), tag));
    }
}

#[test]
fn connection_reducer_and_procedure_messages() {
    use spacetimedb_sdk::{ConnectionId, Identity, TimeDuration, Timestamp};
    let mut messages = vec![ServerMessage::InitialConnection(InitialConnection {
        identity: Identity::ONE,
        connection_id: ConnectionId::from_u128(u128::MAX),
        token: "token".into(),
    })];
    for result in [
        ReducerOutcome::OkEmpty,
        ReducerOutcome::Err(vec![1, 2, 3].into()),
        ReducerOutcome::InternalError("error".into()),
        ReducerOutcome::Ok(ReducerOk {
            ret_value: vec![4, 5].into(),
            transaction_update: TransactionUpdate {
                query_sets: vec![].into(),
            },
        }),
    ] {
        messages.push(
            ReducerResult {
                request_id: 7,
                timestamp: Timestamp::from_micros_since_unix_epoch(-123),
                result,
            }
            .into(),
        );
    }
    for status in [
        ProcedureStatus::Returned(vec![6, 7].into()),
        ProcedureStatus::InternalError("error".into()),
    ] {
        messages.push(
            ProcedureResult {
                status,
                timestamp: Timestamp::UNIX_EPOCH,
                total_host_execution_duration: TimeDuration::from_micros(42),
                request_id: 8,
            }
            .into(),
        );
    }
    for msg in messages {
        for tag in 0..=2 {
            equivalent(&fixture::compress(&bsatn::to_vec(&msg).unwrap(), tag));
        }
    }
}
