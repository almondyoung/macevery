use crate::config::{default_db_path, default_excludes};
use crate::db::Database;
use crate::error::{MacEveryError, Result};
use crate::model::SearchOptions;
use crate::scanner::{index_paths, IndexRequest};
use crate::search;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_QUERIES: &[&str] = &["pdf", "kind:dir code", "ext:pdf", "mtime:7d"];

pub struct BenchOptions {
    pub roots: Vec<PathBuf>,
    pub queries_path: Option<PathBuf>,
    pub rebuild: bool,
    pub json: bool,
}

pub fn run(options: BenchOptions) -> Result<()> {
    let report = collect(options)?;
    if report.json {
        print_json(&report);
    } else {
        print_text(&report);
    }
    Ok(())
}

struct BenchReport {
    json: bool,
    entries: i64,
    db_size_bytes: u64,
    indexed_entries: Option<u64>,
    index_time_ms: Option<f64>,
    queries: Vec<QueryBench>,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

struct QueryBench {
    query: String,
    duration_ms: f64,
    results: usize,
}

fn collect(options: BenchOptions) -> Result<BenchReport> {
    let db_path = default_db_path();
    let db = Database::open(db_path.clone())?;
    let mut indexed_entries = None;
    let mut index_time_ms = None;

    if options.rebuild || !options.roots.is_empty() {
        let roots = if options.roots.is_empty() {
            db.stats()?
                .roots
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        } else {
            options.roots.clone()
        };
        if roots.is_empty() {
            return Err(MacEveryError::Cli(
                "bench --rebuild requires roots or an existing index configuration".to_string(),
            ));
        }

        let started = Instant::now();
        let summary = index_paths(
            &db,
            &IndexRequest {
                roots,
                excludes: default_excludes(),
                rebuild: options.rebuild,
            },
        )?;
        index_time_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
        indexed_entries = Some(summary.indexed);
    }

    let queries = load_queries(options.queries_path.as_deref())?;
    let mut query_benches = Vec::new();
    for query in queries {
        let started = Instant::now();
        let results = search::search(
            &db,
            &SearchOptions {
                query: query.clone(),
                limit: 50,
                ..SearchOptions::default()
            },
        )?;
        query_benches.push(QueryBench {
            query,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            results: results.len(),
        });
    }

    let stats = db.stats()?;
    let mut durations = query_benches
        .iter()
        .map(|query| query.duration_ms)
        .collect::<Vec<_>>();

    Ok(BenchReport {
        json: options.json,
        entries: stats.files + stats.dirs + stats.symlinks + stats.apps + stats.others,
        db_size_bytes: db_size_bytes(&db_path),
        indexed_entries,
        index_time_ms,
        p50_ms: percentile(&mut durations, 50.0),
        p95_ms: percentile(&mut durations, 95.0),
        p99_ms: percentile(&mut durations, 99.0),
        queries: query_benches,
    })
}

fn load_queries(path: Option<&Path>) -> Result<Vec<String>> {
    let values = if let Some(path) = path {
        fs::read_to_string(path)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        DEFAULT_QUERIES
            .iter()
            .map(|value| value.to_string())
            .collect()
    };

    if values.is_empty() {
        return Err(MacEveryError::Cli(
            "bench queries file did not contain any queries".to_string(),
        ));
    }
    Ok(values)
}

fn db_size_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        sidecar_path(path, "wal"),
        sidecar_path(path, "shm"),
    ]
    .iter()
    .filter_map(|path| fs::metadata(path).ok())
    .map(|metadata| metadata.len())
    .sum()
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.to_string_lossy()))
}

fn percentile(values: &mut [f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((values.len() - 1) as f64 * percentile / 100.0).ceil() as usize;
    values[rank.min(values.len() - 1)]
}

fn print_text(report: &BenchReport) {
    println!("entries: {}", report.entries);
    println!("db_size: {}", format_bytes(report.db_size_bytes));
    if let Some(indexed) = report.indexed_entries {
        println!("indexed_entries: {indexed}");
    }
    if let Some(ms) = report.index_time_ms {
        println!("index_time: {:.1} ms", ms);
    }
    println!("query_p50: {:.2} ms", report.p50_ms);
    println!("query_p95: {:.2} ms", report.p95_ms);
    println!("query_p99: {:.2} ms", report.p99_ms);
    println!("slowest_queries:");
    for query in slowest_queries(report).into_iter().take(10) {
        println!(
            "  {:.2} ms\t{} results\t{}",
            query.duration_ms, query.results, query.query
        );
    }
}

fn print_json(report: &BenchReport) {
    print!(
        "{{\"entries\":{},\"db_size_bytes\":{},\"indexed_entries\":{},\"index_time_ms\":{},\"query_p50_ms\":{:.3},\"query_p95_ms\":{:.3},\"query_p99_ms\":{:.3},\"queries\":[",
        report.entries,
        report.db_size_bytes,
        json_u64(report.indexed_entries),
        json_f64(report.index_time_ms),
        report.p50_ms,
        report.p95_ms,
        report.p99_ms
    );
    for (index, query) in report.queries.iter().enumerate() {
        if index > 0 {
            print!(",");
        }
        print!(
            "{{\"query\":\"{}\",\"duration_ms\":{:.3},\"results\":{}}}",
            json_escape(&query.query),
            query.duration_ms,
            query.results
        );
    }
    println!("]}}");
}

fn slowest_queries(report: &BenchReport) -> Vec<&QueryBench> {
    let mut queries = report.queries.iter().collect::<Vec<_>>();
    queries.sort_by(|a, b| {
        b.duration_ms
            .partial_cmp(&a.duration_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    queries
}

fn json_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn json_f64(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "null".to_string())
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

fn format_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.1} GiB", mib / 1024.0)
    } else {
        format!("{mib:.1} MiB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_handles_empty_and_sorted_values() {
        assert_eq!(percentile(&mut [], 50.0), 0.0);
        let mut values = vec![10.0, 1.0, 5.0, 20.0];
        assert_eq!(percentile(&mut values, 50.0), 10.0);
        assert_eq!(percentile(&mut values, 95.0), 20.0);
    }
}
