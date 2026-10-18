//! Candidate-only layout diagnostics; CPU/RSS comparison uses the paired drivers.
//! rustc --edition=2021 -O --extern macevery=... tools/index_memory_round4.rs -o ...
//! Pass an existing isolated /private/tmp SQLite clone, never the live database.
use macevery::{compact_index::CompactIndex, db::Database};

fn main() {
    let path = std::fs::canonicalize(std::env::args().nth(1).expect("isolated DB path")).unwrap();
    assert!(
        path.starts_with("/private/tmp"),
        "isolated /private/tmp clone required"
    );
    let database = Database::open_existing(path).unwrap();
    let index = CompactIndex::load(&database).unwrap();
    let memory = index.memory_breakdown();
    println!(
        concat!(
            "{{\"records\":{},\"total_capacity_bytes\":{},",
            "\"record_capacity_bytes\":{},\"metadata_capacity_bytes\":{},",
            "\"string_capacity_bytes\":{},\"string_used_bytes\":{},",
            "\"extension_capacity_bytes\":{},\"container_bytes\":{},",
            "\"narrow_metadata_blocks\":{},\"wide_metadata_blocks\":{},",
            "\"live_metadata_escapes\":{}}}"
        ),
        index.len(),
        index.memory_usage_bytes(),
        memory.record_capacity_bytes,
        memory.metadata_capacity_bytes,
        memory.string_capacity_bytes,
        memory.string_used_bytes,
        memory.extension_capacity_bytes,
        memory.container_bytes,
        memory.narrow_metadata_blocks,
        memory.wide_metadata_blocks,
        memory.live_metadata_escapes
    );
}
