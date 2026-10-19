//! Round-5 candidate layout accounting. Pass a disposable /private/tmp clone.
//! CPU/RSS are measured by the unchanged paired drivers, not this diagnostic.
use macevery::{compact_index::CompactIndex, db::Database};

fn main() {
    let path = std::fs::canonicalize(std::env::args().nth(1).expect("isolated DB clone")).unwrap();
    assert!(path.starts_with("/private/tmp"), "isolated clone required");
    let db = Database::open_existing(path).unwrap();
    let index = CompactIndex::load(&db).unwrap();
    let m = index.memory_breakdown();
    let capacities = [
        ("record_capacity_bytes", m.record_capacity_bytes),
        ("metadata_capacity_bytes", m.metadata_capacity_bytes),
        ("string_capacity_bytes", m.string_capacity_bytes),
        ("extension_capacity_bytes", m.extension_capacity_bytes),
        ("container_bytes", m.container_bytes),
        ("prefix_string_bytes", m.prefix_string_bytes),
        ("prefix_entry_capacity_bytes", m.prefix_entry_capacity_bytes),
        (
            "prefix_lookup_capacity_bytes",
            m.prefix_lookup_capacity_bytes,
        ),
    ];
    let total = capacities
        .iter()
        .map(|(_, bytes)| *bytes as u64)
        .sum::<u64>();
    assert_eq!(total, index.memory_usage_bytes());
    let prefix =
        m.prefix_string_bytes + m.prefix_entry_capacity_bytes + m.prefix_lookup_capacity_bytes;
    assert!(prefix <= 64 * 1024 * 1024);
    let mut fields = capacities
        .iter()
        .map(|(name, bytes)| format!("\"{name}\":{bytes}"))
        .collect::<Vec<_>>();
    fields.extend([
        format!("\"records\":{}", index.len()),
        format!("\"total_capacity_bytes\":{total}"),
        format!("\"string_used_bytes\":{}", m.string_used_bytes),
        format!("\"prefix_entries\":{}", m.prefix_entries),
        format!("\"prefix_owned_capacity_bytes\":{prefix}"),
        format!("\"narrow_metadata_blocks\":{}", m.narrow_metadata_blocks),
        format!("\"wide_metadata_blocks\":{}", m.wide_metadata_blocks),
        format!("\"live_metadata_escapes\":{}", m.live_metadata_escapes),
        "\"sum_matches_total\":true".into(),
    ]);
    println!("{{{}}}", fields.join(","));
}
