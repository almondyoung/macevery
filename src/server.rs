use crate::compact_index::CompactIndex;
use crate::config::default_db_path;
use crate::db::Database;
use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, SearchOptions, SearchResult};
use crate::process_lifetime;
use crate::search;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_ADDR: &str = "127.0.0.1:17649";
const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 2_560 * 1024 * 1024;
const ESTIMATED_RECORD_BYTES: u64 = 900;
const ESTIMATED_COMPACT_RECORD_BYTES: u64 = 420;
const DEFAULT_MAX_ACTIVE_REQUESTS: usize = 64;
const REQUEST_HEADER_LIMIT: usize = 64 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_millis(250);
const REQUEST_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendPreference {
    Auto,
    Memory,
    Compact,
    Sqlite,
}

impl BackendPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Memory => "memory",
            Self::Compact => "compact",
            Self::Sqlite => "sqlite",
        }
    }
}

impl Default for BackendPreference {
    fn default() -> Self {
        Self::Auto
    }
}

impl FromStr for BackendPreference {
    type Err = MacEveryError;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_lowercase().as_str() {
            "auto" | "automatic" => Ok(Self::Auto),
            "memory" | "fast" | "fastest" => Ok(Self::Memory),
            "compact" | "balanced" | "compressed" => Ok(Self::Compact),
            "sqlite" | "sql" | "low-memory" | "low_memory" => Ok(Self::Sqlite),
            _ => Err(MacEveryError::Cli(
                "--backend must be auto, memory, compact, or sqlite".to_string(),
            )),
        }
    }
}

pub fn serve(addr: Option<String>, backend: BackendPreference) -> Result<()> {
    process_lifetime::exit_when_parent_dies_if_requested();

    let addr = addr.unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let db_path = default_db_path();
    let listener = TcpListener::bind(&addr)
        .map_err(|err| MacEveryError::Cli(format!("failed to bind {addr}: {err}")))?;
    eprintln!("macevery search service listening on http://{addr}");

    let state = Arc::new(RwLock::new(ServiceState::load(db_path, backend)?));
    let active_requests = Arc::new(AtomicUsize::new(0));
    let max_active_requests = max_active_requests();
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if !try_acquire_request_slot(&active_requests, max_active_requests) {
                    let _ = write_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        "server busy\n",
                    );
                    continue;
                }
                let state = Arc::clone(&state);
                let guard = ActiveRequestGuard {
                    active: Arc::clone(&active_requests),
                };
                thread::spawn(move || {
                    let _guard = guard;
                    if let Err(err) = handle_stream(stream, state) {
                        eprintln!("serve: request failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("serve: accept failed: {err}"),
        }
    }
    Ok(())
}

struct ActiveRequestGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

enum Backend {
    Memory {
        records: Vec<FileRecord>,
        marker: u128,
    },
    Compact {
        index: CompactIndex,
        marker: u128,
    },
    Sqlite,
}

impl Backend {
    fn mode(&self) -> &'static str {
        match self {
            Self::Memory { .. } => "memory",
            Self::Compact { .. } => "compact",
            Self::Sqlite => "sqlite",
        }
    }

    fn marker(&self) -> Option<u128> {
        match self {
            Self::Memory { marker, .. } | Self::Compact { marker, .. } => Some(*marker),
            Self::Sqlite => None,
        }
    }
}

struct ServiceState {
    db_path: PathBuf,
    requested_backend: BackendPreference,
    backend: Backend,
    records_total: i64,
    estimated_memory_bytes: u64,
    loaded_at: i64,
}

impl ServiceState {
    fn load(db_path: PathBuf, requested_backend: BackendPreference) -> Result<Self> {
        let db = Database::open(db_path.clone())?;
        let stats = db.stats()?;
        let records_total = stats.files + stats.dirs + stats.symlinks + stats.apps + stats.others;
        let full_memory_estimate = estimate_memory_bytes(records_total);
        drop(db);

        let selected = select_backend(requested_backend, records_total);
        let estimated_memory_bytes = estimate_backend_memory_bytes(selected, records_total);
        let backend = match selected {
            BackendPreference::Memory => {
                let db = Database::open(db_path.clone())?;
                let records = db.all_records()?;
                let marker = db_marker(&db_path);
                eprintln!(
                    "serve: loaded {} indexed records into memory ({})",
                    records.len(),
                    format_bytes(full_memory_estimate)
                );
                Backend::Memory { records, marker }
            }
            BackendPreference::Compact => {
                let db = Database::open(db_path.clone())?;
                let index = CompactIndex::load(&db)?;
                let marker = db_marker(&db_path);
                eprintln!(
                    "serve: loaded {} indexed records into balanced index ({})",
                    index.len(),
                    format_bytes(estimated_memory_bytes)
                );
                Backend::Compact { index, marker }
            }
            BackendPreference::Sqlite | BackendPreference::Auto => {
                eprintln!(
                    "serve: using SQLite-backed search for {} indexed records",
                    records_total
                );
                Backend::Sqlite
            }
        };

        Ok(Self {
            db_path,
            requested_backend,
            backend,
            records_total,
            estimated_memory_bytes,
            loaded_at: now_epoch(),
        })
    }

    fn reload(&mut self) -> Result<()> {
        *self = Self::load(self.db_path.clone(), self.requested_backend)?;
        Ok(())
    }

    fn reload_if_changed(&mut self) -> Result<()> {
        if let Some(marker) = self.backend.marker() {
            let current = db_marker(&self.db_path);
            if current != marker {
                self.reload()?;
            }
        }
        Ok(())
    }

    fn refresh_status(&mut self) -> Result<()> {
        if self.backend.marker().is_some() {
            self.reload_if_changed()?;
        } else {
            let db = Database::open(self.db_path.clone())?;
            let stats = db.stats()?;
            self.records_total =
                stats.files + stats.dirs + stats.symlinks + stats.apps + stats.others;
            self.estimated_memory_bytes =
                estimate_backend_memory_bytes(BackendPreference::Sqlite, self.records_total);
            self.loaded_at = now_epoch();
        }
        Ok(())
    }

    fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        match &self.backend {
            Backend::Memory { records, .. } => Ok(search::search_records(records, options)),
            Backend::Compact { index, .. } => Ok(index.search(options)),
            Backend::Sqlite => {
                let db = Database::open(self.db_path.clone())?;
                search::search(&db, options)
            }
        }
    }

    fn status_json(&self) -> String {
        format!(
            "{{\"ok\":true,\"backend\":\"{}\",\"requested_backend\":\"{}\",\"records\":{},\"estimated_memory_bytes\":{},\"loaded_at\":{},\"db_path\":\"{}\"}}\n",
            self.backend.mode(),
            self.requested_backend.as_str(),
            self.records_total,
            self.estimated_memory_bytes,
            self.loaded_at,
            json_escape(&self.db_path.to_string_lossy())
        )
    }
}

fn handle_stream(mut stream: TcpStream, state: Arc<RwLock<ServiceState>>) -> Result<()> {
    stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT))?;
    let request = read_request(&mut stream)?;
    let Some(first_line) = request.lines().next() else {
        return write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            "bad request\n",
        );
    };
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" && method != "POST" {
        return write_response(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "method not allowed\n",
        );
    }

    let (path, query) = split_target(target);
    match path {
        "/health" => write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}\n"),
        "/reload" => {
            let body = {
                let mut state = state
                    .write()
                    .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?;
                state.reload()?;
                state.status_json()
            };
            write_response(&mut stream, "200 OK", "application/json", &body)
        }
        "/status" => {
            let body = {
                let mut state = state
                    .write()
                    .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?;
                state.refresh_status()?;
                state.status_json()
            };
            write_response(&mut stream, "200 OK", "application/json", &body)
        }
        "/search" => {
            let options = parse_search_options(query)?;
            {
                let mut state = state
                    .write()
                    .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?;
                state.reload_if_changed()?;
            }
            let results = {
                let state = state
                    .read()
                    .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?;
                state.search(&options)?
            };
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                &search_results_json(&results),
            )
        }
        _ => write_response(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n",
        ),
    }
}

fn select_backend(requested: BackendPreference, records_total: i64) -> BackendPreference {
    match requested {
        BackendPreference::Auto => {
            let budget = memory_budget_bytes();
            if estimate_memory_bytes(records_total) <= budget {
                BackendPreference::Memory
            } else if estimate_compact_memory_bytes(records_total) <= budget {
                BackendPreference::Compact
            } else {
                BackendPreference::Sqlite
            }
        }
        value => value,
    }
}

fn estimate_memory_bytes(records_total: i64) -> u64 {
    records_total.max(0) as u64 * ESTIMATED_RECORD_BYTES
}

fn estimate_compact_memory_bytes(records_total: i64) -> u64 {
    records_total.max(0) as u64 * ESTIMATED_COMPACT_RECORD_BYTES
}

fn estimate_backend_memory_bytes(backend: BackendPreference, records_total: i64) -> u64 {
    match backend {
        BackendPreference::Auto | BackendPreference::Memory => estimate_memory_bytes(records_total),
        BackendPreference::Compact => estimate_compact_memory_bytes(records_total),
        BackendPreference::Sqlite => 0,
    }
}

fn memory_budget_bytes() -> u64 {
    std::env::var("MACEVERY_MEMORY_BUDGET_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.saturating_mul(1024 * 1024))
        .unwrap_or(DEFAULT_MEMORY_BUDGET_BYTES)
}

fn max_active_requests() -> usize {
    std::env::var("MACEVERY_MAX_ACTIVE_REQUESTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_ACTIVE_REQUESTS)
}

fn try_acquire_request_slot(active: &AtomicUsize, limit: usize) -> bool {
    loop {
        let current = active.load(Ordering::Acquire);
        if current >= limit {
            return false;
        }
        if active
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.1} GiB", mib / 1024.0)
    } else {
        format!("{mib:.0} MiB")
    }
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = [0u8; 8192];
    let mut data = Vec::new();
    let started_at = SystemTime::now();
    loop {
        if started_at.elapsed().unwrap_or_default() > REQUEST_TOTAL_TIMEOUT {
            return Err(MacEveryError::Cli("request header timed out".to_string()));
        }

        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        if data.windows(4).any(|value| value == b"\r\n\r\n") {
            break;
        }
        if data.len() >= REQUEST_HEADER_LIMIT {
            return Err(MacEveryError::Cli(
                "request header is too large".to_string(),
            ));
        }
    }
    String::from_utf8(data).map_err(|_| MacEveryError::Cli("request is not utf-8".to_string()))
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn split_target(target: &str) -> (&str, &str) {
    if let Some((path, query)) = target.split_once('?') {
        (path, query)
    } else {
        (target, "")
    }
}

fn parse_search_options(query: &str) -> Result<SearchOptions> {
    let params = parse_query(query);
    let query = params.get("q").cloned().unwrap_or_default();
    if query.trim().is_empty() {
        return Err(MacEveryError::Cli("missing q parameter".to_string()));
    }

    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(80);
    let ext = params
        .get("ext")
        .map(|value| value.trim_start_matches('.').to_lowercase())
        .filter(|value| !value.is_empty());
    let kind = params
        .get("kind")
        .and_then(|value| FileKind::from_str(value));
    let path_only = params
        .get("path")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let fuzzy = params
        .get("fuzzy")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    Ok(SearchOptions {
        query,
        limit,
        ext,
        kind,
        path_only,
        fuzzy,
        ..SearchOptions::default()
    })
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for part in query.split('&').filter(|value| !value.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hi = hex(bytes[index + 1]);
                let lo = hex(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn search_results_json(results: &[SearchResult]) -> String {
    let mut out = String::from("[");
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"basename\":\"{}\",\"kind\":\"{}\",\"size\":{},\"mtime\":{},\"score\":{}}}",
            json_escape(&result.record.path),
            json_escape(&result.record.basename),
            result.record.kind.as_str(),
            json_i64(result.record.size),
            json_i64(result.record.mtime),
            result.score
        ));
    }
    out.push_str("]\n");
    out
}

fn json_i64(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
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

fn db_marker(path: &Path) -> u128 {
    [
        path.to_path_buf(),
        sidecar_path(path, "wal"),
        sidecar_path(path, "shm"),
    ]
    .iter()
    .filter_map(|path| fs::metadata(path).ok())
    .filter_map(|metadata| metadata.modified().ok())
    .map(system_time_nanos)
    .max()
    .unwrap_or(0)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.to_string_lossy()))
}

fn system_time_nanos(value: SystemTime) -> u128 {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_preference_aliases() {
        assert_eq!(
            BackendPreference::from_str("automatic").unwrap(),
            BackendPreference::Auto
        );
        assert_eq!(
            BackendPreference::from_str("fastest").unwrap(),
            BackendPreference::Memory
        );
        assert_eq!(
            BackendPreference::from_str("balanced").unwrap(),
            BackendPreference::Compact
        );
        assert_eq!(
            BackendPreference::from_str("low-memory").unwrap(),
            BackendPreference::Sqlite
        );
    }

    #[test]
    fn auto_backend_respects_memory_budget() {
        let budget = memory_budget_bytes();
        let memory_records = (budget.saturating_sub(1) / ESTIMATED_RECORD_BYTES) as i64;
        let compact_records = (budget / ESTIMATED_RECORD_BYTES + 1) as i64;
        let sqlite_records = (budget / ESTIMATED_COMPACT_RECORD_BYTES + 1) as i64;
        assert_eq!(
            select_backend(BackendPreference::Auto, sqlite_records),
            BackendPreference::Sqlite
        );
        assert_eq!(
            select_backend(BackendPreference::Auto, compact_records),
            BackendPreference::Compact
        );
        assert_eq!(
            select_backend(BackendPreference::Auto, memory_records),
            BackendPreference::Memory
        );
    }

    #[test]
    fn request_slot_limit_is_enforced() {
        let active = AtomicUsize::new(0);
        assert!(try_acquire_request_slot(&active, 2));
        assert!(try_acquire_request_slot(&active, 2));
        assert!(!try_acquire_request_slot(&active, 2));
        assert_eq!(active.load(Ordering::Acquire), 2);
    }
}
