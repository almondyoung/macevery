use crate::db::Database;
use crate::error::Result;
use crate::model::{FileRecord, PathFilter, PathFilterMode, SearchOptions, SearchResult};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::env;

const DEFAULT_LIMIT: usize = 50;
const MAX_CANDIDATES: usize = 20_000;
const DEFAULT_MAX_FUZZY_CANDIDATES: usize = 120_000;

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
    if options.fuzzy_enabled() {
        let limit = effective_limit(options.limit);
        let max_candidates = fuzzy_candidate_limit(limit);
        let mut candidates = db.candidate_records(&options, max_candidates)?;
        if candidates.is_empty() {
            candidates = db.filtered_records(&options, max_candidates)?;
        }
        return Ok(rank_records(candidates, &options, limit));
    }

    let limit = effective_limit(options.limit);
    let max_candidates = MAX_CANDIDATES.max(limit.saturating_mul(80));
    Ok(rank_records(
        db.candidate_records(&options, max_candidates)?,
        &options,
        limit,
    ))
}

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

#[derive(Eq, PartialEq)]
struct RankedHit<'a> {
    index: usize,
    score: i64,
    mtime: Option<i64>,
    path: &'a str,
}

impl Ord for RankedHit<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.mtime.cmp(&self.mtime))
            .then_with(|| self.path.len().cmp(&other.path.len()))
            .then_with(|| self.path.cmp(other.path))
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
        path: impl FnOnce() -> &'a str,
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
                record.path
            });
        }
    }
    hits.finish()
}

fn fuzzy_candidate_limit(limit: usize) -> usize {
    env::var("MACEVERY_FUZZY_CANDIDATES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_FUZZY_CANDIDATES)
        .max(limit.saturating_mul(200))
}

pub fn rank_record(record: &FileRecord, options: &SearchOptions) -> Option<i64> {
    rank_view(&RecordView::from(record), options)
}

pub fn rank_view(record: &RecordView<'_>, options: &SearchOptions) -> Option<i64> {
    PreparedQuery::new(options).rank(record)
}

// Rust's short-string search remains useful for the common few-character query.
// Longer patterns reuse one Horspool table per request, with a linear fallback
// on highly repetitive inputs instead of a per-record pattern construction.
struct Finder {
    value: String,
    skip: Option<Box<[usize; 256]>>,
    prefix: Vec<usize>,
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
        Self {
            value,
            skip,
            prefix,
        }
    }

    fn find(&self, haystack: &str) -> Option<usize> {
        let Some(skip) = &self.skip else {
            return haystack.find(&self.value);
        };
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
    fn lazy_hit_path_is_only_read_for_candidates_with_a_competitive_prefix() {
        let mut hits = TopHits::new(1);
        hits.consider(0, 1, Some(100), 4, || "same");
        hits.consider(1, 2, Some(100), 4, || {
            panic!("worse score should not read path")
        });
        hits.consider(1, 1, Some(99), 4, || {
            panic!("older time should not read path")
        });
        hits.consider(1, 1, Some(100), 5, || {
            panic!("longer path should not read path")
        });
        hits.consider(2, 1, Some(100), 4, || "same");
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
