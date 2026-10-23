use crate::compact_index::{ApplyOutcome, CompactIndex, SnapshotDiff, SnapshotDiffLimits};
use crate::config::default_db_path;
use crate::db::{ContentObserver, ContentStamp, Database, FileChange, FileChangeRead};
use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, SearchOptions, SearchResult};
use crate::process_lifetime;
use crate::search;
use crate::serve_memory::{MemoryAudit, Snapshot as MemorySnapshot, TrafficCounters};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_ADDR: &str = "127.0.0.1:17649";
const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 2_560 * 1024 * 1024;
const ESTIMATED_RECORD_BYTES: u64 = 900;
const ESTIMATED_COMPACT_RECORD_BYTES: u64 = 420;
const DEFAULT_MAX_ACTIVE_REQUESTS: usize = 64;
const DEFAULT_MAX_ACTIVE_SEARCHES: usize = 2;
const MAX_INCREMENTAL_EVENTS: usize = 4096;
const MAX_INCREMENTAL_IDS: usize = 4096;
const SNAPSHOT_DIFF_LIMITS: SnapshotDiffLimits = SnapshotDiffLimits {
    max_changes: MAX_INCREMENTAL_IDS,
    max_payload_bytes: crate::db::FILE_CHANGE_MAX_PAYLOAD_BYTES,
    max_scan_rows: 10_000_000,
    max_duration: Duration::from_secs(6),
};
const REQUEST_HEADER_LIMIT: usize = 64 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_millis(250);
const REQUEST_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackendPreference {
    #[default]
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
    let auth_token: Option<Arc<str>> = std::env::var("MACEVERY_AUTH_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| Arc::from(v.as_str()));
    let listener = TcpListener::bind(&addr)
        .map_err(|err| MacEveryError::Cli(format!("failed to bind {addr}: {err}")))?;
    if auth_token.is_some() {
        eprintln!("macevery search service listening on http://{addr} (token auth enabled)");
    } else {
        eprintln!("macevery search service listening on http://{addr} (no auth — debug mode)");
    }

    let state = ServiceState::load(db_path, backend)?;
    let traffic = state.memory_audit.traffic();
    state.memory_audit.start();
    let state = Arc::new(RwLock::new(state));
    let active_requests = Arc::new(AtomicUsize::new(0));
    let max_active_requests = max_active_requests();
    let search_slots = Arc::new(SearchSlots {
        active: Arc::new(AtomicUsize::new(0)),
        limit: max_active_searches(),
        traffic: Arc::clone(&traffic),
    });
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let Some(guard) =
                    ActiveRequestGuard::http(&active_requests, max_active_requests, &traffic)
                else {
                    let _ = write_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        "server busy\n",
                    );
                    continue;
                };
                let state = Arc::clone(&state);
                let auth_token = auth_token.clone();
                let search_slots = Arc::clone(&search_slots);
                thread::spawn(move || {
                    let _guard = guard;
                    if let Err(err) =
                        handle_stream(stream, state, auth_token.as_deref(), search_slots)
                    {
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
    traffic: Arc<TrafficCounters>,
    completion: CompletionKind,
}

enum CompletionKind {
    Http,
    Search,
}

impl ActiveRequestGuard {
    fn http(
        active: &Arc<AtomicUsize>,
        limit: usize,
        traffic: &Arc<TrafficCounters>,
    ) -> Option<Self> {
        traffic.http_accept();
        if !try_acquire_request_slot(active, limit) {
            traffic.http_reject();
            return None;
        }
        Some(Self {
            active: Arc::clone(active),
            traffic: Arc::clone(traffic),
            completion: CompletionKind::Http,
        })
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        match self.completion {
            CompletionKind::Http => self.traffic.http_complete(),
            CompletionKind::Search => self.traffic.search_complete(),
        }
    }
}

struct SearchSlots {
    active: Arc<AtomicUsize>,
    limit: usize,
    traffic: Arc<TrafficCounters>,
}

impl SearchSlots {
    fn try_acquire(&self) -> Option<ActiveRequestGuard> {
        try_acquire_request_slot(&self.active, self.limit).then(|| {
            self.traffic.search_start();
            ActiveRequestGuard {
                active: Arc::clone(&self.active),
                traffic: Arc::clone(&self.traffic),
                completion: CompletionKind::Search,
            }
        })
    }
}

enum Backend {
    Packed(CompactIndex),
    Sqlite(Mutex<Database>),
    Unavailable,
}

impl Backend {
    #[cfg(test)]
    fn memory_usage_bytes(&self) -> u64 {
        match self {
            Self::Packed(index) => index.memory_usage_bytes(),
            Self::Sqlite(_) | Self::Unavailable => 0,
        }
    }

    fn loaded_records(&self) -> usize {
        match self {
            Self::Packed(index) => index.len(),
            Self::Sqlite(_) | Self::Unavailable => 0,
        }
    }

    fn is_ready(&self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

struct ServiceState {
    db_path: PathBuf,
    requested_backend: BackendPreference,
    selected_backend: BackendPreference,
    observer: ContentObserver,
    backend: Backend,
    loaded_stamp: Option<ContentStamp>,
    records_total: i64,
    estimated_memory_bytes: u64,
    loaded_at: i64,
    load_attempts: u64,
    successful_loads: u64,
    last_load_error: Option<String>,
    incremental_attempts: u64,
    incremental_applies: u64,
    incremental_change_ids: u64,
    last_incremental_change_ids: usize,
    refreshed_at: i64,
    last_refresh_method: &'static str,
    last_fallback_reason: Option<&'static str>,
    last_refresh_error: Option<String>,
    pending_full_reason: Option<&'static str>,
    // Computed only when index ownership changes; /status uses the cached sum.
    memory_snapshot: MemorySnapshot,
    memory_audit: Arc<MemoryAudit>,
}

// The first stamp SELECT pins the same WAL snapshot used by the full load.
// A later commit is deliberately picked up by the next request's observer check.
fn with_read_snapshot<T>(
    db: &Database,
    read: impl FnOnce(&ContentStamp) -> Result<T>,
) -> Result<(ContentStamp, T)> {
    db.begin_read()?;
    let result = (|| {
        let stamp = db.content_stamp()?;
        let value = read(&stamp)?;
        Ok((stamp, value))
    })();
    match result {
        Ok(value) => {
            if let Err(error) = db.commit() {
                let _ = db.rollback();
                return Err(error);
            }
            Ok(value)
        }
        Err(error) => {
            let _ = db.rollback();
            Err(error)
        }
    }
}

impl ServiceState {
    fn load(db_path: PathBuf, requested_backend: BackendPreference) -> Result<Self> {
        // Bootstrap/migrate once. Hot-path observation and queries are read-only
        // and do not rerun schema initialization for each HTTP request.
        drop(Database::open(db_path.clone())?);
        let observer = ContentObserver::open(db_path.clone())?;
        let memory_audit = MemoryAudit::new(&db_path);
        let mut state = Self {
            db_path,
            requested_backend,
            selected_backend: requested_backend,
            observer,
            backend: Backend::Unavailable,
            loaded_stamp: None,
            records_total: 0,
            estimated_memory_bytes: 0,
            loaded_at: 0,
            load_attempts: 0,
            successful_loads: 0,
            last_load_error: None,
            incremental_attempts: 0,
            incremental_applies: 0,
            incremental_change_ids: 0,
            last_incremental_change_ids: 0,
            refreshed_at: 0,
            last_refresh_method: "unavailable",
            last_fallback_reason: None,
            last_refresh_error: None,
            pending_full_reason: None,
            memory_snapshot: MemorySnapshot::default(),
            memory_audit,
        };
        if let Err(error) = state.full_reload("startup_full", None) {
            // Keep the listener usable after a failed full load. A later search
            // can retry; no old or partially loaded records are published.
            eprintln!("serve: initial index load failed, will retry: {error}");
        }
        Ok(state)
    }

    fn load_backend(&self) -> Result<(Backend, BackendPreference, ContentStamp, i64)> {
        let mut db = Database::open_read_only(self.db_path.clone())?;
        if matches!(
            db.content_stamp(),
            Err(MacEveryError::Sqlite(ref message))
                if message.contains("no such table: macevery_content_state")
        ) || !db.has_change_journal()?
        {
            // An older database may have been atomically installed after startup,
            // or may predate the bounded change journal. Install once without
            // CREATE, then reopen the full snapshot read-only.
            drop(db);
            drop(Database::open_existing(self.db_path.clone())?);
            db = Database::open_read_only(self.db_path.clone())?;
        }
        let (stamp, (selected, index, records_total)) = with_read_snapshot(&db, |_| {
            let selected = if self.requested_backend == BackendPreference::Auto {
                let stats = db.stats()?;
                select_backend(
                    BackendPreference::Auto,
                    stats.files + stats.dirs + stats.symlinks + stats.apps + stats.others,
                )
            } else {
                self.requested_backend
            };
            if matches!(
                selected,
                BackendPreference::Memory | BackendPreference::Compact
            ) {
                let index = CompactIndex::load(&db)?;
                let records_total = index.len() as i64;
                Ok((selected, Some(index), records_total))
            } else {
                let stats = db.stats()?;
                Ok((
                    selected,
                    None,
                    (stats.files + stats.dirs + stats.symlinks + stats.apps + stats.others),
                ))
            }
        })?;
        let backend = match index {
            Some(index) => Backend::Packed(index),
            None => Backend::Sqlite(Mutex::new(db)),
        };
        Ok((backend, selected, stamp, records_total))
    }

    fn invalidate_backend(&mut self) {
        self.backend = Backend::Unavailable;
        self.loaded_stamp = None;
        self.records_total = 0;
        self.estimated_memory_bytes = 0;
        self.loaded_at = 0;
        self.refreshed_at = 0;
        self.memory_snapshot = MemorySnapshot::default();
    }

    fn collect_memory(&self, phase: &'static str) -> MemorySnapshot {
        let mut snapshot = MemorySnapshot {
            phase,
            backend: self.selected_backend.as_str(),
            revision: self.loaded_stamp.as_ref().map(|stamp| stamp.revision),
            database_device: self
                .loaded_stamp
                .as_ref()
                .map(|stamp| stamp.file_identity.device),
            database_inode: self
                .loaded_stamp
                .as_ref()
                .map(|stamp| stamp.file_identity.inode),
            database_token: self.loaded_stamp.as_ref().map(|stamp| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                stamp.database_id.hash(&mut hash);
                hash.finish()
            }),
            records: self.backend.loaded_records(),
            load_attempts: self.load_attempts,
            successful_loads: self.successful_loads,
            incremental_attempts: self.incremental_attempts,
            incremental_applies: self.incremental_applies,
            incremental_change_ids: self.incremental_change_ids,
            last_change_ids: self.last_incremental_change_ids,
            method: self.last_refresh_method,
            fallback_reason: self.last_fallback_reason,
            ..MemorySnapshot::default()
        };
        if let Backend::Packed(index) = &self.backend {
            snapshot.memory = index.memory_breakdown();
            let memory = snapshot.memory;
            snapshot.capacity_bytes = [
                memory.record_capacity_bytes,
                memory.metadata_capacity_bytes,
                memory.string_capacity_bytes,
                memory.extension_capacity_bytes,
                memory.container_bytes,
                memory.prefix_string_bytes,
                memory.prefix_entry_capacity_bytes,
                memory.prefix_lookup_capacity_bytes,
            ]
            .into_iter()
            .fold(0u64, |sum, bytes| sum.saturating_add(bytes as u64));
            snapshot.set_fragmentation(index.fragmentation_stats());
        }
        snapshot
    }

    fn publish_memory(&mut self, phase: &'static str) {
        let snapshot = self.collect_memory(phase);
        self.memory_snapshot = snapshot;
        self.memory_audit.publish(snapshot);
    }

    fn cache_memory(&mut self, phase: &'static str) {
        let snapshot = self.collect_memory(phase);
        self.memory_snapshot = snapshot;
        self.memory_audit.cache_only(snapshot);
    }

    fn reload(&mut self) -> Result<()> {
        self.full_reload("explicit_full", None)
    }

    fn full_reload(
        &mut self,
        method: &'static str,
        fallback_reason: Option<&'static str>,
    ) -> Result<()> {
        self.full_reload_with_capture(method, fallback_reason, true)
    }

    fn full_reload_with_capture(
        &mut self,
        method: &'static str,
        fallback_reason: Option<&'static str>,
        capture_before: bool,
    ) -> Result<()> {
        self.load_attempts = self.load_attempts.saturating_add(1);
        self.last_fallback_reason = fallback_reason;
        if capture_before {
            self.publish_memory("full_before");
        }
        // Drop all old index arenas before allocating a replacement. While the
        // write lock is held searches wait; /health does not take this lock.
        self.invalidate_backend();
        self.cache_memory("full_loading");
        match self.load_backend() {
            Ok((backend, selected, stamp, records_total)) => {
                self.backend = backend;
                self.selected_backend = selected;
                self.loaded_stamp = Some(stamp);
                self.records_total = records_total;
                self.estimated_memory_bytes =
                    estimate_backend_memory_bytes(selected, records_total);
                self.loaded_at = now_epoch();
                self.refreshed_at = self.loaded_at;
                self.successful_loads = self.successful_loads.saturating_add(1);
                self.last_load_error = None;
                self.last_refresh_error = None;
                self.last_refresh_method = method;
                self.pending_full_reason = None;
                self.publish_memory("full_after");
                eprintln!(
                    "serve: loaded {} records, backend={}, index storage {} (allocated capacity, not process RSS)",
                    records_total,
                    selected.as_str(),
                    format_bytes(self.memory_snapshot.capacity_bytes)
                );
                Ok(())
            }
            Err(error) => {
                self.last_load_error = Some(error.to_string());
                self.last_refresh_error = Some(error.to_string());
                self.last_refresh_method = "full_failed";
                self.publish_memory("full_failed");
                Err(error)
            }
        }
    }

    fn needs_reload(&self) -> Result<bool> {
        if !self.backend.is_ready() {
            return Ok(true);
        }
        // Missing/replaced/legacy databases also enter the retryable loader,
        // rather than trapping every later request in an observer-only error.
        Ok(match self.observer.content_stamp() {
            Ok(current) => self.loaded_stamp.as_ref() != Some(&current),
            Err(_) => true,
        })
    }

    fn reload_if_changed(&mut self) -> Result<()> {
        self.reload_if_changed_with_limits(SNAPSHOT_DIFF_LIMITS)
    }

    fn reload_if_changed_with_limits(&mut self, limits: SnapshotDiffLimits) -> Result<()> {
        // Recheck after taking the write lock: competing requests apply one batch.
        if !self.needs_reload()? {
            return Ok(());
        }
        if !self.backend.is_ready() {
            let reason = self.pending_full_reason.unwrap_or("index_unavailable");
            return self.full_reload("fallback_full", Some(reason));
        }
        let current = match self.observer.content_stamp() {
            Ok(stamp) => stamp,
            Err(_) => return self.full_reload("fallback_full", Some("database_unavailable")),
        };
        let Some(base) = self.loaded_stamp.clone() else {
            return self.full_reload("fallback_full", Some("missing_loaded_stamp"));
        };
        if current.database_id != base.database_id || current.file_identity != base.file_identity {
            return self.full_reload("fallback_full", Some("database_replaced"));
        }
        if !matches!(self.backend, Backend::Packed(_)) {
            return self.full_reload("fallback_full", Some("backend_not_packed"));
        }
        self.incremental_attempts = self.incremental_attempts.saturating_add(1);
        let mut refresh_method = "incremental";
        let delta: Result<FileChangeRead> = (|| {
            let db = Database::open_read_only(self.db_path.clone())?;
            let (_, delta) = with_read_snapshot(&db, |target| {
                let delta =
                    db.file_changes_since(&base, MAX_INCREMENTAL_EVENTS, MAX_INCREMENTAL_IDS)?;
                if matches!(
                    delta,
                    FileChangeRead::Fallback {
                        reason: "history_gap"
                    }
                ) {
                    let Backend::Packed(index) = &self.backend else {
                        return Ok(delta);
                    };
                    return match index.snapshot_diff(&db, limits)? {
                        SnapshotDiff::Available(changes) => {
                            refresh_method = "snapshot_reconcile";
                            Ok(FileChangeRead::Available {
                                target: target.clone(),
                                changes,
                            })
                        }
                        SnapshotDiff::Fallback(reason) => Ok(FileChangeRead::Fallback { reason }),
                    };
                }
                Ok(delta)
            })?;
            Ok(delta)
        })();
        match delta {
            Ok(FileChangeRead::Fallback { reason }) => {
                self.full_reload("fallback_full", Some(reason))
            }
            Ok(FileChangeRead::Available { target, changes }) => {
                self.apply_delta_with_method(target, changes, refresh_method)
            }
            Err(error) => {
                // No packed mutation occurred. Keep the old snapshot labelled
                // stale; callers receive 503 until a refresh succeeds.
                self.last_refresh_method = "incremental_failed";
                self.last_refresh_error = Some(error.to_string());
                self.publish_memory("incremental_read_failed");
                Err(error)
            }
        }
    }

    #[cfg(test)]
    fn apply_delta(&mut self, target: ContentStamp, changes: Vec<FileChange>) -> Result<()> {
        self.apply_delta_with_method(target, changes, "incremental")
    }

    fn apply_delta_with_method(
        &mut self,
        target: ContentStamp,
        changes: Vec<FileChange>,
        method: &'static str,
    ) -> Result<()> {
        let change_ids = changes.len();
        self.publish_memory("incremental_before");
        let outcome = match &mut self.backend {
            Backend::Packed(index) => index.apply_changes(&changes),
            _ => {
                return self.full_reload_with_capture(
                    "fallback_full",
                    Some("backend_not_packed"),
                    false,
                );
            }
        };
        drop(changes);
        match outcome {
            Ok(ApplyOutcome::NeedsFullReload(reason)) => {
                // This outcome guarantees no index mutation. incremental_before
                // already captured this ownership; a duplicate full_before would
                // consume the two-capture window and suppress a fast outcome.
                self.full_reload_with_capture("fallback_full", Some(reason), false)
            }
            Ok(ApplyOutcome::Applied) => {
                let records_total = self.backend.loaded_records() as i64;
                let selected = if self.requested_backend == BackendPreference::Auto {
                    select_backend(BackendPreference::Auto, records_total)
                } else {
                    self.selected_backend
                };
                if selected == BackendPreference::Sqlite {
                    // Applied has changed the index, so its before-drop snapshot
                    // is distinct. General refresh storms may still be coalesced.
                    return self.full_reload("fallback_full", Some("auto_backend_change"));
                }
                // The read transaction has committed and every mutation
                // succeeded before the snapshot stamp becomes visible.
                self.loaded_stamp = Some(target);
                self.records_total = records_total;
                self.selected_backend = selected;
                self.estimated_memory_bytes =
                    estimate_backend_memory_bytes(selected, records_total);
                self.refreshed_at = now_epoch();
                self.incremental_applies = self.incremental_applies.saturating_add(1);
                self.incremental_change_ids = self
                    .incremental_change_ids
                    .saturating_add(change_ids as u64);
                self.last_incremental_change_ids = change_ids;
                self.last_refresh_method = method;
                self.last_fallback_reason = None;
                self.last_load_error = None;
                self.last_refresh_error = None;
                self.publish_memory("incremental_after");
                Ok(())
            }
            Err(error) => {
                // An unexpected apply failure may have changed slots or
                // arenas. Never expose a partially updated old snapshot.
                self.invalidate_backend();
                self.pending_full_reason = Some("apply_failed");
                self.last_fallback_reason = Some("apply_failed");
                self.last_refresh_method = "incremental_failed";
                self.last_refresh_error = Some(error.to_string());
                self.publish_memory("incremental_failed");
                Err(error)
            }
        }
    }

    fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        match &self.backend {
            Backend::Packed(index) => Ok(index.search(options)),
            Backend::Sqlite(database) => {
                let db = database.lock().map_err(|_| {
                    MacEveryError::Sqlite("search database lock poisoned".to_string())
                })?;
                search::search(&db, options)
            }
            Backend::Unavailable => Err(MacEveryError::Cli(
                "search index is unavailable; retry".to_string(),
            )),
        }
    }

    fn status_json(&self) -> String {
        // Report loaded state plus a cheap freshness check. Merely looking at
        // status must not allocate or reload a multi-million-record index.
        let observed = self.observer.content_stamp();
        let stale = observed
            .as_ref()
            .map_or(true, |stamp| self.loaded_stamp.as_ref() != Some(stamp));
        let ready = self.backend.is_ready();
        let revision = self
            .loaded_stamp
            .as_ref()
            .map(|stamp| stamp.revision.to_string())
            .unwrap_or_else(|| "null".into());
        let current_revision = observed
            .as_ref()
            .map(|stamp| stamp.revision.to_string())
            .unwrap_or_else(|_| "null".into());
        let observation_error = observed.err().map(|error| error.to_string());
        let error = self.last_load_error.as_ref().or(observation_error.as_ref());
        let error_json = error
            .map(|message| format!("\"{}\"", json_escape(message)))
            .unwrap_or_else(|| "null".into());
        format!(
            "{{\"ok\":{},\"ready\":{},\"stale\":{},\"backend\":\"{}\",\"requested_backend\":\"{}\",\"records\":{},\"loaded_records\":{},\"estimated_memory_bytes\":{},\"actual_index_bytes\":{},\"loaded_at\":{},\"reload_count\":{},\"reload_attempts\":{},\"content_revision\":{},\"current_content_revision\":{},\"last_load_error\":{},\"incremental_attempts\":{},\"incremental_applies\":{},\"incremental_change_ids\":{},\"last_incremental_change_ids\":{},\"refreshed_at\":{},\"last_refresh_method\":\"{}\",\"last_fallback_reason\":{},\"last_refresh_error\":{},\"db_path\":\"{}\"}}\n",
            ready && !stale,
            ready,
            stale,
            self.selected_backend.as_str(),
            self.requested_backend.as_str(),
            self.records_total,
            self.backend.loaded_records(),
            self.estimated_memory_bytes,
            self.memory_snapshot.capacity_bytes,
            self.loaded_at,
            self.successful_loads.saturating_sub(1),
            self.load_attempts.saturating_sub(1),
            revision,
            current_revision,
            error_json,
            self.incremental_attempts,
            self.incremental_applies,
            self.incremental_change_ids,
            self.last_incremental_change_ids,
            self.refreshed_at,
            self.last_refresh_method,
            self.last_fallback_reason.map(|reason| format!("\"{}\"", json_escape(reason))).unwrap_or_else(|| "null".into()),
            self.last_refresh_error.as_ref().map(|error| format!("\"{}\"", json_escape(error))).unwrap_or_else(|| "null".into()),
            json_escape(&self.db_path.to_string_lossy())
        )
    }
}

fn ensure_fresh(state: &RwLock<ServiceState>) -> Result<()> {
    let needs_reload = state
        .read()
        .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?
        .needs_reload()?;
    if needs_reload {
        state
            .write()
            .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?
            .reload_if_changed()?;
    }
    Ok(())
}

fn handle_stream(
    mut stream: TcpStream,
    state: Arc<RwLock<ServiceState>>,
    auth_token: Option<&str>,
    search_slots: Arc<SearchSlots>,
) -> Result<()> {
    stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TOTAL_TIMEOUT))?;
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

    if path != "/health" {
        if let Some(expected) = auth_token {
            if !request_has_valid_token(&request, expected) {
                return write_response(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain; charset=utf-8",
                    "unauthorized\n",
                );
            }
        }
    }

    match path {
        "/health" => write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}\n"),
        "/reload" => {
            let (status, body) = {
                let mut state = state
                    .write()
                    .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?;
                let status = match state.reload() {
                    Ok(()) => "200 OK",
                    Err(error) => {
                        eprintln!("serve: reload failed, will retry: {error}");
                        "503 Service Unavailable"
                    }
                };
                (status, state.status_json())
            };
            write_response(&mut stream, status, "application/json", &body)
        }
        "/status" => {
            let body = state
                .read()
                .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?
                .status_json();
            write_response(&mut stream, "200 OK", "application/json", &body)
        }
        "/search" => {
            let options = parse_search_options(query)?;
            let Some(_search_guard) = search_slots.try_acquire() else {
                return write_response(
                    &mut stream,
                    "503 Service Unavailable",
                    "text/plain; charset=utf-8",
                    "search busy; retry the latest query\n",
                );
            };
            if let Err(error) = ensure_fresh(&state) {
                eprintln!("serve: index unavailable, will retry: {error}");
                return write_response(
                    &mut stream,
                    "503 Service Unavailable",
                    "text/plain; charset=utf-8",
                    "search index unavailable; retry the latest query\n",
                );
            }
            let results = {
                let state = state
                    .read()
                    .map_err(|_| MacEveryError::Cli("service state lock poisoned".to_string()))?;
                state.search(&options)
            };
            let results = match results {
                Ok(results) => results,
                Err(error) => {
                    // A forced reload can fail between ensure_fresh and this read.
                    // Return backpressure rather than closing TCP and provoking
                    // the GUI's separate CLI fallback.
                    eprintln!("serve: search unavailable, will retry: {error}");
                    return write_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        "search index unavailable; retry the latest query\n",
                    );
                }
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

fn max_active_searches() -> usize {
    std::env::var("MACEVERY_MAX_ACTIVE_SEARCHES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_ACTIVE_SEARCHES)
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

fn request_has_valid_token(request: &str, expected: &str) -> bool {
    for line in request.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("authorization") {
            return authorization_value_matches(value.trim(), expected);
        }
    }
    false
}

fn authorization_value_matches(value: &str, expected: &str) -> bool {
    let mut parts = value.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    if first.eq_ignore_ascii_case("bearer") {
        return parts.next() == Some(expected) && parts.next().is_none();
    }
    value == expected
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
    let kind = params.get("kind").and_then(|value| FileKind::parse(value));
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

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_database(label: &str) -> (PathBuf, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "macevery-server-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("index.sqlite");
        let db = Database::open(path.clone()).unwrap();
        db.upsert_file(&crate::model::FileRecord::new(
            "/fixture/alpha.txt".into(),
            FileKind::File,
            1,
        ))
        .unwrap();
        drop(db);
        (directory, path)
    }

    fn query(value: &str) -> SearchOptions {
        SearchOptions {
            query: value.into(),
            limit: 300,
            ..Default::default()
        }
    }

    #[test]
    fn pre_mutation_delta_fallback_keeps_before_and_outcome_vm_captures() {
        let (directory, path) = test_database("fallback-memory-captures");
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        // A fresh enabled audit makes this test independent of the process's
        // diagnostic environment and startup captures; no timer or log I/O.
        state.memory_audit = MemoryAudit::for_test(&path);
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer.execute_batch("INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at) VALUES(-50,'/fixture/code-short.txt','/fixture/code-short.txt','code-short.txt','code-short.txt','file',2);").unwrap();
        state.reload_if_changed().unwrap();
        assert_eq!(
            state.last_fallback_reason,
            Some("new non-monotonic record ID")
        );
        assert_eq!(state.successful_loads, 2);
        assert_eq!(
            state.memory_audit.pending_phases(),
            (vec!["incremental_before", "full_after"], 0)
        );
        assert_eq!(state.memory_snapshot.phase, "full_after");
        assert_eq!(state.search(&query("code")).unwrap().len(), 1);
        assert_matches_full_snapshot(&state, &path);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn assert_matches_full_snapshot(state: &ServiceState, path: &std::path::Path) {
        let db = Database::open_read_only(path.to_path_buf()).unwrap();
        let expected = search::search_records(&db.all_records().unwrap(), &query("*"));
        assert_eq!(state.search(&query("*")).unwrap(), expected);
        assert_eq!(
            state.records_total as usize,
            db.all_records().unwrap().len()
        );
        assert_eq!(
            state.memory_snapshot.capacity_bytes,
            state.backend.memory_usage_bytes()
        );
        assert_eq!(
            state.memory_snapshot.revision,
            state.loaded_stamp.as_ref().map(|stamp| stamp.revision)
        );
    }

    fn seed_records(path: &std::path::Path, count: usize) {
        let db = Database::open_existing(path.to_path_buf()).unwrap();
        db.begin().unwrap();
        for index in 0..count {
            db.upsert_file(&crate::model::FileRecord::new(
                format!("/fixture/seed-{index:05}.txt"),
                FileKind::File,
                1,
            ))
            .unwrap();
        }
        db.commit().unwrap();
    }

    #[test]
    fn incremental_mixed_waves_id_mutation_and_reused_id_match_full_snapshot() {
        let (directory, path) = test_database("mixed-delta");
        seed_records(&path, 64);
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        for sql in [
            "BEGIN IMMEDIATE;
             UPDATE files SET basename='renamed-alpha.txt',basename_lower='renamed-alpha.txt',size=99,mtime=300 WHERE id=1;
             DELETE FROM files WHERE id=2;
             UPDATE files SET id=1000 WHERE id=3;
             INSERT INTO files(path,path_lower,basename,basename_lower,kind,indexed_at) VALUES('/fixture/new.txt','/fixture/new.txt','new.txt','new.txt','file',2);
             COMMIT;",
            "DELETE FROM files WHERE id=1001;",
            "INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at) VALUES(1001,'/fixture/reused.txt','/fixture/reused.txt','reused.txt','reused.txt','file',3);",
        ] {
            writer.execute_batch(sql).unwrap();
            let before = state.incremental_applies;
            state.reload_if_changed().unwrap();
            assert_eq!(state.incremental_applies, before + 1);
            assert_eq!(state.successful_loads, 1);
            assert_matches_full_snapshot(&state, &path);
            assert!(!state.needs_reload().unwrap());
        }
        assert_eq!(state.incremental_applies, 3);
        // Explicit reload remains full even when there are no new commits.
        state.reload().unwrap();
        assert_eq!(state.successful_loads, 2);
        assert_eq!(state.last_refresh_method, "explicit_full");
        assert_matches_full_snapshot(&state, &path);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sqlite_replace_hidden_victims_are_removed_from_packed_snapshot() {
        let (directory, path) = test_database("replace-victims");
        seed_records(&path, 64);
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "UPDATE files SET path='/fixture/a' WHERE id=1;
            UPDATE files SET path='/fixture/b' WHERE id=2;
            UPDATE files SET path='/fixture/c' WHERE id=3;
            UPDATE files SET path='/fixture/d' WHERE id=4;",
            )
            .unwrap();
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        writer
            .execute_batch(
                "PRAGMA recursive_triggers=OFF; BEGIN IMMEDIATE;
            INSERT OR REPLACE INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at)
                VALUES(1000,'/fixture/a','/fixture/a','a','a','file',2);
            DELETE FROM files WHERE id=1000;
            UPDATE OR REPLACE files SET id=4,path='/fixture/b' WHERE id=3;
            COMMIT;",
            )
            .unwrap();
        state.reload_if_changed().unwrap();
        assert_eq!(state.successful_loads, 1);
        assert_eq!(state.incremental_applies, 1);
        assert_eq!(state.last_incremental_change_ids, 5);
        assert_matches_full_snapshot(&state, &path);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn history_gap_reconciles_complete_snapshot_without_a_full_load() {
        let (directory, path) = test_database("gap-reconcile");
        seed_records(&path, 8);
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE; UPDATE files SET basename='ΟΣİ-renamed',basename_lower='οςi-renamed',path_lower=NULL,size=NULL,mtime=-123,ctime=456,dev=77,inode=88,indexed_at=987654321 WHERE id=1; DELETE FROM files WHERE id=9; INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at) VALUES(100,'/fixture/appended.txt','/fixture/appended.txt','appended.txt','appended.txt','file',2); DELETE FROM macevery_file_changes; COMMIT;").unwrap();
        state.reload_if_changed().unwrap();
        assert_eq!(state.successful_loads, 1);
        assert_eq!(state.last_refresh_method, "snapshot_reconcile");
        assert_eq!(state.incremental_applies, 1);
        assert_eq!(state.last_incremental_change_ids, 3);
        assert!(!state.needs_reload().unwrap());
        assert_matches_full_snapshot(&state, &path);
        writer.execute_batch("BEGIN IMMEDIATE; UPDATE files SET size=42 WHERE id=1; UPDATE files SET size=NULL WHERE id=1; DELETE FROM macevery_file_changes; COMMIT;").unwrap();
        state.reload_if_changed().unwrap();
        assert_eq!(state.last_incremental_change_ids, 0);
        assert_eq!(state.successful_loads, 1);
        assert_eq!(state.last_refresh_method, "snapshot_reconcile");
        assert!(!state.needs_reload().unwrap());
        assert_matches_full_snapshot(&state, &path);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn history_gap_reconcile_publishes_only_pinned_target_and_rolls_back_read_errors() {
        let (directory, path) = test_database("gap-snapshot");
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "UPDATE files SET size=10 WHERE id=1; DELETE FROM macevery_file_changes;",
            )
            .unwrap();
        let reader = Database::open_read_only(path.clone()).unwrap();
        let original = state.loaded_stamp.clone();
        let failed: Result<(ContentStamp, SnapshotDiff)> = with_read_snapshot(&reader, |_| {
            let Backend::Packed(index) = &state.backend else {
                panic!("packed expected")
            };
            let _ = index.snapshot_diff(&reader, SNAPSHOT_DIFF_LIMITS)?;
            Err(MacEveryError::Cli(
                "injected failure after complete diff".into(),
            ))
        });
        assert!(failed.is_err());
        assert_eq!(state.loaded_stamp, original);
        // A new BEGIN proves the failed read transaction was rolled back.
        reader.begin_read().unwrap();
        reader.commit().unwrap();
        let concurrent_path = path.clone();
        let (target, outcome) = with_read_snapshot(&reader, |_| {
            thread::spawn(move || {
                let writer = crate::sqlite::Connection::open(&concurrent_path).unwrap();
                writer
                    .execute_batch("UPDATE files SET size=20 WHERE id=1;")
                    .unwrap();
            })
            .join()
            .unwrap();
            let Backend::Packed(index) = &state.backend else {
                panic!("packed expected")
            };
            index.snapshot_diff(&reader, SNAPSHOT_DIFF_LIMITS)
        })
        .unwrap();
        let SnapshotDiff::Available(changes) = outcome else {
            panic!("complete diff expected")
        };
        state
            .apply_delta_with_method(target.clone(), changes, "snapshot_reconcile")
            .unwrap();
        assert_eq!(state.loaded_stamp, Some(target));
        assert_eq!(
            state.search(&query("alpha")).unwrap()[0].record.size,
            Some(10)
        );
        assert!(state.needs_reload().unwrap());
        state.reload_if_changed().unwrap();
        assert_eq!(state.successful_loads, 1);
        assert_eq!(
            state.search(&query("alpha")).unwrap()[0].record.size,
            Some(20)
        );
        assert_matches_full_snapshot(&state, &path);
        drop(reader);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn history_gap_reconcile_limits_nonmonotonic_and_fragmentation_keep_full_fallback() {
        for (label, mutation, limits, reason) in [
            ("scan", "UPDATE files SET size=2;", SnapshotDiffLimits { max_scan_rows: 0, ..SNAPSHOT_DIFF_LIMITS }, "reconcile_scan_limit"),
            ("time", "UPDATE files SET size=2;", SnapshotDiffLimits { max_duration: Duration::ZERO, ..SNAPSHOT_DIFF_LIMITS }, "reconcile_time_limit"),
            ("bytes", "UPDATE files SET size=2;", SnapshotDiffLimits { max_payload_bytes: 1, ..SNAPSHOT_DIFF_LIMITS }, "reconcile_byte_limit"),
            ("changes", "UPDATE files SET size=2;", SnapshotDiffLimits { max_changes: 0, ..SNAPSHOT_DIFF_LIMITS }, "reconcile_change_limit"),
            ("nonmonotonic", "INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at) VALUES(-50,'/fixture/new','/fixture/new','new','new','file',2);", SNAPSHOT_DIFF_LIMITS, "new non-monotonic record ID"),
        ] {
            let (directory, path) = test_database(label);
            let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
            let writer = crate::sqlite::Connection::open(&path).unwrap();
            writer.execute_batch(mutation).unwrap();
            writer.execute_batch("DELETE FROM macevery_file_changes;").unwrap();
            state.reload_if_changed_with_limits(limits).unwrap();
            assert_eq!(state.successful_loads, 2, "{label}");
            assert_eq!(state.incremental_applies, 0, "{label}");
            assert_eq!(state.last_fallback_reason, Some(reason), "{label}");
            assert_matches_full_snapshot(&state, &path);
            drop(writer); drop(state); std::fs::remove_dir_all(directory).unwrap();
        }
        for (label, count, mutation, reason) in [
            (
                "many-changes",
                4096,
                "UPDATE files SET indexed_at=2;",
                "reconcile_change_limit",
            ),
            (
                "fragmentation",
                2048,
                "DELETE FROM files WHERE id<=1025;",
                "packed tombstone budget exceeded",
            ),
        ] {
            let (directory, path) = test_database(label);
            seed_records(&path, count);
            let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
            let writer = crate::sqlite::Connection::open(&path).unwrap();
            writer.execute_batch(mutation).unwrap();
            writer
                .execute_batch("DELETE FROM macevery_file_changes; UPDATE macevery_content_state SET journal_floor=revision;")
                .unwrap();
            state.reload_if_changed().unwrap();
            assert_eq!(state.successful_loads, 2, "{label}");
            assert_eq!(state.incremental_applies, 0, "{label}");
            assert_eq!(state.last_fallback_reason, Some(reason), "{label}");
            assert_matches_full_snapshot(&state, &path);
            drop(writer);
            drop(state);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn missing_journal_and_event_overflow_fall_back_to_full_and_recover() {
        for (label, mutation, reason) in [
            (
                "missing",
                "UPDATE files SET size=99 WHERE id=1; DROP TABLE macevery_file_changes;"
                    .to_string(),
                "journal_unavailable",
            ),
            (
                "overflow",
                format!(
                    "BEGIN IMMEDIATE;{}COMMIT;",
                    "UPDATE files SET size=COALESCE(size,0)+1 WHERE id=1;"
                        .repeat(MAX_INCREMENTAL_EVENTS + 1)
                ),
                "event_limit",
            ),
        ] {
            let (directory, path) = test_database(label);
            let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
            let writer = crate::sqlite::Connection::open(&path).unwrap();
            writer.execute_batch(&mutation).unwrap();
            state.reload_if_changed().unwrap();
            assert_eq!(state.successful_loads, 2, "{label}");
            assert_eq!(state.incremental_applies, 0, "{label}");
            assert_eq!(state.last_fallback_reason, Some(reason), "{label}");
            assert_matches_full_snapshot(&state, &path);
            assert!(Database::open_read_only(path.clone())
                .unwrap()
                .has_change_journal()
                .unwrap());
            // Manual schema corruption can leave the DDL writer's parsed
            // trigger schema stale. A fresh external writer also verifies the
            // repaired on-disk journal, rather than that connection's cache.
            drop(writer);
            let writer = crate::sqlite::Connection::open(&path).unwrap();
            writer
                .execute_batch("UPDATE files SET size=100001 WHERE id=1;")
                .unwrap();
            state.reload_if_changed().unwrap();
            assert_eq!(
                state.incremental_applies, 1,
                "{label} did not recover incrementally"
            );
            assert_eq!(state.successful_loads, 2, "{label}");
            assert_matches_full_snapshot(&state, &path);
            drop(writer);
            drop(state);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn journal_unique_id_cap_and_nonmonotonic_new_ids_fall_back_safely() {
        let (directory, path) = test_database("id-limit");
        seed_records(&path, 2050);
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch("UPDATE files SET id=id+10000 WHERE id>1;")
            .unwrap();
        state.reload_if_changed().unwrap();
        assert_eq!(state.last_fallback_reason, Some("id_limit"));
        assert_eq!(state.successful_loads, 2);
        assert_matches_full_snapshot(&state, &path);
        writer.execute_batch("INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at) VALUES(-50,'/fixture/nonmonotonic.txt','/fixture/nonmonotonic.txt','nonmonotonic.txt','nonmonotonic.txt','file',2);").unwrap();
        state.reload_if_changed().unwrap();
        assert_eq!(state.successful_loads, 3);
        assert_eq!(state.last_refresh_method, "fallback_full");
        assert!(state.last_fallback_reason.is_some());
        assert_matches_full_snapshot(&state, &path);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn delta_snapshot_publishes_only_its_target_then_picks_up_later_commit() {
        let (directory, path) = test_database("delta-snapshot");
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        let base = state.loaded_stamp.clone().unwrap();
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch("UPDATE files SET size=10 WHERE id=1;")
            .unwrap();
        let reader = Database::open_read_only(path.clone()).unwrap();
        let (_, delta) = with_read_snapshot(&reader, |_| {
            writer.execute_batch("UPDATE files SET size=20 WHERE id=1;")?;
            reader.file_changes_since(&base, MAX_INCREMENTAL_EVENTS, MAX_INCREMENTAL_IDS)
        })
        .unwrap();
        let FileChangeRead::Available { target, changes } = delta else {
            panic!("expected bounded delta")
        };
        let pinned = target.clone();
        state.apply_delta(target, changes).unwrap();
        assert_eq!(state.loaded_stamp.as_ref(), Some(&pinned));
        assert_eq!(
            state.search(&query("alpha")).unwrap()[0].record.size,
            Some(10)
        );
        assert!(state.needs_reload().unwrap());
        assert!(state.status_json().contains("\"stale\":true"));
        state.reload_if_changed().unwrap();
        assert_eq!(
            state.search(&query("alpha")).unwrap()[0].record.size,
            Some(20)
        );
        assert_eq!(state.successful_loads, 1);
        assert_eq!(state.incremental_applies, 2);
        assert_matches_full_snapshot(&state, &path);
        drop(reader);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn delta_read_error_keeps_old_stamp_stale_and_can_retry_without_poisoning() {
        let (directory, path) = test_database("delta-read-error");
        let state =
            RwLock::new(ServiceState::load(path.clone(), BackendPreference::Memory).unwrap());
        let base = state.read().unwrap().loaded_stamp.clone();
        let writer = crate::sqlite::Connection::open(&path).unwrap();
        writer.execute_batch("UPDATE files SET size=77 WHERE id=1; ALTER TABLE files RENAME TO temporarily_unavailable;").unwrap();
        assert!(ensure_fresh(&state).is_err());
        {
            let loaded = state.read().unwrap();
            assert_eq!(loaded.loaded_stamp, base);
            assert_eq!(loaded.incremental_applies, 0);
            assert!(loaded.needs_reload().unwrap());
            assert!(loaded.last_refresh_error.is_some());
            assert!(loaded.status_json().contains("\"stale\":true"));
        }
        writer
            .execute_batch("ALTER TABLE temporarily_unavailable RENAME TO files;")
            .unwrap();
        ensure_fresh(&state).unwrap();
        let loaded = state.read().unwrap();
        assert_eq!(
            loaded.search(&query("alpha")).unwrap()[0].record.size,
            Some(77)
        );
        assert_eq!(loaded.successful_loads, 1);
        assert_eq!(loaded.incremental_applies, 1);
        drop(loaded);
        drop(writer);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unexpected_apply_error_discards_index_and_forces_full_recovery() {
        let (directory, path) = test_database("delta-apply-error");
        let state =
            RwLock::new(ServiceState::load(path.clone(), BackendPreference::Memory).unwrap());
        {
            let mut loaded = state.write().unwrap();
            let target = loaded.loaded_stamp.clone().unwrap();
            let mut malformed =
                crate::model::FileRecord::new("/fixture/invalid.txt".into(), FileKind::File, 1);
            malformed.id = 999;
            assert!(loaded
                .apply_delta(
                    target,
                    vec![FileChange {
                        id: 1,
                        record: Some(malformed)
                    }]
                )
                .is_err());
            assert!(matches!(loaded.backend, Backend::Unavailable));
            assert!(loaded.loaded_stamp.is_none());
            assert_eq!(loaded.backend.memory_usage_bytes(), 0);
            assert!(loaded.search(&query("alpha")).is_err());
        }
        ensure_fresh(&state).unwrap();
        let loaded = state.read().unwrap();
        assert_eq!(loaded.last_fallback_reason, Some("apply_failed"));
        assert_eq!(loaded.successful_loads, 2);
        assert_eq!(loaded.incremental_applies, 0);
        assert_matches_full_snapshot(&loaded, &path);
        drop(loaded);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn packed_memory_preserves_name_and_metadata_only_changes_do_not_reload() {
        let (directory, path) = test_database("metadata");
        let mut state = ServiceState::load(path.clone(), BackendPreference::Memory).unwrap();
        assert!(matches!(state.backend, Backend::Packed(_)));
        assert_eq!(state.selected_backend, BackendPreference::Memory);
        assert_eq!(state.backend.loaded_records(), 1);
        assert!(state.backend.memory_usage_bytes() > 0);
        let initial_stamp = state.loaded_stamp.clone();
        let initial_capacity = state.backend.memory_usage_bytes();
        let initial_memory_snapshot = state.memory_snapshot;
        let db = Database::open(path.clone()).unwrap();
        db.set_meta("watch_last_event_id", "4242").unwrap();
        db.set_roots(&["/different/root".into()]).unwrap();
        db.set_excludes(&["node_modules".into()]).unwrap();
        drop(db);
        let connection = crate::sqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(connection);
        assert!(!state.needs_reload().unwrap());
        state.reload_if_changed().unwrap();
        assert_eq!(state.successful_loads, 1);
        assert_eq!(state.load_attempts, 1);
        assert_eq!(state.loaded_stamp, initial_stamp);
        assert_eq!(state.backend.memory_usage_bytes(), initial_capacity);
        let status = state.status_json();
        for _ in 0..10 {
            assert_eq!(state.status_json(), status);
        }
        assert_eq!(state.memory_snapshot, initial_memory_snapshot);
        assert_eq!(state.memory_snapshot.capacity_bytes, initial_capacity);
        assert!(status.contains("\"backend\":\"memory\""));
        assert!(status.contains("\"reload_count\":0"));
        assert!(status.contains("\"loaded_records\":1"));
        assert!(status.contains("\"stale\":false"));
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn external_file_commit_is_lazy_and_visible_after_one_incremental_refresh() {
        let (directory, path) = test_database("external");
        let state = Arc::new(RwLock::new(
            ServiceState::load(path.clone(), BackendPreference::Memory).unwrap(),
        ));
        let connection = crate::sqlite::Connection::open(&path).unwrap();
        // No Database::upsert_file call: externally issued DML must invalidate too.
        connection.execute_batch("UPDATE files SET basename='renamed.txt', basename_lower='renamed.txt' WHERE path='/fixture/alpha.txt';").unwrap();
        drop(connection);
        {
            let loaded = state.read().unwrap();
            assert!(loaded.needs_reload().unwrap());
            assert!(loaded.status_json().contains("\"stale\":true"));
            assert_eq!(loaded.successful_loads, 1); // /status did not load anything.
        }
        let workers = (0..8)
            .map(|_| {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    ensure_fresh(&state).unwrap();
                    let loaded = state.read().unwrap();
                    let results = loaded.search(&query("renamed")).unwrap();
                    assert_eq!(results.len(), 1);
                    assert_eq!(results[0].record.basename, "renamed.txt");
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let loaded = state.read().unwrap();
        assert_eq!(loaded.successful_loads, 1);
        assert_eq!(loaded.load_attempts, 1);
        assert_eq!(loaded.incremental_attempts, 1);
        assert_eq!(loaded.incremental_applies, 1);
        assert_eq!(loaded.last_refresh_method, "incremental");
        assert!(!loaded.needs_reload().unwrap());
        drop(loaded);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_full_load_drops_old_index_and_recovers_without_poisoning() {
        let (directory, path) = test_database("recover");
        let state =
            RwLock::new(ServiceState::load(path.clone(), BackendPreference::Memory).unwrap());
        let connection = crate::sqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("ALTER TABLE files RENAME TO temporarily_unavailable;")
            .unwrap();
        {
            let mut loaded = state.write().unwrap();
            assert!(loaded.reload().is_err());
            assert!(matches!(loaded.backend, Backend::Unavailable));
            assert_eq!(loaded.backend.memory_usage_bytes(), 0);
            assert_eq!(loaded.records_total, 0);
            assert!(loaded.loaded_stamp.is_none());
            assert!(loaded.last_load_error.is_some());
            assert!(loaded.search(&query("alpha")).is_err());
            assert!(loaded.status_json().contains("\"ready\":false"));
        }
        connection
            .execute_batch("ALTER TABLE temporarily_unavailable RENAME TO files;")
            .unwrap();
        drop(connection);
        ensure_fresh(&state).unwrap();
        let loaded = state.read().unwrap();
        assert_eq!(loaded.search(&query("alpha")).unwrap().len(), 1);
        assert_eq!(loaded.successful_loads, 2);
        assert_eq!(loaded.load_attempts, 3);
        assert!(loaded.last_load_error.is_none());
        drop(loaded);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn database_replacement_reopens_observer_and_cached_sqlite_search() {
        for preference in [BackendPreference::Memory, BackendPreference::Sqlite] {
            let (directory, path) = test_database("replacement");
            let mut state = ServiceState::load(path.clone(), preference).unwrap();
            let initial_identity = state.loaded_stamp.as_ref().unwrap().file_identity;
            let replacement = directory.join("replacement.sqlite");
            let db = Database::open(replacement.clone()).unwrap();
            db.upsert_file(&crate::model::FileRecord::new(
                "/fixture/beta.txt".into(),
                FileKind::File,
                1,
            ))
            .unwrap();
            drop(db);
            std::fs::rename(replacement, &path).unwrap();
            assert!(state.needs_reload().unwrap());
            state.reload_if_changed().unwrap();
            assert_ne!(
                state.loaded_stamp.as_ref().unwrap().file_identity,
                initial_identity
            );
            assert!(state.search(&query("alpha")).unwrap().is_empty());
            assert_eq!(state.search(&query("beta")).unwrap().len(), 1);
            assert_eq!(state.successful_loads, 2);
            drop(state);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn temporarily_missing_database_is_not_recreated_and_recovers() {
        let (directory, path) = test_database("missing");
        let state =
            RwLock::new(ServiceState::load(path.clone(), BackendPreference::Memory).unwrap());
        let moved = directory.join("temporarily-moved.sqlite");
        std::fs::rename(&path, &moved).unwrap();
        assert!(ensure_fresh(&state).is_err());
        assert!(
            !path.exists(),
            "Reload must not create an empty replacement database"
        );
        assert!(matches!(
            state.read().unwrap().backend,
            Backend::Unavailable
        ));
        std::fs::rename(moved, &path).unwrap();
        ensure_fresh(&state).unwrap();
        assert_eq!(
            state.read().unwrap().search(&query("alpha")).unwrap().len(),
            1
        );
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replacing_with_a_legacy_database_migrates_once_and_recovers() {
        let (directory, path) = test_database("legacy");
        let state =
            RwLock::new(ServiceState::load(path.clone(), BackendPreference::Memory).unwrap());
        let replacement = directory.join("legacy.sqlite");
        let writer = crate::sqlite::Connection::open(&replacement).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE files (
                id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, path_lower TEXT,
                basename TEXT NOT NULL, basename_lower TEXT NOT NULL, ext_lower TEXT,
                kind TEXT NOT NULL, size INTEGER, mtime INTEGER, ctime INTEGER,
                dev INTEGER, inode INTEGER, indexed_at INTEGER NOT NULL
             );
             INSERT INTO files(path,path_lower,basename,basename_lower,kind,indexed_at)
             VALUES('/fixture/beta.txt','/fixture/beta.txt','beta.txt','beta.txt','file',1);",
            )
            .unwrap();
        drop(writer);
        std::fs::rename(replacement, &path).unwrap();
        ensure_fresh(&state).unwrap();
        let loaded = state.read().unwrap();
        assert_eq!(loaded.search(&query("beta")).unwrap().len(), 1);
        assert!(loaded.search(&query("alpha")).unwrap().is_empty());
        assert!(!loaded.needs_reload().unwrap());
        assert_eq!(loaded.successful_loads, 2);
        drop(loaded);
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stamp_and_full_index_are_pinned_to_one_read_snapshot() {
        let (directory, path) = test_database("snapshot");
        let observer = ContentObserver::open(path.clone()).unwrap();
        let reader = Database::open_read_only(path.clone()).unwrap();
        let (loaded_stamp, index) = with_read_snapshot(&reader, |_| {
            // This commit happens after the stamp SELECT pinned the read snapshot.
            let writer = crate::sqlite::Connection::open(&path)?;
            writer.execute_batch("INSERT INTO files(path,path_lower,basename,basename_lower,kind,indexed_at) VALUES('/fixture/beta.txt','/fixture/beta.txt','beta.txt','beta.txt','file',2);")?;
            CompactIndex::load(&reader)
        }).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.search(&query("beta")).is_empty());
        assert_ne!(loaded_stamp, observer.content_stamp().unwrap());
        let (new_stamp, new_index) =
            with_read_snapshot(&reader, |_| CompactIndex::load(&reader)).unwrap();
        assert_eq!(new_index.len(), 2);
        assert_eq!(new_index.search(&query("beta")).len(), 1);
        assert_eq!(new_stamp, observer.content_stamp().unwrap());
        drop(reader);
        drop(observer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn search_slots_are_bounded_and_released_on_drop() {
        let slots = SearchSlots {
            active: Arc::new(AtomicUsize::new(0)),
            limit: 2,
            traffic: Arc::new(TrafficCounters::default()),
        };
        let first = slots.try_acquire().unwrap();
        let second = slots.try_acquire().unwrap();
        assert!(slots.try_acquire().is_none());
        drop(first);
        let replacement = slots.try_acquire().unwrap();
        drop(second);
        drop(replacement);
        assert_eq!(slots.active.load(Ordering::Acquire), 0);
        assert_eq!(slots.traffic.totals(), [0, 0, 0, 3, 3]);
    }

    #[test]
    fn request_traffic_counts_errors_and_unwind_without_leaking_slots() {
        let active = Arc::new(AtomicUsize::new(0));
        let traffic = Arc::new(TrafficCounters::default());
        let first = ActiveRequestGuard::http(&active, 1, &traffic).unwrap();
        assert!(ActiveRequestGuard::http(&active, 1, &traffic).is_none());
        assert_eq!(traffic.totals(), [2, 0, 1, 0, 0]);
        drop(first);
        let failed = || -> Result<()> {
            let _guard = ActiveRequestGuard::http(&active, 1, &traffic).unwrap();
            Err(MacEveryError::Cli("injected handler failure".into()))
        };
        assert!(failed().is_err());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ActiveRequestGuard::http(&active, 1, &traffic).unwrap();
            panic!("injected handler unwind");
        }));
        assert!(unwound.is_err());
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(traffic.totals(), [4, 3, 1, 0, 0]);
    }

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

    #[test]
    fn auth_header_accepts_case_insensitive_bearer_scheme() {
        let request =
            "GET /status HTTP/1.1\r\nhost: 127.0.0.1\r\naUtHoRiZaTiOn: bEaReR secret-token\r\n\r\n";
        assert!(request_has_valid_token(request, "secret-token"));
    }

    #[test]
    fn auth_header_rejects_extra_bearer_parts() {
        let request = "GET /status HTTP/1.1\r\nAuthorization: Bearer secret-token trailing\r\n\r\n";
        assert!(!request_has_valid_token(request, "secret-token"));
    }
}
