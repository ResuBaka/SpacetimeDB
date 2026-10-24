#[cfg(test)]
mod sparse_dispatch_tests {
    use super::*;
    use __sdk::DbUpdate as _;

    fn person(id: u32) -> Person {
        Person {
            id,
            name: format!("person-{id}"),
            age: 20,
        }
    }

    fn rows(id: u32) -> __ws::common::BsatnRowList {
        let bytes = __sats::bsatn::to_vec(&person(id)).unwrap();
        __ws::common::BsatnRowList::new(__ws::common::RowSizeHint::FixedSize(bytes.len() as u16), bytes.into())
    }

    fn table(name: &'static str, rows: __ws::common::BsatnRowList) -> __ws::v2::SingleTableRows {
        __ws::v2::SingleTableRows {
            table: name.into(),
            rows,
        }
    }

    #[test]
    fn empty_updates_and_diffs_are_vector_sized_and_allocate_nothing() {
        assert_eq!(std::mem::size_of::<DbUpdate>(), std::mem::size_of::<Vec<()>>());
        assert_eq!(std::mem::size_of::<AppliedDiff<'_>>(), std::mem::size_of::<Vec<()>>());
        assert_eq!(DbUpdate::default().tables.capacity(), 0);
        assert_eq!(AppliedDiff::default().tables.capacity(), 0);
    }

    #[test]
    fn wire_capacity_hints_avoid_four_entry_growth_and_are_schema_bounded() {
        let single = DbUpdate::parse_initial_rows(__ws::v2::QueryRows {
            tables: vec![table("person", Default::default())].into(),
        })
        .unwrap();
        assert_eq!(single.tables.capacity(), 1);
        let repeated = DbUpdate::parse_initial_rows(__ws::v2::QueryRows {
            tables: (0..100).map(|_| table("person", Default::default())).collect(),
        })
        .unwrap();
        assert_eq!(repeated.tables.len(), 1);
        assert_eq!(repeated.tables.capacity(), 6, "six-table fixture bounds the hint");
    }

    #[test]
    fn repeated_chunks_merge_in_row_order_and_tables_keep_dispatch_order() {
        let mut update = DbUpdate::parse_initial_rows(__ws::v2::QueryRows {
            tables: vec![
                table("person", rows(2)),
                table("logged_out_player", Default::default()),
                table("person", rows(1)),
            ]
            .into(),
        })
        .unwrap();
        assert_eq!(update.tables.len(), 2);
        assert!(update.tables.windows(2).all(|pair| pair[0].order() < pair[1].order()));
        match &update.tables[1] {
            __DbTableUpdate::person(person) => {
                assert_eq!(person.inserts.iter().map(|r| r.row.id).collect::<Vec<_>>(), [2, 1]);
                assert!(person.deletes.is_empty());
            }
            _ => panic!("expected person table after logged_out_player"),
        }
        update.mark_initial();
        match &update.tables[0] {
            __DbTableUpdate::logged_out_player(empty) => {
                assert!(empty.has_changes(), "present empty initial table must survive");
                assert!(empty.inserts.is_empty());
            }
            _ => panic!("missing empty initial table"),
        }
    }

    #[test]
    fn unsubscribe_rows_remain_deletes_and_unknown_names_remain_errors() {
        let update = DbUpdate::parse_unsubscribe_rows(__ws::v2::QueryRows {
            tables: vec![table("person", rows(3)), table("person", rows(4))].into(),
        })
        .unwrap();
        assert_eq!(update.tables.len(), 1);
        match &update.tables[0] {
            __DbTableUpdate::person(person) => {
                assert!(person.inserts.is_empty());
                assert_eq!(person.deletes.iter().map(|r| r.row.id).collect::<Vec<_>>(), [3, 4]);
            }
            _ => panic!("expected person table"),
        }
        assert!(DbUpdate::parse_initial_rows(__ws::v2::QueryRows {
            tables: vec![table("unknown", Default::default())].into(),
        })
        .is_err());
    }

    #[test]
    fn callback_lookup_accepts_shared_access_and_missing_registrations_are_false() {
        let callbacks = __sdk::DbCallbacks::<RemoteModule>::default();
        let shared = &callbacks;
        assert!(!shared.has_callbacks("person"));
        assert!(!shared.has_callbacks("unknown"));
    }

    #[test]
    fn first_chunk_moves_its_row_allocation_without_a_copy() {
        let parsed = __sdk::parse_row_list_as_inserts::<Person>(rows(7)).unwrap();
        let allocation = parsed.inserts.as_ptr();
        let mut update = DbUpdate::default();
        update.append(__DbTableUpdate::person(parsed));
        match &update.tables[0] {
            __DbTableUpdate::person(person) => assert_eq!(person.inserts.as_ptr(), allocation),
            _ => panic!("expected person"),
        }
    }

    #[test]
    fn views_apply_after_tables_but_callbacks_keep_accessor_order() {
        let mut update = DbUpdate::parse_initial_rows(__ws::v2::QueryRows {
            tables: vec![
                table("my_player", Default::default()),
                table("person", Default::default()),
                table("logged_out_player", Default::default()),
            ]
            .into(),
        })
        .unwrap();
        assert!(matches!(update.tables[0], __DbTableUpdate::logged_out_player(_)));
        assert!(matches!(update.tables[1], __DbTableUpdate::person(_)));
        assert!(matches!(update.tables[2], __DbTableUpdate::my_player(_)));
        update.tables.sort_unstable_by_key(__DbTableUpdate::callback_order);
        assert!(matches!(update.tables[1], __DbTableUpdate::my_player(_)));
        assert!(matches!(update.tables[2], __DbTableUpdate::person(_)));

        let mut applied = AppliedDiff {
            tables: vec![
                __AppliedTableDiff::person(Default::default()),
                __AppliedTableDiff::my_player(Default::default()),
                __AppliedTableDiff::logged_out_player(Default::default()),
            ],
        };
        applied.tables.sort_unstable_by_key(__AppliedTableDiff::callback_order);
        assert!(matches!(applied.tables[0], __AppliedTableDiff::logged_out_player(_)));
        assert!(matches!(applied.tables[1], __AppliedTableDiff::my_player(_)));
        assert!(matches!(applied.tables[2], __AppliedTableDiff::person(_)));
    }

    #[test]
    fn transaction_chunks_merge_across_query_sets_without_losing_deletes() {
        let table_update = |id| __ws::v2::TableUpdate {
            table_name: "person".into(),
            rows: vec![__ws::v2::TableUpdateRows::PersistentTable(
                __ws::v2::PersistentTableRows {
                    inserts: rows(id),
                    deletes: rows(id + 10),
                },
            )]
            .into(),
        };
        let update = DbUpdate::try_from(__ws::v2::TransactionUpdate {
            query_sets: vec![
                __ws::v2::QuerySetUpdate {
                    query_set_id: __ws::common::QuerySetId::new(1),
                    tables: vec![table_update(1)].into(),
                },
                __ws::v2::QuerySetUpdate {
                    query_set_id: __ws::common::QuerySetId::new(2),
                    tables: vec![table_update(2)].into(),
                },
            ]
            .into(),
        })
        .unwrap();
        assert_eq!(update.tables.len(), 1);
        match &update.tables[0] {
            __DbTableUpdate::person(person) => {
                assert_eq!(person.inserts.iter().map(|r| r.row.id).collect::<Vec<_>>(), [1, 2]);
                assert_eq!(person.deletes.iter().map(|r| r.row.id).collect::<Vec<_>>(), [11, 12]);
            }
            _ => panic!("expected person"),
        }
    }
}
