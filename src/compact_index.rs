use crate::db::{Database, DbRecordRef};
use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, SearchOptions, SearchResult};
use crate::search::{effective_limit, top_hits, PreparedQuery, RecordView};
use std::borrow::Cow;
use std::collections::HashMap;
use std::mem::size_of;

const ARENA_BYTES: usize = 64 * 1024 * 1024;
const RECORDS_PER_BLOCK: usize = 64 * 1024;
const INITIAL_ARENA_BYTES: usize = 64 * 1024;
const NO_EXTENSION: u32 = u32::MAX;

/// Searchable fields remain borrowed from bounded arenas; only final hits are decoded.
pub struct CompactIndex {
    arenas: Vec<String>,
    records: Vec<Vec<CompactRecord>>,
    extensions: Vec<Box<str>>,
    len: usize,
    records_per_block: usize,
}

impl Default for CompactIndex {
    fn default() -> Self {
        Self {
            arenas: Vec::new(),
            records: Vec::new(),
            extensions: Vec::new(),
            len: 0,
            records_per_block: RECORDS_PER_BLOCK,
        }
    }
}

struct CompactRecord {
    id: i64,
    indexed_at: i64,
    values: [i64; 5],
    path: Slice,
    path_lower: Slice,
    basename: Slice,
    basename_lower: Slice,
    arena: u32,
    ext: u32,
    kind: FileKind,
    present: u8,
}

impl CompactRecord {
    fn optional(&self, field: usize) -> Option<i64> {
        (self.present & (1 << field) != 0).then_some(self.values[field])
    }
}

#[derive(Clone, Copy, Default)]
struct Slice {
    start: u32,
    len: u32,
}

impl Slice {
    fn checked(start: usize, len: usize) -> Result<Self> {
        let end = start.checked_add(len).ok_or_else(index_too_large)?;
        if end > u32::MAX as usize {
            return Err(index_too_large());
        }
        Ok(Self {
            start: u32::try_from(start).map_err(|_| index_too_large())?,
            len: u32::try_from(len).map_err(|_| index_too_large())?,
        })
    }
}

fn index_too_large() -> MacEveryError {
    MacEveryError::Cli(
        "packed index row or dictionary exceeds its checked 32-bit representation".into(),
    )
}

struct IndexBuilder {
    index: CompactIndex,
    extension_ids: HashMap<Box<str>, u32>,
    arena_bytes: usize,
}

impl IndexBuilder {
    fn new(arena_bytes: usize, records_per_block: usize) -> Self {
        assert!(arena_bytes > 0 && records_per_block > 0);
        Self {
            index: CompactIndex {
                records_per_block,
                ..CompactIndex::default()
            },
            extension_ids: HashMap::new(),
            arena_bytes,
        }
    }

    fn push(&mut self, record: DbRecordRef<'_>) -> Result<()> {
        // Match Database::all_records, including legacy NULL/empty columns and
        // context-sensitive Unicode lowercase (for example Greek final sigma).
        let lowered = record
            .path_lower
            .filter(|value| !value.is_empty())
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(record.path.to_lowercase()));
        let fields = [
            record.path,
            lowered.as_ref(),
            record.basename,
            record.basename_lower,
        ];
        let mut parents = [None; 4];
        let mut required = 0usize;
        for (i, value) in fields.iter().enumerate() {
            Slice::checked(0, value.len())?;
            parents[i] = fields[..i]
                .iter()
                .position(|parent| parent.ends_with(*value));
            if parents[i].is_none() {
                required = required
                    .checked_add(value.len())
                    .ok_or_else(index_too_large)?;
            }
        }
        Slice::checked(0, required)?;
        let arena = self.reserve_row(required)?;
        let mut slices = [Slice::default(); 4];
        for (i, value) in fields.iter().enumerate() {
            slices[i] = if let Some(parent) = parents[i] {
                let offset = fields[parent].len() - value.len();
                Slice::checked(slices[parent].start as usize + offset, value.len())?
            } else {
                let slice = Slice::checked(self.index.arenas[arena].len(), value.len())?;
                self.index.arenas[arena].push_str(value);
                slice
            };
        }
        let ext = match record.ext_lower {
            None => NO_EXTENSION,
            Some(value) => match self.extension_ids.get(value) {
                Some(id) => *id,
                None => {
                    let id = u32::try_from(self.index.extensions.len())
                        .map_err(|_| index_too_large())?;
                    if id == NO_EXTENSION {
                        return Err(index_too_large());
                    }
                    self.index.extensions.push(value.into());
                    self.extension_ids.insert(value.into(), id);
                    id
                }
            },
        };
        if self
            .index
            .records
            .last()
            .is_none_or(|block| block.len() == self.index.records_per_block)
        {
            let mut block = Vec::new();
            block
                .try_reserve_exact(self.index.records_per_block)
                .map_err(|err| {
                    MacEveryError::Cli(format!("cannot allocate packed record block: {err}"))
                })?;
            self.index.records.push(block);
        }
        let optional = [
            record.size,
            record.mtime,
            record.ctime,
            record.dev,
            record.inode,
        ];
        let mut values = [0; 5];
        let mut present = 0;
        for (i, value) in optional.into_iter().enumerate() {
            if let Some(value) = value {
                values[i] = value;
                present |= 1 << i;
            }
        }
        self.index.records.last_mut().unwrap().push(CompactRecord {
            id: record.id,
            indexed_at: record.indexed_at,
            values,
            path: slices[0],
            path_lower: slices[1],
            basename: slices[2],
            basename_lower: slices[3],
            arena: u32::try_from(arena).map_err(|_| index_too_large())?,
            ext,
            kind: record.kind,
            present,
        });
        self.index.len = self.index.len.checked_add(1).ok_or_else(index_too_large)?;
        Ok(())
    }

    fn reserve_row(&mut self, bytes: usize) -> Result<usize> {
        let needs_block = self
            .index
            .arenas
            .last()
            .is_none_or(|arena| bytes > self.arena_bytes.saturating_sub(arena.len()));
        if needs_block {
            u32::try_from(self.index.arenas.len()).map_err(|_| index_too_large())?;
            self.index.arenas.push(String::new());
        }
        let id = self.index.arenas.len() - 1;
        let arena = &mut self.index.arenas[id];
        let needed = arena.len().checked_add(bytes).ok_or_else(index_too_large)?;
        Slice::checked(0, needed)?;
        if arena.capacity() < needed {
            // Growth is bounded by one arena, never by the complete string pool.
            // A row larger than the normal arena gets its own checked block.
            let capacity = needed
                .max(arena.capacity().saturating_mul(2))
                .max(INITIAL_ARENA_BYTES.min(self.arena_bytes))
                .min(self.arena_bytes.max(needed));
            arena
                .try_reserve_exact(capacity - arena.len())
                .map_err(|err| {
                    MacEveryError::Cli(format!("cannot allocate packed string arena: {err}"))
                })?;
        }
        Ok(id)
    }
}

impl CompactIndex {
    pub fn load(db: &Database) -> Result<Self> {
        let mut builder = IndexBuilder::new(ARENA_BYTES, RECORDS_PER_BLOCK);
        db.for_each_record_ref(|record| builder.push(record))?;
        Ok(builder.index)
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Allocated container/payload capacity, excluding allocator overhead and
    /// SQLite/load temporaries. This is not resident memory or process RSS.
    pub fn memory_usage_bytes(&self) -> u64 {
        let mut bytes = size_of::<Self>() as u64;
        for amount in [
            self.arenas.capacity() * size_of::<String>(),
            self.records.capacity() * size_of::<Vec<CompactRecord>>(),
            self.extensions.capacity() * size_of::<Box<str>>(),
        ] {
            bytes = bytes.saturating_add(amount as u64);
        }
        for arena in &self.arenas {
            bytes = bytes.saturating_add(arena.capacity() as u64);
        }
        for block in &self.records {
            bytes = bytes.saturating_add((block.capacity() * size_of::<CompactRecord>()) as u64);
        }
        for ext in &self.extensions {
            bytes = bytes.saturating_add(ext.len() as u64);
        }
        bytes
    }

    pub fn search(&self, options: &SearchOptions) -> Vec<SearchResult> {
        let options = options.normalized();
        let query = PreparedQuery::new(&options);
        top_hits(
            self.iter_records()
                .enumerate()
                .map(|(index, record)| (index, self.view(record))),
            &query,
            effective_limit(options.limit),
        )
        .into_iter()
        .map(|hit| SearchResult {
            record: self.to_file_record(self.record_at(hit.index)),
            score: hit.score,
        })
        .collect()
    }

    fn iter_records(&self) -> impl Iterator<Item = &CompactRecord> {
        self.records.iter().flat_map(|block| block.iter())
    }

    fn record_at(&self, index: usize) -> &CompactRecord {
        &self.records[index / self.records_per_block][index % self.records_per_block]
    }

    fn get(&self, record: &CompactRecord, slice: Slice) -> &str {
        let start = slice.start as usize;
        &self.arenas[record.arena as usize][start..start + slice.len as usize]
    }

    fn ext(&self, record: &CompactRecord) -> Option<&str> {
        (record.ext != NO_EXTENSION).then(|| self.extensions[record.ext as usize].as_ref())
    }

    fn view<'a>(&'a self, record: &'a CompactRecord) -> RecordView<'a> {
        RecordView {
            path: self.get(record, record.path),
            path_lower: self.get(record, record.path_lower),
            basename: self.get(record, record.basename),
            basename_lower: self.get(record, record.basename_lower),
            ext_lower: self.ext(record),
            kind: &record.kind,
            mtime: record.optional(1),
        }
    }

    fn to_file_record(&self, record: &CompactRecord) -> FileRecord {
        FileRecord {
            id: record.id,
            path: self.get(record, record.path).into(),
            path_lower: self.get(record, record.path_lower).into(),
            basename: self.get(record, record.basename).into(),
            basename_lower: self.get(record, record.basename_lower).into(),
            ext_lower: self.ext(record).map(str::to_string),
            kind: record.kind.clone(),
            size: record.optional(0),
            mtime: record.optional(1),
            ctime: record.optional(2),
            dev: record.optional(3),
            inode: record.optional(4),
            indexed_at: record.indexed_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn compact_index_matches_glob_case_insensitive_search() {
        let temp = unique_temp_dir("macevery-compact-test");
        fs::create_dir_all(&temp).unwrap();
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.upsert_file(&FileRecord::new(
            "/tmp/ChatGPT Notes.PDF".to_string(),
            FileKind::File,
            1,
        ))
        .unwrap();
        db.upsert_file(&FileRecord::new(
            "/tmp/sqlite_result_code.h".to_string(),
            FileKind::File,
            1,
        ))
        .unwrap();

        let index = CompactIndex::load(&db).unwrap();
        let results = index.search(&SearchOptions {
            query: "*gpt*pdf".to_string(),
            limit: 10,
            ..SearchOptions::default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].record.basename, "ChatGPT Notes.PDF");

        let _ = fs::remove_dir_all(temp);
    }

    fn push_record(builder: &mut IndexBuilder, record: &FileRecord) {
        builder
            .push(DbRecordRef {
                id: record.id,
                path: &record.path,
                path_lower: Some(&record.path_lower),
                basename: &record.basename,
                basename_lower: &record.basename_lower,
                ext_lower: record.ext_lower.as_deref(),
                kind: record.kind.clone(),
                size: record.size,
                mtime: record.mtime,
                ctime: record.ctime,
                dev: record.dev,
                inode: record.inode,
                indexed_at: record.indexed_at,
            })
            .unwrap();
    }

    fn from_records(records: &[FileRecord]) -> CompactIndex {
        let mut builder = IndexBuilder::new(ARENA_BYTES, RECORDS_PER_BLOCK);
        for record in records {
            push_record(&mut builder, record);
        }
        builder.index
    }

    #[test]
    fn compact_top_k_matches_independent_full_sort_reference() {
        let temp = unique_temp_dir("macevery-packed-differential");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.begin().unwrap();
        for record in crate::search::test_reference::records() {
            db.upsert_file(&record).unwrap();
        }
        db.commit().unwrap();
        let records = db.all_records().unwrap();
        let index = CompactIndex::load(&db).unwrap();
        for mut options in crate::search::test_reference::options() {
            for limit in [
                0,
                1,
                2,
                7,
                50,
                80,
                300,
                records.len(),
                records.len() + 1,
                usize::MAX,
            ] {
                options.limit = limit;
                assert_eq!(
                    index.search(&options),
                    crate::search::test_reference::search_records(&records, &options),
                    "compact {options:?}"
                );
            }
        }
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn compact_top_k_retains_stable_ties_at_the_limit_boundary() {
        let records = (0..128)
            .map(|i| {
                let mut record = FileRecord::new("/tmp/report".into(), FileKind::File, 1);
                record.id = 1000 - i;
                record.mtime = Some(42);
                record
            })
            .collect::<Vec<_>>();
        let index = from_records(&records);
        let options = SearchOptions {
            query: "report".into(),
            limit: 63,
            ..Default::default()
        };
        assert_eq!(
            index
                .search(&options)
                .iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>(),
            records
                .iter()
                .take(63)
                .map(|record| record.id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn packed_database_fields_preserve_i64_null_empty_and_non_suffix_values() {
        use crate::sqlite::Connection;
        let temp = unique_temp_dir("macevery-packed-fields");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        let extremes = [i64::MIN, i64::MAX, -1, 0, 1, i64::MAX - 1];
        db.begin().unwrap();
        for i in 0..32usize {
            let mut record = FileRecord::new(
                format!("/tmp/报告-{i}/ΟΣ.txt"),
                match i % 5 {
                    0 => FileKind::File,
                    1 => FileKind::Directory,
                    2 => FileKind::Symlink,
                    3 => FileKind::App,
                    _ => FileKind::Other,
                },
                extremes[i % extremes.len()],
            );
            if i % 3 == 0 {
                record.basename = format!("logical-📷-{i}");
                record.basename_lower = format!("独立-Σ-{i}");
            } else if i % 3 == 1 {
                record.basename.clear();
                record.basename_lower.clear();
            }
            record.ext_lower = match i % 3 {
                0 => None,
                1 => Some(String::new()),
                _ => Some("PDF".into()),
            };
            let values = std::array::from_fn::<_, 5, _>(|field| {
                (i & (1 << field) != 0).then_some(extremes[(i + field) % extremes.len()])
            });
            [
                record.size,
                record.mtime,
                record.ctime,
                record.dev,
                record.inode,
            ] = values;
            db.upsert_file(&record).unwrap();
        }
        for (i, value) in extremes.into_iter().enumerate() {
            let mut record = FileRecord::new(format!("/tmp/extreme-{i}"), FileKind::File, value);
            record.size = Some(value);
            record.mtime = Some(value);
            record.ctime = Some(value);
            record.dev = Some(value);
            record.inode = Some(value);
            db.upsert_file(&record).unwrap();
        }
        db.commit().unwrap();
        let conn = Connection::open(db.path()).unwrap();
        for i in 0..32usize {
            let mut update = conn
                .prepare("UPDATE files SET id=?1,path_lower=?2 WHERE path=?3")
                .unwrap();
            let id = match i {
                0 => i64::MIN,
                1 => i64::MAX,
                _ => -(i as i64) - 1000,
            };
            let path = format!("/tmp/报告-{i}/ΟΣ.txt");
            let custom = format!("/custom-lower-{i}/ος.txt");
            update.bind_i64(1, Some(id)).unwrap();
            update
                .bind_text(
                    2,
                    match i % 4 {
                        0 => None,
                        1 => Some(""),
                        2 => Some(&custom),
                        _ => Some(&path),
                    },
                )
                .unwrap();
            update.bind_text(3, Some(&path)).unwrap();
            update.step().unwrap();
        }
        let expected = db.all_records().unwrap();
        let actual = CompactIndex::load(&db).unwrap();
        assert_eq!(
            actual
                .iter_records()
                .map(|record| actual.to_file_record(record))
                .collect::<Vec<_>>(),
            expected
        );
        for query in ["*", "报告", "~ος", "*logical*", "Σ", "custom-lower"] {
            for limit in [0, 1, 3, 32, usize::MAX] {
                let options = SearchOptions {
                    query: query.into(),
                    limit,
                    ..Default::default()
                };
                assert_eq!(
                    actual.search(&options),
                    crate::search::test_reference::search_records(&expected, &options),
                    "{options:?}"
                );
            }
        }
        drop(conn);
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn unicode_context_lowercase_matches_database_memory_backend() {
        use crate::sqlite::Connection;
        let temp = unique_temp_dir("macevery-packed-sigma");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.upsert_file(&FileRecord::new("/tmp/ΟΣ".into(), FileKind::File, 1))
            .unwrap();
        let conn = Connection::open(db.path()).unwrap();
        for sql in [
            "UPDATE files SET path_lower = NULL",
            "UPDATE files SET path_lower = ''",
            "UPDATE files SET path_lower = '/tmp/ος'",
        ] {
            conn.execute_batch(sql).unwrap();
            let expected = db.all_records().unwrap();
            let index = CompactIndex::load(&db).unwrap();
            let options = SearchOptions {
                query: "ΟΣ".into(),
                path_only: true,
                ..Default::default()
            };
            let actual = index.search(&options);
            assert_eq!(actual.len(), 1);
            assert_eq!(
                actual,
                crate::search::test_reference::search_records(&expected, &options)
            );
            assert_eq!(actual[0].record.path_lower, "/tmp/ος");
        }
        drop(conn);
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn packed_blocks_dictionary_and_non_suffix_slices_match_source_records() {
        let mut builder = IndexBuilder::new(64, 3);
        let mut records = Vec::new();
        for i in 0..12 {
            let mut record = FileRecord::new(
                if i == 0 {
                    String::new()
                } else if i == 5 {
                    format!("/big/{}/ΟΣ", "目录".repeat(80))
                } else {
                    format!("/part-{i}/file.txt")
                },
                FileKind::File,
                i,
            );
            record.id = i + 1;
            record.ext_lower = match i % 3 {
                0 => None,
                1 => Some("".into()),
                _ => Some("txt".into()),
            };
            if i % 2 == 0 {
                record.basename = format!("non-suffix-{i}");
                record.basename_lower = record.basename.to_lowercase();
            }
            push_record(&mut builder, &record);
            records.push(record);
        }
        let index = builder.index;
        assert_eq!(index.records.len(), 4);
        assert!(index.arenas.len() > 2);
        assert!(index.arenas.iter().any(|arena| arena.len() > 64));
        assert_eq!(index.extensions.len(), 2);
        assert_eq!(
            index
                .iter_records()
                .map(|record| index.to_file_record(record))
                .collect::<Vec<_>>(),
            records
        );
        for limit in [0, 1, 4, 6, 12, 100] {
            let options = SearchOptions {
                query: "*".into(),
                limit,
                ..Default::default()
            };
            assert_eq!(
                index.search(&options),
                crate::search::test_reference::search_records(&records, &options)
            );
        }
        assert!(index.memory_usage_bytes() >= (records.len() * size_of::<CompactRecord>()) as u64);
        assert!(index.memory_usage_bytes() < 64 * 1024);
        println!(
            "packed_record_bytes={} fixture_capacity_bytes={}",
            size_of::<CompactRecord>(),
            index.memory_usage_bytes()
        );
    }

    #[test]
    fn suffix_reuse_and_empty_lowercase_fallback_are_lossless() {
        let mut record = FileRecord::new("/lower/file.txt".into(), FileKind::File, 1);
        record.path_lower.clear();
        let index = from_records(std::slice::from_ref(&record));
        let packed = index.record_at(0);
        assert_eq!(packed.path.start, packed.path_lower.start);
        assert_eq!(packed.basename.start, packed.basename_lower.start);
        assert_eq!(
            packed.basename.start as usize,
            packed.path.start as usize + record.path.len() - record.basename.len()
        );
        record.path_lower = record.path.to_lowercase();
        assert_eq!(index.to_file_record(packed), record);
    }

    #[test]
    fn checked_slices_reject_large_offsets_without_truncation() {
        assert!(Slice::checked(u32::MAX as usize, 0).is_ok());
        assert!(Slice::checked(u32::MAX as usize, 1).is_err());
        assert!(Slice::checked(usize::MAX, 1).is_err());
        if usize::BITS > 32 {
            assert!(Slice::checked(0, u32::MAX as usize + 1).is_err());
        }
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
