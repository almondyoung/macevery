use crate::config::{default_db_path, default_excludes, expand_tilde};
use crate::db::Database;
use crate::error::{MacEveryError, Result};
use crate::macos;
use crate::model::{FileKind, IndexStats, SearchOptions, SearchResult};
use crate::scanner::{index_paths, IndexRequest};
use crate::search;
use crate::server;
use crate::watch;
use std::fs;
use std::path::PathBuf;

pub fn run<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter().collect::<Vec<_>>();
    if args.is_empty() {
        return usage();
    }
    let _program = args.remove(0);
    let Some(command) = args.first().cloned() else {
        return usage();
    };
    args.remove(0);

    match command.as_str() {
        "index" => cmd_index(args),
        "search" => cmd_search(args),
        "open" => cmd_open_or_reveal(args, false),
        "reveal" => cmd_open_or_reveal(args, true),
        "status" => cmd_status(args),
        "clean" => cmd_clean(args),
        "watch" => cmd_watch(args),
        "serve" => cmd_serve(args),
        "-h" | "--help" | "help" => usage(),
        _ => Err(MacEveryError::Cli(format!(
            "unknown command '{command}'\n\n{}",
            usage_text()
        ))),
    }
}

fn cmd_index(args: Vec<String>) -> Result<()> {
    let mut rebuild = false;
    let mut excludes = default_excludes();
    let mut roots = Vec::new();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--rebuild" => rebuild = true,
            "--exclude" => {
                let Some(pattern) = iter.next() else {
                    return Err(MacEveryError::Cli(
                        "--exclude requires a pattern".to_string(),
                    ));
                };
                excludes.push(pattern);
            }
            "-h" | "--help" => {
                print!("{}", usage_text());
                return Ok(());
            }
            _ if arg.starts_with("--exclude=") => {
                excludes.push(arg.trim_start_matches("--exclude=").to_string());
            }
            _ if arg.starts_with('-') => {
                return Err(MacEveryError::Cli(format!("unknown index option '{arg}'")));
            }
            _ => roots.push(expand_tilde(&arg)),
        }
    }

    if roots.is_empty() {
        return Err(MacEveryError::Cli(
            "index requires at least one root path".to_string(),
        ));
    }

    let db = Database::open(default_db_path())?;
    let summary = index_paths(
        &db,
        &IndexRequest {
            roots,
            excludes,
            rebuild,
        },
    )?;
    println!("indexed {} entries", summary.indexed);
    for warning in summary.warnings.iter().take(25) {
        eprintln!("warning: {warning}");
    }
    if summary.warnings.len() > 25 {
        eprintln!(
            "warning: {} additional indexing warnings omitted",
            summary.warnings.len() - 25
        );
    }
    Ok(())
}

fn cmd_search(args: Vec<String>) -> Result<()> {
    let (options, json) = parse_search_args(args)?;
    let db = Database::open(default_db_path())?;
    let results = search::search(&db, &options)?;
    if json {
        print_search_json(&results);
    } else {
        print_search_table(&results);
    }
    Ok(())
}

fn cmd_open_or_reveal(args: Vec<String>, reveal: bool) -> Result<()> {
    let (options, _json) = parse_search_args(args)?;
    let db = Database::open(default_db_path())?;
    let mut options = options;
    options.limit = 1;
    let result = search::search(&db, &options)?
        .into_iter()
        .next()
        .ok_or_else(|| MacEveryError::Cli("no matching indexed file found".to_string()))?;

    if reveal {
        macos::reveal_path(&result.record.path)
    } else {
        macos::open_path(&result.record.path)
    }
}

fn cmd_status(args: Vec<String>) -> Result<()> {
    let json = args.iter().any(|arg| arg == "--json");
    let db = Database::open(default_db_path())?;
    let stats = db.stats()?;
    if json {
        print_status_json(&stats);
    } else {
        println!("database: {}", stats.db_path);
        println!("files: {}", stats.files);
        println!("directories: {}", stats.dirs);
        println!("apps: {}", stats.apps);
        println!("symlinks: {}", stats.symlinks);
        println!("other: {}", stats.others);
        println!(
            "last indexed: {}",
            stats
                .last_indexed_at
                .map(|value| value.to_string())
                .unwrap_or_else(|| "never".to_string())
        );
        println!("roots:");
        for root in &stats.roots {
            println!("  {root}");
        }
        println!("excludes:");
        for exclude in &stats.excludes {
            println!("  {exclude}");
        }
    }
    Ok(())
}

fn cmd_clean(args: Vec<String>) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{}", usage_text());
        return Ok(());
    }
    let path = default_db_path();
    let wal = sidecar_path(&path, "wal");
    let shm = sidecar_path(&path, "shm");
    for path in [path, wal, shm] {
        if path.exists() {
            fs::remove_file(&path)?;
            println!("removed {}", path.to_string_lossy());
        }
    }
    Ok(())
}

fn cmd_watch(args: Vec<String>) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{}", usage_text());
        return Ok(());
    }
    watch::watch_index(default_db_path())
}

fn cmd_serve(args: Vec<String>) -> Result<()> {
    let mut addr = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--addr" => {
                let Some(value) = iter.next() else {
                    return Err(MacEveryError::Cli("--addr requires host:port".to_string()));
                };
                addr = Some(value);
            }
            "-h" | "--help" => {
                print!("{}", usage_text());
                return Ok(());
            }
            _ if arg.starts_with("--addr=") => {
                addr = Some(arg["--addr=".len()..].to_string());
            }
            _ => return Err(MacEveryError::Cli(format!("unknown serve option '{arg}'"))),
        }
    }
    server::serve(addr)
}

fn parse_search_args(args: Vec<String>) -> Result<(SearchOptions, bool)> {
    let mut query_parts = Vec::new();
    let mut limit = 50usize;
    let mut json = false;
    let mut ext = None;
    let mut kind = None;
    let mut path_only = false;
    let mut fuzzy = false;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--path" => path_only = true,
            "--fuzzy" => fuzzy = true,
            "--limit" => {
                let Some(value) = iter.next() else {
                    return Err(MacEveryError::Cli("--limit requires a number".to_string()));
                };
                limit = value
                    .parse()
                    .map_err(|_| MacEveryError::Cli("--limit must be a number".to_string()))?;
            }
            "--ext" => {
                let Some(value) = iter.next() else {
                    return Err(MacEveryError::Cli("--ext requires a value".to_string()));
                };
                ext = Some(value.trim_start_matches('.').to_lowercase());
            }
            "--kind" => {
                let Some(value) = iter.next() else {
                    return Err(MacEveryError::Cli("--kind requires a value".to_string()));
                };
                kind = Some(FileKind::from_str(&value).ok_or_else(|| {
                    MacEveryError::Cli(
                        "--kind must be file, dir, symlink, app, or other".to_string(),
                    )
                })?);
            }
            _ if arg.starts_with("--limit=") => {
                limit = arg["--limit=".len()..]
                    .parse()
                    .map_err(|_| MacEveryError::Cli("--limit must be a number".to_string()))?;
            }
            _ if arg.starts_with("--ext=") => {
                ext = Some(arg["--ext=".len()..].trim_start_matches('.').to_lowercase());
            }
            _ if arg.starts_with("--kind=") => {
                let value = &arg["--kind=".len()..];
                kind = Some(FileKind::from_str(value).ok_or_else(|| {
                    MacEveryError::Cli(
                        "--kind must be file, dir, symlink, app, or other".to_string(),
                    )
                })?);
            }
            _ if arg.starts_with('-') => {
                return Err(MacEveryError::Cli(format!("unknown search option '{arg}'")));
            }
            _ => query_parts.push(arg),
        }
    }

    let query = query_parts.join(" ");
    if query.trim().is_empty() {
        return Err(MacEveryError::Cli("search query is required".to_string()));
    }

    Ok((
        SearchOptions {
            query,
            limit,
            ext,
            kind,
            path_only,
            fuzzy,
            ..SearchOptions::default()
        },
        json,
    ))
}

fn print_search_table(results: &[SearchResult]) {
    for result in results {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            result.record.path,
            result.record.kind.as_str(),
            result
                .record
                .size
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            result
                .record
                .mtime
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            result.score
        );
    }
}

fn print_search_json(results: &[SearchResult]) {
    println!("[");
    for (index, result) in results.iter().enumerate() {
        let comma = if index + 1 == results.len() { "" } else { "," };
        println!(
            "  {{\"path\":\"{}\",\"basename\":\"{}\",\"kind\":\"{}\",\"size\":{},\"mtime\":{},\"score\":{}}}{}",
            json_escape(&result.record.path),
            json_escape(&result.record.basename),
            result.record.kind.as_str(),
            json_i64(result.record.size),
            json_i64(result.record.mtime),
            result.score,
            comma
        );
    }
    println!("]");
}

fn print_status_json(stats: &IndexStats) {
    println!(
        "{{\"db_path\":\"{}\",\"files\":{},\"dirs\":{},\"apps\":{},\"symlinks\":{},\"others\":{},\"last_indexed_at\":{},\"roots\":{},\"excludes\":{}}}",
        json_escape(&stats.db_path),
        stats.files,
        stats.dirs,
        stats.apps,
        stats.symlinks,
        stats.others,
        json_i64(stats.last_indexed_at),
        json_string_array(&stats.roots),
        json_string_array(&stats.excludes)
    );
}

fn json_i64(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn json_string_array(values: &[String]) -> String {
    let inner = values
        .iter()
        .map(|value| format!("\"{}\"", json_escape(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
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

fn sidecar_path(path: &std::path::Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.to_string_lossy()))
}

fn usage() -> Result<()> {
    print!("{}", usage_text());
    Ok(())
}

fn usage_text() -> &'static str {
    "MacEvery - Everything-like macOS filename/path search\n\n\
Usage:\n\
  macevery index [--rebuild] [--exclude PATTERN...] PATHS...\n\
  macevery search QUERY [--limit N] [--json] [--ext EXT] [--kind file|dir|symlink|app] [--path] [--fuzzy]\n\
  macevery search \"ext:pdf kind:file path:Downloads mtime:7d invoice\"\n\
  macevery open QUERY\n\
  macevery reveal QUERY\n\
  macevery status [--json]\n\
  macevery clean\n\
  macevery watch\n\
  macevery serve [--addr 127.0.0.1:17649]\n"
}
