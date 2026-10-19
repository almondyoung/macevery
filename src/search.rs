use crate::db::{Database, DbRecordRef};
use crate::error::Result;
use crate::model::{FileRecord, PathFilter, PathFilterMode, SearchOptions, SearchResult};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

const DEFAULT_LIMIT: usize = 50;

pub struct RecordView<'a> {
    pub path: &'a str,
    pub path_lower: &'a str,
    pub basename: &'a str,
    pub basename_lower: &'a str,
    pub ext_lower: Option<&'a str>,
    pub kind: &'a crate::model::FileKind,
    pub mtime: Option<i64>,
}

impl<'a> From<&'a FileRecord> for RecordView<'a> {
    fn from(record: &'a FileRecord) -> Self {
        Self {
            path: &record.path,
            path_lower: &record.path_lower,
            basename: &record.basename,
            basename_lower: &record.basename_lower,
            ext_lower: record.ext_lower.as_deref(),
            kind: &record.kind,
            mtime: record.mtime,
        }
    }
}

pub fn search(db: &Database, options: &SearchOptions) -> Result<Vec<SearchResult>> {
    let options = options.normalized();
    let query = PreparedQuery::new(&options);
    let limit = effective_limit(options.limit);
    db.with_read_snapshot(|| {
        // UTF-16 databases may reinterpret raw BLOB text. Their correctness
        // fallback is the complete decoded row stream, without byte prefilters.
        if db.uses_utf8_encoding()? {
            if let Some((lower, upper)) = sqlite_basename_prefix(&query) {
                let mut hits = OwnedTopHits::new(limit);
                db.for_each_basename_prefix_ref(lower, &upper, |record| {
                    hits.consider_record(record, &query);
                    Ok(())
                })?;
                // With one plain term, a non-prefix basename scores at least 250
                // before the nonnegative path-length penalty (also when fuzzy is
                // enabled). Only a strict bound proves no unvisited row can win.
                if hits.is_complete_below(250) {
                    return Ok(hits.finish());
                }
            }
            if !options.ext_filters.is_empty() {
                let mut hits = OwnedTopHits::new(limit);
                if db.for_each_extension_ref(&options.ext_filters, |record| {
                    hits.consider_record(record, &query);
                    Ok(())
                })? {
                    return Ok(hits.finish());
                }
            }
            let union_literal = sqlite_required_literal(&query);
            let basename_proof = sqlite_basename_literal(&query);
            if let Some(literal) = union_literal.or_else(|| basename_proof.map(|proof| proof.0)) {
                let mut hits = OwnedTopHits::new(limit);
                if !options.path_only {
                    db.for_each_basename_literal_ref(literal, |record| {
                        // Raw invalid-UTF8 BLOBs may pass instr while decoding to an
                        // empty basename. They belong in the path pass, not both sets.
                        if record.basename_lower.contains(literal) {
                            hits.consider_record(record, &query);
                        }
                        Ok(())
                    })?;
                    // A match outside this complete basename set pays at least
                    // the selected term's path/fuzzy bound. Every other term
                    // and the path-length penalty contributes a nonnegative score.
                    if basename_proof.is_some_and(|(selected, bound)| {
                        selected == literal && hits.is_complete_below(bound)
                    }) {
                        return Ok(hits.finish());
                    }
                }
                if union_literal.is_some() {
                    // For a nonfuzzy required term, every true match must contain
                    // its literal in basename or lowered path. These two complete
                    // passes form a disjoint union and need no score early-stop.
                    db.for_each_path_literal_ref(literal, !options.path_only, |record| {
                        hits.consider_record(record, &query);
                        Ok(())
                    })?;
                    return Ok(hits.finish());
                }
            }
        }
        // A pass without a proof is discarded: visit every row once in the
        // same snapshot, without LIKE/glob/mtime candidate truncation.
        let mut hits = OwnedTopHits::new(limit);
        db.for_each_record_ref(|record| {
            hits.consider_record(record, &query);
            Ok(())
        })?;
        Ok(hits.finish())
    })
}

fn sqlite_basename_prefix<'a>(query: &'a PreparedQuery<'_>) -> Option<(&'a str, String)> {
    if query.options.path_only || query.terms.len() != 1 {
        return None;
    }
    let term = &query.terms[0];
    let lower = term.finder.value.as_str();
    if term.glob.is_some() || lower.is_empty() || !lower.is_ascii() || lower.contains('\0') {
        return None;
    }
    let mut upper = lower.as_bytes().to_vec();
    let last = upper.last_mut()?;
    *last = last.checked_add(1)?;
    // An ASCII DEL suffix has no UTF-8 one-byte successor; just use the full
    // stream for that unusual query. No query bytes are changed or discarded.
    Some((lower, String::from_utf8(upper).ok()?))
}

fn sqlite_basename_literal<'a>(query: &'a PreparedQuery<'_>) -> Option<(&'a str, i64)> {
    if query.options.path_only {
        return None;
    }
    let (literal, term) = sqlite_selected_literal(query)?;
    let score_bound = match &term.glob {
        Some(glob) => i64::try_from(glob.pattern.len()).ok()?.checked_add(520)?,
        // All basename literal matches are present. Without one, the path
        // substring branch is >=550 and fuzzy branches are >=1000.
        None => 550,
    };
    Some((literal, score_bound))
}

fn sqlite_term_literal(term: &PreparedTerm) -> Option<&str> {
    let literal = match &term.glob {
        // Every literal segment is necessary. Its bytes are contiguous UTF-8
        // because glob delimiters are ASCII character boundaries.
        Some(glob) => glob
            .pattern
            .split(['*', '?'])
            .filter(|value| !value.is_empty() && !value.contains('\0'))
            .max_by_key(|value| value.len())?,
        None => term.finder.value.as_str(),
    };
    (!literal.is_empty() && !literal.contains('\0')).then_some(literal)
}

fn sqlite_required_literal<'a>(query: &'a PreparedQuery<'_>) -> Option<&'a str> {
    if query.fuzzy {
        return None;
    }
    sqlite_selected_literal(query).map(|selected| selected.0)
}

fn sqlite_selected_literal<'a>(
    query: &'a PreparedQuery<'_>,
) -> Option<(&'a str, &'a PreparedTerm)> {
    // Share selection between proof and union: retain the owning term even if
    // different terms have identical literal bytes but different score bounds.
    // Terms are ANDed, so any term's necessary literal covers a true nonfuzzy
    // match; choose the longest (last on equal lengths) to reduce row fetches.
    query
        .terms
        .iter()
        .filter_map(|term| sqlite_term_literal(term).map(|literal| (literal, term)))
        .max_by_key(|(literal, _)| literal.len())
}

struct OwnedRankedHit {
    score: i64,
    record: FileRecord,
}

impl OwnedRankedHit {
    fn compare_ref(&self, score: i64, record: &DbRecordRef<'_>) -> Ordering {
        score
            .cmp(&self.score)
            .then_with(|| self.record.mtime.cmp(&record.mtime))
            .then_with(|| record.path.len().cmp(&self.record.path.len()))
            .then_with(|| record.path.cmp(&self.record.path))
            // Full packed loads visit signed IDs in ascending order. SQLite
            // prefix ranges have a different visit order, so encode that last
            // stable tie explicitly rather than using the stream position.
            .then_with(|| record.id.cmp(&self.record.id))
    }
}

impl Ord for OwnedRankedHit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.record.mtime.cmp(&self.record.mtime))
            .then_with(|| self.record.path.len().cmp(&other.record.path.len()))
            .then_with(|| self.record.path.cmp(&other.record.path))
            .then_with(|| self.record.id.cmp(&other.record.id))
    }
}
impl PartialOrd for OwnedRankedHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for OwnedRankedHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for OwnedRankedHit {}

struct OwnedTopHits {
    hits: BinaryHeap<OwnedRankedHit>,
    limit: usize,
}

impl OwnedTopHits {
    fn new(limit: usize) -> Self {
        Self {
            hits: BinaryHeap::new(),
            limit,
        }
    }

    fn consider_record(&mut self, record: DbRecordRef<'_>, query: &PreparedQuery<'_>) {
        if !query.matches_metadata(&record.kind, record.mtime, record.ext_lower) {
            return;
        }
        let lowered = record
            .path_lower
            .filter(|path| !path.is_empty())
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(record.path.to_lowercase()));
        let Some(score) = query.rank_lowered(
            &lowered,
            record.basename_lower,
            record.path.len(),
            record.mtime,
        ) else {
            return;
        };
        if self.limit == 0
            || self.hits.len() >= self.limit
                && self
                    .hits
                    .peek()
                    .is_some_and(|worst| worst.compare_ref(score, &record) != Ordering::Less)
        {
            return;
        }
        // SQLite borrows expire at the next step. Copy only a competitive
        // winner; the heap retains at most K complete results, never the scan.
        let hit = OwnedRankedHit {
            score,
            record: FileRecord {
                id: record.id,
                path: record.path.to_owned(),
                path_lower: lowered.into_owned(),
                basename: record.basename.to_owned(),
                basename_lower: record.basename_lower.to_owned(),
                ext_lower: record.ext_lower.map(ToOwned::to_owned),
                kind: record.kind,
                size: record.size,
                mtime: record.mtime,
                ctime: record.ctime,
                dev: record.dev,
                inode: record.inode,
                indexed_at: record.indexed_at,
            },
        };
        if self.hits.len() < self.limit {
            self.hits.push(hit);
        } else {
            *self.hits.peek_mut().unwrap() = hit;
        }
    }

    fn is_complete_below(&self, score_bound: i64) -> bool {
        self.hits.len() == self.limit
            && self
                .hits
                .peek()
                .is_some_and(|worst| worst.score < score_bound)
    }

    fn finish(self) -> Vec<SearchResult> {
        self.hits
            .into_sorted_vec()
            .into_iter()
            .map(|hit| SearchResult {
                record: hit.record,
                score: hit.score,
            })
            .collect()
    }
}

#[cfg(test)]
fn rank_records(
    records: Vec<FileRecord>,
    options: &SearchOptions,
    limit: usize,
) -> Vec<SearchResult> {
    let query = PreparedQuery::new(options);
    let hits = top_hits(
        records
            .iter()
            .enumerate()
            .map(|(index, record)| (index, RecordView::from(record))),
        &query,
        limit,
    );
    // Move the selected SQLite candidates into results rather than cloning them.
    let mut selected = hits
        .into_iter()
        .enumerate()
        .map(|(rank, hit)| (hit.index, rank, hit.score))
        .collect::<Vec<_>>();
    selected.sort_unstable_by_key(|hit| hit.0);
    let mut selected = selected.into_iter().peekable();
    let mut results = Vec::with_capacity(selected.len());
    for (index, record) in records.into_iter().enumerate() {
        if selected.peek().map(|hit| hit.0) == Some(index) {
            let (_, rank, score) = selected.next().unwrap();
            results.push((rank, SearchResult { record, score }));
        }
    }
    results.sort_unstable_by_key(|result| result.0);
    results.into_iter().map(|(_, result)| result).collect()
}

pub fn search_records(records: &[FileRecord], options: &SearchOptions) -> Vec<SearchResult> {
    let options = options.normalized();
    let query = PreparedQuery::new(&options);
    top_hits(
        records
            .iter()
            .enumerate()
            .map(|(index, record)| (index, RecordView::from(record))),
        &query,
        effective_limit(options.limit),
    )
    .into_iter()
    .map(|hit| SearchResult {
        record: records[hit.index].clone(),
        score: hit.score,
    })
    .collect()
}

pub(crate) fn effective_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit
    }
}

pub(crate) struct ScoredIndex {
    pub(crate) index: usize,
    pub(crate) score: i64,
}

/// A complete original path with an optional shared prefix. Comparison is on
/// concatenated bytes, independent of where either path is split.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathView<'a> {
    prefix: &'a [u8],
    suffix: &'a [u8],
}

impl<'a> PathView<'a> {
    pub(crate) fn new(prefix: &'a str, suffix: &'a str) -> Self {
        Self {
            prefix: prefix.as_bytes(),
            suffix: suffix.as_bytes(),
        }
    }
    fn len(&self) -> usize {
        self.prefix.len() + self.suffix.len()
    }
}
impl<'a> From<&'a str> for PathView<'a> {
    fn from(path: &'a str) -> Self {
        Self::new("", path)
    }
}
impl Ord for PathView<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Shared dictionary entries (and raw paths with an empty prefix) can
        // compare their contiguous suffix directly without rereading a prefix.
        if self.prefix.as_ptr() == other.prefix.as_ptr() && self.prefix.len() == other.prefix.len()
        {
            return self.suffix.cmp(other.suffix);
        }
        let left = [self.prefix, self.suffix];
        let right = [other.prefix, other.suffix];
        let (mut l, mut r, mut lo, mut ro) = (0, 0, 0, 0);
        loop {
            while l < 2 && lo == left[l].len() {
                l += 1;
                lo = 0;
            }
            while r < 2 && ro == right[r].len() {
                r += 1;
                ro = 0;
            }
            if l == 2 || r == 2 {
                return (r == 2).cmp(&(l == 2));
            }
            let length = (left[l].len() - lo).min(right[r].len() - ro);
            let order = left[l][lo..lo + length].cmp(&right[r][ro..ro + length]);
            if order != Ordering::Equal {
                return order;
            }
            lo += length;
            ro += length;
        }
    }
}
impl PartialOrd for PathView<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for PathView<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for PathView<'_> {}

#[derive(Eq, PartialEq)]
struct RankedHit<'a> {
    index: usize,
    score: i64,
    mtime: Option<i64>,
    path: PathView<'a>,
}

impl Ord for RankedHit<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.mtime.cmp(&self.mtime))
            .then_with(|| self.path.len().cmp(&other.path.len()))
            .then_with(|| self.path.cmp(&other.path))
            // The previous stable sort retained input order for equal keys.
            .then_with(|| self.index.cmp(&other.index))
    }
}

impl PartialOrd for RankedHit<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) struct TopHits<'a> {
    hits: BinaryHeap<RankedHit<'a>>,
    limit: usize,
}

impl<'a> TopHits<'a> {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            hits: BinaryHeap::new(),
            limit,
        }
    }

    pub(crate) fn consider(
        &mut self,
        index: usize,
        score: i64,
        mtime: Option<i64>,
        path_len: usize,
        path: impl FnOnce() -> PathView<'a>,
    ) {
        if self.limit == 0 {
            return;
        }
        if self.hits.len() >= self.limit {
            let worst = self.hits.peek().unwrap();
            // Most broad matches lose before a path needs to be sliced/decoded.
            if score
                .cmp(&worst.score)
                .then_with(|| worst.mtime.cmp(&mtime))
                .then_with(|| path_len.cmp(&worst.path.len()))
                == Ordering::Greater
            {
                return;
            }
        }
        let hit = RankedHit {
            index,
            score,
            mtime,
            path: path(),
        };
        if self.hits.len() < self.limit {
            self.hits.push(hit);
        } else if self.hits.peek().is_some_and(|worst| hit < *worst) {
            *self.hits.peek_mut().unwrap() = hit;
        }
    }

    pub(crate) fn finish(self) -> Vec<ScoredIndex> {
        self.hits
            .into_sorted_vec()
            .into_iter()
            .map(|hit| ScoredIndex {
                index: hit.index,
                score: hit.score,
            })
            .collect()
    }
}

pub(crate) fn top_hits<'a>(
    records: impl IntoIterator<Item = (usize, RecordView<'a>)>,
    query: &PreparedQuery<'_>,
    limit: usize,
) -> Vec<ScoredIndex> {
    let mut hits = TopHits::new(limit);
    for (index, record) in records {
        if let Some(score) = query.rank(&record) {
            hits.consider(index, score, record.mtime, record.path.len(), || {
                record.path.into()
            });
        }
    }
    hits.finish()
}

pub fn rank_record(record: &FileRecord, options: &SearchOptions) -> Option<i64> {
    rank_view(&RecordView::from(record), options)
}

pub fn rank_view(record: &RecordView<'_>, options: &SearchOptions) -> Option<i64> {
    PreparedQuery::new(options).rank(record)
}

// Cache both short and long patterns once per request. Short UTF-8 needles
// still match bytes: every successful match has the same first byte offset as
// str::find, without rebuilding its string searcher for each indexed record.
#[derive(Default)]
struct ShortFinder {
    bytes: [u8; 8],
    first: u64,
    last: u64,
    len: usize,
}

impl ShortFinder {
    fn new(needle: &[u8]) -> Self {
        let mut finder = Self::default();
        if !needle.is_empty() && needle.len() <= 7 {
            finder.bytes[..needle.len()].copy_from_slice(needle);
            finder.first = u64::from_le_bytes([needle[0]; 8]);
            finder.last = u64::from_le_bytes([needle[needle.len() - 1]; 8]);
            finder.len = needle.len();
        }
        finder
    }

    #[inline]
    fn find(&self, haystack: &str) -> Option<usize> {
        match self.len {
            0 => Some(0),
            // A valid one-byte UTF-8 needle is ASCII; the standard char search
            // uses its optimized single-byte scan without a substring searcher.
            1 => haystack.find(char::from(self.bytes[0])),
            2 => self.find_bytes::<2>(haystack.as_bytes()),
            3 => self.find_bytes::<3>(haystack.as_bytes()),
            4 => self.find_bytes::<4>(haystack.as_bytes()),
            5 => self.find_bytes::<5>(haystack.as_bytes()),
            6 => self.find_bytes::<6>(haystack.as_bytes()),
            7 => self.find_bytes::<7>(haystack.as_bytes()),
            _ => unreachable!(),
        }
    }

    fn find_bytes<const N: usize>(&self, haystack: &[u8]) -> Option<usize> {
        let starts = haystack.len().checked_sub(N)? + 1;
        let needle = &self.bytes[..N];
        let mut offset = 0;
        while starts - offset >= 8 {
            // Both reads are within the haystack, including the last legal
            // candidate start. No padding or unaligned pointer reads are used.
            let first = u64::from_le_bytes(haystack[offset..offset + 8].try_into().unwrap());
            let last =
                u64::from_le_bytes(haystack[offset + N - 1..offset + N + 7].try_into().unwrap());
            let mut candidates =
                zero_byte_mask(first ^ self.first) & zero_byte_mask(last ^ self.last);
            while candidates != 0 {
                let candidate = offset + candidates.trailing_zeros() as usize / 8;
                // The word filter may have extra candidate bits due to byte
                // borrows. A complete fixed-length comparison rejects them.
                if haystack[candidate..candidate + N] == *needle {
                    return Some(candidate);
                }
                candidates &= candidates - 1;
            }
            offset += 8;
        }
        while offset < starts {
            if haystack[offset..offset + N] == *needle {
                return Some(offset);
            }
            offset += 1;
        }
        None
    }
}

#[inline]
fn zero_byte_mask(word: u64) -> u64 {
    word.wrapping_sub(0x0101_0101_0101_0101) & !word & 0x8080_8080_8080_8080
}

struct Finder {
    value: String,
    skip: Option<Box<[usize; 256]>>,
    prefix: Vec<usize>,
    short: ShortFinder,
}

impl Finder {
    fn new(value: String) -> Self {
        let bytes = value.as_bytes();
        let (skip, prefix) = if bytes.len() >= 8 {
            let mut skip = Box::new([bytes.len(); 256]);
            for (i, byte) in bytes[..bytes.len() - 1].iter().enumerate() {
                skip[*byte as usize] = bytes.len() - i - 1;
            }
            let mut prefix = vec![0; bytes.len()];
            for i in 1..bytes.len() {
                let mut matched = prefix[i - 1];
                while matched > 0 && bytes[i] != bytes[matched] {
                    matched = prefix[matched - 1];
                }
                if bytes[i] == bytes[matched] {
                    matched += 1;
                }
                prefix[i] = matched;
            }
            (Some(skip), prefix)
        } else {
            (None, Vec::new())
        };
        let short = ShortFinder::new(bytes);
        Self {
            value,
            skip,
            prefix,
            short,
        }
    }

    #[inline]
    fn find(&self, haystack: &str) -> Option<usize> {
        match &self.skip {
            None => self.short.find(haystack),
            Some(skip) => self.find_long(haystack, skip),
        }
    }

    fn find_long(&self, haystack: &str, skip: &[usize; 256]) -> Option<usize> {
        let needle = self.value.as_bytes();
        let haystack = haystack.as_bytes();
        if needle.len() > haystack.len() {
            return None;
        }
        let mut offset = 0;
        let mut compared = 0usize;
        let budget = haystack.len().saturating_mul(4);
        while offset <= haystack.len() - needle.len() {
            let mut i = needle.len();
            while i > 0 && haystack[offset + i - 1] == needle[i - 1] {
                i -= 1;
                compared += 1;
            }
            if i == 0 {
                return Some(offset);
            }
            compared += 1;
            if compared > budget {
                return self.linear_find(haystack);
            }
            offset += skip[haystack[offset + needle.len() - 1] as usize];
        }
        None
    }

    fn linear_find(&self, haystack: &[u8]) -> Option<usize> {
        let needle = self.value.as_bytes();
        let mut matched = 0;
        for (i, byte) in haystack.iter().enumerate() {
            while matched > 0 && *byte != needle[matched] {
                matched = self.prefix[matched - 1];
            }
            if *byte == needle[matched] {
                matched += 1;
            }
            if matched == needle.len() {
                return Some(i + 1 - matched);
            }
        }
        None
    }
}

struct PreparedGlob {
    pattern: String,
    literal: Option<Finder>,
    all_stars: bool,
    single_literal: bool,
}

impl PreparedGlob {
    fn new(pattern: String) -> Self {
        let mut literals = pattern.split(['*', '?']).filter(|part| !part.is_empty());
        let literal = literals.next().map(|value| Finder::new(value.into()));
        let single_literal = !pattern.contains('?') && literals.next().is_none();
        let all_stars = pattern.bytes().all(|byte| byte == b'*');
        Self {
            pattern,
            literal,
            all_stars,
            single_literal,
        }
    }

    fn score(&self, haystack: &str) -> Option<i64> {
        if self.all_stars {
            return Some(self.pattern.len() as i64);
        }
        // A required literal being absent rules out the whole glob immediately.
        let start = match &self.literal {
            Some(literal) => literal.find(haystack)?,
            None => 0,
        };
        let matches = if self.single_literal {
            let literal = self.literal.as_ref().unwrap().value.as_str();
            (self.pattern.starts_with('*') || haystack.starts_with(literal))
                && (self.pattern.ends_with('*') || haystack.ends_with(literal))
        } else {
            glob_match(haystack.as_bytes(), self.pattern.as_bytes())
        };
        matches.then_some(start as i64 * 2 + self.pattern.len() as i64)
    }
}

struct PreparedTerm {
    finder: Finder,
    glob: Option<PreparedGlob>,
}

struct PreparedPathFilter {
    mode: PathFilterMode,
    finder: Finder,
    glob: Option<PreparedGlob>,
}

impl PreparedPathFilter {
    fn new(filter: &PathFilter) -> Self {
        Self {
            mode: filter.mode.clone(),
            finder: Finder::new(filter.value.clone()),
            glob: has_glob_syntax(&filter.value).then(|| PreparedGlob::new(filter.value.clone())),
        }
    }
    fn matches(&self, path: &str) -> bool {
        let matches = |text: &str, component: bool| match &self.glob {
            Some(glob) => glob.score(text).is_some(),
            None if component => text == self.finder.value,
            None => self.finder.find(text).is_some(),
        };
        match self.mode {
            PathFilterMode::Contains => matches(path, false),
            PathFilterMode::Component => path
                .split('/')
                .filter(|part| !part.is_empty())
                .any(|part| matches(part, true)),
        }
    }
}

pub(crate) struct PreparedQuery<'a> {
    options: &'a SearchOptions,
    terms: Vec<PreparedTerm>,
    path_filters: Vec<PreparedPathFilter>,
    excluded_path_filters: Vec<PreparedPathFilter>,
    fuzzy: bool,
    needs_text: bool,
}

impl<'a> PreparedQuery<'a> {
    pub(crate) fn new(options: &'a SearchOptions) -> Self {
        let terms = options
            .terms()
            .into_iter()
            .map(|term| {
                let value = term.to_lowercase();
                PreparedTerm {
                    glob: has_glob_syntax(&term).then(|| PreparedGlob::new(value.clone())),
                    finder: Finder::new(value),
                }
            })
            .collect::<Vec<_>>();
        let needs_text = !options.path_filters.is_empty()
            || !options.excluded_path_filters.is_empty()
            || terms
                .iter()
                .any(|term| term.glob.as_ref().is_none_or(|glob| !glob.all_stars));
        Self {
            options,
            terms,
            path_filters: options
                .path_filters
                .iter()
                .map(PreparedPathFilter::new)
                .collect(),
            excluded_path_filters: options
                .excluded_path_filters
                .iter()
                .map(PreparedPathFilter::new)
                .collect(),
            fuzzy: options.fuzzy_enabled(),
            needs_text,
        }
    }

    pub(crate) fn needs_text(&self) -> bool {
        self.needs_text
    }
    pub(crate) fn needs_basename(&self) -> bool {
        !self.options.path_only && !self.terms.is_empty() && self.needs_text
    }
    pub(crate) fn needs_extension(&self) -> bool {
        !self.options.ext_filters.is_empty() || !self.options.excluded_ext_filters.is_empty()
    }

    pub(crate) fn matches_metadata(
        &self,
        kind: &crate::model::FileKind,
        mtime: Option<i64>,
        ext: Option<&str>,
    ) -> bool {
        let options = self.options;
        if !options.kind_filters.is_empty() && !options.kind_filters.contains(kind)
            || options.excluded_kind_filters.contains(kind)
        {
            return false;
        }
        if options
            .modified_after
            .is_some_and(|after| mtime.is_none_or(|time| time < after))
            || options
                .modified_before
                .is_some_and(|before| mtime.is_some_and(|time| time >= before))
        {
            return false;
        }
        if !options.ext_filters.is_empty()
            && !ext.is_some_and(|ext| options.ext_filters.iter().any(|filter| filter == ext))
        {
            return false;
        }
        !ext.is_some_and(|ext| {
            options
                .excluded_ext_filters
                .iter()
                .any(|filter| filter == ext)
        })
    }

    pub(crate) fn rank(&self, record: &RecordView<'_>) -> Option<i64> {
        if !self.matches_metadata(record.kind, record.mtime, record.ext_lower) {
            return None;
        }
        self.rank_lowered(
            record.path_lower,
            record.basename_lower,
            record.path.len(),
            record.mtime,
        )
    }

    pub(crate) fn rank_lowered(
        &self,
        path: &str,
        basename: &str,
        path_len: usize,
        mtime: Option<i64>,
    ) -> Option<i64> {
        if self.path_filters.iter().any(|filter| !filter.matches(path))
            || self
                .excluded_path_filters
                .iter()
                .any(|filter| filter.matches(path))
        {
            return None;
        }
        if self.terms.is_empty() {
            return Some(10_000 - mtime.unwrap_or(0).min(9_000));
        }
        let mut score = 0;
        for term in &self.terms {
            let token = term.finder.value.as_str();
            let token_score = if let Some(glob) = &term.glob {
                if self.options.path_only {
                    glob.score(path).map(|value| value + 450)
                } else {
                    glob.score(basename)
                        .map(|value| value + 180)
                        .or_else(|| glob.score(path).map(|value| value + 520))
                }
            } else if self.options.path_only {
                term.finder
                    .find(path)
                    .map(|index| 300 + index.min(500) as i64)
            } else if basename == token {
                Some(0)
            } else if basename.starts_with(token) {
                Some(100 + token.len() as i64)
            } else if let Some(index) = term.finder.find(basename) {
                Some(250 + index as i64)
            } else if let Some(index) = term.finder.find(path) {
                Some(550 + index.min(500) as i64)
            } else if self.fuzzy {
                fuzzy_score(basename, token)
                    .map(|value| value + 1_000)
                    .or_else(|| fuzzy_score(path, token).map(|value| value + 1_900))
            } else {
                None
            };
            score += token_score?;
        }
        Some(score + (path_len as i64).min(500))
    }
}

fn fuzzy_score(haystack: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }

    let mut last_index: Option<i64> = None;
    let mut gaps = 0i64;
    let mut start = 0i64;
    let mut needle_chars = needle.chars();
    let mut current = needle_chars.next()?;

    for (index, ch) in haystack.chars().enumerate() {
        if ch == current {
            if let Some(last) = last_index {
                gaps += (index as i64 - last - 1).max(0);
            } else {
                start = index as i64;
            }
            last_index = Some(index as i64);
            if let Some(next) = needle_chars.next() {
                current = next;
            } else {
                return Some(start * 3 + gaps * 5 + needle.len() as i64);
            }
        }
    }
    None
}

fn has_glob_syntax(value: &str) -> bool {
    value.contains('*') || value.contains('?')
}

fn glob_match(haystack: &[u8], pattern: &[u8]) -> bool {
    let (mut h, mut p) = (0usize, 0usize);
    let mut star = None;
    let mut match_after_star = 0usize;

    while h < haystack.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == haystack[h]) {
            h += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            match_after_star = h;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            match_after_star += 1;
            h = match_after_star;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }

    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::model::{FileKind, FileRecord};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn record(path: &str) -> FileRecord {
        FileRecord::new(path.to_string(), FileKind::File, 1)
    }

    fn db_record(record: &FileRecord) -> DbRecordRef<'_> {
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

    #[test]
    fn sqlite_prefix_bounds_cover_every_ascii_suffix_without_changing_query_bytes() {
        let options = SearchOptions::default();
        let mut prepared = PreparedQuery::new(&options);
        for byte in 1..=126u8 {
            let needle = format!("code{}", byte as char);
            prepared.terms = vec![PreparedTerm {
                finder: Finder::new(needle.clone()),
                glob: None,
            }];
            let (lower, upper) = sqlite_basename_prefix(&prepared).unwrap();
            assert_eq!(lower, needle);
            for value in [
                needle.clone(),
                format!("{needle}\0rest"),
                format!("{needle}Σ/汉"),
                format!("before{needle}"),
                "code".to_owned(),
                upper.clone(),
            ] {
                assert_eq!(
                    value.starts_with(&needle),
                    value.as_bytes() >= lower.as_bytes() && value.as_bytes() < upper.as_bytes(),
                    "needle={needle:?}, value={value:?}"
                );
            }
        }
        for needle in ["", "code\0", "a\0b", "code\u{7f}", "Σ", "汉"] {
            prepared.terms = vec![PreparedTerm {
                finder: Finder::new(needle.to_owned()),
                glob: None,
            }];
            assert!(sqlite_basename_prefix(&prepared).is_none(), "{needle:?}");
        }
        for query in ["*code", "code?", "code report", "", "~"] {
            let options = SearchOptions {
                query: query.to_owned(),
                ..Default::default()
            };
            assert!(sqlite_basename_prefix(&PreparedQuery::new(&options)).is_none());
        }
        let options = SearchOptions {
            query: "code".into(),
            path_only: true,
            ..Default::default()
        };
        assert!(sqlite_basename_prefix(&PreparedQuery::new(&options)).is_none());
    }

    #[test]
    fn owned_sqlite_heap_has_strict_score_bound_and_canonical_signed_id_ties() {
        let options = SearchOptions {
            query: "code".into(),
            ..Default::default()
        };
        let prepared = PreparedQuery::new(&options);
        let mut hits = OwnedTopHits::new(2);
        for length in [145, 146] {
            let mut value = record(&"x".repeat(length));
            value.basename_lower = "code-physical-name-is-independent".into();
            hits.consider_record(db_record(&value), &prepared);
        }
        assert_eq!(hits.hits.peek().unwrap().score, 250);
        assert!(!hits.is_complete_below(250));
        assert!(hits.is_complete_below(251));
        let mut incomplete = OwnedTopHits::new(3);
        incomplete.consider_record(db_record(&record("/code")), &prepared);
        assert!(!incomplete.is_complete_below(250));

        let options = SearchOptions {
            query: "code".into(),
            limit: 3,
            ..Default::default()
        };
        let prepared = PreparedQuery::new(&options);
        let mut records = [i64::MIN, -10, 0, 5, i64::MAX]
            .into_iter()
            .map(|id| {
                let mut value = record("/same/code");
                value.id = id;
                value.size = Some(id);
                value.ctime = Some(i64::MIN);
                value.dev = Some(i64::MAX);
                value.indexed_at = id;
                value
            })
            .collect::<Vec<_>>();
        let expected = test_reference::search_records(&records, &options);
        records.reverse();
        let mut hits = OwnedTopHits::new(options.limit);
        for value in &records {
            hits.consider_record(db_record(value), &prepared);
            assert!(hits.hits.len() <= options.limit);
        }
        assert_eq!(hits.finish(), expected);
    }

    #[test]
    fn sqlite_basename_literal_prepass_uses_only_necessary_bytes_and_strict_bounds() {
        for (text, expected) in [
            ("code", Some(("code", 550))),
            ("~code", Some(("code", 550))),
            ("汉", Some(("汉", 550))),
            ("*.rs", Some((".rs", 524))),
            ("*a?longer*Σ", Some(("longer", 532))),
            ("\0*code", Some(("code", 526))),
            ("?", None),
            ("***", None),
            ("a\0b", None),
            ("code report", Some(("report", 550))),
            ("~code report", Some(("report", 550))),
            ("code *test*", Some(("test", 526))),
            ("*test* code", Some(("code", 550))),
            ("code *code*", Some(("code", 526))),
            ("*code* code", Some(("code", 550))),
        ] {
            let options = SearchOptions {
                query: text.into(),
                ..Default::default()
            }
            .normalized();
            let prepared = PreparedQuery::new(&options);
            assert_eq!(sqlite_basename_literal(&prepared), expected, "{text:?}");
        }
        for (text, basename, path_len, bound) in
            [("code", "xcode", 299, 550), ("*.rs", ".rs", 340, 524)]
        {
            let options = SearchOptions {
                query: text.into(),
                ..Default::default()
            };
            let prepared = PreparedQuery::new(&options);
            let mut value = record(&"x".repeat(path_len));
            value.basename_lower = basename.into();
            let mut hits = OwnedTopHits::new(1);
            hits.consider_record(db_record(&value), &prepared);
            assert_eq!(hits.hits.peek().unwrap().score, bound);
            assert!(!hits.is_complete_below(bound));
            assert!(hits.is_complete_below(bound + 1));
        }
    }

    #[test]
    fn multi_term_basename_proof_tracks_selected_term_and_keeps_strict_boundary() {
        for (text, basename, path_len, literal, bound) in [
            ("code test", "test_code", 191, "test", 550),
            ("~code test", "test_code", 191, "test", 550),
            ("code *test*", "testcode", 86, "test", 526),
            ("~code *test*", "testcode", 86, "test", 526),
            ("*test* code", "testcode", 110, "code", 550),
            ("code *code*", "code", 340, "code", 526),
            ("*code* code", "code", 364, "code", 550),
        ] {
            let options = SearchOptions {
                query: text.into(),
                ..Default::default()
            }
            .normalized();
            let prepared = PreparedQuery::new(&options);
            assert_eq!(
                sqlite_basename_literal(&prepared),
                Some((literal, bound)),
                "{text:?}"
            );
            if !prepared.fuzzy {
                assert_eq!(sqlite_required_literal(&prepared), Some(literal));
            } else {
                assert!(sqlite_required_literal(&prepared).is_none());
            }
            let mut value = record(&"x".repeat(path_len));
            value.basename_lower = basename.into();
            let mut hits = OwnedTopHits::new(1);
            hits.consider_record(db_record(&value), &prepared);
            assert_eq!(hits.hits.peek().unwrap().score, bound, "{text:?}");
            assert!(!hits.is_complete_below(bound), "{text:?}");
            value.path.pop();
            hits.consider_record(db_record(&value), &prepared);
            assert!(hits.is_complete_below(bound), "{text:?}");
            let path_only = SearchOptions {
                path_only: true,
                ..options
            };
            assert!(sqlite_basename_literal(&PreparedQuery::new(&path_only)).is_none());
        }
    }

    #[test]
    fn sqlite_basename_prepasses_retain_outside_path_competitors_and_fuzzy_results() {
        let temp = unique_temp_dir("macevery-basename-prepass-proof");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        for (path, basename) in [
            (format!("/{}/xcode", "long".repeat(100)), "xcode"),
            ("/code/x".into(), "x"),
            (format!("/{}/x.rs", "long".repeat(100)), "x.rs"),
            ("/short.rs".into(), "independent-name"),
            ("/z/c-o-d-e".into(), "c-o-d-e"),
            ("/汉/ab汉.rs".into(), "ab汉.rs"),
        ] {
            let mut value = record(&path);
            value.basename_lower = basename.into();
            db.upsert_file(&value).unwrap();
        }
        let records = db.all_records().unwrap();
        for query in ["code", "~code", "*.rs", "*?汉*", "*汉*", "?", "***"] {
            for limit in [1, 2, 7, 300] {
                let options = SearchOptions {
                    query: query.into(),
                    limit,
                    ..Default::default()
                };
                assert_eq!(
                    search(&db, &options).unwrap(),
                    test_reference::search_records(&records, &options),
                    "{options:?}"
                );
            }
        }
        let options = SearchOptions {
            query: "code".into(),
            limit: 1,
            ..Default::default()
        };
        assert_eq!(search(&db, &options).unwrap()[0].record.path, "/code/x");
        let options = SearchOptions {
            query: "*.rs".into(),
            limit: 1,
            ..Default::default()
        };
        assert_eq!(search(&db, &options).unwrap()[0].record.path, "/汉/ab汉.rs");
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn sqlite_literal_union_handles_independent_fields_path_only_and_all_required_terms() {
        let temp = unique_temp_dir("macevery-literal-union");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        for (path, lowered, basename, ext) in [
            ("/physical/basename-code", "/report/route", "code", "rs"),
            (
                "/physical/basename-report",
                "/code/route",
                "report",
                "swift",
            ),
            ("/physical/path-only", "/code/route", "code", "rs"),
            ("/physical/no-path-code", "/different", "code", "rs"),
            (
                "/physical/longest-glob-literal",
                "/a/longer",
                "has-a",
                "swift",
            ),
            ("/physical/basename-glob", "/different", "alonger", "rs"),
            ("/physical/path-glob", "/a/longer", "outside", "rs"),
            ("/physical/fuzzy", "/different", "c-o-d-e", "rs"),
            ("/cheap/test_code", "/independent", "test_code", "rs"),
            ("/cheap/testcode", "/independent", "testcode", "swift"),
            ("/physical/fuzzy-multi", "/test", "c-o-d-e", "rs"),
        ] {
            let mut value = record(path);
            value.path_lower = lowered.into();
            value.basename_lower = basename.into();
            value.ext_lower = Some(ext.into());
            db.upsert_file(&value).unwrap();
        }
        let records = db.all_records().unwrap();
        let cases = [
            "code",
            "code report",
            "code test",
            "~code test",
            "code *test*",
            "~code *test*",
            "*test* code",
            "code *code*",
            "*code* code",
            "code missing-long-term",
            "*a*longer*",
            "*?longer*",
            "code *port*",
            "~code",
            "ext:rs ~code",
            "ext:rs|swift code",
            "path:route code",
            "part:route code",
            "!path:different code",
            "zzzzzz-not-present-anywhere",
        ];
        for query in cases {
            for path_only in [false, true] {
                for limit in [0, 1, 2, 300] {
                    let options = SearchOptions {
                        query: query.into(),
                        path_only,
                        limit,
                        ..Default::default()
                    };
                    assert_eq!(
                        search(&db, &options).unwrap(),
                        test_reference::search_records(&records, &options),
                        "{options:?}"
                    );
                }
            }
        }
        let options = SearchOptions {
            query: "*a*longer*".into(),
            limit: 300,
            ..Default::default()
        };
        let results = search(&db, &options).unwrap();
        assert!(results
            .iter()
            .any(|hit| hit.record.path == "/physical/longest-glob-literal"));
        let options = SearchOptions {
            query: "code".into(),
            path_only: true,
            limit: 300,
            ..Default::default()
        };
        let results = search(&db, &options).unwrap();
        assert!(results
            .iter()
            .any(|hit| hit.record.path == "/physical/path-only"));
        assert!(!results
            .iter()
            .any(|hit| hit.record.path == "/physical/no-path-code"));
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn sqlite_stream_matches_independent_reference_for_all_query_modes_and_limits() {
        let temp = unique_temp_dir("macevery-exact-stream");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.begin().unwrap();
        for value in test_reference::records() {
            db.upsert_file(&value).unwrap();
        }
        for path in ["/汉", "/abc", "/x/axbyczd", "/x/abcd", "/code-old"] {
            db.upsert_file(&record(path)).unwrap();
        }
        db.commit().unwrap();
        let records = db.all_records().unwrap();
        let mut cases = test_reference::options();
        cases.extend([
            SearchOptions {
                query: "????".into(),
                path_only: true,
                ..Default::default()
            },
            SearchOptions {
                query: "~abcd".into(),
                ..Default::default()
            },
            SearchOptions {
                query: "code".into(),
                fuzzy: true,
                ..Default::default()
            },
        ]);
        for mut options in cases {
            for limit in [0, 1, 2, 7, 50, 300, records.len() + 1, usize::MAX] {
                options.limit = limit;
                assert_eq!(
                    search(&db, &options).unwrap(),
                    test_reference::search_records(&records, &options),
                    "{options:?}"
                );
            }
        }
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn sqlite_prefix_proof_falls_back_for_outside_competitors_or_too_few_hits() {
        let temp = unique_temp_dir("macevery-prefix-proof");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.upsert_file(&record(&format!("/{}/code-prefix", "long".repeat(80))))
            .unwrap();
        db.upsert_file(&record("/xcode")).unwrap();
        db.upsert_file(&record("/co-de")).unwrap();
        let records = db.all_records().unwrap();
        for query in ["code", "~code"] {
            for limit in [1, 2, 3, 300] {
                let options = SearchOptions {
                    query: query.into(),
                    limit,
                    ..Default::default()
                };
                let got = search(&db, &options).unwrap();
                assert_eq!(got, test_reference::search_records(&records, &options));
                assert_eq!(got[0].record.path, "/xcode");
            }
        }
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn sqlite_exact_search_does_not_drop_old_hits_after_twenty_four_thousand_candidates() {
        let temp = unique_temp_dir("macevery-old-hit-recall");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.begin().unwrap();
        for i in 0..24_100 {
            let mut value = record(&format!("/new/sqlite-code-path-{i:05}"));
            value.mtime = Some(100 + i);
            db.upsert_file(&value).unwrap();
        }
        let mut old = record("/code");
        old.mtime = Some(-1);
        db.upsert_file(&old).unwrap();
        db.commit().unwrap();
        let records = db.all_records().unwrap();
        let options = SearchOptions {
            query: "code".into(),
            limit: 300,
            ..Default::default()
        };
        let got = search(&db, &options).unwrap();
        assert_eq!(got, test_reference::search_records(&records, &options));
        assert_eq!(got[0].record.path, "/code");
        assert_eq!(got.len(), 300);
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn exact_beats_prefix_and_path() {
        let exact = record("/tmp/report");
        let prefix = record("/tmp/report-final.pdf");
        let path = record("/tmp/reporting/final.pdf");
        let options = SearchOptions {
            query: "report".to_string(),
            limit: 10,
            ..SearchOptions::default()
        };
        assert!(rank_record(&exact, &options) < rank_record(&prefix, &options));
        assert!(rank_record(&prefix, &options) < rank_record(&path, &options));
    }

    #[test]
    fn uppercase_query_is_case_insensitive() {
        let options = SearchOptions {
            query: "PDF".to_string(),
            limit: 10,
            ..SearchOptions::default()
        };
        assert!(rank_record(&record("/tmp/file.pdf"), &options).is_some());
        assert!(rank_record(&record("/tmp/file.PDF"), &options).is_some());
    }

    #[test]
    fn glob_matches_case_insensitive_basename_and_path() {
        let options = SearchOptions {
            query: "*gpt*pdf".to_string(),
            limit: 10,
            ..SearchOptions::default()
        };
        assert!(rank_record(&record("/tmp/ChatGPT Notes.PDF"), &options).is_some());
        assert!(rank_record(&record("/tmp/reports/gpt-summary-final.pdf"), &options).is_some());
        assert!(rank_record(&record("/tmp/reports/gpt-summary-final.txt"), &options).is_none());
    }

    #[test]
    fn plain_query_does_not_fuzzy_match() {
        let options = SearchOptions {
            query: "leetcode".to_string(),
            limit: 10,
            ..SearchOptions::default()
        };
        assert!(rank_record(&record("/tmp/sqlite_result_code.h"), &options).is_none());
        assert!(rank_record(&record("/tmp/LeetCode 101.pdf"), &options).is_some());
    }

    #[test]
    fn tilde_query_enables_fuzzy_match() {
        let options = SearchOptions {
            query: "~leetcode".to_string(),
            limit: 10,
            ..SearchOptions::default()
        };
        assert!(rank_record(&record("/tmp/sqlite_result_code.h"), &options).is_some());
    }

    #[test]
    fn inline_filters_match_extension_kind_path_and_mtime() {
        let mut pdf = record("/Users/me/Downloads/LeetCode 101.pdf");
        pdf.mtime = Some(1_700_000_000);
        let mut old_pdf = record("/Users/me/Downloads/Old LeetCode.pdf");
        old_pdf.mtime = Some(1_600_000_000);
        let mut note = record("/Users/me/Documents/LeetCode.txt");
        note.mtime = Some(1_700_000_000);

        let options = SearchOptions {
            query: "ext:pdf path:downloads mtime:7d leetcode".to_string(),
            limit: 10,
            ..SearchOptions::default()
        }
        .normalized_at(1_700_100_000);

        assert!(rank_record(&pdf, &options).is_some());
        assert!(rank_record(&old_pdf, &options).is_none());
        assert!(rank_record(&note, &options).is_none());
        assert_eq!(options.terms(), vec!["leetcode"]);
    }

    #[test]
    fn inline_kind_filter_matches_directories() {
        let mut dir = FileRecord::new("/tmp/code".to_string(), FileKind::Directory, 1);
        dir.mtime = Some(1);
        let mut file = record("/tmp/code.txt");
        file.mtime = Some(1);

        let options = SearchOptions {
            query: "kind:folder code".to_string(),
            limit: 10,
            ..SearchOptions::default()
        }
        .normalized_at(10);

        assert!(rank_record(&dir, &options).is_some());
        assert!(rank_record(&file, &options).is_none());
    }

    #[test]
    fn inline_filters_support_multi_ext_and_negated_path() {
        let mut pdf = record("/Users/me/Downloads/Invoice.pdf");
        pdf.mtime = Some(1);
        let mut docx = record("/Users/me/Downloads/Invoice.docx");
        docx.mtime = Some(1);
        let mut library_pdf = record("/Users/me/Library/Invoice.pdf");
        library_pdf.mtime = Some(1);
        let mut png = record("/Users/me/Downloads/Invoice.png");
        png.mtime = Some(1);

        let options = SearchOptions {
            query: "ext:pdf|docx !path:Library invoice".to_string(),
            limit: 10,
            ..SearchOptions::default()
        }
        .normalized_at(10);

        assert!(rank_record(&pdf, &options).is_some());
        assert!(rank_record(&docx, &options).is_some());
        assert!(rank_record(&library_pdf, &options).is_none());
        assert!(rank_record(&png, &options).is_none());
    }

    #[test]
    fn path_component_filter_requires_exact_segment() {
        let mut exact = record("/Users/me/Downloads/report.pdf");
        exact.mtime = Some(1);
        let mut partial = record("/Users/me/MyDownloads/report.pdf");
        partial.mtime = Some(1);

        let options = SearchOptions {
            query: "part:Downloads report".to_string(),
            limit: 10,
            ..SearchOptions::default()
        }
        .normalized_at(10);

        assert!(rank_record(&exact, &options).is_some());
        assert!(rank_record(&partial, &options).is_none());
    }

    #[test]
    fn quoted_filter_values_preserve_spaces() {
        let mut exact = record("/Users/me/Library/Application Support/report.pdf");
        exact.mtime = Some(1);
        let mut other = record("/Users/me/Library/Application/report.pdf");
        other.mtime = Some(1);

        let options = SearchOptions {
            query: "path:\"Application Support\" report".to_string(),
            limit: 10,
            ..SearchOptions::default()
        }
        .normalized_at(10);

        assert!(rank_record(&exact, &options).is_some());
        assert!(rank_record(&other, &options).is_none());
    }

    #[test]
    fn sqlite_fuzzy_search_uses_candidates() {
        let temp = unique_temp_dir("macevery-fuzzy-test");
        fs::create_dir_all(&temp).unwrap();
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        db.upsert_file(&record("/tmp/sqlite_result_code.h"))
            .unwrap();
        db.upsert_file(&record("/tmp/LeetCode 101.pdf")).unwrap();

        let results = search(
            &db,
            &SearchOptions {
                query: "~leetcode".to_string(),
                limit: 10,
                ..SearchOptions::default()
            },
        )
        .unwrap();

        assert!(results
            .iter()
            .any(|result| result.record.basename == "LeetCode 101.pdf"));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn prepared_search_and_owned_candidates_match_full_sort_reference() {
        let records = test_reference::records();
        for mut options in test_reference::options() {
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
                let expected = test_reference::search_records(&records, &options);
                assert_eq!(
                    search_records(&records, &options),
                    expected,
                    "memory {options:?}"
                );
                let normalized = options.normalized();
                assert_eq!(
                    rank_records(records.clone(), &normalized, effective_limit(limit)),
                    expected,
                    "owned candidates {options:?}"
                );
            }
        }
    }

    #[test]
    fn public_single_record_ranking_retains_unprepared_options_semantics() {
        let records = test_reference::records();
        for options in test_reference::options() {
            for record in &records {
                assert_eq!(
                    rank_record(record, &options),
                    test_reference::rank_view(&RecordView::from(record), &options),
                    "record={} options={options:?}",
                    record.path
                );
            }
        }
    }

    #[test]
    fn top_k_preserves_input_order_for_equal_sort_keys() {
        let mut records = (0..128)
            .map(|i| {
                let mut record = record("/tmp/report");
                record.id = 1000 - i;
                record.mtime = Some(42);
                record
            })
            .collect::<Vec<_>>();
        for _ in 0..2 {
            for limit in [1, 63, 64, 65, 128, usize::MAX] {
                let options = SearchOptions {
                    query: "report".into(),
                    limit,
                    ..Default::default()
                };
                let actual = search_records(&records, &options);
                assert_eq!(
                    actual.iter().map(|hit| hit.record.id).collect::<Vec<_>>(),
                    records
                        .iter()
                        .take(limit)
                        .map(|record| record.id)
                        .collect::<Vec<_>>()
                );
            }
            records.reverse();
        }
    }

    #[test]
    fn cached_finders_match_str_find_including_repetitive_unicode_inputs() {
        let mut state = 0x6789_0123_4567_abcd_u64;
        let alphabet = [
            "a", "b", "c", "A", "Σ", "ς", "İ", "\u{307}", "报", "📷", "/", "\0",
        ];
        let mut generate = |len: usize| {
            let mut text = String::new();
            for _ in 0..len {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push_str(alphabet[(state >> 32) as usize % alphabet.len()]);
            }
            text
        };
        for round in 0..1500 {
            let haystack = generate(round % 137);
            let independent = generate(round % 25);
            let boundary = haystack
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(haystack.len()))
                .collect::<Vec<_>>();
            let from = boundary[round % boundary.len()];
            let to = boundary[(round * 7) % boundary.len()];
            for needle in [
                independent.as_str(),
                &haystack[from.min(to)..from.max(to)],
                "",
                "aaaaaaaaaaaaaab",
            ] {
                let finder = Finder::new(needle.into());
                assert_eq!(
                    finder.find(&haystack),
                    haystack.find(needle),
                    "{haystack:?} / {needle:?}"
                );
                if finder.skip.is_some() {
                    assert_eq!(
                        finder.linear_find(haystack.as_bytes()),
                        haystack.find(needle)
                    );
                }
            }
        }
        // Horspool otherwise degenerates on this input: exercise the linear guard.
        let haystack = "a".repeat(100_000);
        let needle = format!("{}b{}", "a".repeat(64), "a".repeat(64));
        assert_eq!(
            Finder::new(needle.clone()).find(&haystack),
            haystack.find(&needle)
        );
        assert_eq!(
            Finder::new(format!("{}b", "a".repeat(64))).find(&(haystack + "b")),
            Some(99_936)
        );
    }

    #[test]
    fn short_finders_preserve_first_offsets_at_every_word_and_utf8_boundary() {
        let needles = [
            "a", "/", "\0", "ab", "aa", "\0\0", "Σ", "abc", "aba", "aa\0", "报", "i\u{307}",
            "code", "aaaa", "a\0ba", "📷", "ΣΣ", "coder", "ababa", "Σabc", "报ab", "\0報a",
            "report", "aaaaaa", "报告", "ΣΣΣ", "reportx", "aaaaaaa", "📷abc", "報Σab",
        ];
        let mut seen_lengths = [false; 8];
        for needle in needles {
            assert!((1..=7).contains(&needle.len()));
            seen_lengths[needle.len()] = true;
            let finder = Finder::new(needle.into());
            for offset in 0..34 {
                for separator in ["", "x", "\0", "Σ", "📷"] {
                    let haystack =
                        format!("{}{needle}{separator}{needle}tail", "\u{1}".repeat(offset));
                    for end in haystack
                        .char_indices()
                        .map(|(index, _)| index)
                        .chain(std::iter::once(haystack.len()))
                    {
                        let text = &haystack[..end];
                        assert_eq!(
                            finder.find(text),
                            text.find(needle),
                            "needle={needle:?} offset={offset} end={end}"
                        );
                    }
                }
            }
        }
        assert!(seen_lengths[1..].iter().all(|seen| *seen));
        for (haystack, needle) in [
            ("aaaaaaaabaaaa", "aaab"),
            ("ababababababa", "ababa"),
            ("\0\u{1}\u{1}\0\u{1}\u{1}abcdef", "\0\u{1}\u{1}"),
            ("📷📷📷报告📷", "📷报"),
        ] {
            assert_eq!(
                Finder::new(needle.into()).find(haystack),
                haystack.find(needle)
            );
        }
    }

    #[test]
    fn short_anchor_filters_cannot_drop_matches_with_zero_borrows_or_high_bytes() {
        let mut state = 0xbac8_0402_0100_ff81_u64;
        for turn in 0..10_000 {
            let mut next_byte = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Extra zero/one bytes exercise borrow-generated candidates.
                [0, 1, 0x7f, 0x80, 0xff, state as u8][state as usize % 6]
            };
            let mut haystack = (0..turn % 83).map(|_| next_byte()).collect::<Vec<_>>();
            let len = 2 + turn % 6;
            let needle = (0..len).map(|_| next_byte()).collect::<Vec<_>>();
            if haystack.len() >= len && turn % 2 == 0 {
                let offset = turn % (haystack.len() - len + 1);
                haystack[offset..offset + len].copy_from_slice(&needle);
            }
            let finder = ShortFinder::new(&needle);
            let actual = match len {
                2 => finder.find_bytes::<2>(&haystack),
                3 => finder.find_bytes::<3>(&haystack),
                4 => finder.find_bytes::<4>(&haystack),
                5 => finder.find_bytes::<5>(&haystack),
                6 => finder.find_bytes::<6>(&haystack),
                7 => finder.find_bytes::<7>(&haystack),
                _ => unreachable!(),
            };
            assert_eq!(
                actual,
                haystack.windows(len).position(|part| part == needle),
                "turn={turn} haystack={haystack:?} needle={needle:?}"
            );
        }
    }

    #[test]
    fn short_query_full_results_match_the_independent_legacy_scorer() {
        let mut records = test_reference::records();
        for (i, path) in [
            "/code",
            "/tmp/code",
            "/tmp/codecode/codecode",
            "/tmp/codex",
            "/tmp/xcode",
            "/tmp/xxcodecode",
            "/tmp/报告/code📷",
            "/tmp/📷/Σ/code",
            "/tmp/ΣΣ",
            "/tmp/报告",
            "/tmp/aaaaaaa",
            "/tmp/abababa",
            "/tmp/a\0code\0📷abc",
        ]
        .iter()
        .enumerate()
        {
            let mut record = record(path);
            record.id = 5000 + i as i64;
            record.mtime = Some((i % 3) as i64);
            records.push(record);
        }
        for needle in [
            "a", "ab", "abc", "code", "coder", "report", "reportx", "Σ", "报告", "📷", "📷abc",
            "\0",
        ] {
            for query in [
                needle.to_string(),
                format!("*{needle}*"),
                format!("{needle}*"),
                format!("*{needle}"),
            ] {
                for path_only in [false, true] {
                    for fuzzy in [false, true] {
                        for limit in [0, 1, 3, 17, usize::MAX] {
                            let options = SearchOptions {
                                query: query.clone(),
                                path_only,
                                fuzzy,
                                limit,
                                ..Default::default()
                            };
                            assert_eq!(
                                search_records(&records, &options),
                                test_reference::search_records(&records, &options),
                                "{options:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn prepared_globs_match_frozen_byte_glob_scores_exhaustively() {
        fn words(alphabet: &[&str], max_len: usize) -> Vec<String> {
            let mut words = vec![String::new()];
            let mut level = vec![String::new()];
            for _ in 0..max_len {
                level = level
                    .iter()
                    .flat_map(|prefix| alphabet.iter().map(move |ch| format!("{prefix}{ch}")))
                    .collect();
                words.extend(level.iter().cloned());
            }
            words
        }
        let haystacks = words(&["a", "b", "Σ", "📷"], 3);
        let patterns = words(&["a", "b", "*", "?", "Σ"], 4);
        for pattern in patterns.into_iter().filter(|value| has_glob_syntax(value)) {
            let prepared = PreparedGlob::new(pattern.clone());
            for haystack in &haystacks {
                assert_eq!(
                    prepared.score(haystack),
                    test_reference::glob_score(haystack, &pattern),
                    "{haystack:?} / {pattern:?}"
                );
            }
        }
        for pattern in [
            "*longmissingliteral*",
            "**report**",
            "report***",
            "***report",
            "*报告📷报告*",
        ] {
            let prepared = PreparedGlob::new(pattern.into());
            for haystack in [
                "",
                "report",
                "report-report",
                "prefixreport",
                "reportlast",
                "报告📷报告/报告📷报告",
            ] {
                assert_eq!(
                    prepared.score(haystack),
                    test_reference::glob_score(haystack, pattern)
                );
            }
        }
    }

    #[test]
    fn path_views_compare_concatenated_bytes_at_every_split_including_inside_utf8() {
        let paths = [
            "",
            "/",
            "/ABC/zxxx",
            "/ABC/x/aa",
            "/ABC/",
            "/ABC/x",
            "报告/Σ/📷",
            "报告/Σ/📷a",
            "a\0b",
            "a\0a",
        ];
        for left in paths {
            for right in paths {
                for l in 0..=left.len() {
                    for r in 0..=right.len() {
                        let view = PathView {
                            prefix: &left.as_bytes()[..l],
                            suffix: &left.as_bytes()[l..],
                        };
                        let other = PathView {
                            prefix: &right.as_bytes()[..r],
                            suffix: &right.as_bytes()[r..],
                        };
                        assert_eq!(
                            view.cmp(&other),
                            left.cmp(right),
                            "{left:?}@{l} / {right:?}@{r}"
                        );
                        assert_eq!(view == other, left == right);
                        assert_eq!(view.len(), left.len());
                    }
                }
            }
        }
    }

    #[test]
    fn split_path_heap_uses_full_byte_order_and_stable_index_ties() {
        let paths = [
            "/ABC/zxxx",
            "/ABC/x/aa",
            "/ABC/xaaa",
            "/ABC/x/aa",
            "/ABC/aaaa",
            "/ABC/x/ab",
        ];
        let splits = [5, 7, 0, 5, 9, 0];
        for limit in [1, 2, 3, 6, usize::MAX] {
            let mut hits = TopHits::new(limit);
            for (index, path) in paths.iter().enumerate() {
                let (prefix, suffix) = path.split_at(splits[index]);
                hits.consider(index, 1, Some(99), path.len(), || {
                    PathView::new(prefix, suffix)
                });
            }
            let actual = hits
                .finish()
                .into_iter()
                .map(|hit| hit.index)
                .collect::<Vec<_>>();
            let mut expected = (0..paths.len()).collect::<Vec<_>>();
            expected.sort_by(|left, right| {
                paths[*left]
                    .len()
                    .cmp(&paths[*right].len())
                    .then_with(|| paths[*left].cmp(paths[*right]))
            });
            expected.truncate(limit);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn lazy_hit_path_is_only_read_for_candidates_with_a_competitive_prefix() {
        let mut hits = TopHits::new(1);
        hits.consider(0, 1, Some(100), 4, || "same".into());
        hits.consider(1, 2, Some(100), 4, || {
            panic!("worse score should not read path")
        });
        hits.consider(1, 1, Some(99), 4, || {
            panic!("older time should not read path")
        });
        hits.consider(1, 1, Some(100), 5, || {
            panic!("longer path should not read path")
        });
        hits.consider(2, 1, Some(100), 4, || "same".into());
        assert_eq!(hits.finish()[0].index, 0);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}

// Keep the previous per-record scorer and complete stable sort as an oracle.
// It deliberately does not call PreparedQuery or the bounded candidate heap.
#[cfg(test)]
pub(crate) mod test_reference {
    use super::*;
    use crate::model::FileKind;
    use std::borrow::Cow;

    fn record_matches_filters(record: &RecordView<'_>, options: &SearchOptions) -> bool {
        if !options.ext_filters.is_empty()
            && !record
                .ext_lower
                .map(|ext| options.ext_filters.iter().any(|filter| filter == ext))
                .unwrap_or(false)
        {
            return false;
        }

        if record
            .ext_lower
            .map(|ext| {
                options
                    .excluded_ext_filters
                    .iter()
                    .any(|filter| filter == ext)
            })
            .unwrap_or(false)
        {
            return false;
        }

        if !options.kind_filters.is_empty() && !options.kind_filters.contains(record.kind) {
            return false;
        }

        if options.excluded_kind_filters.contains(record.kind) {
            return false;
        }

        if let Some(modified_after) = options.modified_after {
            if !record
                .mtime
                .map(|mtime| mtime >= modified_after)
                .unwrap_or(false)
            {
                return false;
            }
        }

        if let Some(modified_before) = options.modified_before {
            if record
                .mtime
                .map(|mtime| mtime >= modified_before)
                .unwrap_or(false)
            {
                return false;
            }
        }

        for path_filter in &options.path_filters {
            if !path_filter_matches(record.path_lower, path_filter) {
                return false;
            }
        }

        for path_filter in &options.excluded_path_filters {
            if path_filter_matches(record.path_lower, path_filter) {
                return false;
            }
        }

        true
    }

    fn path_filter_matches(path: &str, filter: &PathFilter) -> bool {
        match filter.mode {
            PathFilterMode::Contains => {
                if has_glob_syntax(&filter.value) {
                    glob_match(path.as_bytes(), filter.value.as_bytes())
                } else {
                    path.contains(&filter.value)
                }
            }
            PathFilterMode::Component => {
                path.split('/')
                    .filter(|value| !value.is_empty())
                    .any(|component| {
                        if has_glob_syntax(&filter.value) {
                            glob_match(component.as_bytes(), filter.value.as_bytes())
                        } else {
                            component == filter.value
                        }
                    })
            }
        }
    }

    fn rank_text(haystack: &str, needle: &str, base: i64) -> Option<i64> {
        fuzzy_score(haystack, needle).map(|value| base + value)
    }

    fn fuzzy_score(haystack: &str, needle: &str) -> Option<i64> {
        if needle.is_empty() {
            return Some(0);
        }

        let mut last_index: Option<i64> = None;
        let mut gaps = 0i64;
        let mut start = 0i64;
        let mut needle_chars = needle.chars();
        let mut current = needle_chars.next()?;

        for (index, ch) in haystack.chars().enumerate() {
            if ch == current {
                if let Some(last) = last_index {
                    gaps += (index as i64 - last - 1).max(0);
                } else {
                    start = index as i64;
                }
                last_index = Some(index as i64);
                if let Some(next) = needle_chars.next() {
                    current = next;
                } else {
                    return Some(start * 3 + gaps * 5 + needle.len() as i64);
                }
            }
        }
        None
    }

    fn has_glob_syntax(value: &str) -> bool {
        value.contains('*') || value.contains('?')
    }

    pub(super) fn glob_score(haystack: &str, pattern: &str) -> Option<i64> {
        let start_penalty = first_literal_offset(haystack, pattern).unwrap_or(0) as i64;
        if glob_match(haystack.as_bytes(), pattern.as_bytes()) {
            Some(start_penalty * 2 + pattern.len() as i64)
        } else {
            None
        }
    }

    fn first_literal_offset(haystack: &str, pattern: &str) -> Option<usize> {
        let literal = pattern.split(['*', '?']).find(|value| !value.is_empty())?;
        haystack.find(literal)
    }

    fn glob_match(haystack: &[u8], pattern: &[u8]) -> bool {
        let (mut h, mut p) = (0usize, 0usize);
        let mut star = None;
        let mut match_after_star = 0usize;

        while h < haystack.len() {
            if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == haystack[h]) {
                h += 1;
                p += 1;
            } else if p < pattern.len() && pattern[p] == b'*' {
                star = Some(p);
                match_after_star = h;
                p += 1;
            } else if let Some(star_index) = star {
                p = star_index + 1;
                match_after_star += 1;
                h = match_after_star;
            } else {
                return false;
            }
        }

        while p < pattern.len() && pattern[p] == b'*' {
            p += 1;
        }

        p == pattern.len()
    }

    pub(crate) fn search_records(
        records: &[FileRecord],
        options: &SearchOptions,
    ) -> Vec<SearchResult> {
        let options = options.normalized();
        let mut results = records
            .iter()
            .filter_map(|record| {
                rank_view(&RecordView::from(record), &options).map(|score| SearchResult {
                    record: record.clone(),
                    score,
                })
            })
            .collect::<Vec<_>>();
        results.sort_by(|a, b| {
            a.score
                .cmp(&b.score)
                .then_with(|| b.record.mtime.cmp(&a.record.mtime))
                .then_with(|| a.record.path.len().cmp(&b.record.path.len()))
                .then_with(|| a.record.path.cmp(&b.record.path))
        });
        results.truncate(if options.limit == 0 {
            50
        } else {
            options.limit
        });
        results
    }

    pub(crate) fn rank_view(record: &RecordView<'_>, options: &SearchOptions) -> Option<i64> {
        if !record_matches_filters(record, options) {
            return None;
        }

        let tokens = options.terms();
        if tokens.is_empty() {
            return Some(10_000 - record.mtime.unwrap_or(0).min(9_000));
        }

        let case_sensitive = false;
        let basename =
            comparable_record_text(record.basename, record.basename_lower, case_sensitive);
        let path = comparable_record_text(record.path, record.path_lower, case_sensitive);
        let mut score = 0;

        for token in tokens {
            let is_glob = has_glob_syntax(&token);
            let token = comparable_query_text(&token, case_sensitive);
            let token_score = if is_glob && options.path_only {
                glob_score(&path, &token).map(|value| value + 450)
            } else if is_glob {
                glob_score(&basename, &token)
                    .map(|value| value + 180)
                    .or_else(|| glob_score(&path, &token).map(|value| value + 520))
            } else if options.path_only {
                path.find(token.as_ref())
                    .map(|index| 300 + index.min(500) as i64)
            } else if basename == token {
                Some(0)
            } else if basename.starts_with(token.as_ref()) {
                Some(100 + token.len() as i64)
            } else if let Some(index) = basename.find(token.as_ref()) {
                Some(250 + index as i64)
            } else if let Some(index) = path.find(token.as_ref()) {
                Some(550 + index.min(500) as i64)
            } else if options.fuzzy_enabled() {
                rank_text(&basename, &token, 0)
                    .map(|value| value + 1_000)
                    .or_else(|| rank_text(&path, &token, 500).map(|value| value + 1_400))
            } else {
                None
            };

            match token_score {
                Some(value) => score += value,
                None => return None,
            }
        }

        score += (record.path.len() as i64).min(500);
        Some(score)
    }

    fn comparable_record_text<'a>(
        original: &'a str,
        lowered: &'a str,
        case_sensitive: bool,
    ) -> Cow<'a, str> {
        if case_sensitive {
            Cow::Borrowed(original)
        } else {
            Cow::Borrowed(lowered)
        }
    }

    fn comparable_query_text(value: &str, case_sensitive: bool) -> Cow<'_, str> {
        if case_sensitive {
            Cow::Borrowed(value)
        } else {
            Cow::Owned(value.to_lowercase())
        }
    }

    pub(crate) fn records() -> Vec<FileRecord> {
        let names = [
            "report",
            "Report.PDF",
            "report-final.txt",
            "sqlite_result_code.h",
            "LeetCode 101.pdf",
            "报告.PDF",
            "İstanbul.txt",
            "Straße.md",
            "RÉSUMÉ.pdf",
            "ΟΣ.txt",
            "αβγ.txt",
            "📷 camera.jpg",
            "unrelated.rs",
        ];
        let parents = [
            "/tmp",
            "/Users/me/Downloads",
            "/Users/me/Library/Application Support",
            "/Users/me/MyDownloads",
            "/Users/me/报告",
        ];
        let mut records = (0..260)
            .map(|i| {
                let kind = match i % 5 {
                    0 => FileKind::File,
                    1 => FileKind::Directory,
                    2 => FileKind::Symlink,
                    3 => FileKind::App,
                    _ => FileKind::Other,
                };
                let mut record = FileRecord::new(
                    format!(
                        "{}/{}/{}",
                        parents[(i / 13) % parents.len()],
                        i % 7,
                        names[i % names.len()]
                    ),
                    kind,
                    1000 + i as i64,
                );
                record.id = i as i64 + 1;
                record.mtime = match i % 6 {
                    0 => None,
                    1 => Some(-1),
                    2 => Some(0),
                    3 => Some(500),
                    4 => Some(9000),
                    _ => Some(1_700_000_000),
                };
                record.size = Some(i as i64 * 17);
                record.ctime = Some(i as i64);
                record.dev = Some((i % 2) as i64);
                record.inode = Some(i as i64 + 1234);
                record
            })
            .collect::<Vec<_>>();
        // Same score, mtime, path length and path; only input order may break ties.
        for id in [900, 42, 701, 2, 500] {
            let mut record = FileRecord::new("/tmp/report".into(), FileKind::File, id);
            record.id = id;
            record.mtime = Some(9000);
            records.insert(7, record);
        }
        records
    }

    pub(crate) fn options() -> Vec<SearchOptions> {
        let mut cases = [
            "",
            "report",
            "REPORT",
            "report pdf",
            "~rpt",
            "~leetcode",
            "*port*",
            "*",
            "?",
            "*.PDF",
            "报告",
            "İ",
            "i\u{307}",
            "STRASSE",
            "Straße",
            "RÉSUMÉ",
            "ΟΣ",
            "α",
            "📷",
            "missing-query",
            "kind:app",
            "kind:folder report",
            "ext:PDF|txt report",
            "!ext:pdf report",
            "!kind:file report",
            "part:Downloads report",
            "path:downloads report",
            "!path:Library report",
            "path:\"Application Support\" report",
            r#"report \"final\""#,
            "report ~pdf",
        ]
        .into_iter()
        .map(|query| SearchOptions {
            query: query.into(),
            ..Default::default()
        })
        .collect::<Vec<_>>();
        for query in ["report", "*port*", "~rpt", "报告"] {
            cases.push(SearchOptions {
                query: query.into(),
                path_only: true,
                ..Default::default()
            });
        }
        cases.push(SearchOptions {
            query: "rpt".into(),
            fuzzy: true,
            ..Default::default()
        });
        cases.push(SearchOptions {
            query: "report".into(),
            ext: Some("PDF".into()),
            ..Default::default()
        });
        cases.push(SearchOptions {
            query: "*".into(),
            kind: Some(FileKind::Symlink),
            ..Default::default()
        });
        cases.push(SearchOptions {
            query: "report".into(),
            modified_after: Some(500),
            modified_before: Some(9001),
            ..Default::default()
        });
        cases.push(SearchOptions {
            query: "*".into(),
            excluded_path_filters: vec![PathFilter {
                value: "downloads".into(),
                mode: PathFilterMode::Component,
            }],
            ..Default::default()
        });
        cases.push(SearchOptions {
            query: "*".into(),
            excluded_path_filters: vec![PathFilter {
                value: "*library*".into(),
                mode: PathFilterMode::Contains,
            }],
            ..Default::default()
        });
        cases
    }
}
