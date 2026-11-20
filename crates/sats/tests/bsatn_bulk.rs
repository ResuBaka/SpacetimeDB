use spacetimedb_sats::{bsatn, de::Deserialize, i256, ser::Serialize, u256};

#[test]
fn numeric_arrays_match_scalar_wire_format() {
    macro_rules! check {
        ($ty:ty, $($value:expr),*) => {{
            let values: &[$ty] = &[$($value),*];
            let mut expected = (values.len() as u32).to_le_bytes().to_vec();
            for v in values {
                expected.extend(v.to_le_bytes());
            }
            assert_eq!(bsatn::to_len(values).unwrap(), expected.len());
            assert_eq!(bsatn::to_vec(values).unwrap(), expected);

            let mut buf = vec![0xff]; // Exercise unaligned payloads and a nonempty writer.
            bsatn::to_writer(&mut buf, values).unwrap();
            buf.push(0xaa);
            let mut reader = &buf[1..];
            let decoded: Vec<$ty> = bsatn::from_reader(&mut reader).unwrap();
            assert_eq!(reader, [0xaa]);
            // Compare bits, including NaN payloads and negative zero.
            assert!(decoded.iter().map(|x| x.to_le_bytes()).eq(values.iter().map(|x| x.to_le_bytes())));
            for end in 0..expected.len() {
                assert!(bsatn::from_slice::<Vec<$ty>>(&expected[..end]).is_err());
            }
        }};
    }
    check!(u16,);
    check!(i8, -128, 0, 127);
    check!(u16, 0x1234, u16::MAX);
    check!(i16, -1234, i16::MIN);
    check!(u32, 0x1234_5678);
    check!(i32, -1234, i32::MIN);
    check!(u64, 1, u64::MAX);
    check!(i64, -1, i64::MIN);
    check!(u128, u128::MAX - 3);
    check!(i128, i128::MIN + 17);
    check!(u256, u256::MAX, u256::ONE);
    check!(i256, i256::MIN, i256::MINUS_ONE);
    check!(f32, -0.0, f32::from_bits(0x7fc0_0123));
    check!(f64, -0.0, f64::from_bits(0xfff8_0000_0000_0001));
    assert!(bsatn::from_slice::<Vec<u128>>(&u32::MAX.to_le_bytes()).is_err());
    assert_eq!(
        bsatn::from_slice::<Vec<bool>>(&[1, 0, 0, 0, 2]),
        Err(bsatn::DecodeError::InvalidBool(2))
    );
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats)]
#[repr(C)]
struct Dense {
    a: i64,
    b: u32,
    c: f32,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats)]
#[repr(C)]
struct Padded {
    a: u8,
    b: u64,
}

#[test]
fn nested_products_match_wire_order_at_any_alignment() {
    let expected = bsatn::to_vec(&(
        vec![(-7_i64, 42_u32, -1.5_f32)],
        vec![(3_u8, 9_u64)],
        Some((2_i64, 3_u32, 4_f32)),
    ))
    .unwrap();
    let value = (
        vec![Dense { a: -7, b: 42, c: -1.5 }],
        vec![Padded { a: 3, b: 9 }],
        Some(Dense { a: 2, b: 3, c: 4.0 }),
    );
    type Rows = (Vec<Dense>, Vec<Padded>, Option<Dense>);
    assert_eq!(bsatn::to_vec(&value).unwrap(), expected);
    for offset in 0..8 {
        let mut buf = vec![0xff; offset];
        buf.extend(&expected);
        assert_eq!(bsatn::from_slice::<Rows>(&buf[offset..]).unwrap(), value);
    }
    for end in 0..expected.len() {
        assert!(bsatn::from_slice::<Rows>(&expected[..end]).is_err());
    }
}

#[derive(Debug, PartialEq, Clone, Copy, bytemuck::Zeroable, bytemuck::Pod, Serialize, Deserialize)]
#[sats(crate = spacetimedb_sats, bsatn_pod)]
#[repr(C)]
struct PodRow {
    id: u64,
    coords: f64,
}

#[test]
fn checked_pod_rows_match_scalars_and_reject_truncation() {
    let rows = vec![PodRow { id: 42, coords: -0.0 }, PodRow { id: 7, coords: 2.5 }];
    let expected = bsatn::to_vec(&vec![(42u64, -0.0f64), (7, 2.5)]).unwrap();
    assert_eq!(bsatn::to_vec(&rows).unwrap(), expected);
    for offset in 0..8 {
        let mut buf = vec![0xa5; offset];
        buf.extend(&expected);
        buf.push(0xff);
        let mut reader = &buf[offset..];
        let got = bsatn::from_reader::<Vec<PodRow>>(&mut reader).unwrap();
        assert_eq!(got, rows);
        assert_eq!(got[0].coords.to_bits(), (-0.0f64).to_bits());
        assert_eq!(reader, [0xff]);
    }
    for end in 0..expected.len() {
        assert!(bsatn::from_slice::<Vec<PodRow>>(&expected[..end]).is_err());
    }
    assert!(bsatn::from_slice::<Vec<PodRow>>(&u32::MAX.to_le_bytes()).is_err());
    assert!(bsatn::from_slice::<Vec<PodRow>>(&0u32.to_le_bytes())
        .unwrap()
        .is_empty());
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[sats(crate = spacetimedb_sats)]
struct Mixed {
    id: u64,
    label: String,
    tag: Option<i32>,
}

#[test]
fn mixed_size_hint_matches_wire_length() {
    for text in ["", "subscription", "Bücher 🦀"] {
        for tag in [None, Some(-9)] {
            let row = Mixed {
                id: 1,
                label: text.into(),
                tag,
            };
            let wire = bsatn::to_vec(&(1u64, text, tag)).unwrap();
            assert_eq!(row.bsatn_size_hint(), Some(wire.len()));
            assert_eq!(bsatn::to_vec(&row).unwrap(), wire);
            assert_eq!(bsatn::from_slice::<Mixed>(&wire).unwrap(), row);
        }
    }
}

#[test]
fn string_validation_matches_std_at_simd_boundaries() {
    for n in [0, 1, 63, 64, 65, 127, 128, 4096] {
        let mut bytes = vec![b'x'; n];
        for suffix in [&b""[..], "🦀".as_bytes(), &[0xff], &[0xc0, 0x80], &[0xf0, 0x9f, 0xa6]] {
            bytes.truncate(n);
            bytes.extend_from_slice(suffix);
            let wire = bsatn::to_vec(&bytes).unwrap();
            let expected = std::str::from_utf8(&bytes).map_err(|_| bsatn::DecodeError::InvalidUtf8);
            assert_eq!(bsatn::from_slice::<&str>(&wire), expected);
            assert_eq!(bsatn::from_slice::<String>(&wire), expected.map(str::to_owned));
        }
    }
}

#[test]
fn ordinary_products_and_sums_keep_scalar_wire_format() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[sats(crate = spacetimedb_sats)]
    struct Fields {
        x: u16,
        y: i64,
        enabled: bool,
        label: String,
        z: f32,
        index: u8,
    }
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[sats(crate = spacetimedb_sats)]
    enum Packet<R> {
        Empty,
        Rows { rows: Box<[R]> },
    }
    let row = Fields {
        x: 4097,
        y: -17,
        enabled: true,
        label: "abc".into(),
        z: -1.5,
        index: 9,
    };
    let packet = Packet::Rows { rows: Box::new([row]) };
    let mut wire = vec![1, 1, 0, 0, 0];
    wire.extend(bsatn::to_vec(&(4097u16, -17i64, true, "abc", -1.5f32, 9u8)).unwrap());
    assert_eq!(packet.bsatn_size_hint(), Some(wire.len()));
    assert_eq!(bsatn::to_vec(&packet).unwrap(), wire);
    assert_eq!(bsatn::from_slice::<Packet<Fields>>(&wire).unwrap(), packet);
    for end in 0..wire.len() {
        assert!(bsatn::from_slice::<Packet<Fields>>(&wire[..end]).is_err());
    }
    wire[5 + 2 + 8] = 2;
    assert_eq!(
        bsatn::from_slice::<Packet<Fields>>(&wire),
        Err(bsatn::DecodeError::InvalidBool(2))
    );
    assert_eq!(
        bsatn::from_slice::<Packet<Fields>>(&[7]),
        Err(bsatn::DecodeError::InvalidTag {
            tag: 7,
            sum_name: Some("Packet".into())
        })
    );
}

#[test]
fn primitive_spelling_is_not_a_wire_layout_guarantee() {
    #[allow(non_camel_case_types)]
    type u32 = String;
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[sats(crate = spacetimedb_sats)]
    struct Named {
        id: u64,
        value: u32,
        flag: u8,
    }

    let row = Named {
        id: 5,
        value: "variable-width value".into(),
        flag: 3,
    };
    let wire = bsatn::to_vec(&(row.id, row.value.as_str(), row.flag)).unwrap();
    assert_eq!(bsatn::to_vec(&row).unwrap(), wire);
    assert_eq!(bsatn::from_slice::<Named>(&wire).unwrap(), row);
    let mut list = 9u32.to_le_bytes().to_vec();
    list.extend(wire.repeat(9));
    let rows = bsatn::from_slice::<Vec<Named>>(&list).unwrap();
    assert!(rows.iter().all(|item| item == &row));
    assert_eq!(bsatn::to_vec(&rows).unwrap(), list);
}

#[test]
fn ordinary_numeric_rows_use_wire_width_not_rust_stride() {
    #[derive(Debug, PartialEq, Deserialize, Serialize)]
    #[sats(crate = spacetimedb_sats)]
    struct Point {
        layer: u8,
        entity: u64,
        x: i32,
    }
    let rows: Vec<_> = (0..17)
        .map(|i| Point {
            layer: 2,
            entity: u64::MAX - i,
            x: -(i as i32),
        })
        .collect();
    let mut expected = (rows.len() as u32).to_le_bytes().to_vec();
    for row in &rows {
        expected.push(row.layer);
        expected.extend(row.entity.to_le_bytes());
        expected.extend(row.x.to_le_bytes());
    }
    assert_eq!(rows.bsatn_size_hint(), Some(expected.len()));
    assert_eq!(bsatn::to_len(&rows).unwrap(), expected.len());
    assert_eq!(bsatn::to_vec(&rows).unwrap(), expected);
    let mut reused = vec![0x7f];
    bsatn::to_writer(&mut reused, &rows).unwrap();
    assert_eq!(reused[0], 0x7f);
    assert_eq!(&reused[1..], expected);
    let mut shared = bytes::BytesMut::from(&b"x"[..]);
    bsatn::to_writer(&mut shared, &rows).unwrap();
    assert_eq!(&shared[1..], expected);
    let mut fixed_buf = vec![0xee; expected.len() + 3];
    let mut dst = fixed_buf[1..].as_mut();
    bsatn::to_writer(&mut dst, &rows).unwrap();
    assert_eq!(dst, [0xee; 2]);
    assert_eq!(&fixed_buf[1..1 + expected.len()], expected);
    #[cfg(feature = "serde")]
    {
        let json = serde_json::to_value(spacetimedb_sats::serde::SerdeWrapper::from_ref(&rows)).unwrap();
        assert_eq!(json.as_array().unwrap().len(), rows.len());
        assert_eq!(json[0], serde_json::json!({"layer": 2, "entity": u64::MAX, "x": 0}));
    }
    assert_eq!(bsatn::from_slice::<Vec<Point>>(&expected).unwrap(), rows);
    assert!(bsatn::from_slice::<Vec<Point>>(&expected[..expected.len() - 1]).is_err());

    // A short scalar run should preserve field-specific errors and cursor position.
    let mut partial = &expected[4..8];
    let err = bsatn::from_reader::<Point>(&mut partial).unwrap_err();
    assert_eq!(
        err,
        bsatn::DecodeError::BufferLength {
            for_type: "u64",
            expected: 8,
            given: 3
        }
    );
    assert_eq!(partial, &expected[5..8]);
}

#[test]
fn unfinished_fixed_row_batch_is_not_published() {
    use spacetimedb_sats::buffer::BufWriter;
    fn check<B: BufWriter + AsRef<[u8]>>(mut dst: B) {
        let fail = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dst.put_fixed_rows(&[1, 2, 3], |&n| {
                assert!(n < 3);
                [n; 7]
            });
        }));
        assert!(fail.is_err());
        dst.put_slice(&[42]);
        assert_eq!(dst.as_ref(), [9, 42]);
    }
    check(vec![9]);
    check(bytes::BytesMut::from(&[9][..]));
}

#[test]
fn size_hints_do_not_scan_long_variable_width_lists() {
    let small = vec![Some("a"), None, Some("bb")];
    assert_eq!(small.bsatn_size_hint(), Some(bsatn::to_len(&small).unwrap()));
    let large = vec!["abc"; 8192];
    assert_eq!(large.bsatn_size_hint(), None);
    let fixed = vec![3u64; 8192];
    assert_eq!(fixed.bsatn_size_hint(), Some(4 + 8 * fixed.len()));
}

#[cfg(feature = "serde")]
#[test]
fn packed_arrays_keep_their_json_representation() {
    use spacetimedb_sats::serde::SerdeWrapper;
    let rows = vec![Dense { a: -7, b: 42, c: -1.5 }];
    let json = serde_json::json!([{"a": -7, "b": 42, "c": -1.5}]);
    assert_eq!(serde_json::to_value(SerdeWrapper::from_ref(&rows)).unwrap(), json);
    assert_eq!(
        serde_json::from_value::<SerdeWrapper<Vec<Dense>>>(json).unwrap().0,
        rows
    );
    let pods = vec![PodRow { id: 7, coords: 1.25 }];
    let json = serde_json::json!([{"id": 7, "coords": 1.25}]);
    assert_eq!(serde_json::to_value(SerdeWrapper::from_ref(&pods)).unwrap(), json);
    assert_eq!(
        serde_json::from_value::<SerdeWrapper<Vec<PodRow>>>(json).unwrap().0,
        pods
    );
}
