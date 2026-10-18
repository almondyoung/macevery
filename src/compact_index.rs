use crate::db::{Database, DbRecordRef, FileChange};
use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, SearchOptions, SearchResult};
use crate::search::{effective_limit, PreparedQuery, TopHits};
use std::borrow::Cow;
use std::collections::HashMap;
use std::mem::size_of;

const ARENA_BYTES: usize = 64 * 1024 * 1024;
const RECORDS_PER_BLOCK: usize = 64 * 1024;
const INITIAL_ARENA_BYTES: usize = 64 * 1024;
const NO_EXTENSION: u32 = u32::MAX;
const TOMBSTONE: u8 = 1 << 7;
const MAX_DEAD_STRING_BYTES: usize = 32 * 1024 * 1024;
const MAX_EXTENSION_GROWTH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    NeedsFullReload(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FragmentationStats {
    pub dead_records: usize,
    pub dead_string_bytes: usize,
    pub extension_growth_bytes: usize,
}

/// Searchable fields remain borrowed from bounded arenas; only final hits are decoded.
pub struct CompactIndex {
    arenas: Vec<String>,
    records: Vec<Vec<CompactRecord>>,
    extensions: Vec<Box<str>>,
    len: usize,
    physical_len: usize,
    records_per_block: usize,
    arena_bytes: usize,
    ids_sorted: bool,
    fragmentation: FragmentationStats,
}

impl Default for CompactIndex {
    fn default() -> Self {
        Self {
            arenas: Vec::new(),
            records: Vec::new(),
            extensions: Vec::new(),
            len: 0,
            physical_len: 0,
            records_per_block: RECORDS_PER_BLOCK,
            arena_bytes: ARENA_BYTES,
            ids_sorted: true,
            fragmentation: FragmentationStats {
                dead_records: 0,
                dead_string_bytes: 0,
                extension_growth_bytes: 0,
            },
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
    fn is_live(&self) -> bool {
        self.present & TOMBSTONE == 0
    }
    fn string_bytes(&self) -> usize {
        [
            self.path,
            self.path_lower,
            self.basename,
            self.basename_lower,
        ]
        .iter()
        .map(|slice| slice.start as usize + slice.len as usize)
        .max()
        .unwrap()
            - self.path.start as usize
    }
    fn update_metadata(&mut self, record: &DbRecordRef<'_>, ext: u32) {
        self.id = record.id;
        self.indexed_at = record.indexed_at;
        self.kind = record.kind.clone();
        self.ext = ext;
        self.present = 0;
        for (i, value) in [
            record.size,
            record.mtime,
            record.ctime,
            record.dev,
            record.inode,
        ]
        .into_iter()
        .enumerate()
        {
            self.values[i] = value.unwrap_or(0);
            if value.is_some() {
                self.present |= 1 << i;
            }
        }
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

struct RowText<'a> {
    lowered: Cow<'a, str>,
    parents: [Option<usize>; 4],
    required: usize,
}

impl<'a> RowText<'a> {
    fn new(record: &DbRecordRef<'a>) -> Result<Self> {
        // Match all_records for NULL/empty and context-sensitive Unicode lowercase.
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
        Ok(Self {
            lowered,
            parents,
            required,
        })
    }
}

struct IndexBuilder {
    index: CompactIndex,
    extension_ids: HashMap<Box<str>, u32>,
}

impl IndexBuilder {
    fn new(arena_bytes: usize, records_per_block: usize) -> Self {
        assert!(arena_bytes > 0 && records_per_block > 0);
        Self {
            index: CompactIndex {
                records_per_block,
                arena_bytes,
                ..CompactIndex::default()
            },
            extension_ids: HashMap::new(),
        }
    }
    fn push(&mut self, record: DbRecordRef<'_>) -> Result<()> {
        let text = RowText::new(&record)?;
        let ext = self
            .index
            .intern_extension(record.ext_lower, Some(&mut self.extension_ids))?;
        let packed = self.index.pack_record(&record, &text, ext)?;
        self.index.append(packed)
    }
}

struct DeltaPlan<'a> {
    slot: Option<usize>,
    // None means deletion or unchanged strings; metadata still changes in place.
    text: Option<RowText<'a>>,
}

#[derive(Clone, Copy)]
struct DeltaLimits {
    dead_strings: usize,
    dead_records: usize,
    extension_growth: usize,
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
    pub fn fragmentation_stats(&self) -> FragmentationStats {
        self.fragmentation
    }

    /// NeedsFullReload is returned before any mutation. On Err, callers must
    /// discard this index: a previous change in this batch may already be applied.
    pub fn apply_changes(&mut self, changes: &[FileChange]) -> Result<ApplyOutcome> {
        self.apply_changes_with_limits(
            changes,
            DeltaLimits {
                dead_strings: MAX_DEAD_STRING_BYTES,
                dead_records: (self.physical_len / 20).clamp(1024, 65_536),
                extension_growth: MAX_EXTENSION_GROWTH_BYTES,
            },
        )
    }

    fn apply_changes_with_limits(
        &mut self,
        changes: &[FileChange],
        limits: DeltaLimits,
    ) -> Result<ApplyOutcome> {
        if !self.ids_sorted {
            return Ok(ApplyOutcome::NeedsFullReload("non-canonical record order"));
        }
        let mut plan = Vec::with_capacity(changes.len());
        let mut previous = None;
        let last_id = (self.physical_len > 0).then(|| self.record_at(self.physical_len - 1).id);
        let mut fragmentation = self.fragmentation;
        let mut next_len = self.len;
        // Only extensions newly introduced by this batch are tracked here;
        // there is deliberately no permanent full-ID or extension HashMap.
        let mut new_extensions = HashMap::<&str, ()>::new();
        for change in changes {
            if previous.is_some_and(|id| id >= change.id) {
                return Err(MacEveryError::Cli(
                    "packed changes must have unique ascending IDs".into(),
                ));
            }
            previous = Some(change.id);
            if change
                .record
                .as_ref()
                .is_some_and(|record| record.id != change.id)
            {
                return Err(MacEveryError::Cli(
                    "packed change ID differs from its record ID".into(),
                ));
            }
            let slot = self.find_id(change.id);
            let old = slot.map(|slot| self.record_at(slot));
            let text = if let Some(record) = &change.record {
                if old.is_none() && last_id.is_some_and(|last| change.id < last) {
                    return Ok(ApplyOutcome::NeedsFullReload("new non-monotonic record ID"));
                }
                let borrowed = borrow_record(record);
                let text = RowText::new(&borrowed)?;
                let unchanged = old.is_some_and(|old| self.same_strings(old, &borrowed, &text));
                if let Some(old) = old {
                    if !old.is_live() {
                        fragmentation.dead_records -= 1;
                        next_len = next_len.checked_add(1).ok_or_else(index_too_large)?;
                        if unchanged {
                            fragmentation.dead_string_bytes -= old.string_bytes();
                        }
                    } else if !unchanged {
                        fragmentation.dead_string_bytes = fragmentation
                            .dead_string_bytes
                            .checked_add(old.string_bytes())
                            .ok_or_else(index_too_large)?;
                    }
                } else {
                    next_len = next_len.checked_add(1).ok_or_else(index_too_large)?;
                }
                if let Some(ext) = record.ext_lower.as_deref() {
                    if !self
                        .extensions
                        .iter()
                        .any(|existing| existing.as_ref() == ext)
                        && new_extensions.insert(ext, ()).is_none()
                    {
                        fragmentation.extension_growth_bytes = fragmentation
                            .extension_growth_bytes
                            .checked_add(ext.len().saturating_add(size_of::<Box<str>>()))
                            .ok_or_else(index_too_large)?;
                    }
                }
                (!unchanged).then_some(text)
            } else {
                if let Some(old) = old.filter(|old| old.is_live()) {
                    fragmentation.dead_records += 1;
                    fragmentation.dead_string_bytes = fragmentation
                        .dead_string_bytes
                        .checked_add(old.string_bytes())
                        .ok_or_else(index_too_large)?;
                    next_len -= 1;
                }
                None
            };
            plan.push(DeltaPlan { slot, text });
        }
        if fragmentation.dead_records > limits.dead_records {
            return Ok(ApplyOutcome::NeedsFullReload(
                "packed tombstone budget exceeded",
            ));
        }
        if fragmentation.dead_string_bytes > limits.dead_strings {
            return Ok(ApplyOutcome::NeedsFullReload(
                "packed dead string budget exceeded",
            ));
        }
        if fragmentation.extension_growth_bytes > limits.extension_growth {
            return Ok(ApplyOutcome::NeedsFullReload(
                "packed extension growth budget exceeded",
            ));
        }
        if self
            .extensions
            .len()
            .checked_add(new_extensions.len())
            .is_none_or(|len| len >= NO_EXTENSION as usize)
        {
            return Ok(ApplyOutcome::NeedsFullReload(
                "packed extension dictionary capacity exceeded",
            ));
        }
        // All reasons for a clean fallback are checked above. Allocation/encoding
        // errors below use Err so the service cannot publish a partially applied stamp.
        for (change, plan) in changes.iter().zip(plan) {
            if let Some(record) = &change.record {
                let borrowed = borrow_record(record);
                let ext = self.intern_extension(borrowed.ext_lower, None)?;
                if let Some(text) = plan.text {
                    let packed = self.pack_record(&borrowed, &text, ext)?;
                    if let Some(slot) = plan.slot {
                        *self.record_at_mut(slot) = packed;
                    } else {
                        self.append(packed)?;
                    }
                } else {
                    self.record_at_mut(plan.slot.unwrap())
                        .update_metadata(&borrowed, ext);
                }
            } else if let Some(slot) = plan.slot {
                self.record_at_mut(slot).present |= TOMBSTONE;
            }
        }
        self.len = next_len;
        self.fragmentation = fragmentation;
        Ok(ApplyOutcome::Applied)
    }

    fn same_strings(
        &self,
        old: &CompactRecord,
        record: &DbRecordRef<'_>,
        text: &RowText<'_>,
    ) -> bool {
        self.get(old, old.path) == record.path
            && self.get(old, old.path_lower) == text.lowered
            && self.get(old, old.basename) == record.basename
            && self.get(old, old.basename_lower) == record.basename_lower
    }

    fn find_id(&self, id: i64) -> Option<usize> {
        let (mut low, mut high) = (0, self.physical_len);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.record_at(middle).id.cmp(&id) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    fn intern_extension(
        &mut self,
        ext: Option<&str>,
        map: Option<&mut HashMap<Box<str>, u32>>,
    ) -> Result<u32> {
        let Some(value) = ext else {
            return Ok(NO_EXTENSION);
        };
        let existing = match map.as_ref() {
            Some(map) => map.get(value).copied(),
            None => self
                .extensions
                .iter()
                .position(|ext| ext.as_ref() == value)
                .map(|id| id as u32),
        };
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = u32::try_from(self.extensions.len()).map_err(|_| index_too_large())?;
        if id == NO_EXTENSION {
            return Err(index_too_large());
        }
        self.extensions.try_reserve(1).map_err(|err| {
            MacEveryError::Cli(format!("cannot allocate packed extension: {err}"))
        })?;
        self.extensions.push(value.into());
        if let Some(map) = map {
            map.insert(value.into(), id);
        }
        Ok(id)
    }

    fn pack_record(
        &mut self,
        record: &DbRecordRef<'_>,
        text: &RowText<'_>,
        ext: u32,
    ) -> Result<CompactRecord> {
        let arena = self.reserve_row(text.required)?;
        let fields = [
            record.path,
            text.lowered.as_ref(),
            record.basename,
            record.basename_lower,
        ];
        let mut slices = [Slice::default(); 4];
        for (i, value) in fields.iter().enumerate() {
            slices[i] = if let Some(parent) = text.parents[i] {
                Slice::checked(
                    slices[parent].start as usize + fields[parent].len() - value.len(),
                    value.len(),
                )?
            } else {
                let slice = Slice::checked(self.arenas[arena].len(), value.len())?;
                self.arenas[arena].push_str(value);
                slice
            };
        }
        let mut packed = CompactRecord {
            id: record.id,
            indexed_at: record.indexed_at,
            values: [0; 5],
            path: slices[0],
            path_lower: slices[1],
            basename: slices[2],
            basename_lower: slices[3],
            arena: u32::try_from(arena).map_err(|_| index_too_large())?,
            ext,
            kind: record.kind.clone(),
            present: 0,
        };
        packed.update_metadata(record, ext);
        Ok(packed)
    }

    fn append(&mut self, packed: CompactRecord) -> Result<()> {
        if self.physical_len > 0 && self.record_at(self.physical_len - 1).id >= packed.id {
            self.ids_sorted = false;
        }
        if self
            .records
            .last()
            .is_none_or(|block| block.len() == self.records_per_block)
        {
            let mut block = Vec::new();
            block
                .try_reserve_exact(self.records_per_block)
                .map_err(|err| {
                    MacEveryError::Cli(format!("cannot allocate packed record block: {err}"))
                })?;
            self.records.push(block);
        }
        self.records.last_mut().unwrap().push(packed);
        self.len = self.len.checked_add(1).ok_or_else(index_too_large)?;
        self.physical_len = self
            .physical_len
            .checked_add(1)
            .ok_or_else(index_too_large)?;
        Ok(())
    }

    fn reserve_row(&mut self, bytes: usize) -> Result<usize> {
        let needs_block = self
            .arenas
            .last()
            .is_none_or(|arena| bytes > self.arena_bytes.saturating_sub(arena.len()));
        if needs_block {
            u32::try_from(self.arenas.len()).map_err(|_| index_too_large())?;
            self.arenas.push(String::new());
        }
        let id = self.arenas.len() - 1;
        let arena = &mut self.arenas[id];
        let needed = arena.len().checked_add(bytes).ok_or_else(index_too_large)?;
        Slice::checked(0, needed)?;
        if arena.capacity() < needed {
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

    /// Allocated container/payload capacity; excludes allocator and SQLite overhead, not RSS.
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
        let mut hits = TopHits::new(effective_limit(options.limit));
        for (index, record) in self.iter_records().enumerate() {
            if !record.is_live() {
                continue;
            }
            let mtime = record.optional(1);
            let ext = if query.needs_extension() {
                self.ext(record)
            } else {
                None
            };
            if !query.matches_metadata(&record.kind, mtime, ext) {
                continue;
            }
            // One arena lookup, two lowercase slices at most. Original basename
            // is never needed for ranking; original path is lazy for losing hits.
            let arena = &self.arenas[record.arena as usize];
            let get = |slice: Slice| {
                &arena[slice.start as usize..slice.start as usize + slice.len as usize]
            };
            let path = if query.needs_text() {
                get(record.path_lower)
            } else {
                ""
            };
            let basename = if query.needs_basename() {
                get(record.basename_lower)
            } else {
                ""
            };
            if let Some(score) = query.rank_lowered(path, basename, record.path.len as usize, mtime)
            {
                hits.consider(index, score, mtime, record.path.len as usize, || {
                    get(record.path)
                });
            }
        }
        hits.finish()
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
    fn record_at_mut(&mut self, index: usize) -> &mut CompactRecord {
        &mut self.records[index / self.records_per_block][index % self.records_per_block]
    }
    fn get(&self, record: &CompactRecord, slice: Slice) -> &str {
        let start = slice.start as usize;
        &self.arenas[record.arena as usize][start..start + slice.len as usize]
    }
    fn ext(&self, record: &CompactRecord) -> Option<&str> {
        (record.ext != NO_EXTENSION).then(|| self.extensions[record.ext as usize].as_ref())
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

fn borrow_record(record: &FileRecord) -> DbRecordRef<'_> {
    DbRecordRef {
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

    fn live_records(index: &CompactIndex) -> Vec<FileRecord> {
        index
            .iter_records()
            .filter(|record| record.is_live())
            .map(|record| index.to_file_record(record))
            .collect()
    }

    fn assert_delta_matches(index: &CompactIndex, records: &[FileRecord]) {
        assert_eq!(live_records(index), records);
        assert_eq!(index.len(), records.len());
        for mut options in crate::search::test_reference::options() {
            for limit in [0, 1, 3, usize::MAX] {
                options.limit = limit;
                assert_eq!(
                    index.search(&options),
                    crate::search::test_reference::search_records(records, &options),
                    "{options:?}"
                );
            }
        }
    }

    #[test]
    fn packed_delta_updates_metadata_without_growing_strings_and_reuses_tombstones() {
        let mut records = (1..=7)
            .map(|id| {
                let mut record =
                    FileRecord::new(format!("/tmp/{id}/report.txt"), FileKind::File, id);
                record.id = id * 10;
                record.mtime = Some(10);
                record
            })
            .collect::<Vec<_>>();
        let mut builder = IndexBuilder::new(96, 3);
        for record in &records {
            push_record(&mut builder, record);
        }
        let mut index = builder.index;
        let memory = index.memory_usage_bytes();
        let arena_lengths = index.arenas.iter().map(String::len).collect::<Vec<_>>();
        records[2].kind = FileKind::Other;
        records[2].size = Some(i64::MIN);
        records[2].mtime = None;
        records[2].ctime = Some(i64::MAX);
        records[2].dev = Some(-1);
        records[2].inode = Some(0);
        records[2].indexed_at = i64::MIN;
        assert_eq!(
            index
                .apply_changes(&[FileChange {
                    id: 30,
                    record: Some(records[2].clone())
                }])
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(index.memory_usage_bytes(), memory);
        assert_eq!(
            index.arenas.iter().map(String::len).collect::<Vec<_>>(),
            arena_lengths
        );
        assert_delta_matches(&index, &records);

        let removed = records.remove(1);
        assert_eq!(
            index
                .apply_changes(&[FileChange {
                    id: 20,
                    record: None
                }])
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(index.fragmentation_stats().dead_records, 1);
        assert_delta_matches(&index, &records);
        // Reactivation of the same ID and strings reclaims its dead byte budget.
        assert_eq!(
            index
                .apply_changes(&[FileChange {
                    id: 20,
                    record: Some(removed.clone())
                }])
                .unwrap(),
            ApplyOutcome::Applied
        );
        records.insert(1, removed);
        assert_eq!(index.fragmentation_stats().dead_records, 0);
        assert_eq!(index.fragmentation_stats().dead_string_bytes, 0);
        assert_eq!(index.memory_usage_bytes(), memory);

        let mut changed = FileRecord::new("/new/ΟΣ/📷 report.PDF".into(), FileKind::App, i64::MAX);
        changed.id = 40;
        changed.basename = "logical-other-name".into();
        changed.basename_lower = "独立-ος-name".into();
        changed.ext_lower = Some("".into());
        changed.ctime = Some(i64::MIN);
        changed.inode = Some(i64::MAX);
        let mut appended = FileRecord::new("/new/报告/report.txt".into(), FileKind::Directory, 999);
        appended.id = 80;
        assert_eq!(
            index
                .apply_changes(&[
                    FileChange {
                        id: 10,
                        record: None
                    },
                    FileChange {
                        id: 40,
                        record: Some(changed.clone())
                    },
                    FileChange {
                        id: 80,
                        record: Some(appended.clone())
                    },
                ])
                .unwrap(),
            ApplyOutcome::Applied
        );
        records.remove(0);
        records[2] = changed;
        records.push(appended);
        assert_eq!(index.physical_len, 8);
        assert_eq!(size_of::<CompactRecord>(), 104);
        assert_delta_matches(&index, &records);
        // Deleting an unknown ID is an idempotent no-op, including low IDs.
        assert_eq!(
            index
                .apply_changes(&[FileChange {
                    id: -123,
                    record: None
                }])
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_delta_matches(&index, &records);
    }

    #[test]
    fn clean_fallback_preflight_never_changes_records_or_capacity() {
        let mut record = FileRecord::new("/tmp/report.txt".into(), FileKind::File, 1);
        record.id = 10;
        let mut last = record.clone();
        last.id = 30;
        last.path = "/tmp/last.txt".into();
        last.path_lower = last.path.to_lowercase();
        let mut index = from_records(&[record.clone(), last]);
        let records = live_records(&index);
        let stats = index.fragmentation_stats();
        let bytes = index.memory_usage_bytes();
        let no_dead = DeltaLimits {
            dead_strings: 0,
            dead_records: 0,
            extension_growth: 0,
        };
        let mut lower = record.clone();
        lower.id = 20;
        let mut updated = record.clone();
        updated.size = Some(99);
        assert_eq!(
            index
                .apply_changes(&[
                    FileChange {
                        id: 10,
                        record: Some(updated.clone())
                    },
                    FileChange {
                        id: 20,
                        record: Some(lower)
                    },
                ])
                .unwrap(),
            ApplyOutcome::NeedsFullReload("new non-monotonic record ID")
        );
        let delete = [FileChange {
            id: 10,
            record: None,
        }];
        assert_eq!(
            index.apply_changes_with_limits(&delete, no_dead).unwrap(),
            ApplyOutcome::NeedsFullReload("packed tombstone budget exceeded")
        );
        updated.path = "/new/report.txt".into();
        updated.path_lower = updated.path.to_lowercase();
        assert_eq!(
            index
                .apply_changes_with_limits(
                    &[FileChange {
                        id: 10,
                        record: Some(updated.clone())
                    }],
                    no_dead
                )
                .unwrap(),
            ApplyOutcome::NeedsFullReload("packed dead string budget exceeded")
        );
        updated = record.clone();
        updated.ext_lower = Some("new-extension".into());
        assert_eq!(
            index
                .apply_changes_with_limits(
                    &[FileChange {
                        id: 10,
                        record: Some(updated)
                    }],
                    no_dead
                )
                .unwrap(),
            ApplyOutcome::NeedsFullReload("packed extension growth budget exceeded")
        );
        assert!(index
            .apply_changes(&[FileChange {
                id: 11,
                record: Some(record.clone())
            }])
            .is_err());
        assert!(index
            .apply_changes(&[
                FileChange {
                    id: 10,
                    record: None
                },
                FileChange {
                    id: 10,
                    record: None
                }
            ])
            .is_err());
        assert_eq!(live_records(&index), records);
        assert_eq!(index.fragmentation_stats(), stats);
        assert_eq!(index.memory_usage_bytes(), bytes);
    }

    #[test]
    fn repeated_string_changes_have_a_hard_rebuild_budget() {
        let mut record = FileRecord::new("/tmp/ΟΣ/report-0.txt".into(), FileKind::File, 1);
        record.id = 1;
        let mut builder = IndexBuilder::new(64, 2);
        push_record(&mut builder, &record);
        let mut index = builder.index;
        let limits = DeltaLimits {
            dead_strings: 128,
            dead_records: 1,
            extension_growth: 1024,
        };
        let mut applied = 0;
        for round in 1..100 {
            let mut next =
                FileRecord::new(format!("/tmp/ΟΣ/report-{round}.txt"), FileKind::File, round);
            next.id = 1;
            let before = live_records(&index);
            let bytes = index.memory_usage_bytes();
            match index
                .apply_changes_with_limits(
                    &[FileChange {
                        id: 1,
                        record: Some(next.clone()),
                    }],
                    limits,
                )
                .unwrap()
            {
                ApplyOutcome::Applied => {
                    applied += 1;
                    assert_eq!(live_records(&index), vec![next]);
                    assert!(index.fragmentation_stats().dead_string_bytes <= limits.dead_strings);
                }
                ApplyOutcome::NeedsFullReload(reason) => {
                    assert_eq!(reason, "packed dead string budget exceeded");
                    assert_eq!(live_records(&index), before);
                    assert_eq!(index.memory_usage_bytes(), bytes);
                    assert!(applied > 0 && applied < 10);
                    return;
                }
            }
        }
        panic!("fragmented strings must eventually require a rebuild");
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
