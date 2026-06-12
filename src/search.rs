use crate::db::Database;
use crate::error::Result;
use crate::model::{FileRecord, PathFilter, PathFilterMode, SearchOptions, SearchResult};
use std::borrow::Cow;

const DEFAULT_LIMIT: usize = 50;
const MAX_CANDIDATES: usize = 20_000;

pub fn search(db: &Database, options: &SearchOptions) -> Result<Vec<SearchResult>> {
    let options = options.normalized();
    if options.fuzzy_enabled() {
        return Ok(search_records(&db.all_records()?, &options));
    }

    let limit = if options.limit == 0 {
        DEFAULT_LIMIT
    } else {
        options.limit
    };
    let max_candidates = MAX_CANDIDATES.max(limit.saturating_mul(80));
    let mut results = db
        .candidate_records(&options, max_candidates)?
        .into_iter()
        .filter_map(|record| {
            rank_record(&record, &options).map(|score| SearchResult { record, score })
        })
        .collect::<Vec<_>>();

    results.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| b.record.mtime.cmp(&a.record.mtime))
            .then_with(|| a.record.path.len().cmp(&b.record.path.len()))
            .then_with(|| a.record.path.cmp(&b.record.path))
    });
    results.truncate(limit);
    Ok(results)
}

pub fn search_records(records: &[FileRecord], options: &SearchOptions) -> Vec<SearchResult> {
    let options = options.normalized();
    let limit = if options.limit == 0 {
        DEFAULT_LIMIT
    } else {
        options.limit
    };
    let mut results = records
        .iter()
        .filter_map(|record| {
            rank_record(record, &options).map(|score| SearchResult {
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
    results.truncate(limit);
    results
}

pub fn rank_record(record: &FileRecord, options: &SearchOptions) -> Option<i64> {
    if !record_matches_filters(record, options) {
        return None;
    }

    let tokens = options.terms();
    if tokens.is_empty() {
        return Some(10_000 - record.mtime.unwrap_or(0).min(9_000));
    }

    let case_sensitive = false;
    let basename = comparable_record_text(&record.basename, &record.basename_lower, case_sensitive);
    let path = comparable_record_text(&record.path, &record.path_lower, case_sensitive);
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

fn record_matches_filters(record: &FileRecord, options: &SearchOptions) -> bool {
    if !options.ext_filters.is_empty()
        && !record
            .ext_lower
            .as_ref()
            .map(|ext| options.ext_filters.contains(ext))
            .unwrap_or(false)
    {
        return false;
    }

    if record
        .ext_lower
        .as_ref()
        .map(|ext| options.excluded_ext_filters.contains(ext))
        .unwrap_or(false)
    {
        return false;
    }

    if !options.kind_filters.is_empty() && !options.kind_filters.contains(&record.kind) {
        return false;
    }

    if options.excluded_kind_filters.contains(&record.kind) {
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
        if !path_filter_matches(&record.path_lower, path_filter) {
            return false;
        }
    }

    for path_filter in &options.excluded_path_filters {
        if path_filter_matches(&record.path_lower, path_filter) {
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

fn glob_score(haystack: &str, pattern: &str) -> Option<i64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileKind, FileRecord};

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
}
