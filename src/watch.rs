use crate::error::{MacEveryError, Result};
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use crate::db::Database;
    use crate::process_lifetime;
    use crate::scanner::{refresh_path, refresh_roots, ExcludeMatcher, IndexSummary};
    use std::collections::hash_map::RandomState;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{CStr, CString};
    use std::fs::{self, File, OpenOptions};
    use std::hash::BuildHasher;
    use std::io::{self, Write};
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_void};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    type CFIndex = isize;
    type CFTimeInterval = f64;
    type Boolean = u8;
    type CFAllocatorRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFRunLoopRef = *const c_void;
    type FSEventStreamRef = *mut c_void;
    type ConstFSEventStreamRef = *const c_void;
    type FSEventStreamEventFlags = u32;
    type FSEventStreamEventId = u64;
    type FSEventStreamCreateFlags = u32;
    type FSEventStreamCallback = unsafe extern "C" fn(
        ConstFSEventStreamRef,
        *mut c_void,
        usize,
        *mut c_void,
        *const FSEventStreamEventFlags,
        *const FSEventStreamEventId,
    );

    #[repr(C)]
    struct FSEventStreamContext {
        version: CFIndex,
        info: *mut c_void,
        retain: *const c_void,
        release: *const c_void,
        copy_description: *const c_void,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopDefaultMode: CFStringRef;
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFArrayCreate(
            allocator: CFAllocatorRef,
            values: *const *const c_void,
            num_values: CFIndex,
            callbacks: *const c_void,
        ) -> CFArrayRef;
        fn CFRelease(value: *const c_void);
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopRun();
    }

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *mut FSEventStreamContext,
            paths_to_watch: CFArrayRef,
            since_when: FSEventStreamEventId,
            latency: CFTimeInterval,
            flags: FSEventStreamCreateFlags,
        ) -> FSEventStreamRef;
        fn FSEventStreamScheduleWithRunLoop(
            stream_ref: FSEventStreamRef,
            run_loop: CFRunLoopRef,
            run_loop_mode: CFStringRef,
        );
        fn FSEventStreamStart(stream_ref: FSEventStreamRef) -> Boolean;
        fn FSEventStreamStop(stream_ref: FSEventStreamRef);
        fn FSEventStreamInvalidate(stream_ref: FSEventStreamRef);
        fn FSEventStreamRelease(stream_ref: FSEventStreamRef);
        fn FSEventsGetCurrentEventId() -> FSEventStreamEventId;
    }

    const UTF8_ENCODING: u32 = 0x0800_0100;
    const SINCE_NOW: FSEventStreamEventId = u64::MAX;
    const CREATE_FLAG_NO_DEFER: FSEventStreamCreateFlags = 0x0000_0002;
    const CREATE_FLAG_WATCH_ROOT: FSEventStreamCreateFlags = 0x0000_0004;
    const CREATE_FLAG_FILE_EVENTS: FSEventStreamCreateFlags = 0x0000_0010;

    const FLAG_MUST_SCAN_SUBDIRS: FSEventStreamEventFlags = 0x0000_0001;
    const FLAG_USER_DROPPED: FSEventStreamEventFlags = 0x0000_0002;
    const FLAG_KERNEL_DROPPED: FSEventStreamEventFlags = 0x0000_0004;
    const FLAG_IDS_WRAPPED: FSEventStreamEventFlags = 0x0000_0008;
    const FLAG_HISTORY_DONE: FSEventStreamEventFlags = 0x0000_0010;
    const FLAG_ROOT_CHANGED: FSEventStreamEventFlags = 0x0000_0020;
    const META_LAST_EVENT_ID: &str = "watch_last_event_id";

    #[derive(Default)]
    struct FilterCounters {
        received: AtomicU64,
        rejected: AtomicU64,
        flags: AtomicU32,
    }

    #[derive(Clone, Debug)]
    struct FsEvent {
        path: String,
        flags: FSEventStreamEventFlags,
        id: FSEventStreamEventId,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum WatchAction {
        RefreshPaths,
        RescanRoots,
    }

    #[derive(Clone)]
    struct EventFilter {
        db_path: PathBuf,
        db_dir: Option<PathBuf>,
        excludes: ExcludeMatcher,
        counters: Arc<FilterCounters>,
    }

    impl EventFilter {
        fn new(db_path: &Path, excludes: &[String]) -> Self {
            Self {
                db_path: db_path.to_path_buf(),
                db_dir: db_path.parent().map(PathBuf::from),
                excludes: ExcludeMatcher::new(excludes),
                counters: Arc::new(FilterCounters::default()),
            }
        }

        fn accepts(&self, path: &str, flags: FSEventStreamEventFlags) -> bool {
            let global_recovery = action_for_flags(flags) == WatchAction::RescanRoots;
            // Our own WAL/diagnostic writes must not generate even a periodic
            // filter-summary write. Global signals still bypass this guard.
            if !global_recovery
                && is_ignored_event_path(path, &self.db_path, self.db_dir.as_deref())
            {
                return false;
            }
            self.counters.received.fetch_add(1, Ordering::Relaxed);
            self.counters.flags.fetch_or(flags, Ordering::Relaxed);
            // Recovery takes precedence even when combined with history/excludes.
            let accepted = global_recovery
                || ((flags & FLAG_HISTORY_DONE == 0 || flags & FLAG_MUST_SCAN_SUBDIRS != 0)
                    && !self.excludes.is_excluded(Path::new(path)));
            if !accepted {
                self.counters.rejected.fetch_add(1, Ordering::Relaxed);
            }
            accepted
        }
    }

    struct CallbackState {
        sender: Sender<FsEvent>,
        filter: EventFilter,
    }

    #[derive(Default)]
    struct PendingWork {
        paths: BTreeMap<String, FSEventStreamEventFlags>,
        retry_paths: BTreeMap<String, FSEventStreamEventFlags>,
        rescan_roots: bool,
        retry_roots: bool,
        max_event_id: FSEventStreamEventId,
        deferred_cleanups: BTreeMap<PathBuf, i64>,
        checkpoint_reset: bool,
        checkpoint_invalidated: bool,
        scope_fallback: bool,
        event_count: u64,
        raw_flags: u32,
        first_events: [Option<EventSample>; 3],
        last_event: Option<EventSample>,
        recovery_event: Option<EventSample>,
        hash_state: RandomState,
    }

    #[derive(Clone, Copy)]
    struct EventSample {
        flags: u32,
        id: u64,
        path_hash: u64,
    }

    impl EventSample {
        fn json(self) -> String {
            format!(
                r#"{{"flags":{},"id":{},"path_hash":"{:016x}"}}"#,
                self.flags, self.id, self.path_hash
            )
        }
    }

    impl PendingWork {
        fn add(&mut self, event: FsEvent) {
            self.record_event(&event);
            if action_for_flags(event.flags) == WatchAction::RescanRoots {
                self.rescan_roots = true;
                self.paths.clear();
            } else if !self.rescan_roots {
                self.paths
                    .entry(event.path)
                    .and_modify(|flags| *flags |= event.flags)
                    .or_insert(event.flags);
            }
        }

        fn record_event(&mut self, event: &FsEvent) {
            if event.flags & FLAG_IDS_WRAPPED != 0 {
                self.max_event_id = event.id;
                self.checkpoint_reset = true;
                self.checkpoint_invalidated = false;
            } else {
                self.max_event_id = self.max_event_id.max(event.id);
            }
            let sample = EventSample {
                flags: event.flags,
                id: event.id,
                path_hash: self.hash_state.hash_one(&event.path),
            };
            if self.event_count < self.first_events.len() as u64 {
                self.first_events[self.event_count as usize] = Some(sample);
            }
            self.event_count = self.event_count.saturating_add(1);
            self.raw_flags |= event.flags;
            self.last_event = Some(sample);
            if self.recovery_event.is_none()
                && action_for_flags(event.flags) == WatchAction::RescanRoots
            {
                self.recovery_event = Some(sample);
            }
        }

        fn add_scoped(&mut self, event: FsEvent, roots: &[PathBuf]) {
            if action_for_flags(event.flags) == WatchAction::RescanRoots
                || event.flags & FLAG_MUST_SCAN_SUBDIRS == 0
            {
                self.add(event);
                return;
            }
            self.record_event(&event);
            match subtree_targets(Path::new(&event.path), roots) {
                Some(paths) if !self.rescan_roots => {
                    for path in paths {
                        self.paths
                            .entry(path)
                            .and_modify(|flags| *flags |= event.flags)
                            .or_insert(event.flags);
                    }
                }
                Some(_) => {}
                None => {
                    self.scope_fallback = true;
                    self.rescan_roots = true;
                    self.paths.clear();
                    self.recovery_event = self.last_event;
                }
            }
        }

        fn clear_diagnostics(&mut self) {
            self.scope_fallback = false;
            self.event_count = 0;
            self.raw_flags = 0;
            self.first_events = [None; 3];
            self.last_event = None;
            self.recovery_event = None;
        }

        fn has_retries(&self) -> bool {
            self.retry_roots || !self.retry_paths.is_empty()
        }

        fn has_work(&self) -> bool {
            self.rescan_roots || !self.paths.is_empty() || self.has_retries()
        }

        fn has_pending(&self) -> bool {
            self.has_work()
                || self.max_event_id > 0
                || self.checkpoint_reset
                || !self.deferred_cleanups.is_empty()
        }
    }

    fn subtree_targets(path: &Path, roots: &[PathBuf]) -> Option<Vec<String>> {
        let known_namespace = |path: &Path| {
            path.is_absolute()
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        };
        if !known_namespace(path) || roots.iter().any(|root| !known_namespace(root)) {
            return None;
        }
        let mut targets = Vec::new();
        for root in roots {
            if path.starts_with(root) {
                targets.push(path.to_string_lossy().into_owned());
            } else if root.starts_with(path) {
                targets.push(root.to_string_lossy().into_owned());
            }
        }
        // FSEvents can report /private/tmp for configured /tmp (and vice versa).
        // With no lexical intersection, absence of work is not evidence that
        // this event is outside the configured namespaces. Recover conservatively
        // using those roots; do not rewrite their persistent path namespace.
        if targets.is_empty() {
            None
        } else {
            Some(fold_refresh_paths(targets))
        }
    }

    const LOG_FILE_BYTES: u64 = 256 * 1024;
    const LOG_RECORD_BYTES: usize = 2048;
    const LOG_WINDOW: Duration = Duration::from_secs(60);
    const LOG_RECORDS_PER_WINDOW: u32 = 120;
    const LOG_FILE_NAME: &str = "macevery-watch-trigger.jsonl";

    // Values from the macOS SDK sys/fcntl.h. No new runtime dependency.
    const OPEN_NOFOLLOW: i32 = 0x100;
    const LOCK_EXCLUSIVE_NONBLOCKING: i32 = 0x02 | 0x04;
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    #[derive(Default)]
    struct LogInfo {
        retry_due: bool,
        indexed: u64,
        written: u64,
        unchanged: u64,
        stale_removed: u64,
        warnings: usize,
        elapsed_ms: u128,
        received: u64,
        rejected: u64,
        received_flags: u32,
    }

    impl LogInfo {
        fn scanned(summary: &IndexSummary, started: Instant) -> Self {
            Self {
                indexed: summary.indexed,
                written: summary.rows_written,
                unchanged: summary.unchanged,
                stale_removed: summary.deleted,
                warnings: summary.warnings.len(),
                elapsed_ms: started.elapsed().as_millis(),
                ..Self::default()
            }
        }
    }

    struct TriggerLog {
        path: PathBuf,
        window: Instant,
        records: u32,
        suppressed: u64,
        oversized: u64,
        io_failures: u64,
        unavailable_until: Option<Instant>,
    }

    impl TriggerLog {
        fn new(db_path: &Path) -> Self {
            Self {
                path: db_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(LOG_FILE_NAME),
                window: Instant::now(),
                records: 0,
                suppressed: 0,
                oversized: 0,
                io_failures: 0,
                unavailable_until: None,
            }
        }

        fn record(
            &mut self,
            kind: &str,
            scope: &str,
            pending: &PendingWork,
            path: Option<&Path>,
            info: LogInfo,
        ) {
            let now = Instant::now();
            if now.duration_since(self.window) >= LOG_WINDOW {
                self.window = now;
                self.records = 0;
            }
            // Leave twenty slots for recovery starts/outcomes during ordinary storms.
            let priority = scope == "roots" || pending.checkpoint_reset;
            let limit = if priority {
                LOG_RECORDS_PER_WINDOW
            } else {
                LOG_RECORDS_PER_WINDOW - 20
            };
            if self.records >= limit || self.unavailable_until.is_some_and(|until| now < until) {
                self.suppressed = self.suppressed.saturating_add(1);
                return;
            }
            let first = pending
                .first_events
                .iter()
                .flatten()
                .map(|event| event.json())
                .collect::<Vec<_>>()
                .join(",");
            let last = pending
                .last_event
                .map_or_else(|| "null".to_string(), EventSample::json);
            let recovery = pending
                .recovery_event
                .map_or_else(|| "null".to_string(), EventSample::json);
            let sampled = pending.event_count.min(3) + u64::from(pending.event_count > 3);
            let path_hash = path
                .map(|value| {
                    pending
                        .hash_state
                        .hash_one(value.to_string_lossy().as_ref())
                })
                .unwrap_or(0);
            let at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_millis())
                .unwrap_or(0);
            let line = format!(concat!(
                "{{\"v\":1,\"at_ms\":{},\"pid\":{},\"kind\":\"{}\",\"scope\":\"{}\",\"scope_hash\":\"{:016x}\",",
                "\"flags_or\":{},\"event_count\":{},\"first_events\":[{}],\"last_event\":{},\"recovery_event\":{},\"events_outside_first_last\":{},",
                "\"watermark\":{},\"wrap_pending\":{},\"scope_fallback\":{},\"paths\":{},\"retry_paths\":{},\"retry_roots\":{},\"retry_due\":{},",
                "\"indexed\":{},\"rows_written\":{},\"unchanged\":{},\"stale_removed\":{},\"warnings\":{},\"elapsed_ms\":{},",
                "\"received\":{},\"rejected\":{},\"received_flags_or\":{},\"suppressed_records\":{},\"oversized_records\":{},\"io_failures\":{}}}\n"),
                at_ms, std::process::id(), kind, scope, path_hash, pending.raw_flags, pending.event_count,
                first, last, recovery, pending.event_count.saturating_sub(sampled), pending.max_event_id,
                pending.checkpoint_reset, pending.scope_fallback, pending.paths.len(), pending.retry_paths.len(), pending.retry_roots,
                info.retry_due, info.indexed, info.written, info.unchanged, info.stale_removed, info.warnings, info.elapsed_ms,
                info.received, info.rejected, info.received_flags, self.suppressed, self.oversized, self.io_failures);
            if line.len() > LOG_RECORD_BYTES {
                self.oversized = self.oversized.saturating_add(1);
                return;
            }
            self.records += 1;
            match append_trigger_record(&self.path, line.as_bytes()) {
                Ok(()) => {
                    self.suppressed = 0;
                    self.oversized = 0;
                    self.io_failures = 0;
                    self.unavailable_until = None;
                }
                Err(_) => {
                    self.io_failures = self.io_failures.saturating_add(1);
                    self.suppressed = self.suppressed.saturating_add(1);
                    self.unavailable_until = Some(now + LOG_WINDOW);
                    eprintln!("watch: trigger diagnostics unavailable; indexing continues, retrying diagnostics after 60s");
                }
            }
        }
    }

    fn private_log_file(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(OPEN_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic target must be a private regular file",
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }

    fn append_trigger_record(path: &Path, record: &[u8]) -> io::Result<()> {
        if record.len() > LOG_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic record too large",
            ));
        }
        let lock = private_log_file(&path.with_extension("lock"))?;
        if unsafe { flock(lock.as_raw_fd(), LOCK_EXCLUSIVE_NONBLOCKING) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = private_log_file(path)?;
        let size = file.metadata()?.len();
        if size > LOG_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pre-existing diagnostic file exceeds bound",
            ));
        }
        if size + record.len() as u64 > LOG_FILE_BYTES {
            let previous = path.with_extension("jsonl.1");
            match fs::symlink_metadata(&previous) {
                Ok(metadata) => {
                    if !metadata.is_file()
                        || metadata.nlink() != 1
                        || metadata.len() > LOG_FILE_BYTES
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "unsafe diagnostic rotation target",
                        ));
                    }
                    fs::remove_file(&previous)?;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            drop(file);
            fs::rename(path, previous)?;
            file = private_log_file(path)?;
        }
        file.write_all(record)
        // Closing the stable lock descriptor releases flock; no blocking fsync.
    }

    struct CfReleaseGuard {
        value: *const c_void,
    }

    impl CfReleaseGuard {
        fn new(value: *const c_void) -> Self {
            Self { value }
        }

        fn as_ptr(&self) -> *const c_void {
            self.value
        }
    }

    impl Drop for CfReleaseGuard {
        fn drop(&mut self) {
            if !self.value.is_null() {
                unsafe {
                    CFRelease(self.value);
                }
            }
        }
    }

    struct CallbackStateGuard {
        state: *mut CallbackState,
    }

    impl CallbackStateGuard {
        fn new(sender: Sender<FsEvent>, filter: EventFilter) -> Self {
            Self {
                state: Box::into_raw(Box::new(CallbackState { sender, filter })),
            }
        }

        fn as_info(&self) -> *mut c_void {
            self.state.cast()
        }
    }

    impl Drop for CallbackStateGuard {
        fn drop(&mut self) {
            if !self.state.is_null() {
                unsafe {
                    drop(Box::from_raw(self.state));
                }
            }
        }
    }

    struct FSEventStreamGuard {
        stream: FSEventStreamRef,
        started: bool,
    }

    impl FSEventStreamGuard {
        fn new(stream: FSEventStreamRef) -> Option<Self> {
            if stream.is_null() {
                None
            } else {
                Some(Self {
                    stream,
                    started: false,
                })
            }
        }

        fn as_ptr(&self) -> FSEventStreamRef {
            self.stream
        }

        fn start(&mut self) -> bool {
            if unsafe { FSEventStreamStart(self.stream) } == 0 {
                false
            } else {
                self.started = true;
                true
            }
        }
    }

    impl Drop for FSEventStreamGuard {
        fn drop(&mut self) {
            if self.stream.is_null() {
                return;
            }
            unsafe {
                if self.started {
                    FSEventStreamStop(self.stream);
                }
                FSEventStreamInvalidate(self.stream);
                FSEventStreamRelease(self.stream);
            }
            self.stream = ptr::null_mut();
        }
    }

    pub fn watch_index(db_path: PathBuf) -> Result<()> {
        process_lifetime::exit_when_parent_dies_if_requested();

        let db = Database::open(db_path.clone())?;
        let (roots, excludes) = db.roots_and_excludes()?;
        let stored_event_id = db
            .get_meta(META_LAST_EVENT_ID)?
            .and_then(|value| value.parse::<FSEventStreamEventId>().ok());
        drop(db);

        if roots.is_empty() {
            return Err(MacEveryError::Cli(
                "no indexed roots found; run `macevery index --rebuild PATHS...` first".to_string(),
            ));
        }

        let roots = roots.into_iter().map(PathBuf::from).collect::<Vec<_>>();
        let excludes = runtime_excludes(excludes, &db_path);

        let current_system_id = unsafe { FSEventsGetCurrentEventId() };
        let decision = resolve_since_when(stored_event_id, current_system_id);
        let mut initial = PendingWork::default();
        let mut diagnostics = TriggerLog::new(&db_path);
        let since_when = match decision {
            CheckpointDecision::UseCheckpoint(id) => id,
            CheckpointDecision::FullRescan => {
                initial.max_event_id = current_system_id;
                eprintln!(
                    "watch: checkpoint invalid (stored={:?}, system={}), performing full rescan",
                    stored_event_id, current_system_id
                );
                diagnostics.record(
                    "startup_invalid_checkpoint",
                    "roots",
                    &initial,
                    None,
                    LogInfo::default(),
                );
                let started = Instant::now();
                let db = Database::open(db_path.clone())?;
                let summary = match refresh_roots(&db, &roots, &excludes) {
                    Ok(summary) => summary,
                    Err(err) => {
                        diagnostics.record(
                            "scan_failed",
                            "roots",
                            &initial,
                            None,
                            LogInfo::default(),
                        );
                        return Err(err);
                    }
                };
                diagnostics.record(
                    if summary.warnings.is_empty() {
                        "scan_complete"
                    } else {
                        "scan_incomplete"
                    },
                    "roots",
                    &initial,
                    None,
                    LogInfo::scanned(&summary, started),
                );
                let counts = (summary.indexed, summary.deleted);
                if summary.warnings.is_empty() {
                    if let Err(err) =
                        db.set_meta(META_LAST_EVENT_ID, &current_system_id.to_string())
                    {
                        diagnostics.record(
                            "checkpoint_failed",
                            "checkpoint",
                            &initial,
                            None,
                            LogInfo::default(),
                        );
                        return Err(err);
                    }
                    diagnostics.record(
                        "checkpoint_complete",
                        "checkpoint",
                        &initial,
                        None,
                        LogInfo::default(),
                    );
                    initial.max_event_id = 0;
                } else {
                    log_scan_warnings("initial root rescan", &summary);
                    retain_incomplete(&mut initial, summary);
                    if !initial.has_work() {
                        initial.retry_roots = true;
                    }
                }
                eprintln!(
                    "watch: initial full rescan finished, {} entries, {} stale removed",
                    counts.0, counts.1
                );
                drop(db);
                current_system_id
            }
        };
        let filter = EventFilter::new(&db_path, &excludes);
        let (sender, receiver) = mpsc::channel::<FsEvent>();
        spawn_processor(
            db_path,
            roots.clone(),
            excludes,
            receiver,
            initial,
            diagnostics,
            Arc::clone(&filter.counters),
        );

        let mut root_strings = Vec::new();
        let mut root_values = Vec::new();
        for root in &roots {
            let c_path = CString::new(root.to_string_lossy().as_bytes())
                .map_err(|_| MacEveryError::Cli("watch root contains NUL byte".to_string()))?;
            let cf_string =
                unsafe { CFStringCreateWithCString(ptr::null(), c_path.as_ptr(), UTF8_ENCODING) };
            if cf_string.is_null() {
                return Err(MacEveryError::Cli(format!(
                    "failed to create CFString for {}",
                    root.to_string_lossy()
                )));
            }
            root_strings.push(CfReleaseGuard::new(cf_string));
            root_values.push(cf_string);
        }

        let paths = CfReleaseGuard::new(unsafe {
            CFArrayCreate(
                ptr::null(),
                root_values.as_ptr(),
                root_values.len() as CFIndex,
                ptr::null(),
            )
        });
        if paths.as_ptr().is_null() {
            return Err(MacEveryError::Cli(
                "failed to create FSEvents root array".to_string(),
            ));
        }

        let state = CallbackStateGuard::new(sender, filter);
        let mut context = FSEventStreamContext {
            version: 0,
            info: state.as_info(),
            retain: ptr::null(),
            release: ptr::null(),
            copy_description: ptr::null(),
        };

        let flags = CREATE_FLAG_NO_DEFER | CREATE_FLAG_WATCH_ROOT | CREATE_FLAG_FILE_EVENTS;
        let stream = unsafe {
            FSEventStreamCreate(
                ptr::null(),
                event_callback,
                &mut context,
                paths.as_ptr(),
                since_when,
                0.35,
                flags,
            )
        };
        let mut stream = FSEventStreamGuard::new(stream)
            .ok_or_else(|| MacEveryError::Cli("failed to create FSEventStream".to_string()))?;

        unsafe {
            FSEventStreamScheduleWithRunLoop(
                stream.as_ptr(),
                CFRunLoopGetCurrent(),
                kCFRunLoopDefaultMode,
            );
        }
        if !stream.start() {
            return Err(MacEveryError::Cli(
                "failed to start FSEventStream".to_string(),
            ));
        }

        if since_when == SINCE_NOW {
            eprintln!("watching {} indexed roots from now", roots.len());
        } else {
            eprintln!(
                "watching {} indexed roots from event checkpoint {}",
                roots.len(),
                since_when
            );
        }
        unsafe {
            CFRunLoopRun();
        }
        Ok(())
    }

    const BATCH_IDLE: Duration = Duration::from_millis(150);
    const MAX_BATCH_WAIT: Duration = Duration::from_secs(1);
    const MAX_BATCH_EVENTS: usize = 4096;
    const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);
    const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

    fn spawn_processor(
        db_path: PathBuf,
        roots: Vec<PathBuf>,
        excludes: Vec<String>,
        receiver: Receiver<FsEvent>,
        mut pending: PendingWork,
        mut diagnostics: TriggerLog,
        counters: Arc<FilterCounters>,
    ) {
        thread::spawn(move || {
            let db = match Database::open(db_path) {
                Ok(db) => db,
                Err(err) => {
                    diagnostics.record(
                        "database_open_failed",
                        "none",
                        &pending,
                        None,
                        LogInfo::default(),
                    );
                    eprintln!("watch: failed to open database: {err}");
                    return;
                }
            };
            let mut retry_delay = MIN_RETRY_DELAY;
            let mut retry_at = pending.has_pending().then(|| Instant::now() + retry_delay);

            loop {
                let first = if pending.has_pending() {
                    let wait = retry_at
                        .unwrap_or_else(Instant::now)
                        .saturating_duration_since(Instant::now());
                    receiver.recv_timeout(wait)
                } else {
                    // One wake/minute also reports excluded-only storms; no DB access.
                    receiver.recv_timeout(LOG_WINDOW)
                };
                let mut disconnected = false;
                match first {
                    Ok(event) => {
                        pending.add_scoped(event, &roots);
                        let deadline = Instant::now() + MAX_BATCH_WAIT;
                        // Bound both latency and the number drained under constant writes.
                        for _ in 1..MAX_BATCH_EVENTS {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            match receiver.recv_timeout(BATCH_IDLE.min(remaining)) {
                                Ok(event) => pending.add_scoped(event, &roots),
                                Err(RecvTimeoutError::Timeout) => break,
                                Err(RecvTimeoutError::Disconnected) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => disconnected = true,
                }
                let retry_due =
                    disconnected || retry_at.is_none_or(|deadline| Instant::now() >= deadline);
                let received = counters.received.swap(0, Ordering::Relaxed);
                let rejected = counters.rejected.swap(0, Ordering::Relaxed);
                let received_flags = counters.flags.swap(0, Ordering::Relaxed);
                if received > 0 || diagnostics.suppressed > 0 || diagnostics.io_failures > 0 {
                    diagnostics.record(
                        "filter_summary",
                        "callback_interval",
                        &pending,
                        None,
                        LogInfo {
                            received,
                            rejected,
                            received_flags,
                            ..LogInfo::default()
                        },
                    );
                }
                let complete = if invalidate_wrapped_checkpoint(&db, &mut pending).is_ok() {
                    process_pending_logged(
                        &mut pending,
                        retry_due,
                        |path| refresh_path(&db, path, &excludes),
                        || refresh_roots(&db, &roots, &excludes),
                        Some(&mut diagnostics),
                    );
                    persist_checkpoint_logged(&db, &mut pending, Some(&mut diagnostics))
                } else {
                    diagnostics.record(
                        "checkpoint_reset_failed",
                        "checkpoint",
                        &pending,
                        None,
                        LogInfo::default(),
                    );
                    false
                };
                if complete {
                    retry_delay = MIN_RETRY_DELAY;
                    retry_at = None;
                } else if retry_due || retry_at.is_none() {
                    retry_at = Some(Instant::now() + retry_delay);
                    retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                }
                if disconnected {
                    return;
                }
            }
        });
    }

    fn log_scan_warnings(context: &str, summary: &IndexSummary) {
        eprintln!(
            "watch: {context} incomplete ({} warnings), retry retained",
            summary.warnings.len()
        );
        for warning in summary.warnings.iter().take(3) {
            eprintln!("watch: {warning}");
        }
    }

    #[cfg(test)]
    fn process_pending<F, R>(pending: &mut PendingWork, retry_due: bool, refresh: F, rescan: R)
    where
        F: FnMut(&Path) -> Result<IndexSummary>,
        R: FnMut() -> Result<IndexSummary>,
    {
        process_pending_logged(pending, retry_due, refresh, rescan, None);
    }

    fn process_pending_logged<F, R>(
        pending: &mut PendingWork,
        retry_due: bool,
        mut refresh: F,
        mut rescan: R,
        mut diagnostics: Option<&mut TriggerLog>,
    ) where
        F: FnMut(&Path) -> Result<IndexSummary>,
        R: FnMut() -> Result<IndexSummary>,
    {
        let retrying = retry_due && pending.has_retries();
        if retry_due {
            pending.paths.append(&mut pending.retry_paths);
            pending.rescan_roots |= std::mem::take(&mut pending.retry_roots);
        }
        if std::mem::take(&mut pending.rescan_roots) {
            let started = Instant::now();
            log_work(
                &mut diagnostics,
                "scan_start",
                "roots",
                pending,
                None,
                LogInfo {
                    retry_due: retrying,
                    ..LogInfo::default()
                },
            );
            match rescan() {
                Ok(summary) if summary.warnings.is_empty() => {
                    log_work(
                        &mut diagnostics,
                        "scan_complete",
                        "roots",
                        pending,
                        None,
                        LogInfo::scanned(&summary, started),
                    );
                    eprintln!(
                        "watch: rescanned roots, {} entries, {} stale removed",
                        summary.indexed, summary.deleted
                    );
                    pending.paths.clear();
                    pending.retry_paths.clear();
                    pending.retry_roots = false;
                    return;
                }
                Ok(summary) => {
                    log_work(
                        &mut diagnostics,
                        "scan_incomplete",
                        "roots",
                        pending,
                        None,
                        LogInfo::scanned(&summary, started),
                    );
                    log_scan_warnings("root rescan", &summary);
                    pending.paths.clear();
                    pending.retry_paths.clear();
                    pending.retry_roots = summary.retry_paths.is_empty();
                    retain_incomplete(pending, summary);
                    return;
                }
                Err(err) => {
                    log_work(
                        &mut diagnostics,
                        "scan_failed",
                        "roots",
                        pending,
                        None,
                        LogInfo {
                            elapsed_ms: started.elapsed().as_millis(),
                            ..LogInfo::default()
                        },
                    );
                    eprintln!("watch: root rescan failed: {err}; retry retained");
                    pending.retry_roots = true;
                    // Fresh paths can still progress while a whole-root retry waits.
                }
            }
        }
        let paths = fold_refresh_paths(pending.paths.keys().cloned().collect());
        pending.paths.clear();
        let mut indexed = 0_u64;
        let mut deleted = 0_u64;
        let total = paths.len();
        for path in paths {
            let started = Instant::now();
            log_work(
                &mut diagnostics,
                "scan_start",
                "subtree",
                pending,
                Some(Path::new(&path)),
                LogInfo {
                    retry_due: retrying,
                    ..LogInfo::default()
                },
            );
            let retry = match refresh(Path::new(&path)) {
                Ok(summary) if summary.warnings.is_empty() => {
                    log_work(
                        &mut diagnostics,
                        "scan_complete",
                        "subtree",
                        pending,
                        Some(Path::new(&path)),
                        LogInfo::scanned(&summary, started),
                    );
                    indexed += summary.indexed;
                    deleted += summary.deleted;
                    pending
                        .retry_paths
                        .retain(|old, _| !Path::new(old).starts_with(&path));
                    false
                }
                Ok(summary) => {
                    log_work(
                        &mut diagnostics,
                        "scan_incomplete",
                        "subtree",
                        pending,
                        Some(Path::new(&path)),
                        LogInfo::scanned(&summary, started),
                    );
                    log_scan_warnings(&path, &summary);
                    let retry_original = summary.retry_paths.is_empty();
                    retain_incomplete(pending, summary);
                    retry_original
                }
                Err(err) => {
                    log_work(
                        &mut diagnostics,
                        "scan_failed",
                        "subtree",
                        pending,
                        Some(Path::new(&path)),
                        LogInfo {
                            elapsed_ms: started.elapsed().as_millis(),
                            ..LogInfo::default()
                        },
                    );
                    eprintln!("watch: refresh failed for {path}: {err}; retry retained");
                    true
                }
            };
            if retry {
                pending.retry_paths.insert(path, 0);
            }
        }
        if total > 0 {
            eprintln!("watch: processed {total} paths ({} pending retries, {indexed} entries, {deleted} stale removed)", pending.retry_paths.len());
        }
    }

    fn log_work(
        diagnostics: &mut Option<&mut TriggerLog>,
        kind: &str,
        scope: &str,
        pending: &PendingWork,
        path: Option<&Path>,
        info: LogInfo,
    ) {
        if let Some(log) = diagnostics.as_deref_mut() {
            log.record(kind, scope, pending, path, info);
        }
    }

    fn retain_incomplete(pending: &mut PendingWork, summary: IndexSummary) {
        for path in summary.retry_paths {
            pending
                .retry_paths
                .insert(path.to_string_lossy().into_owned(), 0);
        }
        for (path, generation) in summary.deferred_cleanups {
            pending
                .deferred_cleanups
                .entry(path)
                .and_modify(|previous| *previous = (*previous).max(generation))
                .or_insert(generation);
        }
    }

    fn invalidate_wrapped_checkpoint(db: &Database, pending: &mut PendingWork) -> Result<()> {
        if pending.checkpoint_reset && !pending.checkpoint_invalidated {
            // Old numeric IDs cannot survive a crash after an epoch wrap.
            db.set_meta(META_LAST_EVENT_ID, "0")?;
            pending.checkpoint_invalidated = true;
        }
        Ok(())
    }

    #[cfg(test)]
    fn persist_checkpoint(db: &Database, pending: &mut PendingWork) -> bool {
        persist_checkpoint_logged(db, pending, None)
    }

    fn persist_checkpoint_logged(
        db: &Database,
        pending: &mut PendingWork,
        mut diagnostics: Option<&mut TriggerLog>,
    ) -> bool {
        if invalidate_wrapped_checkpoint(db, pending).is_err() {
            log_work(
                &mut diagnostics,
                "checkpoint_reset_failed",
                "checkpoint",
                pending,
                None,
                LogInfo::default(),
            );
            return false;
        }
        // A later successful batch cannot advance past an earlier failed path.
        if pending.has_work() {
            log_work(
                &mut diagnostics,
                "checkpoint_blocked",
                "checkpoint",
                pending,
                None,
                LogInfo::default(),
            );
            return false;
        }
        if !pending.deferred_cleanups.is_empty() {
            let cleanup = (|| {
                db.begin()?;
                for (path, generation) in &pending.deferred_cleanups {
                    db.delete_stale_under_roots(std::slice::from_ref(path), *generation)?;
                }
                db.commit()
            })();
            if let Err(err) = cleanup {
                let _ = db.rollback();
                eprintln!("watch: deferred cleanup failed: {err}; retry retained");
                log_work(
                    &mut diagnostics,
                    "cleanup_failed",
                    "checkpoint",
                    pending,
                    None,
                    LogInfo::default(),
                );
                return false;
            }
            pending.deferred_cleanups.clear();
        }
        if pending.max_event_id > 0 || pending.checkpoint_reset {
            if let Err(err) = db.set_meta(META_LAST_EVENT_ID, &pending.max_event_id.to_string()) {
                eprintln!("watch: failed to persist event checkpoint: {err}; retry retained");
                log_work(
                    &mut diagnostics,
                    "checkpoint_failed",
                    "checkpoint",
                    pending,
                    None,
                    LogInfo::default(),
                );
                return false;
            }
            log_work(
                &mut diagnostics,
                "checkpoint_complete",
                "checkpoint",
                pending,
                None,
                LogInfo::default(),
            );
            pending.max_event_id = 0;
            pending.checkpoint_reset = false;
            pending.checkpoint_invalidated = false;
        }
        pending.clear_diagnostics();
        true
    }

    fn runtime_excludes(mut excludes: Vec<String>, db_path: &Path) -> Vec<String> {
        for pattern in [".local/share/macevery", ".codex", ".gemini", ".cursor"] {
            push_exclude_once(&mut excludes, pattern.to_string());
        }
        if let Some(parent) = db_path.parent() {
            push_exclude_once(&mut excludes, parent.to_string_lossy().to_string());
        }
        excludes
    }

    fn push_exclude_once(excludes: &mut Vec<String>, pattern: String) {
        if !excludes.iter().any(|value| value == &pattern) {
            excludes.push(pattern);
        }
    }

    fn is_ignored_event_path(path: &str, db_path: &Path, db_dir: Option<&Path>) -> bool {
        let event_path = Path::new(path);
        if event_path == db_path {
            return true;
        }
        if let Some(db_dir) = db_dir {
            if event_path == db_dir || event_path.starts_with(db_dir) {
                return true;
            }
        }
        false
    }

    fn fold_refresh_paths(mut paths: Vec<String>) -> Vec<String> {
        paths.sort();
        let mut ancestors = BTreeSet::<String>::new();
        let mut folded = Vec::new();
        for path in paths {
            let covered = Path::new(&path).ancestors().any(|ancestor| {
                ancestor
                    .to_str()
                    .is_some_and(|value| ancestors.contains(value))
            });
            if !covered {
                ancestors.insert(path.clone());
                folded.push(path);
            }
        }
        folded
    }

    #[cfg(test)]
    fn batch_action(batch: &[FsEvent]) -> WatchAction {
        if batch
            .iter()
            .any(|event| action_for_flags(event.flags) == WatchAction::RescanRoots)
        {
            WatchAction::RescanRoots
        } else {
            WatchAction::RefreshPaths
        }
    }

    fn action_for_flags(flags: FSEventStreamEventFlags) -> WatchAction {
        if flags & (FLAG_USER_DROPPED | FLAG_KERNEL_DROPPED | FLAG_IDS_WRAPPED | FLAG_ROOT_CHANGED)
            != 0
        {
            WatchAction::RescanRoots
        } else {
            WatchAction::RefreshPaths
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CheckpointDecision {
        UseCheckpoint(FSEventStreamEventId),
        FullRescan,
    }

    fn resolve_since_when(
        stored: Option<FSEventStreamEventId>,
        current_system_id: FSEventStreamEventId,
    ) -> CheckpointDecision {
        let Some(stored_id) = stored else {
            return CheckpointDecision::FullRescan;
        };
        if stored_id == 0 || stored_id == SINCE_NOW {
            return CheckpointDecision::FullRescan;
        }
        if stored_id > current_system_id {
            return CheckpointDecision::FullRescan;
        }
        CheckpointDecision::UseCheckpoint(stored_id)
    }

    unsafe extern "C" fn event_callback(
        _stream_ref: ConstFSEventStreamRef,
        client_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const FSEventStreamEventFlags,
        event_ids: *const FSEventStreamEventId,
    ) {
        if client_info.is_null() || event_paths.is_null() || event_flags.is_null() {
            return;
        }

        let state = &*(client_info.cast::<CallbackState>());
        let paths = event_paths.cast::<*const c_char>();
        for index in 0..num_events {
            let path_ptr = *paths.add(index);
            if path_ptr.is_null() {
                continue;
            }
            let path = CStr::from_ptr(path_ptr).to_string_lossy();
            let flags = *event_flags.add(index);
            if !state.filter.accepts(&path, flags) {
                continue;
            }
            let path = path.into_owned();
            let id = if event_ids.is_null() {
                0
            } else {
                *event_ids.add(index)
            };
            let _ = state.sender.send(FsEvent { path, flags, id });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_dir(label: &str) -> PathBuf {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            std::env::temp_dir().join(format!("macevery-{label}-{}-{unique}", std::process::id()))
        }

        fn event(path: &Path, id: u64) -> FsEvent {
            FsEvent {
                path: path.to_string_lossy().into_owned(),
                flags: 0,
                id,
            }
        }

        fn process_actual(db: &Database, roots: &[PathBuf], pending: &mut PendingWork) {
            process_pending(
                pending,
                true,
                |path| refresh_path(db, path, &[]),
                || refresh_roots(db, roots, &[]),
            );
        }

        #[test]
        fn callback_excludes_noise_before_queue_but_keeps_recovery_events() {
            let (sender, receiver) = mpsc::channel();
            let state = CallbackState {
                sender,
                filter: EventFilter::new(
                    Path::new("/private/tmp/index-state/index.sqlite"),
                    &[".git".to_string(), "node_modules".to_string()],
                ),
            };
            let paths = [
                "/project/.git/objects",
                "/project/node_modules/pkg",
                "/private/tmp/index-state/index.sqlite-wal",
                "/project/src/new.rs",
                "/project/.git/objects",
                "/project/node_modules/pkg",
                "/private/tmp/index-state/index.sqlite",
            ]
            .map(|path| CString::new(path).unwrap());
            let mut pointers = paths.iter().map(|path| path.as_ptr()).collect::<Vec<_>>();
            let flags = [
                0,
                0,
                0,
                0,
                FLAG_USER_DROPPED,
                FLAG_MUST_SCAN_SUBDIRS,
                FLAG_ROOT_CHANGED,
            ];
            let ids = [1, 2, 3, 4, 5, 6, 7];
            unsafe {
                event_callback(
                    ptr::null(),
                    (&state as *const CallbackState).cast_mut().cast(),
                    pointers.len(),
                    pointers.as_mut_ptr().cast(),
                    flags.as_ptr(),
                    ids.as_ptr(),
                );
            }
            let events = receiver.try_iter().collect::<Vec<_>>();
            assert_eq!(
                events.iter().map(|event| event.id).collect::<Vec<_>>(),
                [4, 5, 7]
            );
            // Rejected events cannot reach a transaction, scan, or checkpoint write.
            let mut pending = PendingWork::default();
            for event in events {
                pending.add(event);
            }
            assert!(pending.rescan_roots);
            assert!(pending.paths.is_empty());
            assert_eq!(pending.max_event_id, 7);
        }

        #[test]
        fn combined_recovery_flags_override_excludes_but_must_alone_does_not() {
            let filter = EventFilter::new(Path::new("/state/index.sqlite"), &[".git".to_string()]);
            // All subsets of the six control flags exercise priority, including
            // history-only and Must+History on the diagnostic directory itself.
            for bits in 0..64_u32 {
                let global = bits
                    & (FLAG_USER_DROPPED
                        | FLAG_KERNEL_DROPPED
                        | FLAG_IDS_WRAPPED
                        | FLAG_ROOT_CHANGED)
                    != 0;
                assert_eq!(action_for_flags(bits) == WatchAction::RescanRoots, global);
                assert_eq!(
                    filter.accepts("/state/macevery-watch-trigger.jsonl", bits),
                    global
                );
                assert_eq!(filter.accepts("/root/.git/objects", bits), global);
                assert_eq!(
                    filter.accepts("/root/src", bits),
                    global || bits & FLAG_HISTORY_DONE == 0 || bits & FLAG_MUST_SCAN_SUBDIRS != 0
                );
            }
            assert_eq!(filter.counters.received.load(Ordering::Relaxed), 64 * 3 - 4);
            assert!(filter.counters.rejected.load(Ordering::Relaxed) > 0);
        }

        #[test]
        fn must_scope_intersects_roots_and_folds_overlaps_without_lookalikes() {
            let roots =
                ["/project/a", "/project/a/nested", "/project/a-b", "/other"].map(PathBuf::from);
            let targets = |path| subtree_targets(Path::new(path), &roots).unwrap();
            assert_eq!(targets("/project/a/child"), ["/project/a/child"]);
            assert_eq!(targets("/project"), ["/project/a", "/project/a-b"]);
            assert_eq!(targets("/project/a/nested"), ["/project/a/nested"]);
            assert!(subtree_targets(Path::new("/unrelated"), &roots).is_none());
            assert!(subtree_targets(Path::new("relative/path"), &roots).is_none());
            assert!(subtree_targets(Path::new("/project/a/../other"), &roots).is_none());
            let mut pending = PendingWork::default();
            pending.add_scoped(
                FsEvent {
                    path: "relative/path".into(),
                    flags: FLAG_MUST_SCAN_SUBDIRS,
                    id: 42,
                },
                &roots,
            );
            assert!(pending.scope_fallback && pending.rescan_roots);
            assert_eq!(pending.max_event_id, 42);
        }

        #[test]
        fn unknown_root_namespaces_and_aliases_fall_back_instead_of_skipping_recovery() {
            for (event_path, roots) in [
                ("/project/a/child", vec![PathBuf::from("project/a")]),
                (
                    "/project/a/child",
                    vec![
                        PathBuf::from("/project/a"),
                        PathBuf::from("/project/other/../a"),
                    ],
                ),
                (
                    "/private/tmp/project/child",
                    vec![PathBuf::from("/tmp/project")],
                ),
                (
                    "/tmp/project/child",
                    vec![PathBuf::from("/private/tmp/project")],
                ),
                ("/unrelated/child", vec![PathBuf::from("/project/a")]),
            ] {
                assert!(subtree_targets(Path::new(event_path), &roots).is_none());
                let mut pending = PendingWork::default();
                pending.add_scoped(
                    FsEvent {
                        path: event_path.into(),
                        flags: FLAG_MUST_SCAN_SUBDIRS,
                        id: 9,
                    },
                    &roots,
                );
                assert!(pending.scope_fallback && pending.rescan_roots);
                assert!(pending.paths.is_empty());
                let mut rescanned = false;
                process_pending(
                    &mut pending,
                    true,
                    |_| {
                        panic!("unknown scope must not be silently skipped or scanned under the event alias")
                    },
                    || {
                        rescanned = true;
                        Ok(IndexSummary::default())
                    },
                );
                assert!(rescanned);
                assert!(!pending.has_work());
                assert_eq!(pending.max_event_id, 9);
            }
        }

        #[test]
        fn in_scope_must_with_history_reaches_scan_cleanup_and_checkpoint() {
            use crate::scanner::{index_paths, IndexRequest};
            let temp = temp_dir("watch-must-history");
            let root = temp.join("root");
            let other = temp.join("other");
            let subtree = root.join("target");
            let gone = subtree.join("gone.txt");
            let fresh = subtree.join("fresh.txt");
            std::fs::create_dir_all(&subtree).unwrap();
            std::fs::create_dir_all(&other).unwrap();
            std::fs::write(&gone, "old").unwrap();
            std::fs::write(other.join("untouched.txt"), "keep").unwrap();
            let roots = [root, other.clone()];
            // A sibling DB directory avoids excluding the filesystem fixture.
            let db_path = temp.join("state/index.sqlite");
            let db = Database::open(db_path.clone()).unwrap();
            index_paths(
                &db,
                &IndexRequest {
                    roots: roots.to_vec(),
                    excludes: vec![],
                    rebuild: true,
                },
            )
            .unwrap();
            db.set_meta(META_LAST_EVENT_ID, "100").unwrap();
            let neighbor = db
                .all_records()
                .unwrap()
                .into_iter()
                .filter(|row| Path::new(&row.path).starts_with(&other))
                .collect::<Vec<_>>();
            std::fs::remove_file(&gone).unwrap();
            std::fs::write(&fresh, "new").unwrap();
            let filter = EventFilter::new(&db_path, &[]);
            let event = FsEvent {
                flags: FLAG_MUST_SCAN_SUBDIRS | FLAG_HISTORY_DONE,
                ..event(&subtree, 101)
            };
            assert!(
                filter.accepts(&event.path, event.flags),
                "history cannot erase the simultaneous subtree-recovery requirement"
            );
            assert!(!filter.accepts(&event.path, FLAG_HISTORY_DONE));
            let mut pending = PendingWork::default();
            pending.add_scoped(event, &roots);
            assert!(!pending.rescan_roots && !pending.scope_fallback);
            let mut scanned = false;
            process_pending(
                &mut pending,
                true,
                |path| {
                    assert_eq!(path, subtree);
                    scanned = true;
                    refresh_path(&db, path, &[])
                },
                || panic!("Must+History within known scope stays local"),
            );
            assert!(scanned && persist_checkpoint(&db, &mut pending));
            assert!(db.kind_for_path(&gone.to_string_lossy()).unwrap().is_none());
            assert!(db
                .kind_for_path(&fresh.to_string_lossy())
                .unwrap()
                .is_some());
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("101")
            );
            let after = db
                .all_records()
                .unwrap()
                .into_iter()
                .filter(|row| Path::new(&row.path).starts_with(&other))
                .collect::<Vec<_>>();
            assert_eq!(neighbor, after);
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn must_and_ordinary_subtrees_remove_orphans_without_touching_other_roots() {
            use crate::scanner::{index_paths, IndexRequest};
            for flags in [0, FLAG_MUST_SCAN_SUBDIRS] {
                let temp = temp_dir("watch-scoped-lifecycle");
                let root = temp.join("root");
                let other = temp.join("other");
                let subtree = root.join("sub/子目录");
                let gone = subtree.join("gone.txt");
                std::fs::create_dir_all(&subtree).unwrap();
                std::fs::create_dir_all(&other).unwrap();
                std::fs::write(&gone, "old").unwrap();
                std::fs::write(other.join("untouched.txt"), "keep").unwrap();
                let roots = [root.clone(), root.join("sub"), other.clone()];
                let db = Database::open(temp.join("index.sqlite")).unwrap();
                index_paths(
                    &db,
                    &IndexRequest {
                        roots: roots.to_vec(),
                        excludes: vec![],
                        rebuild: true,
                    },
                )
                .unwrap();
                let untouched = db
                    .all_records()
                    .unwrap()
                    .into_iter()
                    .filter(|record| !Path::new(&record.path).starts_with(&subtree))
                    .collect::<Vec<_>>();
                std::fs::remove_file(&gone).unwrap();
                let new = subtree.join("new.txt");
                std::fs::write(&new, "new content").unwrap();
                let mut pending = PendingWork::default();
                pending.add_scoped(
                    FsEvent {
                        flags,
                        ..event(&subtree, 1)
                    },
                    &roots,
                );
                process_pending(
                    &mut pending,
                    true,
                    |path| {
                        assert_eq!(path, subtree);
                        refresh_path(&db, path, &[])
                    },
                    || panic!("Must-only cannot scan all roots"),
                );
                assert!(persist_checkpoint(&db, &mut pending));
                assert!(db.kind_for_path(&gone.to_string_lossy()).unwrap().is_none());
                assert!(db.kind_for_path(&new.to_string_lossy()).unwrap().is_some());
                let after = db
                    .all_records()
                    .unwrap()
                    .into_iter()
                    .filter(|record| !Path::new(&record.path).starts_with(&subtree))
                    .collect::<Vec<_>>();
                assert_eq!(
                    untouched, after,
                    "IDs, indexed_at and every field outside scope remain unchanged"
                );
                // Simulate a coalesced deletion after the parent row has already
                // disappeared, while stale descendant rows remain indexed.
                db.delete_path_exact(&subtree.to_string_lossy()).unwrap();
                std::fs::remove_dir_all(&subtree).unwrap();
                pending.add_scoped(
                    FsEvent {
                        flags,
                        ..event(&subtree, 2)
                    },
                    &roots,
                );
                process_actual(&db, &roots, &mut pending);
                assert!(persist_checkpoint(&db, &mut pending));
                assert!(db.kind_for_path(&new.to_string_lossy()).unwrap().is_none());
                assert_eq!(db.all_records().unwrap(), untouched);
                drop(db);
                std::fs::remove_dir_all(temp).unwrap();
            }
        }

        #[test]
        fn wrapped_epoch_invalidates_old_checkpoint_before_recovery_and_blocks_until_complete() {
            let temp = temp_dir("watch-wrap");
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            db.set_meta(META_LAST_EVENT_ID, "1000").unwrap();
            let mut pending = PendingWork::default();
            pending.add(event(Path::new("/root/before"), 1100));
            pending.add_scoped(
                FsEvent {
                    path: "/excluded/.git".into(),
                    flags: FLAG_IDS_WRAPPED | FLAG_MUST_SCAN_SUBDIRS,
                    id: 2,
                },
                &[PathBuf::from("/root")],
            );
            pending.add(event(Path::new("/root/after"), 3));
            assert_eq!(pending.max_event_id, 3);
            invalidate_wrapped_checkpoint(&db, &mut pending).unwrap();
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("0")
            );
            process_pending(
                &mut pending,
                true,
                |_| panic!("global recovery"),
                || Err(MacEveryError::Cli("injected scan failure".into())),
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("0")
            );
            process_pending(
                &mut pending,
                true,
                |_| panic!("global recovery retry"),
                || Ok(IndexSummary::default()),
            );
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("3")
            );
            assert!(!pending.checkpoint_reset);
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn must_incomplete_subtree_defers_cleanup_and_retries_only_failed_child() {
            use crate::model::{FileKind, FileRecord};
            let temp = temp_dir("watch-must-incomplete");
            let subtree = temp.join("root/subtree");
            let failed = subtree.join("failed");
            let keep = failed.join("keep.txt");
            let stale = subtree.join("deleted.txt");
            let neighbor = temp.join("other/untouched.txt");
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            for path in [&keep, &stale, &neighbor] {
                db.upsert_file(&FileRecord::new(
                    path.to_string_lossy().into_owned(),
                    FileKind::File,
                    1,
                ))
                .unwrap();
            }
            let untouched = db
                .all_records()
                .unwrap()
                .into_iter()
                .find(|r| r.path == neighbor.to_string_lossy())
                .unwrap();
            db.set_meta(META_LAST_EVENT_ID, "5").unwrap();
            let roots = [temp.join("root"), temp.join("other")];
            let mut pending = PendingWork::default();
            pending.add_scoped(
                FsEvent {
                    flags: FLAG_MUST_SCAN_SUBDIRS,
                    ..event(&subtree, 6)
                },
                &roots,
            );
            process_pending(
                &mut pending,
                true,
                |path| {
                    assert_eq!(path, subtree);
                    // Model a scanner read failure after committing the visited part
                    // of the subtree. The cleanup generation must remain deferred.
                    Ok(IndexSummary {
                        warnings: vec!["injected unreadable child".into()],
                        retry_paths: vec![failed.clone()],
                        deferred_cleanups: vec![(subtree.clone(), 2)],
                        ..IndexSummary::default()
                    })
                },
                || panic!("Must-only recovery remains scoped"),
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            assert!(db
                .kind_for_path(&stale.to_string_lossy())
                .unwrap()
                .is_some());
            assert!(db.kind_for_path(&keep.to_string_lossy()).unwrap().is_some());
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("5")
            );
            std::fs::create_dir_all(&failed).unwrap();
            std::fs::write(&keep, "restored").unwrap();
            process_pending(
                &mut pending,
                true,
                |path| {
                    assert_eq!(path, failed);
                    refresh_path(&db, path, &[])
                },
                || panic!("healthy roots are not rescanned on retry"),
            );
            assert!(persist_checkpoint(&db, &mut pending));
            let records = db.all_records().unwrap();
            assert!(!records.iter().any(|r| r.path == stale.to_string_lossy()));
            assert!(records.iter().any(|r| r.path == keep.to_string_lossy()));
            assert_eq!(
                records
                    .iter()
                    .find(|r| r.path == neighbor.to_string_lossy())
                    .unwrap(),
                &untouched
            );
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("6")
            );
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn diagnostics_keep_bounded_raw_event_samples_without_paths() {
            let temp = temp_dir("watch-diagnostic-samples");
            std::fs::create_dir_all(&temp).unwrap();
            let db_path = temp.join("index.sqlite");
            let mut log = TriggerLog::new(&db_path);
            let mut pending = PendingWork::default();
            for id in 1..=10 {
                pending.add_scoped(
                    FsEvent {
                        path: format!("/private/secret-file-{id}"),
                        flags: if id == 7 {
                            FLAG_USER_DROPPED | FLAG_MUST_SCAN_SUBDIRS
                        } else {
                            FLAG_MUST_SCAN_SUBDIRS
                        },
                        id,
                    },
                    &[PathBuf::from("/private")],
                );
            }
            log.record("scan_start", "roots", &pending, None, LogInfo::default());
            let line = std::fs::read_to_string(&log.path).unwrap();
            assert_eq!(line.lines().count(), 1);
            assert!(line.len() <= LOG_RECORD_BYTES);
            assert!(!line.contains("secret-file") && !line.contains(temp.to_str().unwrap()));
            assert!(line.contains("\"event_count\":10"));
            assert!(line.contains("\"events_outside_first_last\":6"));
            assert!(line.contains(&format!(
                "\"recovery_event\":{}",
                pending.recovery_event.unwrap().json()
            )));
            assert!(line.contains(&format!(
                "\"last_event\":{}",
                pending.last_event.unwrap().json()
            )));
            for sample in pending.first_events.iter().flatten() {
                assert!(line.contains(&sample.json()));
            }
            assert_eq!(
                std::fs::metadata(&log.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let filter = EventFilter::new(&db_path, &runtime_excludes(vec![], &db_path));
            assert!(!filter.accepts(log.path.to_str().unwrap(), FLAG_MUST_SCAN_SUBDIRS));
            assert_eq!(
                filter.counters.received.load(Ordering::Relaxed),
                0,
                "diagnostic writes never schedule their own summary"
            );
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn diagnostic_rate_limits_reserve_recovery_slots_and_report_suppression() {
            let temp = temp_dir("watch-diagnostic-rate");
            std::fs::create_dir_all(&temp).unwrap();
            let mut log = TriggerLog::new(&temp.join("index.sqlite"));
            let pending = PendingWork::default();
            for _ in 0..(LOG_RECORDS_PER_WINDOW + 5) {
                log.record(
                    "scan_complete",
                    "subtree",
                    &pending,
                    None,
                    LogInfo::default(),
                );
            }
            assert_eq!(log.records, LOG_RECORDS_PER_WINDOW - 20);
            assert_eq!(log.suppressed, 25);
            log.record("scan_start", "roots", &pending, None, LogInfo::default());
            assert_eq!(log.suppressed, 0);
            let text = std::fs::read_to_string(&log.path).unwrap();
            assert!(text
                .lines()
                .last()
                .unwrap()
                .contains("\"suppressed_records\":25"));
            log.records = LOG_RECORDS_PER_WINDOW;
            log.record("scan_start", "roots", &pending, None, LogInfo::default());
            assert_eq!(log.suppressed, 1);
            log.window = Instant::now() - LOG_WINDOW;
            log.record("scan_complete", "roots", &pending, None, LogInfo::default());
            assert_eq!(log.records, 1);
            assert_eq!(log.suppressed, 0);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn diagnostics_rotate_with_fixed_disk_bound_and_reject_unsafe_targets() {
            use std::os::unix::fs::symlink;
            let temp = temp_dir("watch-diagnostic-rotation");
            std::fs::create_dir_all(&temp).unwrap();
            let path = temp.join(LOG_FILE_NAME);
            for _ in 0..3 {
                private_log_file(&path)
                    .unwrap()
                    .set_len(LOG_FILE_BYTES - 1)
                    .unwrap();
                append_trigger_record(&path, b"{}\n").unwrap();
                assert_eq!(std::fs::metadata(&path).unwrap().len(), 3);
                assert!(
                    std::fs::metadata(path.with_extension("jsonl.1"))
                        .unwrap()
                        .len()
                        <= LOG_FILE_BYTES
                );
            }
            assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 3);
            assert!(append_trigger_record(&path, &vec![0; LOG_RECORD_BYTES + 1]).is_err());
            let victim = temp.join("keep-original");
            std::fs::write(&victim, "untouched").unwrap();
            std::fs::remove_file(&path).unwrap();
            symlink(&victim, &path).unwrap();
            assert!(append_trigger_record(&path, b"{}\n").is_err());
            std::fs::remove_file(&path).unwrap();
            std::fs::hard_link(&victim, &path).unwrap();
            assert!(append_trigger_record(&path, b"{}\n").is_err());
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn diagnostic_io_failure_backs_off_without_changing_work_or_checkpoint() {
            let temp = temp_dir("watch-diagnostic-error");
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            let mut log = TriggerLog::new(&temp.join("missing-parent/index.sqlite"));
            let mut pending = PendingWork::default();
            pending.add(event(Path::new("/root/new"), 10));
            process_pending_logged(
                &mut pending,
                true,
                |_| Ok(IndexSummary::default()),
                || panic!("path event"),
                Some(&mut log),
            );
            assert!(persist_checkpoint_logged(&db, &mut pending, Some(&mut log)));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("10")
            );
            assert_eq!(log.io_failures, 1);
            assert!(log.suppressed >= 3);
            std::fs::create_dir_all(log.path.parent().unwrap()).unwrap();
            log.unavailable_until = None;
            log.record(
                "diagnostics_retry",
                "none",
                &pending,
                None,
                LogInfo::default(),
            );
            assert_eq!(log.io_failures, 0);
            assert!(std::fs::read_to_string(&log.path)
                .unwrap()
                .contains("\"io_failures\":1"));
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn failed_path_blocks_later_checkpoint_until_actual_recovery() {
            use crate::model::{FileKind, FileRecord};
            use std::os::unix::fs::symlink;
            let temp = temp_dir("watch-retry");
            let root = temp.join("root");
            std::fs::create_dir_all(&root).unwrap();
            let looping = root.join("loop");
            symlink("loop", &looping).unwrap();
            let failed = looping.join("indexed-dir");
            let child = failed.join("keep.txt");
            let good = root.join("good.txt");
            std::fs::write(&good, "new").unwrap();
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            for (path, kind) in [(&failed, FileKind::Directory), (&child, FileKind::File)] {
                db.upsert_file(&FileRecord::new(
                    path.to_string_lossy().into_owned(),
                    kind,
                    1,
                ))
                .unwrap();
            }
            db.set_meta(META_LAST_EVENT_ID, "100").unwrap();
            let mut pending = PendingWork::default();
            pending.add(event(&failed, 101));
            process_actual(&db, std::slice::from_ref(&root), &mut pending);
            assert!(!persist_checkpoint(&db, &mut pending));
            pending.add(event(&good, 102));
            // Model an old failure in backoff: only the fresh event is processed.
            process_pending(
                &mut pending,
                false,
                |path| {
                    assert_eq!(path, good);
                    refresh_path(&db, path, &[])
                },
                || panic!("fresh events must not force a root retry"),
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("100")
            );
            assert_eq!(
                db.kind_for_path(&child.to_string_lossy()).unwrap(),
                Some(FileKind::File)
            );
            assert_eq!(
                db.kind_for_path(&good.to_string_lossy()).unwrap(),
                Some(FileKind::File)
            );
            std::fs::remove_file(looping).unwrap();
            std::fs::create_dir_all(&failed).unwrap();
            std::fs::write(&child, "restored").unwrap();
            process_actual(&db, std::slice::from_ref(&root), &mut pending);
            assert!(persist_checkpoint(&db, &mut pending));
            assert!(!pending.has_pending());
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("102")
            );
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn partial_root_rescan_retries_only_failed_subtree_and_defers_safe_cleanup() {
            use crate::model::{FileKind, FileRecord};
            use std::os::unix::fs::symlink;
            let temp = temp_dir("watch-scoped-retry");
            let healthy = temp.join("healthy");
            std::fs::create_dir_all(&healthy).unwrap();
            let looping = temp.join("loop");
            symlink("loop", &looping).unwrap();
            let failed = looping.join("indexed-dir");
            let keep = failed.join("keep.txt");
            let stale = healthy.join("deleted.txt");
            let fresh = healthy.join("fresh.txt");
            let roots = vec![healthy.clone(), failed.clone()];
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            for (path, kind) in [
                (&failed, FileKind::Directory),
                (&keep, FileKind::File),
                (&stale, FileKind::File),
            ] {
                db.upsert_file(&FileRecord::new(
                    path.to_string_lossy().into_owned(),
                    kind,
                    1,
                ))
                .unwrap();
            }
            db.set_meta(META_LAST_EVENT_ID, "100").unwrap();
            let mut pending = PendingWork::default();
            pending.add(FsEvent {
                path: healthy.to_string_lossy().into_owned(),
                flags: FLAG_USER_DROPPED,
                id: 201,
            });
            process_actual(&db, &roots, &mut pending);
            assert!(!pending.rescan_roots && !pending.retry_roots);
            assert_eq!(
                pending.retry_paths.keys().cloned().collect::<Vec<_>>(),
                [failed.to_string_lossy().into_owned()]
            );
            assert_eq!(pending.deferred_cleanups.len(), 2);
            assert!(!persist_checkpoint(&db, &mut pending));
            std::fs::write(&fresh, "fresh").unwrap();
            pending.add(event(&fresh, 202));
            process_pending(
                &mut pending,
                false,
                |path| {
                    assert_eq!(path, fresh);
                    refresh_path(&db, path, &[])
                },
                || panic!("permanent subtree failure must not rescan healthy roots"),
            );
            assert!(db
                .kind_for_path(&fresh.to_string_lossy())
                .unwrap()
                .is_some());
            assert!(!persist_checkpoint(&db, &mut pending));
            process_pending(
                &mut pending,
                true,
                |path| {
                    assert_eq!(path, failed);
                    refresh_path(&db, path, &[])
                },
                || panic!("only the failed subtree may retry"),
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            assert!(db.kind_for_path(&keep.to_string_lossy()).unwrap().is_some());
            std::fs::remove_file(looping).unwrap();
            std::fs::create_dir_all(&failed).unwrap();
            std::fs::write(&keep, "restored").unwrap();
            process_pending(
                &mut pending,
                true,
                |path| refresh_path(&db, path, &[]),
                || panic!("recovery stays scoped"),
            );
            assert!(persist_checkpoint(&db, &mut pending));
            assert!(db
                .kind_for_path(&stale.to_string_lossy())
                .unwrap()
                .is_none());
            assert!(db
                .kind_for_path(&fresh.to_string_lossy())
                .unwrap()
                .is_some());
            assert!(db.kind_for_path(&keep.to_string_lossy()).unwrap().is_some());
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("202")
            );
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn incomplete_root_rescan_keeps_recovery_and_checkpoint_pending() {
            let temp = temp_dir("watch-partial");
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            db.set_meta(META_LAST_EVENT_ID, "100").unwrap();
            let mut pending = PendingWork::default();
            pending.add(FsEvent {
                path: "/excluded/.git".to_string(),
                flags: FLAG_USER_DROPPED,
                id: 201,
            });
            process_pending(
                &mut pending,
                true,
                |_| panic!("root recovery must rescan"),
                || {
                    Ok(IndexSummary {
                        warnings: vec!["unreadable directory".to_string()],
                        ..IndexSummary::default()
                    })
                },
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            assert!(pending.retry_roots);
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("100")
            );
            process_pending(
                &mut pending,
                true,
                |_| panic!("root recovery must rescan"),
                || Ok(IndexSummary::default()),
            );
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("201")
            );
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn directory_create_modify_rename_and_delete_refresh_correctly() {
            use crate::scanner::{index_paths, IndexRequest};
            let temp = temp_dir("watch-lifecycle");
            let root = temp.join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            index_paths(
                &db,
                &IndexRequest {
                    roots: vec![root.clone()],
                    excludes: vec![],
                    rebuild: true,
                },
            )
            .unwrap();
            let mut pending = PendingWork::default();
            let dir = root.join("new-dir");
            let file = dir.join("new.txt");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&file, "a").unwrap();
            pending.add(event(&dir, 1));
            pending.add(event(&file, 2));
            process_actual(&db, std::slice::from_ref(&root), &mut pending);
            assert!(persist_checkpoint(&db, &mut pending));
            assert!(db.kind_for_path(&file.to_string_lossy()).unwrap().is_some());
            std::fs::write(&file, "a longer file").unwrap();
            pending.add(event(&file, 3));
            process_actual(&db, std::slice::from_ref(&root), &mut pending);
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.all_records()
                    .unwrap()
                    .iter()
                    .find(|r| r.path == file.to_string_lossy())
                    .unwrap()
                    .size,
                Some(13)
            );
            let renamed = root.join("renamed-dir");
            std::fs::rename(&dir, &renamed).unwrap();
            pending.add(event(&dir, 4));
            pending.add(event(&renamed, 5));
            process_actual(&db, std::slice::from_ref(&root), &mut pending);
            assert!(persist_checkpoint(&db, &mut pending));
            assert!(db.kind_for_path(&file.to_string_lossy()).unwrap().is_none());
            assert!(db
                .kind_for_path(&renamed.join("new.txt").to_string_lossy())
                .unwrap()
                .is_some());
            std::fs::remove_dir_all(&renamed).unwrap();
            pending.add(event(&renamed, 6));
            process_actual(&db, std::slice::from_ref(&root), &mut pending);
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(db.all_records().unwrap().len(), 1);
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("6")
            );
            drop(db);
            std::fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn fold_paths_handles_root_and_interleaved_lookalike_siblings() {
            assert_eq!(
                fold_refresh_paths(vec![
                    "/a".to_string(),
                    "/a-b".to_string(),
                    "/a/c".to_string()
                ]),
                ["/a", "/a-b"]
            );
            assert_eq!(
                fold_refresh_paths(vec!["/".to_string(), "/a".to_string(), "/b/c".to_string()]),
                ["/"]
            );
        }

        #[test]
        fn dropped_or_root_changed_events_trigger_root_rescan() {
            assert_eq!(
                action_for_flags(FLAG_USER_DROPPED),
                WatchAction::RescanRoots
            );
            assert_eq!(
                action_for_flags(FLAG_KERNEL_DROPPED),
                WatchAction::RescanRoots
            );
            assert_eq!(
                action_for_flags(FLAG_MUST_SCAN_SUBDIRS),
                WatchAction::RefreshPaths
            );
            assert_eq!(action_for_flags(FLAG_IDS_WRAPPED), WatchAction::RescanRoots);
            assert_eq!(
                action_for_flags(FLAG_ROOT_CHANGED),
                WatchAction::RescanRoots
            );
            assert_eq!(action_for_flags(0), WatchAction::RefreshPaths);
        }

        #[test]
        fn batch_uses_root_rescan_if_any_event_requires_it() {
            let batch = vec![
                FsEvent {
                    path: "/tmp/a".to_string(),
                    flags: 0,
                    id: 10,
                },
                FsEvent {
                    path: "/tmp/b".to_string(),
                    flags: FLAG_USER_DROPPED,
                    id: 11,
                },
            ];
            assert_eq!(batch_action(&batch), WatchAction::RescanRoots);
        }

        #[test]
        fn checkpoint_none_triggers_full_rescan() {
            assert_eq!(
                resolve_since_when(None, 5000),
                CheckpointDecision::FullRescan
            );
        }

        #[test]
        fn checkpoint_zero_triggers_full_rescan() {
            assert_eq!(
                resolve_since_when(Some(0), 5000),
                CheckpointDecision::FullRescan
            );
        }

        #[test]
        fn checkpoint_since_now_triggers_full_rescan() {
            assert_eq!(
                resolve_since_when(Some(SINCE_NOW), 5000),
                CheckpointDecision::FullRescan
            );
        }

        #[test]
        fn checkpoint_exceeds_current_triggers_full_rescan() {
            assert_eq!(
                resolve_since_when(Some(10000), 5000),
                CheckpointDecision::FullRescan
            );
        }

        #[test]
        fn checkpoint_valid_returns_use_checkpoint() {
            assert_eq!(
                resolve_since_when(Some(3000), 5000),
                CheckpointDecision::UseCheckpoint(3000)
            );
        }

        #[test]
        fn checkpoint_equal_to_current_is_valid() {
            assert_eq!(
                resolve_since_when(Some(5000), 5000),
                CheckpointDecision::UseCheckpoint(5000)
            );
        }

        #[test]
        fn runtime_excludes_adds_noisy_and_database_paths() {
            let db_path = PathBuf::from("/Users/me/.local/share/macevery/index.sqlite");
            let excludes = runtime_excludes(vec!["node_modules".to_string()], &db_path);
            assert!(excludes.contains(&"node_modules".to_string()));
            assert!(excludes.contains(&".codex".to_string()));
            assert!(excludes.contains(&".gemini".to_string()));
            assert!(excludes.contains(&".cursor".to_string()));
            assert!(excludes.contains(&".local/share/macevery".to_string()));
            assert!(excludes.contains(&"/Users/me/.local/share/macevery".to_string()));
        }

        #[test]
        fn database_events_are_ignored() {
            let db_path = PathBuf::from("/Users/me/.local/share/macevery/index.sqlite");
            let db_dir = db_path.parent();
            assert!(is_ignored_event_path(
                "/Users/me/.local/share/macevery/index.sqlite",
                &db_path,
                db_dir
            ));
            assert!(is_ignored_event_path(
                "/Users/me/.local/share/macevery/index.sqlite-wal",
                &db_path,
                db_dir
            ));
            assert!(!is_ignored_event_path(
                "/Users/me/Documents/file.txt",
                &db_path,
                db_dir
            ));
        }

        #[test]
        fn fold_refresh_paths_removes_children_when_parent_is_present() {
            assert_eq!(
                fold_refresh_paths(vec![
                    "/tmp/project/src/main.rs".to_string(),
                    "/tmp/project".to_string(),
                    "/tmp/project/src".to_string(),
                    "/tmp/projectile/readme.md".to_string(),
                ]),
                vec![
                    "/tmp/project".to_string(),
                    "/tmp/projectile/readme.md".to_string(),
                ]
            );
        }
    }
}

#[cfg(target_os = "macos")]
pub fn watch_index(db_path: PathBuf) -> Result<()> {
    macos_impl::watch_index(db_path)
}

#[cfg(not(target_os = "macos"))]
pub fn watch_index(_db_path: PathBuf) -> Result<()> {
    Err(MacEveryError::Cli(
        "FSEvents watch is only available on macOS".to_string(),
    ))
}
