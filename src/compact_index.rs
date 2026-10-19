use crate::db::{Database, DbRecordRef, FileChange};
use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, SearchOptions, SearchResult};
use crate::search::{effective_limit, PathView, PreparedQuery, TopHits};
use std::borrow::Cow;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::mem::size_of;

const ARENA_BYTES: usize = 64 * 1024 * 1024;
const RECORDS_PER_BLOCK: usize = 64 * 1024;
const INITIAL_ARENA_BYTES: usize = 64 * 1024;
const NO_EXTENSION: u32 = u32::MAX;
const NO_PREFIX: u32 = u32::MAX;
const PREFIX_POOL_BYTES: usize = 64 * 1024 * 1024;
const MIN_PREFIX_BYTES: usize = 16;
const TOMBSTONE: u8 = 1 << 7;
const MAX_DEAD_STRING_BYTES: usize = 32 * 1024 * 1024;
const MAX_EXTENSION_GROWTH_BYTES: usize = 4 * 1024 * 1024;
const METADATA_FIELDS: usize = 6;
const ESCAPED_VALUE: u32 = 1 << 31;
const INLINE_SPAN: i128 = (ESCAPED_VALUE - 1) as i128;

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexMemoryBreakdown {
    pub record_capacity_bytes: usize,
    pub metadata_capacity_bytes: usize,
    pub string_capacity_bytes: usize,
    pub string_used_bytes: usize,
    pub extension_capacity_bytes: usize,
    pub container_bytes: usize,
    pub prefix_string_bytes: usize,
    pub prefix_entry_capacity_bytes: usize,
    pub prefix_lookup_capacity_bytes: usize,
    pub prefix_entries: usize,
    pub narrow_metadata_blocks: usize,
    pub wide_metadata_blocks: usize,
    pub live_metadata_escapes: usize,
}

/// Searchable fields remain borrowed from bounded arenas; only final hits are decoded.
pub struct CompactIndex {
    arenas: Vec<String>,
    records: Vec<Vec<CompactRecord>>,
    metadata: Vec<MetadataBlock>,
    extensions: Vec<Box<str>>,
    prefixes: PrefixPool,
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
            metadata: Vec::new(),
            extensions: Vec::new(),
            prefixes: PrefixPool::new(PREFIX_POOL_BYTES),
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
    path: Slice,
    path_lower: Slice,
    basename: Slice,
    basename_lower: Slice,
    arena: u32,
    ext: u32,
    prefix: u32,
    kind: FileKind,
    present: u8,
}

impl CompactRecord {
    fn is_live(&self) -> bool {
        self.present & TOMBSTONE == 0
    }
    fn update_metadata(&mut self, record: &DbRecordRef<'_>, ext: u32) {
        self.id = record.id;
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
            if value.is_some() {
                self.present |= 1 << i;
            }
        }
    }
}

struct PrefixPool {
    values: Vec<Box<str>>,
    slots: Vec<u32>,
    hasher: RandomState,
    string_bytes: usize,
    limit: usize,
}

impl PrefixPool {
    fn new(limit: usize) -> Self {
        Self {
            values: Vec::new(),
            slots: Vec::new(),
            hasher: RandomState::new(),
            string_bytes: 0,
            limit,
        }
    }
    fn capacity_bytes(&self) -> usize {
        self.string_bytes
            + self.values.capacity() * size_of::<Box<str>>()
            + self.slots.capacity() * size_of::<u32>()
    }
    fn get(&self, id: u32) -> &str {
        if id == NO_PREFIX {
            ""
        } else {
            &self.values[id as usize]
        }
    }
    fn lookup(&self, value: &str, hash: u64) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mut slot = hash as usize & (self.slots.len() - 1);
        for _ in 0..self.slots.len() {
            let id = self.slots[slot];
            if id == NO_PREFIX {
                return None;
            }
            if self.values[id as usize].as_ref() == value {
                return Some(id);
            }
            slot = (slot + 1) & (self.slots.len() - 1);
        }
        None
    }
    fn insert_slot(slots: &mut [u32], id: u32, hash: u64) {
        let mut slot = hash as usize & (slots.len() - 1);
        while slots[slot] != NO_PREFIX {
            slot = (slot + 1) & (slots.len() - 1);
        }
        slots[slot] = id;
    }
    fn intern(&mut self, value: &str) -> Result<Option<u32>> {
        let hash = self.hasher.hash_one(value);
        if let Some(id) = self.lookup(value, hash) {
            return Ok(Some(id));
        }
        let Some(new_len) = self
            .values
            .len()
            .checked_add(1)
            .filter(|len| *len < NO_PREFIX as usize)
        else {
            return Ok(None);
        };
        let Some(payload) = self.string_bytes.checked_add(value.len()) else {
            return Ok(None);
        };
        let value_capacity = if new_len > self.values.capacity() {
            self.values.capacity().saturating_mul(2).max(16)
        } else {
            self.values.capacity()
        };
        let slots_len = if new_len > self.slots.len() / 2 {
            self.slots.len().saturating_mul(2).max(32)
        } else {
            self.slots.len()
        };
        let fits = |entries: usize, slots: usize| {
            payload
                .saturating_add(entries.saturating_mul(size_of::<Box<str>>()))
                .saturating_add(slots.saturating_mul(size_of::<u32>()))
                .saturating_add(size_of::<Self>())
                <= self.limit
        };
        if !fits(value_capacity, slots_len.max(self.slots.capacity())) {
            return Ok(None);
        }
        let mut grown_values = None;
        if value_capacity > self.values.capacity() {
            let mut values = Vec::new();
            values
                .try_reserve_exact(value_capacity)
                .map_err(prefix_allocation)?;
            grown_values = Some(values);
        }
        let mut grown_slots = None;
        if slots_len > self.slots.len() {
            let mut slots = Vec::new();
            slots
                .try_reserve_exact(slots_len)
                .map_err(prefix_allocation)?;
            slots.resize(slots_len, NO_PREFIX);
            grown_slots = Some(slots);
        }
        let actual_values = grown_values
            .as_ref()
            .map_or(self.values.capacity(), Vec::capacity);
        let actual_slots = grown_slots
            .as_ref()
            .map_or(self.slots.capacity(), Vec::capacity);
        // Check actual allocation capacities before moving any existing state.
        if !fits(actual_values, actual_slots) {
            return Ok(None);
        }
        if let Some(mut values) = grown_values {
            values.append(&mut self.values);
            self.values = values;
        }
        if let Some(mut slots) = grown_slots {
            for (id, value) in self.values.iter().enumerate() {
                Self::insert_slot(&mut slots, id as u32, self.hasher.hash_one(value.as_ref()));
            }
            self.slots = slots;
        }
        let id = self.values.len() as u32;
        self.values.push(value.into());
        Self::insert_slot(&mut self.slots, id, hash);
        self.string_bytes = payload;
        debug_assert!(self.capacity_bytes() + size_of::<Self>() <= self.limit);
        Ok(Some(id))
    }
}

fn prefix_allocation(error: std::collections::TryReserveError) -> MacEveryError {
    MacEveryError::Cli(format!("cannot allocate packed path prefix: {error}"))
}

type MetadataValues = [i64; METADATA_FIELDS];
type MetadataInput = [Option<i64>; METADATA_FIELDS];

enum MetadataBlock {
    Narrow(NarrowMetadata),
    Wide(Vec<MetadataValues>),
}

struct NarrowMetadata {
    bases: MetadataValues,
    initialized: u8,
    rows: Vec<[u32; METADATA_FIELDS]>,
    escapes: Vec<i64>,
    // Vacant escape entries hold the next free index, or -1. Each live
    // slot/field owns its entry, so repeated updates reuse storage in place.
    free_head: Option<u32>,
    free_count: usize,
}

impl MetadataBlock {
    fn new(capacity: usize) -> Result<Self> {
        let mut rows = Vec::new();
        rows.try_reserve_exact(capacity)
            .map_err(metadata_allocation)?;
        let narrow = NarrowMetadata {
            bases: [0; METADATA_FIELDS],
            initialized: 0,
            rows,
            escapes: Vec::new(),
            free_head: None,
            free_count: 0,
        };
        let mut block = Self::Narrow(narrow);
        if !block.narrow_fits(0) {
            block.make_wide()?;
        }
        Ok(block)
    }

    fn value(&self, row: usize, field: usize) -> i64 {
        match self {
            Self::Narrow(values) => values.decode(values.rows[row][field], field),
            Self::Wide(values) => values[row][field],
        }
    }

    fn push(&mut self, input: MetadataInput) -> Result<()> {
        self.write(None, input)
    }

    fn update(&mut self, row: usize, input: MetadataInput) -> Result<()> {
        self.write(Some(row), input)
    }

    fn write(&mut self, row: Option<usize>, input: MetadataInput) -> Result<()> {
        if let Self::Narrow(values) = self {
            let codes = values.prepare(input);
            let old = row
                .map(|row| values.rows[row])
                .unwrap_or([0; METADATA_FIELDS]);
            let new_escapes = codes.iter().filter(|code| code.is_none()).count();
            let old_escapes = old
                .iter()
                .filter(|code| **code & ESCAPED_VALUE != 0)
                .count();
            let additional = new_escapes.saturating_sub(old_escapes + values.free_count);
            if self.narrow_fits(additional) {
                let Self::Narrow(values) = self else {
                    unreachable!()
                };
                let needed = values.escapes.len().saturating_add(additional);
                if needed > values.escapes.capacity() {
                    let max_escapes = (values
                        .rows
                        .capacity()
                        .saturating_mul(
                            size_of::<MetadataValues>() - size_of::<[u32; METADATA_FIELDS]>(),
                        )
                        .saturating_sub(size_of::<MetadataValues>())
                        / size_of::<i64>())
                    .min((ESCAPED_VALUE - 1) as usize);
                    let capacity = needed
                        .max(values.escapes.capacity().saturating_mul(2))
                        .max(16)
                        .min(max_escapes);
                    values
                        .escapes
                        .try_reserve_exact(capacity - values.escapes.len())
                        .map_err(metadata_allocation)?;
                }
                // Account for actual allocator capacity, not only logical length.
                if self.narrow_fits(0) {
                    let Self::Narrow(values) = self else {
                        unreachable!()
                    };
                    values.write(row, input, codes, old);
                    return Ok(());
                }
            }
            // A block becomes wide once; pathological i64 distributions cannot
            // retain both the codes/escape table and the full-width payload.
            self.make_wide()?;
        }
        let Self::Wide(values) = self else {
            unreachable!()
        };
        let values_in = input.map(|value| value.unwrap_or(0));
        if let Some(row) = row {
            values[row] = values_in;
        } else {
            values.push(values_in);
        }
        Ok(())
    }

    fn narrow_fits(&self, additional: usize) -> bool {
        let Self::Narrow(values) = self else {
            return false;
        };
        let capacity = values.rows.capacity();
        let escapes = values
            .escapes
            .capacity()
            .max(values.escapes.len().saturating_add(additional));
        escapes < ESCAPED_VALUE as usize
            && capacity
                .saturating_mul(size_of::<[u32; METADATA_FIELDS]>())
                .saturating_add(escapes.saturating_mul(size_of::<i64>()))
                .saturating_add(size_of::<MetadataValues>())
                <= capacity.saturating_mul(size_of::<MetadataValues>())
    }

    fn make_wide(&mut self) -> Result<()> {
        let Self::Narrow(narrow) = self else {
            return Ok(());
        };
        let mut values = Vec::new();
        values
            .try_reserve_exact(narrow.rows.capacity())
            .map_err(metadata_allocation)?;
        for codes in &narrow.rows {
            values.push(std::array::from_fn(|field| {
                narrow.decode(codes[field], field)
            }));
        }
        *self = Self::Wide(values);
        Ok(())
    }

    fn capacity_bytes(&self) -> usize {
        match self {
            Self::Narrow(values) => {
                values.rows.capacity() * size_of::<[u32; METADATA_FIELDS]>()
                    + values.escapes.capacity() * size_of::<i64>()
            }
            Self::Wide(values) => values.capacity() * size_of::<MetadataValues>(),
        }
    }
}

impl NarrowMetadata {
    fn decode(&self, code: u32, field: usize) -> i64 {
        if code & ESCAPED_VALUE != 0 {
            self.escapes[(code & !ESCAPED_VALUE) as usize]
        } else {
            // prepare only emits an inline code when the exact i128 difference
            // is representable; wrapping avoids debug arithmetic on unused NULLs.
            self.bases[field].wrapping_add(i64::from(code))
        }
    }

    fn prepare(&mut self, input: MetadataInput) -> [Option<u32>; METADATA_FIELDS] {
        std::array::from_fn(|field| {
            let Some(value) = input[field] else {
                return Some(0);
            };
            if self.initialized & (1 << field) == 0 {
                self.bases[field] =
                    (i128::from(value) - INLINE_SPAN / 2).max(i128::from(i64::MIN)) as i64;
                self.initialized |= 1 << field;
            }
            let difference = i128::from(value) - i128::from(self.bases[field]);
            (0..=INLINE_SPAN)
                .contains(&difference)
                .then_some(difference as u32)
        })
    }

    fn write(
        &mut self,
        row: Option<usize>,
        input: MetadataInput,
        codes: [Option<u32>; METADATA_FIELDS],
        old: [u32; METADATA_FIELDS],
    ) {
        // Release first so another field in this same row can reuse the vacancy.
        for field in 0..METADATA_FIELDS {
            if old[field] & ESCAPED_VALUE != 0 && codes[field].is_some() {
                let slot = old[field] & !ESCAPED_VALUE;
                self.escapes[slot as usize] = self.free_head.map_or(-1, i64::from);
                self.free_head = Some(slot);
                self.free_count += 1;
            }
        }
        let encoded = std::array::from_fn(|field| {
            if let Some(code) = codes[field] {
                return code;
            }
            let slot = if old[field] & ESCAPED_VALUE != 0 {
                old[field] & !ESCAPED_VALUE
            } else if let Some(slot) = self.free_head {
                let next = self.escapes[slot as usize];
                self.free_head = (next >= 0).then_some(next as u32);
                self.free_count -= 1;
                slot
            } else {
                let slot = self.escapes.len() as u32;
                debug_assert!(slot < ESCAPED_VALUE);
                self.escapes.push(0);
                slot
            };
            self.escapes[slot as usize] = input[field].unwrap();
            slot | ESCAPED_VALUE
        });
        if let Some(row) = row {
            self.rows[row] = encoded;
        } else {
            self.rows.push(encoded);
        }
    }
}

fn metadata_allocation(error: std::collections::TryReserveError) -> MacEveryError {
    MacEveryError::Cli(format!("cannot allocate packed metadata: {error}"))
}

fn metadata_input(record: &DbRecordRef<'_>) -> MetadataInput {
    [
        record.size,
        record.mtime,
        record.ctime,
        record.dev,
        record.inode,
        Some(record.indexed_at),
    ]
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
            lowered.as_ref(),
            record.path,
            record.basename,
            record.basename_lower,
        ];
        // A sum below the representation limit also bounds every individual
        // field and any suffix-reused layout. Only unusually large input needs
        // the previous exact raw-layout validation before delta preflight ends.
        if raw_layout_needs_exact_check(fields.map(str::len)) {
            RowLayout::new(&fields)?;
        }
        Ok(Self { lowered })
    }
}

fn raw_layout_needs_exact_check(lengths: [usize; 4]) -> bool {
    lengths
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .is_none_or(|total| total > u32::MAX as usize)
}

struct RowLayout {
    parents: [Option<usize>; 4],
    required: usize,
}

impl RowLayout {
    fn new(fields: &[&str; 4]) -> Result<Self> {
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
        Ok(Self { parents, required })
    }
}

struct IndexBuilder {
    index: CompactIndex,
    extension_ids: HashMap<Box<str>, u32>,
    // An immutable dictionary ID, not a borrowed SQLite row or arena pointer.
    // It is discarded after load and does not alter the retained pool budget.
    last_prefix: Option<u32>,
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
            last_prefix: None,
        }
    }
    fn push(&mut self, record: DbRecordRef<'_>) -> Result<()> {
        let text = RowText::new(&record)?;
        let ext = self
            .index
            .intern_extension(record.ext_lower, Some(&mut self.extension_ids))?;
        let packed = self
            .index
            .pack_record(&record, &text, ext, self.last_prefix)?;
        let prefix = packed.prefix;
        self.index.append(packed, metadata_input(&record))?;
        if prefix != NO_PREFIX {
            self.last_prefix = Some(prefix);
        }
        Ok(())
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
                            fragmentation.dead_string_bytes -= self.row_string_bytes(old);
                        }
                    } else if !unchanged {
                        fragmentation.dead_string_bytes = fragmentation
                            .dead_string_bytes
                            .checked_add(self.row_string_bytes(old))
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
                        .checked_add(self.row_string_bytes(old))
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
                    let packed = self.pack_record(&borrowed, &text, ext, None)?;
                    if let Some(slot) = plan.slot {
                        self.update_metadata(slot, &borrowed)?;
                        *self.record_at_mut(slot) = packed;
                    } else {
                        self.append(packed, metadata_input(&borrowed))?;
                    }
                } else {
                    let slot = plan.slot.unwrap();
                    self.update_metadata(slot, &borrowed)?;
                    self.record_at_mut(slot).update_metadata(&borrowed, ext);
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
        self.path_view(old) == PathView::from(record.path)
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
        prefix_hint: Option<u32>,
    ) -> Result<CompactRecord> {
        let prefix = if record.path != text.lowered.as_ref() {
            match record
                .path
                .rfind('/')
                .map(|offset| offset + 1)
                .filter(|length| *length >= MIN_PREFIX_BYTES)
            {
                Some(length) => {
                    let parent = &record.path[..length];
                    // Adjacent loaded siblings often share a parent. Full byte
                    // equality is mandatory; a stale/different hint falls back
                    // to the original hash lookup and bounded admission policy.
                    match prefix_hint.filter(|id| {
                        self.prefixes
                            .values
                            .get(*id as usize)
                            .is_some_and(|prefix| prefix.as_ref() == parent)
                    }) {
                        Some(id) => id,
                        None => self.prefixes.intern(parent)?.unwrap_or(NO_PREFIX),
                    }
                }
                None => NO_PREFIX,
            }
        } else {
            NO_PREFIX
        };
        let prefix_len = self.prefixes.get(prefix).len();
        let fields = [
            text.lowered.as_ref(),
            &record.path[prefix_len..],
            record.basename,
            record.basename_lower,
        ];
        let layout = RowLayout::new(&fields)?;
        let arena = self.reserve_row(layout.required)?;
        let mut slices = [Slice::default(); 4];
        for (i, value) in fields.iter().enumerate() {
            slices[i] = if let Some(parent) = layout.parents[i] {
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
            path: Slice {
                start: slices[1].start,
                len: u32::try_from(record.path.len()).map_err(|_| index_too_large())?,
            },
            path_lower: slices[0],
            basename: slices[2],
            basename_lower: slices[3],
            arena: u32::try_from(arena).map_err(|_| index_too_large())?,
            ext,
            prefix,
            kind: record.kind.clone(),
            present: 0,
        };
        packed.update_metadata(record, ext);
        Ok(packed)
    }

    fn append(&mut self, packed: CompactRecord, metadata: MetadataInput) -> Result<()> {
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
            self.metadata.push(MetadataBlock::new(block.capacity())?);
            self.records.push(block);
        }
        self.metadata.last_mut().unwrap().push(metadata)?;
        self.records.last_mut().unwrap().push(packed);
        self.len = self.len.checked_add(1).ok_or_else(index_too_large)?;
        self.physical_len = self
            .physical_len
            .checked_add(1)
            .ok_or_else(index_too_large)?;
        Ok(())
    }

    fn update_metadata(&mut self, index: usize, record: &DbRecordRef<'_>) -> Result<()> {
        self.metadata[index / self.records_per_block]
            .update(index % self.records_per_block, metadata_input(record))
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
        let usage = self.memory_breakdown();
        [
            usage.record_capacity_bytes,
            usage.metadata_capacity_bytes,
            usage.string_capacity_bytes,
            usage.extension_capacity_bytes,
            usage.container_bytes,
            usage.prefix_string_bytes,
            usage.prefix_entry_capacity_bytes,
            usage.prefix_lookup_capacity_bytes,
        ]
        .into_iter()
        .fold(0u64, |sum, bytes| sum.saturating_add(bytes as u64))
    }

    /// Read-only allocation accounting; resident/compressed pages are an OS metric.
    pub fn memory_breakdown(&self) -> IndexMemoryBreakdown {
        let mut usage = IndexMemoryBreakdown {
            record_capacity_bytes: self
                .records
                .iter()
                .map(|block| block.capacity() * size_of::<CompactRecord>())
                .sum(),
            metadata_capacity_bytes: self
                .metadata
                .iter()
                .map(MetadataBlock::capacity_bytes)
                .sum(),
            string_capacity_bytes: self.arenas.iter().map(String::capacity).sum(),
            string_used_bytes: self.arenas.iter().map(String::len).sum(),
            extension_capacity_bytes: self.extensions.iter().map(|ext| ext.len()).sum(),
            prefix_string_bytes: self.prefixes.string_bytes,
            prefix_entry_capacity_bytes: self.prefixes.values.capacity() * size_of::<Box<str>>(),
            prefix_lookup_capacity_bytes: self.prefixes.slots.capacity() * size_of::<u32>(),
            prefix_entries: self.prefixes.values.len(),
            container_bytes: size_of::<Self>()
                + self.arenas.capacity() * size_of::<String>()
                + self.records.capacity() * size_of::<Vec<CompactRecord>>()
                + self.metadata.capacity() * size_of::<MetadataBlock>()
                + self.extensions.capacity() * size_of::<Box<str>>(),
            ..IndexMemoryBreakdown::default()
        };
        for block in &self.metadata {
            match block {
                MetadataBlock::Narrow(values) => {
                    usage.narrow_metadata_blocks += 1;
                    usage.live_metadata_escapes += values.escapes.len() - values.free_count;
                }
                MetadataBlock::Wide(_) => usage.wide_metadata_blocks += 1,
            }
        }
        usage
    }

    pub fn search(&self, options: &SearchOptions) -> Vec<SearchResult> {
        let options = options.normalized();
        let query = PreparedQuery::new(&options);
        let mut hits = TopHits::new(effective_limit(options.limit));
        for (index, (record, metadata, row)) in self
            .records
            .iter()
            .zip(&self.metadata)
            .flat_map(|(records, metadata)| {
                records
                    .iter()
                    .enumerate()
                    .map(move |(row, record)| (record, metadata, row))
            })
            .enumerate()
        {
            if !record.is_live() {
                continue;
            }
            let mtime = (record.present & (1 << 1) != 0).then(|| metadata.value(row, 1));
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
                    let prefix = self.prefixes.get(record.prefix);
                    let suffix = get(Slice {
                        start: record.path.start,
                        len: record.path.len - prefix.len() as u32,
                    });
                    PathView::new(prefix, suffix)
                });
            }
        }
        hits.finish()
            .into_iter()
            .map(|hit| SearchResult {
                record: self.to_file_record(hit.index),
                score: hit.score,
            })
            .collect()
    }
    #[cfg(test)]
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
    fn path_parts<'a>(&'a self, record: &'a CompactRecord) -> (&'a str, &'a str) {
        let prefix = self.prefixes.get(record.prefix);
        let suffix = self.get(
            record,
            Slice {
                start: record.path.start,
                len: record.path.len - prefix.len() as u32,
            },
        );
        (prefix, suffix)
    }
    fn path_view<'a>(&'a self, record: &'a CompactRecord) -> PathView<'a> {
        let (prefix, suffix) = self.path_parts(record);
        PathView::new(prefix, suffix)
    }
    fn row_string_bytes(&self, record: &CompactRecord) -> usize {
        let path = Slice {
            start: record.path.start,
            len: record.path.len - self.prefixes.get(record.prefix).len() as u32,
        };
        let slices = [
            path,
            record.path_lower,
            record.basename,
            record.basename_lower,
        ];
        let start = slices
            .iter()
            .map(|slice| slice.start as usize)
            .min()
            .unwrap();
        let end = slices
            .iter()
            .map(|slice| slice.start as usize + slice.len as usize)
            .max()
            .unwrap();
        end - start
    }
    fn original_path(&self, record: &CompactRecord) -> String {
        let (prefix, suffix) = self.path_parts(record);
        let mut path = String::with_capacity(record.path.len as usize);
        path.push_str(prefix);
        path.push_str(suffix);
        path
    }
    fn optional_metadata(&self, index: usize, record: &CompactRecord, field: usize) -> Option<i64> {
        (record.present & (1 << field) != 0).then(|| {
            self.metadata[index / self.records_per_block]
                .value(index % self.records_per_block, field)
        })
    }
    fn to_file_record(&self, index: usize) -> FileRecord {
        let record = self.record_at(index);
        FileRecord {
            id: record.id,
            path: self.original_path(record),
            path_lower: self.get(record, record.path_lower).into(),
            basename: self.get(record, record.basename).into(),
            basename_lower: self.get(record, record.basename_lower).into(),
            ext_lower: self.ext(record).map(str::to_string),
            kind: record.kind.clone(),
            size: self.optional_metadata(index, record, 0),
            mtime: self.optional_metadata(index, record, 1),
            ctime: self.optional_metadata(index, record, 2),
            dev: self.optional_metadata(index, record, 3),
            inode: self.optional_metadata(index, record, 4),
            indexed_at: self.metadata[index / self.records_per_block]
                .value(index % self.records_per_block, 5),
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
                .enumerate()
                .map(|(index, _)| actual.to_file_record(index))
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
                .enumerate()
                .map(|(slot, _)| index.to_file_record(slot))
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
        assert_eq!(index.to_file_record(0), record);
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
            .enumerate()
            .filter(|(_, record)| record.is_live())
            .map(|(slot, _)| index.to_file_record(slot))
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
        let memory_strings = index.memory_breakdown().string_capacity_bytes;
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
        assert_eq!(
            index.memory_breakdown().string_capacity_bytes,
            memory_strings
        );
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
        assert_eq!(
            index.memory_breakdown().string_capacity_bytes,
            memory_strings
        );

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
        assert_eq!(size_of::<CompactRecord>(), 56);
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

    fn check_metadata(block: &MetadataBlock, row: usize, input: MetadataInput) {
        for (field, value) in input.into_iter().enumerate() {
            if let Some(value) = value {
                assert_eq!(block.value(row, field), value, "row={row}, field={field}");
            }
        }
    }

    #[test]
    fn metadata_codes_preserve_extremes_null_and_exact_inline_boundaries() {
        let mut block = MetadataBlock::new(64).unwrap();
        let mut input = [
            Some(i64::MIN),
            Some(i64::MAX),
            None,
            Some(0),
            Some(-1),
            Some(99),
        ];
        block.push(input).unwrap();
        check_metadata(&block, 0, input);
        for field in 0..METADATA_FIELDS {
            let MetadataBlock::Narrow(values) = &block else {
                panic!("must stay narrow")
            };
            let base = values.bases[field];
            for candidate in [
                Some(i64::MIN),
                Some(i64::MAX),
                None,
                Some(0),
                Some(-1),
                Some(base),
                base.checked_sub(1),
                base.checked_add(INLINE_SPAN as i64),
                base.checked_add(INLINE_SPAN as i64 + 1),
            ] {
                input[field] = candidate;
                block.update(0, input).unwrap();
                check_metadata(&block, 0, input);
            }
        }
        assert!(matches!(block, MetadataBlock::Narrow(_)));
    }

    #[test]
    fn metadata_escape_storage_is_reused_across_fields_and_null_updates() {
        let normal = [
            Some(1),
            Some(1_700_000_000),
            Some(1_700_000_000),
            Some(17),
            Some(900),
            Some(1_790_000_000_000_000_000),
        ];
        let extremes = [
            Some(i64::MIN),
            Some(i64::MAX),
            Some(i64::MIN),
            Some(i64::MAX),
            Some(i64::MIN),
            Some(i64::MAX),
        ];
        let mut block = MetadataBlock::new(64).unwrap();
        for _ in 0..64 {
            block.push(normal).unwrap();
        }
        block.update(0, extremes).unwrap();
        let capacity = block.capacity_bytes();
        for round in 0..500 {
            for (row, input) in [
                (0, normal),
                (1, extremes),
                (1, [None; METADATA_FIELDS]),
                (0, extremes),
                (1, normal),
            ] {
                block.update(row, input).unwrap();
                check_metadata(&block, row, input);
                assert_eq!(block.capacity_bytes(), capacity, "round={round}");
            }
        }
        let MetadataBlock::Narrow(values) = block else {
            panic!("must stay narrow")
        };
        assert_eq!(values.escapes.len(), METADATA_FIELDS);
        assert_eq!(values.free_count, 0);
    }

    #[test]
    fn metadata_overflow_turns_wide_once_and_bounds_payload_capacity() {
        let mut block = MetadataBlock::new(64).unwrap();
        let normal = [Some(0); METADATA_FIELDS];
        let mut expected = vec![normal; 64];
        for input in &expected {
            block.push(*input).unwrap();
        }
        assert!(matches!(block, MetadataBlock::Narrow(_)));
        let budget = 64 * size_of::<MetadataValues>();
        let mut became_wide = false;
        for (row, input) in expected.iter_mut().enumerate() {
            *input = std::array::from_fn(|field| {
                Some(if (row + field) % 2 == 0 {
                    i64::MIN
                } else {
                    i64::MAX
                })
            });
            block.update(row, *input).unwrap();
            assert!(block.capacity_bytes() <= budget);
            if let MetadataBlock::Narrow(_) = block {
                assert!(block.narrow_fits(0));
            } else {
                became_wide = true;
            }
        }
        assert!(became_wide);
        for (row, input) in expected.iter().enumerate() {
            check_metadata(&block, row, *input);
        }
        for row in 0..64 {
            block.update(row, normal).unwrap();
        }
        assert!(matches!(block, MetadataBlock::Wide(_)));
        assert_eq!(block.capacity_bytes(), budget);
    }

    #[test]
    fn common_metadata_updates_leave_all_payload_capacities_unchanged() {
        let mut records = (1..=16)
            .map(|id| {
                let mut row = FileRecord::new(format!("/tmp/report-{id}.txt"), FileKind::File, 100);
                row.id = id;
                row.size = Some(id);
                row.mtime = Some(1_700_000_000 + id);
                row.ctime = row.mtime;
                row.dev = Some(17);
                row.inode = Some(100 + id);
                row
            })
            .collect::<Vec<_>>();
        let mut index = from_records(&records);
        let before = index.memory_breakdown();
        assert_eq!(before.narrow_metadata_blocks, 1);
        assert_eq!(before.live_metadata_escapes, 0);
        assert_eq!(size_of::<CompactRecord>(), 56);
        for round in 1..100 {
            let row = &mut records[5];
            row.size = Some(round);
            row.mtime = Some(1_700_000_000 + round);
            row.ctime = Some(1_700_000_000 - round);
            row.dev = Some(17 + round);
            row.inode = Some(round);
            row.indexed_at = round;
            assert_eq!(
                index
                    .apply_changes(&[FileChange {
                        id: row.id,
                        record: Some(row.clone())
                    }])
                    .unwrap(),
                ApplyOutcome::Applied
            );
            assert_eq!(index.memory_breakdown(), before);
        }
        assert_delta_matches(&index, &records);
        let accounted = before.record_capacity_bytes
            + before.metadata_capacity_bytes
            + before.string_capacity_bytes
            + before.extension_capacity_bytes
            + before.container_bytes
            + before.prefix_string_bytes
            + before.prefix_entry_capacity_bytes
            + before.prefix_lookup_capacity_bytes;
        assert_eq!(index.memory_usage_bytes(), accounted as u64);
        assert_eq!(
            before.record_capacity_bytes + before.metadata_capacity_bytes,
            RECORDS_PER_BLOCK * 80
        );
    }

    #[test]
    fn prefix_pool_has_a_hard_actual_capacity_limit_and_keeps_old_entries_readable() {
        let mut pool = PrefixPool::new(4096);
        let first = "/Shared/Original/Directory/";
        let id = pool.intern(first).unwrap().unwrap();
        let mut admitted = 1;
        for i in 0..1000 {
            let before = (pool.values.len(), pool.capacity_bytes());
            let next = format!("/Unique/Original/Directory/{i}/");
            match pool.intern(&next).unwrap() {
                Some(index) => {
                    admitted += 1;
                    assert_eq!(pool.get(index), next);
                }
                None => assert_eq!((pool.values.len(), pool.capacity_bytes()), before),
            }
            assert!(pool.capacity_bytes() + size_of::<PrefixPool>() <= pool.limit);
            assert_eq!(pool.intern(first).unwrap(), Some(id));
        }
        assert!(admitted > 1 && admitted < 1000);
        assert_eq!(pool.get(id), first);
        let mut zero = PrefixPool::new(0);
        assert_eq!(zero.intern(first).unwrap(), None);
        assert_eq!(zero.capacity_bytes(), 0);
    }

    #[test]
    fn prefix_table_resolves_collisions_and_preserves_ids_through_growth() {
        let mut pool = PrefixPool::new(64 * 1024);
        let collision_group = (0..10_000)
            .map(|i| format!("/Original/Collision/{i}/"))
            .filter(|value| pool.hasher.hash_one(value.as_str()) & 31 == 0)
            .take(40)
            .collect::<Vec<_>>();
        assert_eq!(collision_group.len(), 40);
        for (id, value) in collision_group.iter().enumerate() {
            assert_eq!(pool.intern(value).unwrap(), Some(id as u32));
        }
        assert!(pool.slots.len() > 32);
        for (id, value) in collision_group.iter().enumerate() {
            assert_eq!(pool.intern(value).unwrap(), Some(id as u32));
            assert_eq!(pool.get(id as u32), value);
        }
        assert_eq!(pool.values.len(), 40);
    }

    #[test]
    fn compressed_originals_keep_full_fields_and_only_own_their_row_bytes() {
        let prefix = "/Shared/Long Directory/Σ/";
        let mut records = ["CODE.txt", "report.txt", "Other", "", "报告📷"]
            .into_iter()
            .enumerate()
            .map(|(id, name)| {
                let mut record = FileRecord::new(format!("{prefix}{name}"), FileKind::File, 999);
                record.id = id as i64 + 1;
                record.mtime = Some(99);
                record
            })
            .collect::<Vec<_>>();
        // This basename is longer than the physical original suffix and cannot
        // borrow bytes from the separately interned prefix.
        records[2].basename = records[2].path.clone();
        records[2].basename_lower = "/unrelated/custom-lowercase".into();
        let mut lower = FileRecord::new(
            "/all/lower/directory/report.txt".into(),
            FileKind::File,
            999,
        );
        lower.id = 6;
        records.push(lower);
        let mut custom = FileRecord::new(
            "/Unrelated/Original/Directory/ΟΣ".into(),
            FileKind::Other,
            -1,
        );
        custom.id = 7;
        custom.path_lower = "/entirely-different/ς/报告".into();
        custom.basename = "a very long independent basename 📷/Σ".into();
        custom.basename_lower = "arbitrary-Σ".into();
        records.push(custom);
        let mut index = from_records(&records);
        assert_eq!(size_of::<CompactRecord>(), 56);
        assert_eq!(index.record_at(5).prefix, NO_PREFIX);
        assert_eq!(
            index.record_at(5).path.start,
            index.record_at(5).path_lower.start
        );
        assert_eq!(index.record_at(0).prefix, index.record_at(1).prefix);
        assert_eq!(index.path_parts(index.record_at(0)), (prefix, "CODE.txt"));
        assert_eq!(index.path_parts(index.record_at(3)), (prefix, ""));
        let initial_strings = index.arenas.iter().map(String::len).sum::<usize>();
        assert_eq!(
            (0..records.len())
                .map(|id| index.row_string_bytes(index.record_at(id)))
                .sum::<usize>(),
            initial_strings
        );
        assert_delta_matches(&index, &records);
        let owned = index.row_string_bytes(index.record_at(0));
        let pool = index.prefixes.capacity_bytes();
        let first = records.remove(0);
        index
            .apply_changes(&[FileChange {
                id: 1,
                record: None,
            }])
            .unwrap();
        assert_eq!(index.fragmentation.dead_string_bytes, owned);
        assert_eq!(index.prefixes.capacity_bytes(), pool);
        assert_delta_matches(&index, &records);
        index
            .apply_changes(&[FileChange {
                id: 1,
                record: Some(first.clone()),
            }])
            .unwrap();
        records.insert(0, first);
        assert_eq!(index.fragmentation.dead_string_bytes, 0);
        assert_eq!(index.prefixes.capacity_bytes(), pool);
        assert_eq!(
            index.arenas.iter().map(String::len).sum::<usize>(),
            initial_strings
        );
        assert_delta_matches(&index, &records);
        records[0].path = "/New/Long Original/Directory/report.pdf".into();
        records[0].path_lower = records[0].path.to_lowercase();
        index
            .apply_changes(&[FileChange {
                id: 1,
                record: Some(records[0].clone()),
            }])
            .unwrap();
        assert_eq!(index.fragmentation.dead_string_bytes, owned);
        assert_delta_matches(&index, &records);
    }

    #[test]
    fn prefix_budget_exhaustion_uses_raw_rows_without_losing_shared_or_lower_paths() {
        let mut builder = IndexBuilder::new(128, 3);
        builder.index.prefixes = PrefixPool::new(1536);
        let mut records = Vec::new();
        for i in 0..60 {
            let mut record = FileRecord::new(
                format!("/Unique/Original/Directory/{i}/report.txt"),
                FileKind::File,
                1,
            );
            record.id = i + 1;
            push_record(&mut builder, &record);
            records.push(record);
        }
        let mut index = builder.index;
        let compressed = index
            .iter_records()
            .filter(|row| row.prefix != NO_PREFIX)
            .count();
        assert!(compressed > 0 && compressed < records.len());
        assert!(index.prefixes.capacity_bytes() + size_of::<PrefixPool>() <= 1536);
        let old_prefix = index.record_at(0).prefix;
        let last_index = records.len() - 1;
        records[last_index].path = records[0].path.clone();
        records[last_index].path_lower = records[0].path_lower.clone();
        let last = records.last().unwrap().clone();
        index
            .apply_changes(&[FileChange {
                id: last.id,
                record: Some(last),
            }])
            .unwrap();
        assert_eq!(index.record_at(records.len() - 1).prefix, old_prefix);
        assert_delta_matches(&index, &records);
    }

    #[test]
    fn prefix_admission_occurs_only_after_clean_fallback_preflight() {
        let mut record = FileRecord::new("/all/lower/directory/report".into(), FileKind::File, 1);
        record.id = 10;
        let mut index = from_records(std::slice::from_ref(&record));
        let before = index.memory_breakdown();
        record.path = "/New/Long Original/Directory/report".into();
        record.path_lower = record.path.to_lowercase();
        let mut lower_id = record.clone();
        lower_id.id = 1;
        assert_eq!(
            index
                .apply_changes(&[
                    FileChange {
                        id: 1,
                        record: Some(lower_id)
                    },
                    FileChange {
                        id: 10,
                        record: Some(record.clone())
                    },
                ])
                .unwrap(),
            ApplyOutcome::NeedsFullReload("new non-monotonic record ID")
        );
        assert_eq!(
            index
                .apply_changes_with_limits(
                    &[FileChange {
                        id: 10,
                        record: Some(record)
                    }],
                    DeltaLimits {
                        dead_strings: 0,
                        dead_records: 1024,
                        extension_growth: 1024
                    }
                )
                .unwrap(),
            ApplyOutcome::NeedsFullReload("packed dead string budget exceeded")
        );
        assert_eq!(index.memory_breakdown(), before);
        assert!(index.prefixes.values.is_empty());
    }

    #[test]
    fn raw_layout_upper_bound_keeps_exact_checked32_fallback_boundaries() {
        let maximum = u32::MAX as usize;
        assert!(!raw_layout_needs_exact_check([0, 0, 0, 0]));
        assert!(!raw_layout_needs_exact_check([maximum, 0, 0, 0]));
        assert!(!raw_layout_needs_exact_check([maximum - 3, 1, 1, 1]));
        assert!(raw_layout_needs_exact_check([maximum, 1, 0, 0]));
        assert!(raw_layout_needs_exact_check([
            maximum, maximum, maximum, maximum
        ]));
        assert!(raw_layout_needs_exact_check([usize::MAX, 1, 0, 0]));
        // Suffix aliasing can make a raw sum exceed its physical requirement;
        // the rare branch must keep the exact layout, rather than reject solely
        // on its upper bound. Check those alias rules without a multi-GiB fixture.
        let fields = ["/original/report", "/original/report", "report", ""];
        let layout = RowLayout::new(&fields).unwrap();
        assert_eq!(layout.required, fields[0].len());
        assert_eq!(layout.parents, [None, Some(0), Some(0), Some(0)]);
    }

    #[test]
    fn builder_prefix_hint_preserves_exact_storage_and_full_result_semantics() {
        let mut records = (0..96)
            .map(|i| {
                let mut record = FileRecord::new(
                    format!("/Original Long Parent/Σ-{}/report-{i}.txt", i / 12),
                    FileKind::File,
                    i,
                );
                record.id = i + 1;
                if i % 9 == 0 {
                    record.path = record.path.to_lowercase();
                    record.path_lower = record.path.clone();
                }
                if i % 11 == 0 {
                    record.path_lower.clear();
                }
                if i % 13 == 0 {
                    record.basename = format!("independent basename-📷-{i}");
                    record.basename_lower = record.basename.to_lowercase();
                }
                if i % 17 == 0 {
                    record.path_lower = format!("/custom/lower/{i}");
                }
                record
            })
            .collect::<Vec<_>>();
        let mut hinted = IndexBuilder::new(256, 7);
        let mut uncached = IndexBuilder::new(256, 7);
        for (index, record) in records.iter().enumerate() {
            // A different valid ID and an invalid ID must never override exact
            // parent-byte comparison; both fall back to the existing lookup.
            if index % 7 == 0 {
                hinted.last_prefix = Some(NO_PREFIX);
            } else if index % 5 == 0 {
                hinted.last_prefix = Some(0);
            }
            uncached.last_prefix = None;
            push_record(&mut hinted, record);
            push_record(&mut uncached, record);
        }
        let hinted = hinted.index;
        let ordinary = uncached.index;
        assert_eq!(hinted.memory_breakdown(), ordinary.memory_breakdown());
        assert_eq!(hinted.arenas, ordinary.arenas);
        assert_eq!(hinted.prefixes.values, ordinary.prefixes.values);
        assert_eq!(
            hinted.prefixes.capacity_bytes(),
            ordinary.prefixes.capacity_bytes()
        );
        assert_eq!(size_of::<CompactRecord>(), 56);
        for record in &mut records {
            if record.path_lower.is_empty() {
                record.path_lower = record.path.to_lowercase();
            }
        }
        assert_delta_matches(&hinted, &records);
        assert_delta_matches(&ordinary, &records);
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
