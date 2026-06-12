use crate::config::default_db_path;
use crate::db::Database;
use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, SearchOptions, SearchResult};
use crate::search;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_ADDR: &str = "127.0.0.1:17649";

pub fn serve(addr: Option<String>) -> Result<()> {
    let addr = addr.unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let db_path = default_db_path();
    let listener = TcpListener::bind(&addr)
        .map_err(|err| MacEveryError::Cli(format!("failed to bind {addr}: {err}")))?;
    eprintln!("macevery search service listening on http://{addr}");

    let mut state = ServiceState::load(db_path)?;
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(err) = state.handle_stream(&mut stream) {
                    let _ = write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "text/plain; charset=utf-8",
                        &format!("{err}\n"),
                    );
                }
            }
            Err(err) => eprintln!("serve: accept failed: {err}"),
        }
    }
    Ok(())
}

struct ServiceState {
    db_path: PathBuf,
    records: Vec<FileRecord>,
    marker: u128,
    loaded_at: i64,
}

impl ServiceState {
    fn load(db_path: PathBuf) -> Result<Self> {
        let db = Database::open(db_path.clone())?;
        let records = db.all_records()?;
        let marker = db_marker(&db_path);
        eprintln!("serve: loaded {} indexed records", records.len());
        Ok(Self {
            db_path,
            records,
            marker,
            loaded_at: now_epoch(),
        })
    }

    fn reload(&mut self) -> Result<()> {
        let db = Database::open(self.db_path.clone())?;
        self.records = db.all_records()?;
        self.marker = db_marker(&self.db_path);
        self.loaded_at = now_epoch();
        eprintln!("serve: reloaded {} indexed records", self.records.len());
        Ok(())
    }

    fn reload_if_changed(&mut self) -> Result<()> {
        let marker = db_marker(&self.db_path);
        if marker != self.marker {
            self.reload()?;
        }
        Ok(())
    }

    fn handle_stream(&mut self, stream: &mut TcpStream) -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let request = read_request(stream)?;
        let Some(first_line) = request.lines().next() else {
            return write_response(stream, "400 Bad Request", "text/plain", "bad request\n");
        };
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if method != "GET" && method != "POST" {
            return write_response(
                stream,
                "405 Method Not Allowed",
                "text/plain; charset=utf-8",
                "method not allowed\n",
            );
        }

        let (path, query) = split_target(target);
        match path {
            "/health" => write_response(stream, "200 OK", "application/json", "{\"ok\":true}\n"),
            "/reload" => {
                self.reload()?;
                write_response(stream, "200 OK", "application/json", &self.status_json())
            }
            "/status" => {
                self.reload_if_changed()?;
                write_response(stream, "200 OK", "application/json", &self.status_json())
            }
            "/search" => {
                self.reload_if_changed()?;
                let options = parse_search_options(query)?;
                let results = search::search_records(&self.records, &options);
                write_response(
                    stream,
                    "200 OK",
                    "application/json",
                    &search_results_json(&results),
                )
            }
            _ => write_response(
                stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                "not found\n",
            ),
        }
    }

    fn status_json(&self) -> String {
        format!(
            "{{\"ok\":true,\"records\":{},\"loaded_at\":{},\"db_path\":\"{}\"}}\n",
            self.records.len(),
            self.loaded_at,
            json_escape(&self.db_path.to_string_lossy())
        )
    }
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = [0u8; 8192];
    let mut data = Vec::new();
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        if data.windows(4).any(|value| value == b"\r\n\r\n") || data.len() >= 64 * 1024 {
            break;
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
    .map(system_time_millis)
    .max()
    .unwrap_or(0)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.to_string_lossy()))
}

fn system_time_millis(value: SystemTime) -> u128 {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}
