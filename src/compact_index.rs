use crate::db::{Database, DbRecordRef};
use crate::error::Result;
use crate::model::{FileKind, FileRecord, SearchOptions, SearchResult};
use crate::search::{effective_limit, top_hits, PreparedQuery, RecordView};

#[derive(Default)]
pub struct CompactIndex {
    strings: String,
    records: Vec<CompactRecord>,
}

#[derive(Clone)]
struct CompactRecord {
    id: i64,
    path: Slice,
    path_lower: Slice,
    basename: Slice,
    basename_lower: Slice,
    ext_lower: Option<Slice>,
    kind: FileKind,
    size: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
    dev: Option<i64>,
    inode: Option<i64>,
    indexed_at: i64,
}

#[derive(Clone, Copy)]
struct Slice {
    start: usize,
    len: usize,
}

impl CompactIndex {
    pub fn load(db: &Database) -> Result<Self> {
        let mut index = Self::default();
        db.for_each_record_ref(|record| {
            index.push_ref(record);
            Ok(())
        })?;
        index.strings.shrink_to_fit();
        index.records.shrink_to_fit();
        Ok(index)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn search(&self, options: &SearchOptions) -> Vec<SearchResult> {
        let options = options.normalized();
        let query = PreparedQuery::new(&options);
        let hits = top_hits(
            self.records
                .iter()
                .enumerate()
                .map(|(index, record)| (index, self.view(record))),
            &query,
            effective_limit(options.limit),
        );

        hits.into_iter()
            .map(|hit| SearchResult {
                record: self.to_file_record(&self.records[hit.index]),
                score: hit.score,
            })
            .collect()
    }

    fn push_ref(&mut self, record: DbRecordRef<'_>) {
        let path = self.push_string(record.path);
        let path_lower = self.push_lowercase_string(record.path);
        let basename = self
            .suffix_slice(path, record.basename)
            .unwrap_or_else(|| self.push_string(record.basename));
        let basename_lower = self
            .suffix_slice(path_lower, record.basename_lower)
            .unwrap_or_else(|| self.push_string(record.basename_lower));
        let ext_lower = record.ext_lower.map(|value| self.push_string(value));

        self.records.push(CompactRecord {
            id: record.id,
            path,
            path_lower,
            basename,
            basename_lower,
            ext_lower,
            kind: record.kind,
            size: record.size,
            mtime: record.mtime,
            ctime: record.ctime,
            dev: record.dev,
            inode: record.inode,
            indexed_at: record.indexed_at,
        });
    }

    fn push_string(&mut self, value: &str) -> Slice {
        let start = self.strings.len();
        self.strings.push_str(value);
        let len = self.strings.len() - start;
        Slice { start, len }
    }

    fn push_lowercase_string(&mut self, value: &str) -> Slice {
        let start = self.strings.len();
        for ch in value.chars() {
            for lower in ch.to_lowercase() {
                self.strings.push(lower);
            }
        }
        let len = self.strings.len() - start;
        Slice { start, len }
    }

    fn suffix_slice(&self, parent: Slice, suffix: &str) -> Option<Slice> {
        let parent_value = self.get(parent);
        if parent_value.ends_with(suffix) {
            Some(Slice {
                start: parent.start + parent.len - suffix.len(),
                len: suffix.len(),
            })
        } else {
            None
        }
    }

    fn view<'a>(&'a self, record: &'a CompactRecord) -> RecordView<'a> {
        RecordView {
            path: self.get(record.path),
            path_lower: self.get(record.path_lower),
            basename: self.get(record.basename),
            basename_lower: self.get(record.basename_lower),
            ext_lower: record.ext_lower.map(|slice| self.get(slice)),
            kind: &record.kind,
            mtime: record.mtime,
        }
    }

    fn to_file_record(&self, record: &CompactRecord) -> FileRecord {
        FileRecord {
            id: record.id,
            path: self.get(record.path).to_string(),
            path_lower: self.get(record.path_lower).to_string(),
            basename: self.get(record.basename).to_string(),
            basename_lower: self.get(record.basename_lower).to_string(),
            ext_lower: record.ext_lower.map(|slice| self.get(slice).to_string()),
            kind: record.kind.clone(),
            size: record.size,
            mtime: record.mtime,
            ctime: record.ctime,
            dev: record.dev,
            inode: record.inode,
            indexed_at: record.indexed_at,
        }
    }

    fn get(&self, slice: Slice) -> &str {
        let start = slice.start;
        let end = start + slice.len;
        &self.strings[start..end]
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

    fn from_records(records: &[FileRecord]) -> CompactIndex {
        let mut index = CompactIndex::default();
        for record in records {
            index.push_ref(DbRecordRef {
                id: record.id,
                path: &record.path,
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
            });
        }
        index
    }

    #[test]
    fn compact_top_k_matches_independent_full_sort_reference() {
        let records = crate::search::test_reference::records();
        let index = from_records(&records);
        // Compare the actual compact representation, including its existing Unicode
        // lowercase behavior, rather than changing normalization in this optimization.
        let materialized = index
            .records
            .iter()
            .map(|record| index.to_file_record(record))
            .collect::<Vec<_>>();
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
                    crate::search::test_reference::search_records(&materialized, &options),
                    "compact {options:?}"
                );
            }
        }
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

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
