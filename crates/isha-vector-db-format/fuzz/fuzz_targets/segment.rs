#![no_main]
use libfuzzer_sys::fuzz_target;
use isha_vector_db_format::segment::{Directory, MetaBlock, MetaRecord, Tombstones, VectorBlock};

fuzz_target!(|data: &[u8]| {
    for stride in [1usize, 4, 16, 3072] {
        if let Ok(block) = VectorBlock::open(data, stride) {
            // Every row the block claims must be readable: `rows()` and `row()` cannot disagree.
            for i in 0..block.rows().min(256) {
                assert!(block.row(i).is_some(), "row {i} of {} missing", block.rows());
            }
        }
    }
    if let Ok(dir) = Directory::open(data) {
        // open() validated every id range, so this can never fail afterwards.
        for i in 0..dir.rows().min(1024) {
            assert!(dir.id(i).is_some(), "id {i} unreadable after a successful open");
        }
    }
    if let Ok(meta) = MetaBlock::open(data) {
        for offset in [0u64, 1, 7, 64] {
            let entry = isha_vector_db_format::RowEntry {
                meta_offset: offset,
                meta_len: (data.len() % 97) as u32,
                inv_norm: 1.0,
                id_offset: 0,
                id_len: 0,
                flags: 0,
            };
            let _ = meta.record(&entry);
        }
    }
    if let Ok(t) = Tombstones::decode(data) {
        assert!(t.live_count() <= t.rows, "live count exceeds the rows it covers");
        assert_eq!(Tombstones::decode(&t.encode()).unwrap(), t);
    }
    let _ = MetaRecord::decode(data);
});
