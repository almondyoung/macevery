use crate::error::{MacEveryError, Result};
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use crate::db::Database;
    use crate::process_lifetime;
    use crate::scanner::{
        refresh_path, refresh_path_legacy, refresh_roots, refresh_roots_legacy, ExcludeMatcher,
        IndexSummary,
    };
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
    use std::sync::{Arc, Condvar, Mutex};
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
        fn FSEventStreamSetExclusionPaths(
            stream_ref: FSEventStreamRef,
            paths_to_exclude: CFArrayRef,
        ) -> Boolean;
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

    // Counts describe raw records, not independent drops, scans or lost files.
    // IDs retain delivery order, including wrap; they are not numeric min/max.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct RawEventCounts {
        must_scan: u64,
        user_dropped: u64,
        kernel_dropped: u64,
        ids_wrapped: u64,
        root_changed: u64,
        first_id: Option<u64>,
        last_id: Option<u64>,
    }

    impl RawEventCounts {
        fn record(&mut self, flags: u32, id: u64) {
            for (count, flag) in [
                (&mut self.must_scan, FLAG_MUST_SCAN_SUBDIRS),
                (&mut self.user_dropped, FLAG_USER_DROPPED),
                (&mut self.kernel_dropped, FLAG_KERNEL_DROPPED),
                (&mut self.ids_wrapped, FLAG_IDS_WRAPPED),
                (&mut self.root_changed, FLAG_ROOT_CHANGED),
            ] {
                *count = count.saturating_add(u64::from(flags & flag != 0));
            }
            self.first_id.get_or_insert(id);
            self.last_id = Some(id);
        }

        fn merge(&mut self, incoming: Self) {
            self.must_scan = self.must_scan.saturating_add(incoming.must_scan);
            self.user_dropped = self.user_dropped.saturating_add(incoming.user_dropped);
            self.kernel_dropped = self.kernel_dropped.saturating_add(incoming.kernel_dropped);
            self.ids_wrapped = self.ids_wrapped.saturating_add(incoming.ids_wrapped);
            self.root_changed = self.root_changed.saturating_add(incoming.root_changed);
            self.first_id = self.first_id.or(incoming.first_id);
            self.last_id = incoming.last_id.or(self.last_id);
        }

        fn json(self) -> String {
            format!(
                concat!(
                    "{{\"must_scan\":{},\"user_dropped\":{},\"kernel_dropped\":{},",
                    "\"ids_wrapped\":{},\"root_changed\":{},\"first_id\":{},\"last_id\":{}}}"
                ),
                self.must_scan,
                self.user_dropped,
                self.kernel_dropped,
                self.ids_wrapped,
                self.root_changed,
                self.first_id.map_or("null".into(), |id| id.to_string()),
                self.last_id.map_or("null".into(), |id| id.to_string()),
            )
        }
    }

    #[derive(Default)]
    struct FilterCounters {
        received: AtomicU64,
        rejected: AtomicU64,
        flags: AtomicU32,
        callback_count: AtomicU64,
        callback_events: AtomicU64,
        callback_total_ns: AtomicU64,
        callback_max_ns: AtomicU64,
        callback_signals: Mutex<RawEventCounts>,
    }

    impl FilterCounters {
        fn record_callback_signals(&self, flags: u32, id: u64) {
            if flags
                & (FLAG_MUST_SCAN_SUBDIRS
                    | FLAG_USER_DROPPED
                    | FLAG_KERNEL_DROPPED
                    | FLAG_IDS_WRAPPED
                    | FLAG_ROOT_CHANGED)
                != 0
            {
                self.callback_signals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record(flags, id);
            }
        }

        fn callback_signal_counts(&self) -> RawEventCounts {
            *self
                .callback_signals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        }
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
        inbox: Arc<EventInbox>,
        filter: EventFilter,
    }

    impl Drop for CallbackState {
        fn drop(&mut self) {
            self.inbox.close();
        }
    }

    #[derive(Default)]
    struct PendingWork {
        paths: BTreeMap<String, FSEventStreamEventFlags>,
        path_bytes: usize,
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
        raw_counts: RawEventCounts,
        first_events: [Option<EventSample>; 3],
        last_event: Option<EventSample>,
        recovery_event: Option<EventSample>,
        hash_state: RandomState,
        inbox_overflow: bool,
        worker_overflow: bool,
        full_recovery_obligation: bool,
        full_recovery_generation: Option<i64>,
        worker_scope_failure: bool,
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
        fn with_hash_state(hash_state: RandomState) -> Self {
            Self {
                hash_state,
                ..Self::default()
            }
        }

        fn clear_paths(&mut self) {
            self.paths.clear();
            self.path_bytes = 0;
        }

        fn insert_path(&mut self, path: String, flags: u32) {
            use std::collections::btree_map::Entry;
            match self.paths.entry(path) {
                Entry::Occupied(mut entry) => *entry.get_mut() |= flags,
                Entry::Vacant(entry) => {
                    self.path_bytes = self.path_bytes.saturating_add(entry.key().capacity());
                    entry.insert(flags);
                }
            }
        }

        fn add(&mut self, event: FsEvent) {
            self.record_event(&event);
            if action_for_flags(event.flags) == WatchAction::RescanRoots {
                self.rescan_roots = true;
                self.clear_paths();
            } else if !self.rescan_roots {
                self.insert_path(event.path, event.flags);
            }
        }

        fn record_event(&mut self, event: &FsEvent) {
            self.raw_counts.record(event.flags, event.id);
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
            if action_for_flags(event.flags) == WatchAction::RescanRoots {
                self.add(event);
                return;
            }
            self.record_event(&event);
            match subtree_targets(Path::new(&event.path), roots) {
                Some(paths) if !self.rescan_roots => {
                    for path in paths {
                        self.insert_path(path, event.flags);
                    }
                }
                Some(_) => {}
                None => {
                    self.scope_fallback = true;
                    self.rescan_roots = true;
                    self.clear_paths();
                    self.recovery_event = self.last_event;
                }
            }
        }

        fn clear_diagnostics(&mut self) {
            self.scope_fallback = false;
            self.event_count = 0;
            self.raw_flags = 0;
            self.raw_counts = RawEventCounts::default();
            self.first_events = [None; 3];
            self.last_event = None;
            self.recovery_event = None;
            self.inbox_overflow = false;
            self.worker_overflow = false;
        }

        // Incoming work is a complete, ordered inbox boundary. A new wrap must
        // reset the numeric watermark; older failed work still blocks checkpoint.
        fn merge_incoming(&mut self, mut incoming: Self) {
            if incoming.checkpoint_reset {
                self.max_event_id = incoming.max_event_id;
                self.checkpoint_reset = true;
                self.checkpoint_invalidated = false;
            } else {
                self.max_event_id = self.max_event_id.max(incoming.max_event_id);
            }
            let next = self.event_count.min(self.first_events.len() as u64) as usize;
            for (destination, sample) in self.first_events[next..]
                .iter_mut()
                .zip(incoming.first_events.iter().flatten())
            {
                *destination = Some(*sample);
            }
            self.event_count = self.event_count.saturating_add(incoming.event_count);
            self.raw_flags |= incoming.raw_flags;
            self.raw_counts.merge(incoming.raw_counts);
            self.last_event = incoming.last_event.or(self.last_event);
            self.recovery_event = self.recovery_event.or(incoming.recovery_event);
            self.scope_fallback |= incoming.scope_fallback;
            self.inbox_overflow |= incoming.inbox_overflow;
            self.rescan_roots |= incoming.rescan_roots;
            if self.worker_scope_failure {
                // Retain the original unexpected namespace debt. New events are
                // still represented by the blocked watermark and replay on restart.
            } else if self.rescan_roots {
                self.clear_paths();
            } else {
                for (path, flags) in std::mem::take(&mut incoming.paths) {
                    self.insert_path(path, flags);
                }
            }
        }

        fn has_retries(&self) -> bool {
            self.retry_roots || !self.retry_paths.is_empty() || self.full_recovery_obligation
        }

        fn needs_legacy_generation(&self) -> bool {
            // Only previously committed incomplete scans carry generation-based
            // cleanup debt. A fresh UserDropped/root recovery can use exact seen
            // IDs; ordinary failed transactions left no rows to restamp.
            !self.deferred_cleanups.is_empty() || self.full_recovery_generation.is_some()
        }

        fn has_work(&self) -> bool {
            self.rescan_roots
                || !self.paths.is_empty()
                || self.has_retries()
                || self.worker_scope_failure
        }

        fn has_pending(&self) -> bool {
            self.has_work()
                || self.max_event_id > 0
                || self.checkpoint_reset
                || !self.deferred_cleanups.is_empty()
        }
    }

    // Retained path storage is bounded, including while the DB worker scans.
    // The one incoming OS path/target being inserted is additional temporary
    // storage. A capacity collapse is explicitly diagnosed, never mislabeled as
    // a UserDropped event, and conservatively recovers all configured roots.
    const INBOX_MAX_PATHS: usize = 16_384;
    const INBOX_MAX_PATH_BYTES: usize = 4 * 1024 * 1024;

    fn worker_plan_size(pending: &PendingWork) -> (usize, usize) {
        // Count retained map entries, including separately allocated duplicates
        // between maps. Do not clone all paths just to measure the bound.
        (
            pending.paths.len() + pending.retry_paths.len() + pending.deferred_cleanups.len(),
            pending.paths.keys().map(String::capacity).sum::<usize>()
                + pending
                    .retry_paths
                    .keys()
                    .map(String::capacity)
                    .sum::<usize>()
                + pending
                    .deferred_cleanups
                    .keys()
                    .map(PathBuf::capacity)
                    .sum::<usize>(),
        )
    }

    fn enforce_worker_plan_bound(pending: &mut PendingWork, roots: &[PathBuf]) -> bool {
        let (entries, bytes) = worker_plan_size(pending);
        if !pending.full_recovery_obligation
            && entries <= INBOX_MAX_PATHS
            && bytes <= INBOX_MAX_PATH_BYTES
        {
            return !pending.worker_scope_failure;
        }
        let covered = |path: &Path| {
            !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
                && roots.iter().any(|root| {
                    path.is_absolute() == root.is_absolute()
                        && !root
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir))
                        && path.starts_with(root)
                })
        };
        if !pending.paths.keys().all(|path| covered(Path::new(path)))
            || !pending
                .retry_paths
                .keys()
                .all(|path| covered(Path::new(path)))
            || !pending.deferred_cleanups.keys().all(|path| covered(path))
        {
            // This cannot come from the guarded inbox + scanner rooted at its
            // configured paths. Preserve unexpected debt and fail closed rather
            // than claiming a root scan covers an outside/alias namespace.
            pending.worker_scope_failure = true;
            return false;
        }
        pending.full_recovery_obligation = true;
        pending.worker_overflow |= entries > INBOX_MAX_PATHS || bytes > INBOX_MAX_PATH_BYTES;
        pending.full_recovery_generation = pending
            .deferred_cleanups
            .values()
            .copied()
            .chain(pending.full_recovery_generation)
            .max();
        // Exact per-path generations are compressed into the full-root debt
        // above. A later complete scan uses a newer process-monotonic generation
        // and cleans every covered namespace. Partial success cannot clear it.
        pending.clear_paths();
        pending.retry_paths.clear();
        pending.deferred_cleanups.clear();
        pending.retry_roots = true;
        true
    }

    #[derive(Default)]
    struct InboxStats {
        admitted: u64,
        coalesced: u64,
        overflows: u64,
        highwater_paths: usize,
        highwater_bytes: usize,
        first_delivery: Option<Instant>,
        last_delivery: Option<Instant>,
    }

    struct InboxState {
        pending: PendingWork,
        stats: InboxStats,
        closed: bool,
    }

    struct EventInbox {
        state: Mutex<InboxState>,
        ready: Condvar,
        roots: Vec<PathBuf>,
    }

    struct InboxBatch {
        pending: PendingWork,
        stats: InboxStats,
        closed: bool,
    }

    /// Fixed from the first coalescing wait; new arrivals never move it.
    struct BatchWindow {
        deadline: Instant,
    }

    impl BatchWindow {
        fn new(started: Instant) -> Self {
            Self {
                deadline: started + MAX_BATCH_WAIT,
            }
        }

        fn next_wait(
            &self,
            now: Instant,
            recovery: bool,
            idle: bool,
            closed: bool,
        ) -> Option<Duration> {
            if closed || (idle && !recovery) {
                return None;
            }
            let remaining = self.deadline.saturating_duration_since(now);
            (!remaining.is_zero()).then_some(BATCH_IDLE.min(remaining))
        }
    }

    impl EventInbox {
        fn new(roots: Vec<PathBuf>, hash_state: RandomState) -> Self {
            Self {
                state: Mutex::new(InboxState {
                    pending: PendingWork::with_hash_state(hash_state),
                    stats: InboxStats::default(),
                    closed: false,
                }),
                ready: Condvar::new(),
                roots,
            }
        }

        fn push(&self, event: FsEvent) {
            let delivered = Instant::now();
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            if state.closed {
                return;
            }
            let was_empty = state.pending.event_count == 0;
            let previous_paths = state.pending.paths.len();
            state.pending.add_scoped(event, &self.roots);
            state.stats.admitted = state.stats.admitted.saturating_add(1);
            if !was_empty && state.pending.paths.len() <= previous_paths {
                state.stats.coalesced = state.stats.coalesced.saturating_add(1);
            }
            state.stats.highwater_paths =
                state.stats.highwater_paths.max(state.pending.paths.len());
            state.stats.highwater_bytes = state.stats.highwater_bytes.max(state.pending.path_bytes);
            if state.pending.paths.len() > INBOX_MAX_PATHS
                || state.pending.path_bytes > INBOX_MAX_PATH_BYTES
            {
                state.pending.rescan_roots = true;
                state.pending.inbox_overflow = true;
                state.pending.clear_paths();
                state.stats.overflows = state.stats.overflows.saturating_add(1);
            }
            state.stats.first_delivery.get_or_insert(delivered);
            state.stats.last_delivery = Some(delivered);
            drop(state);
            if was_empty {
                self.ready.notify_one();
            }
        }

        fn close(&self) {
            self.state
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .closed = true;
            self.ready.notify_one();
        }

        fn take_locked(state: &mut InboxState) -> InboxBatch {
            let replacement = PendingWork::with_hash_state(state.pending.hash_state.clone());
            InboxBatch {
                pending: std::mem::replace(&mut state.pending, replacement),
                stats: std::mem::take(&mut state.stats),
                closed: state.closed,
            }
        }

        // Swap every admitted event at once, rather than slicing an existing
        // backlog into 4096-event batches separated by multi-minute root scans.
        // The lock is never held during DB, filesystem scanning, logging or I/O.
        // Events arriving after this boundary remain in the next inbox.
        fn take_batch(&self, wait: Duration) -> InboxBatch {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            if state.pending.event_count == 0 && !state.closed {
                let (next, _) = self
                    .ready
                    .wait_timeout_while(state, wait, |state| {
                        state.pending.event_count == 0 && !state.closed
                    })
                    .unwrap_or_else(|err| err.into_inner());
                state = next;
            }
            if state.pending.event_count > 0 && !state.closed {
                // Root recovery uses the existing complete one-second window;
                // ordinary updates still flush on the first idle wait.
                let window = BatchWindow::new(Instant::now());
                let mut idle = false;
                while let Some(wait) = window.next_wait(
                    Instant::now(),
                    state.pending.rescan_roots,
                    idle,
                    state.closed,
                ) {
                    let before = state.stats.admitted;
                    let (next, _) = self
                        .ready
                        .wait_timeout(state, wait)
                        .unwrap_or_else(|err| err.into_inner());
                    state = next;
                    idle = state.stats.admitted == before;
                }
            }
            Self::take_locked(&mut state)
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
    const STARTUP_FILE_NAME: &str = "macevery-watch-startup.json";
    const STARTUP_FILE_BYTES: usize = 1024;

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
        legacy_fallback: bool,
        warnings: usize,
        elapsed_ms: u128,
        received: u64,
        rejected: u64,
        received_flags: u32,
        callback_count: u64,
        callback_events: u64,
        callback_total_ns: u64,
        callback_max_ns: u64,
        inbox_admitted: u64,
        inbox_coalesced: u64,
        inbox_overflows: u64,
        inbox_highwater_paths: usize,
        inbox_highwater_bytes: usize,
        first_delivery_age_ms: u128,
        last_delivery_age_ms: u128,
        upstream_requested: usize,
        upstream_accepted: bool,
    }

    impl LogInfo {
        fn scanned(summary: &IndexSummary, started: Instant) -> Self {
            Self {
                indexed: summary.indexed,
                written: summary.rows_written,
                unchanged: summary.unchanged,
                stale_removed: summary.deleted,
                legacy_fallback: summary.legacy_fallback,
                warnings: summary.warnings.len(),
                elapsed_ms: started.elapsed().as_millis(),
                ..Self::default()
            }
        }
    }

    struct TriggerLog {
        path: PathBuf,
        session: u64,
        counters: Arc<FilterCounters>,
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
                session: RandomState::new().hash_one((std::process::id(), SystemTime::now())),
                counters: Arc::new(FilterCounters::default()),
                window: Instant::now(),
                records: 0,
                suppressed: 0,
                oversized: 0,
                io_failures: 0,
                unavailable_until: None,
            }
        }

        fn startup_summary(
            &self,
            roots: usize,
            upstream_requested: usize,
            upstream_accepted: bool,
            flags: u32,
        ) -> io::Result<()> {
            let record = format!(
                concat!(
                    "{{\"v\":1,\"pid\":{},\"session\":\"{:016x}\",\"stage\":\"configured\",",
                    "\"roots\":{},\"upstream_requested\":{},\"upstream_accepted\":{},",
                    "\"create_flags\":{},\"latency_ms\":350}}\n"
                ),
                std::process::id(),
                self.session,
                roots,
                upstream_requested,
                upstream_accepted,
                flags
            );
            write_startup_summary(
                &self.path.with_file_name(STARTUP_FILE_NAME),
                record.as_bytes(),
            )
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
                "\"indexed\":{},\"rows_written\":{},\"unchanged\":{},\"stale_removed\":{},\"legacy_fallback\":{},\"warnings\":{},\"elapsed_ms\":{},",
                "\"received\":{},\"rejected\":{},\"received_flags_or\":{},",
                "\"callback_count\":{},\"callback_events\":{},\"callback_total_ns\":{},\"callback_max_ns\":{},",
                "\"inbox_admitted\":{},\"inbox_coalesced\":{},\"inbox_overflows\":{},\"inbox_highwater_paths\":{},\"inbox_highwater_bytes\":{},",
                "\"first_delivery_age_ms\":{},\"last_delivery_age_ms\":{},\"inbox_overflow\":{},",
                "\"upstream_requested\":{},\"upstream_accepted\":{},",
                "\"worker_overflow\":{},\"full_recovery_obligation\":{},\"worker_scope_failure\":{},",
                "\"session\":\"{:016x}\",\"admitted_raw\":{},\"callback_signals\":{},",
                "\"suppressed_records\":{},\"oversized_records\":{},\"io_failures\":{}}}\n"),
                at_ms, std::process::id(), kind, scope, path_hash, pending.raw_flags, pending.event_count,
                first, last, recovery, pending.event_count.saturating_sub(sampled), pending.max_event_id,
                pending.checkpoint_reset, pending.scope_fallback, pending.paths.len(), pending.retry_paths.len(), pending.retry_roots,
                info.retry_due, info.indexed, info.written, info.unchanged, info.stale_removed, info.legacy_fallback, info.warnings, info.elapsed_ms,
                info.received, info.rejected, info.received_flags,
                info.callback_count, info.callback_events, info.callback_total_ns, info.callback_max_ns,
                info.inbox_admitted, info.inbox_coalesced, info.inbox_overflows, info.inbox_highwater_paths, info.inbox_highwater_bytes,
                info.first_delivery_age_ms, info.last_delivery_age_ms, pending.inbox_overflow,
                info.upstream_requested, info.upstream_accepted,
                pending.worker_overflow, pending.full_recovery_obligation, pending.worker_scope_failure,
                self.session, pending.raw_counts.json(), self.counters.callback_signal_counts().json(),
                self.suppressed, self.oversized, self.io_failures);
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

    // A fixed-name, once-per-start snapshot survives ordinary log rotation.
    // Like trigger diagnostics, failure is nonfatal and the callback does no IO.
    fn write_startup_summary(path: &Path, record: &[u8]) -> io::Result<()> {
        if record.len() > STARTUP_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "startup summary too large",
            ));
        }
        let lock = private_log_file(&path.with_extension("lock"))?;
        if unsafe { flock(lock.as_raw_fd(), LOCK_EXCLUSIVE_NONBLOCKING) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = private_log_file(path)?;
        if file.metadata()?.len() > STARTUP_FILE_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsafe startup summary size",
            ));
        }
        file.set_len(0)?;
        file.write_all(record)
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
        fn new(inbox: Arc<EventInbox>, filter: EventFilter) -> Self {
            Self {
                state: Box::into_raw(Box::new(CallbackState { inbox, filter })),
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
        // Keep every CF object alive until Drop has stopped/released the stream;
        // do not rely on undocumented ownership in SetExclusionPaths.
        exclusions: Option<(Vec<CfReleaseGuard>, CfReleaseGuard)>,
    }

    impl FSEventStreamGuard {
        fn new(stream: FSEventStreamRef) -> Option<Self> {
            if stream.is_null() {
                None
            } else {
                Some(Self {
                    stream,
                    started: false,
                    exclusions: None,
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
        let inbox = Arc::new(EventInbox::new(roots.clone(), initial.hash_state.clone()));
        let counters = Arc::clone(&filter.counters);
        diagnostics.counters = Arc::clone(&counters);

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

        let state = CallbackStateGuard::new(Arc::clone(&inbox), filter);
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
        let upstream_paths = upstream_exclusion_paths(&db_path, &roots, &excludes);
        let upstream_accepted = configure_upstream_exclusions(&mut stream, &upstream_paths);
        if diagnostics
            .startup_summary(roots.len(), upstream_paths.len(), upstream_accepted, flags)
            .is_err()
        {
            eprintln!("watch: startup diagnostics unavailable; indexing continues");
        }
        diagnostics.record(
            "stream_configuration",
            "stream",
            &initial,
            None,
            LogInfo {
                upstream_requested: upstream_paths.len(),
                upstream_accepted,
                ..LogInfo::default()
            },
        );
        // A false API result leaves the existing callback filter authoritative.
        // The worker never owns or calls the FSEvents stream.
        spawn_processor(
            db_path,
            roots.clone(),
            excludes,
            Arc::clone(&inbox),
            initial,
            diagnostics,
            counters,
        );

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
    const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);
    const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

    fn spawn_processor(
        db_path: PathBuf,
        roots: Vec<PathBuf>,
        excludes: Vec<String>,
        inbox: Arc<EventInbox>,
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
                let wait = if pending.has_pending() {
                    retry_at
                        .unwrap_or_else(Instant::now)
                        .saturating_duration_since(Instant::now())
                } else {
                    // One wake/minute also reports excluded-only storms; no DB access.
                    LOG_WINDOW
                };
                let incoming = inbox.take_batch(wait);
                let disconnected = incoming.closed;
                let ingress = incoming.stats;
                pending.merge_incoming(incoming.pending);
                let scoped_plan = enforce_worker_plan_bound(&mut pending, &roots);
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
                            callback_count: counters.callback_count.swap(0, Ordering::Relaxed),
                            callback_events: counters.callback_events.swap(0, Ordering::Relaxed),
                            callback_total_ns: counters
                                .callback_total_ns
                                .swap(0, Ordering::Relaxed),
                            callback_max_ns: counters.callback_max_ns.swap(0, Ordering::Relaxed),
                            inbox_admitted: ingress.admitted,
                            inbox_coalesced: ingress.coalesced,
                            inbox_overflows: ingress.overflows,
                            inbox_highwater_paths: ingress.highwater_paths,
                            inbox_highwater_bytes: ingress.highwater_bytes,
                            first_delivery_age_ms: ingress
                                .first_delivery
                                .map_or(0, |at| at.elapsed().as_millis()),
                            last_delivery_age_ms: ingress
                                .last_delivery
                                .map_or(0, |at| at.elapsed().as_millis()),
                            ..LogInfo::default()
                        },
                    );
                }
                let checkpoint_epoch_ready =
                    invalidate_wrapped_checkpoint(&db, &mut pending).is_ok();
                let complete = if !scoped_plan {
                    diagnostics.record(
                        "worker_scope_failed",
                        "roots",
                        &pending,
                        None,
                        LogInfo::default(),
                    );
                    false
                } else if checkpoint_epoch_ready {
                    let legacy_generation = pending.needs_legacy_generation();
                    process_pending_logged(
                        &mut pending,
                        retry_due,
                        |path| {
                            if legacy_generation {
                                refresh_path_legacy(&db, path, &excludes)
                            } else {
                                refresh_path(&db, path, &excludes)
                            }
                        },
                        || {
                            if legacy_generation {
                                refresh_roots_legacy(&db, &roots, &excludes)
                            } else {
                                refresh_roots(&db, &roots, &excludes)
                            }
                        },
                        Some(&mut diagnostics),
                    );
                    enforce_worker_plan_bound(&mut pending, &roots)
                        && persist_checkpoint_logged(&db, &mut pending, Some(&mut diagnostics))
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
            pending.path_bytes = pending.paths.keys().map(String::capacity).sum();
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
                    pending.clear_paths();
                    pending.retry_paths.clear();
                    pending.retry_roots = false;
                    if pending.full_recovery_obligation && !pending.worker_scope_failure {
                        // The production rescan closure completed a fresh scan
                        // and its stale cleanup across every configured root.
                        pending.full_recovery_obligation = false;
                        pending.full_recovery_generation = None;
                        pending.deferred_cleanups.clear();
                    }
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
                    pending.clear_paths();
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
        pending.clear_paths();
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

    fn upstream_exclusion_paths(
        db_path: &Path,
        roots: &[PathBuf],
        excludes: &[String],
    ) -> Vec<PathBuf> {
        // The SDK accepts at most eight directories. Only exact, existing,
        // already-excluded namespaces may move upstream. No recursive discovery
        // or canonical rewriting of the persistent index namespace is performed.
        // An uncertain root alias disables this optimization, not recovery.
        if roots.iter().any(|root| {
            !root.is_absolute() || fs::canonicalize(root).ok().as_deref() != Some(root.as_path())
        }) {
            return Vec::new();
        }
        let matcher = ExcludeMatcher::new(excludes);
        let mut candidates = Vec::new();
        if let Some(parent) = db_path.parent() {
            candidates.push(parent.to_path_buf());
        }
        for relative in [
            "Library",
            ".codex",
            ".cargo/registry",
            ".npm",
            ".cache",
            ".gemini",
            ".cursor",
        ] {
            candidates.extend(roots.iter().map(|root| root.join(relative)));
        }
        let mut selected = Vec::new();
        for path in candidates {
            if selected.len() == 8 {
                break;
            }
            if selected.contains(&path)
                || !path.is_absolute()
                || !matcher.is_excluded(&path)
                || !path.is_dir()
                || fs::canonicalize(&path).ok().as_deref() != Some(path.as_path())
                || !roots
                    .iter()
                    .any(|root| path != *root && path.starts_with(root))
                || roots.iter().any(|root| root.starts_with(&path))
            {
                continue;
            }
            selected.push(path);
        }
        selected
    }

    fn configure_upstream_exclusions(stream: &mut FSEventStreamGuard, paths: &[PathBuf]) -> bool {
        configure_upstream_exclusions_with(stream, paths, |stream, paths| {
            (unsafe { FSEventStreamSetExclusionPaths(stream, paths) }) != 0
        })
    }

    fn configure_upstream_exclusions_with(
        stream: &mut FSEventStreamGuard,
        paths: &[PathBuf],
        mut configure: impl FnMut(FSEventStreamRef, CFArrayRef) -> bool,
    ) -> bool {
        if paths.is_empty() || paths.len() > 8 {
            return false;
        }
        let mut strings = Vec::with_capacity(paths.len());
        let mut values = Vec::with_capacity(paths.len());
        for path in paths {
            let Ok(path) = CString::new(path.to_string_lossy().as_bytes()) else {
                return false;
            };
            let string =
                unsafe { CFStringCreateWithCString(ptr::null(), path.as_ptr(), UTF8_ENCODING) };
            if string.is_null() {
                return false;
            }
            strings.push(CfReleaseGuard::new(string));
            values.push(string);
        }
        let array = CfReleaseGuard::new(unsafe {
            CFArrayCreate(
                ptr::null(),
                values.as_ptr(),
                values.len() as CFIndex,
                ptr::null(),
            )
        });
        if array.as_ptr().is_null() {
            return false;
        }
        let accepted = configure(stream.as_ptr(), array.as_ptr());
        stream.exclusions = Some((strings, array));
        accepted
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
        let started = Instant::now();
        let paths = event_paths.cast::<*const c_char>();
        for index in 0..num_events {
            let flags = *event_flags.add(index);
            let id = if event_ids.is_null() {
                0
            } else {
                *event_ids.add(index)
            };
            state.filter.counters.record_callback_signals(flags, id);
            let path_ptr = *paths.add(index);
            if path_ptr.is_null() {
                continue;
            }
            let path = CStr::from_ptr(path_ptr).to_string_lossy();
            if !state.filter.accepts(&path, flags) {
                continue;
            }
            let path = path.into_owned();
            state.inbox.push(FsEvent { path, flags, id });
        }
        let elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        state
            .filter
            .counters
            .callback_total_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        state
            .filter
            .counters
            .callback_max_ns
            .fetch_max(elapsed_ns, Ordering::Relaxed);
        state
            .filter
            .counters
            .callback_events
            .fetch_add(num_events as u64, Ordering::Relaxed);
        state
            .filter
            .counters
            .callback_count
            .fetch_add(1, Ordering::Relaxed);
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
            let inbox = Arc::new(EventInbox::new(
                vec![PathBuf::from("/project")],
                RandomState::new(),
            ));
            let state = CallbackState {
                inbox: Arc::clone(&inbox),
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
            let batch = take_now(&inbox);
            assert_eq!(
                batch
                    .pending
                    .first_events
                    .iter()
                    .flatten()
                    .map(|event| event.id)
                    .collect::<Vec<_>>(),
                [4, 5, 7]
            );
            // Rejected events cannot reach a transaction, scan, or checkpoint write.
            let pending = batch.pending;
            assert!(pending.rescan_roots);
            assert!(pending.paths.is_empty());
            assert_eq!(pending.max_event_id, 7);
            assert_eq!(
                state.filter.counters.callback_count.load(Ordering::Relaxed),
                1
            );
            assert_eq!(
                state
                    .filter
                    .counters
                    .callback_events
                    .load(Ordering::Relaxed),
                7
            );
            assert!(
                state
                    .filter
                    .counters
                    .callback_max_ns
                    .load(Ordering::Relaxed)
                    > 0
            );
        }

        fn take_now(inbox: &EventInbox) -> InboxBatch {
            EventInbox::take_locked(&mut inbox.state.lock().unwrap())
        }

        #[test]
        fn batch_window_keeps_local_idle_flush_and_a_fixed_recovery_deadline() {
            let start = Instant::now();
            let window = BatchWindow::new(start);
            assert_eq!(
                window.next_wait(start, false, false, false),
                Some(BATCH_IDLE)
            );
            // Ordinary local work ends at its idle wait, rather than one second.
            assert_eq!(
                window.next_wait(start + BATCH_IDLE, false, true, false),
                None
            );
            // Recovery crosses the same idle boundary, with no sleeping thread
            // or scheduler-dependent claim that a producer ran before deadline.
            assert_eq!(
                window.next_wait(start + BATCH_IDLE, true, true, false),
                Some(BATCH_IDLE)
            );
            for ms in [300, 450, 600, 750, 900, 999] {
                let now = start + Duration::from_millis(ms);
                let wait = window.next_wait(now, true, false, false).unwrap();
                assert!(now + wait <= start + MAX_BATCH_WAIT);
            }
            // Even a last-instant/continuous arrival never starts a new window.
            assert_eq!(
                window.next_wait(start + MAX_BATCH_WAIT, true, false, false),
                None
            );
            assert_eq!(
                window.next_wait(start + MAX_BATCH_WAIT + BATCH_IDLE, true, false, false),
                None
            );
            assert_eq!(window.next_wait(start, true, false, true), None);
            // Existing inbox/scan/checkpoint tests separately prove that an
            // admitted event after take_locked remains a new recovery obligation.
        }

        #[test]
        fn inbox_coalesces_preexisting_drop_markers_across_old_batch_boundaries() {
            let roots = vec![PathBuf::from("/root")];
            let inbox = EventInbox::new(roots, RandomState::new());
            for id in 1..=9000 {
                inbox.push(FsEvent {
                    path: format!("/root/files/{id}"),
                    flags: if [4100, 8200].contains(&id) {
                        FLAG_USER_DROPPED | FLAG_MUST_SCAN_SUBDIRS
                    } else {
                        0
                    },
                    id,
                });
            }
            let batch = take_now(&inbox);
            assert_eq!(batch.pending.event_count, 9000);
            assert_eq!(batch.stats.admitted, 9000);
            assert_eq!(batch.stats.overflows, 0);
            assert!(batch.pending.rescan_roots);
            assert!(batch.pending.paths.is_empty());
            assert_eq!(batch.pending.max_event_id, 9000);
            let mut pending = batch.pending;
            let mut scans = 0;
            process_pending(
                &mut pending,
                true,
                |_| panic!("full recovery"),
                || {
                    scans += 1;
                    Ok(IndexSummary::default())
                },
            );
            assert_eq!(scans, 1);
            assert_eq!(take_now(&inbox).pending.event_count, 0);
        }

        #[test]
        fn inbox_retains_arrivals_during_scan_and_checkpoint_stops_at_boundary() {
            let temp = temp_dir("inbox-boundary");
            fs::create_dir_all(&temp).unwrap();
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            let hash_state = RandomState::new();
            let inbox = EventInbox::new(vec![PathBuf::from("/root")], hash_state.clone());
            inbox.push(FsEvent {
                path: "/root".into(),
                flags: FLAG_USER_DROPPED,
                id: 100,
            });
            let mut pending = PendingWork::with_hash_state(hash_state);
            pending.merge_incoming(take_now(&inbox).pending);
            process_pending(
                &mut pending,
                true,
                |_| panic!("full recovery"),
                || {
                    inbox.push(FsEvent {
                        path: "/root/new".into(),
                        flags: 0,
                        id: 101,
                    });
                    inbox.push(FsEvent {
                        path: "/root".into(),
                        flags: FLAG_USER_DROPPED,
                        id: 102,
                    });
                    Ok(IndexSummary::default())
                },
            );
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("100")
            );
            let later = take_now(&inbox);
            assert_eq!(later.pending.event_count, 2);
            assert!(later.pending.rescan_roots);
            pending.merge_incoming(later.pending);
            let mut scans = 0;
            process_pending(
                &mut pending,
                true,
                |_| panic!("later loss needs recovery"),
                || {
                    scans += 1;
                    Ok(IndexSummary::default())
                },
            );
            assert_eq!(scans, 1);
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("102")
            );
            drop(db);
            fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn inbox_has_path_and_byte_bounds_without_fabricating_dropped_flags() {
            let inbox = EventInbox::new(vec![PathBuf::from("/root")], RandomState::new());
            for id in 1..=INBOX_MAX_PATHS + 10 {
                inbox.push(FsEvent {
                    path: format!("/root/{id}"),
                    flags: 0,
                    id: id as u64,
                });
                let state = inbox.state.lock().unwrap();
                assert!(state.pending.paths.len() <= INBOX_MAX_PATHS);
                assert!(state.pending.path_bytes <= INBOX_MAX_PATH_BYTES);
            }
            let batch = take_now(&inbox);
            assert!(batch.pending.rescan_roots && batch.pending.inbox_overflow);
            assert_eq!(batch.pending.raw_flags, 0);
            assert!(batch.pending.recovery_event.is_none());
            assert_eq!(batch.stats.overflows, 1);
            assert_eq!(batch.stats.highwater_paths, INBOX_MAX_PATHS + 1);
            assert_eq!(batch.pending.max_event_id, (INBOX_MAX_PATHS + 10) as u64);
            for id in 1..=5 {
                inbox.push(FsEvent {
                    path: format!("/root/{}-{id}", "x".repeat(INBOX_MAX_PATH_BYTES / 4)),
                    flags: 0,
                    id,
                });
            }
            let bytes = take_now(&inbox);
            assert!(bytes.pending.rescan_roots && bytes.pending.inbox_overflow);
            assert_eq!(bytes.stats.overflows, 1);
            assert!(bytes.stats.highwater_paths < INBOX_MAX_PATHS);
            assert!(bytes.stats.highwater_bytes > INBOX_MAX_PATH_BYTES);
        }

        #[test]
        fn inbox_wrap_merge_preserves_failed_work_and_new_epoch_checkpoint() {
            let temp = temp_dir("inbox-wrap-retry");
            fs::create_dir_all(&temp).unwrap();
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            db.set_meta(META_LAST_EVENT_ID, "900").unwrap();
            let hash_state = RandomState::new();
            let inbox = EventInbox::new(vec![PathBuf::from("/root")], hash_state.clone());
            let mut pending = PendingWork::with_hash_state(hash_state);
            pending.retry_paths.insert("/root/failed".into(), 0);
            pending.max_event_id = 1000;
            inbox.push(FsEvent {
                path: "/root".into(),
                flags: FLAG_IDS_WRAPPED | FLAG_HISTORY_DONE,
                id: 2,
            });
            inbox.push(FsEvent {
                path: "/root/new".into(),
                flags: 0,
                id: 3,
            });
            pending.merge_incoming(take_now(&inbox).pending);
            assert_eq!(pending.max_event_id, 3);
            assert!(pending.retry_paths.contains_key("/root/failed"));
            invalidate_wrapped_checkpoint(&db, &mut pending).unwrap();
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("0")
            );
            process_pending(
                &mut pending,
                true,
                |path| {
                    assert_eq!(path, Path::new("/root/failed"));
                    Err(MacEveryError::Cli("injected path retry failure".into()))
                },
                || Err(MacEveryError::Cli("injected recovery failure".into())),
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("0")
            );
            inbox.push(FsEvent {
                path: "/root/later".into(),
                flags: 0,
                id: 4,
            });
            pending.merge_incoming(take_now(&inbox).pending);
            assert!(pending.checkpoint_invalidated);
            process_pending(
                &mut pending,
                true,
                |_| panic!("root retry covers old failure"),
                || Ok(IndexSummary::default()),
            );
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("4")
            );
            drop(db);
            fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn worker_bound_compacts_distinct_failure_batches_and_generation_debt() {
            let roots = vec![PathBuf::from("/root")];
            let mut pending = PendingWork::default();
            for batch in 1..=8 {
                retain_incomplete(
                    &mut pending,
                    IndexSummary {
                        warnings: vec!["injected incomplete scan".into()],
                        retry_paths: (0..3000)
                            .map(|id| PathBuf::from(format!("/root/batch-{batch}/failed-{id}")))
                            .collect(),
                        deferred_cleanups: vec![(
                            PathBuf::from(format!("/root/batch-{batch}")),
                            batch * 100,
                        )],
                        ..IndexSummary::default()
                    },
                );
                pending.max_event_id = batch as u64;
                assert!(enforce_worker_plan_bound(&mut pending, &roots));
                let (entries, bytes) = worker_plan_size(&pending);
                assert!(entries <= INBOX_MAX_PATHS && bytes <= INBOX_MAX_PATH_BYTES);
            }
            assert!(
                pending.worker_overflow && pending.full_recovery_obligation && pending.retry_roots
            );
            assert_eq!(pending.full_recovery_generation, Some(800));
            assert!(pending.retry_paths.is_empty() && pending.deferred_cleanups.is_empty());
            assert_eq!(
                pending.raw_flags, 0,
                "capacity promotion is not UserDropped"
            );
        }

        #[test]
        fn worker_full_recovery_debt_survives_partial_and_failed_retries_until_real_cleanup() {
            use crate::model::{FileKind, FileRecord};
            let temp = temp_dir("worker-generation-debt");
            let root = temp.join("root");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("live-code.txt"), "live").unwrap();
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            let stale = root.join("old-deleted-code.txt");
            db.upsert_file(&FileRecord::new(
                stale.to_string_lossy().into_owned(),
                FileKind::File,
                1,
            ))
            .unwrap();
            db.set_meta(META_LAST_EVENT_ID, "10").unwrap();
            let roots = vec![root.clone()];
            let mut pending = PendingWork {
                max_event_id: 200,
                ..PendingWork::default()
            };
            for id in 0..=INBOX_MAX_PATHS {
                pending.retry_paths.insert(
                    root.join(format!("failed-{id}"))
                        .to_string_lossy()
                        .into_owned(),
                    0,
                );
            }
            pending.deferred_cleanups.insert(root.clone(), 500);
            assert!(enforce_worker_plan_bound(&mut pending, &roots));
            assert_eq!(pending.full_recovery_generation, Some(500));
            process_pending(
                &mut pending,
                true,
                |_| panic!("full recovery required"),
                || {
                    Ok(IndexSummary {
                        warnings: vec!["injected partial scan".into()],
                        retry_paths: vec![root.join("blocked")],
                        deferred_cleanups: vec![(root.clone(), 900)],
                        ..IndexSummary::default()
                    })
                },
            );
            assert!(enforce_worker_plan_bound(&mut pending, &roots));
            assert_eq!(pending.full_recovery_generation, Some(900));
            assert!(!persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("10")
            );
            let mut incoming = PendingWork::with_hash_state(pending.hash_state.clone());
            incoming.add_scoped(event(&root.join("live-code.txt"), 201), &roots);
            pending.merge_incoming(incoming);
            assert!(enforce_worker_plan_bound(&mut pending, &roots));
            process_pending(
                &mut pending,
                false,
                |_| panic!("partial path cannot discharge debt"),
                || panic!("retry not due"),
            );
            assert!(!persist_checkpoint(&db, &mut pending));
            process_pending(
                &mut pending,
                true,
                |_| panic!("full recovery required"),
                || Err(MacEveryError::Cli("injected full retry failure".into())),
            );
            assert!(enforce_worker_plan_bound(&mut pending, &roots));
            assert!(!persist_checkpoint(&db, &mut pending));
            assert_eq!(pending.full_recovery_generation, Some(900));
            assert!(db
                .kind_for_path(&stale.to_string_lossy())
                .unwrap()
                .is_some());
            fs::write(root.join("later-code.txt"), "later").unwrap();
            process_pending(
                &mut pending,
                true,
                |_| panic!("full recovery required"),
                || refresh_roots(&db, &roots, &[]),
            );
            assert!(
                !pending.full_recovery_obligation && pending.full_recovery_generation.is_none()
            );
            assert!(enforce_worker_plan_bound(&mut pending, &roots));
            assert!(persist_checkpoint(&db, &mut pending));
            assert_eq!(
                db.get_meta(META_LAST_EVENT_ID).unwrap().as_deref(),
                Some("201")
            );
            assert!(db
                .kind_for_path(&stale.to_string_lossy())
                .unwrap()
                .is_none());
            assert!(db
                .kind_for_path(&root.join("live-code.txt").to_string_lossy())
                .unwrap()
                .is_some());
            assert!(db
                .kind_for_path(&root.join("later-code.txt").to_string_lossy())
                .unwrap()
                .is_some());
            drop(db);
            fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn ordinary_unknown_or_alias_namespace_falls_back_to_configured_roots() {
            for (root, outside) in [
                ("/tmp/project", "/private/tmp/project/new-code.txt"),
                ("/root/project", "/unrelated/new-code.txt"),
                ("/root/project", "/root/project/../elsewhere"),
            ] {
                let mut pending = PendingWork::default();
                pending.add_scoped(
                    FsEvent {
                        path: outside.into(),
                        flags: 0,
                        id: 20,
                    },
                    &[PathBuf::from(root)],
                );
                assert!(pending.rescan_roots && pending.scope_fallback);
                assert!(pending.paths.is_empty());
                assert_eq!(pending.max_event_id, 20);
            }
            let mut ancestor = PendingWork::default();
            ancestor.add_scoped(
                FsEvent {
                    path: "/root".into(),
                    flags: 0,
                    id: 21,
                },
                &[PathBuf::from("/root/project")],
            );
            assert_eq!(
                ancestor
                    .paths
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["/root/project"]
            );
        }

        #[test]
        fn unexpected_uncovered_cleanup_is_never_claimed_by_configured_root_recovery() {
            let mut pending = PendingWork {
                full_recovery_obligation: true,
                ..PendingWork::default()
            };
            pending
                .deferred_cleanups
                .insert(PathBuf::from("/outside/or-alias"), 123);
            assert!(!enforce_worker_plan_bound(
                &mut pending,
                &[PathBuf::from("/root")]
            ));
            assert!(pending.worker_scope_failure);
            assert_eq!(
                pending
                    .deferred_cleanups
                    .get(Path::new("/outside/or-alias")),
                Some(&123)
            );
            let mut incoming = PendingWork::with_hash_state(pending.hash_state.clone());
            incoming.add(FsEvent {
                path: "/root/new".into(),
                flags: 0,
                id: 22,
            });
            pending.merge_incoming(incoming);
            assert!(pending.paths.is_empty());
            pending.rescan_roots = true;
            process_pending(
                &mut pending,
                true,
                |_| panic!("no admitted new path"),
                || Ok(IndexSummary::default()),
            );
            assert!(pending.full_recovery_obligation && pending.has_work());
            assert_eq!(
                pending
                    .deferred_cleanups
                    .get(Path::new("/outside/or-alias")),
                Some(&123)
            );
        }

        #[test]
        fn upstream_selection_is_bounded_exact_existing_and_preserves_watched_roots() {
            let temp = temp_dir("upstream-selection");
            fs::create_dir_all(&temp).unwrap();
            let temp = fs::canonicalize(temp).unwrap();
            let root = temp.join("root");
            let other = temp.join("other");
            for directory in [
                root.join("state"),
                root.join("Library"),
                other.join("Library"),
                root.join(".codex"),
                root.join(".cargo/registry"),
                root.join(".npm"),
                root.join(".cache"),
                root.join(".gemini"),
                root.join(".cursor"),
            ] {
                fs::create_dir_all(directory).unwrap();
            }
            let db_path = root.join("state/index.sqlite");
            let roots = vec![root.clone(), other.clone()];
            let excludes = runtime_excludes(
                ["Library", ".cargo/registry", ".npm", ".cache"]
                    .map(str::to_string)
                    .to_vec(),
                &db_path,
            );
            let selected = upstream_exclusion_paths(&db_path, &roots, &excludes);
            assert_eq!(selected.len(), 8);
            assert_eq!(selected[0], root.join("state"));
            assert_eq!(selected[1], root.join("Library"));
            assert_eq!(selected[2], other.join("Library"));
            let matcher = ExcludeMatcher::new(&excludes);
            assert!(selected
                .iter()
                .all(|path| path.is_dir() && matcher.is_excluded(path)));
            assert!(selected.iter().all(|path| roots
                .iter()
                .any(|root| path != root && path.starts_with(root))));
            let nested = root.join("Library/nested-root");
            fs::create_dir_all(&nested).unwrap();
            let nested_roots = vec![root.clone(), nested];
            let selected = upstream_exclusion_paths(&db_path, &nested_roots, &excludes);
            assert!(!selected.contains(&root.join("Library")));
            assert!(!selected.contains(&root));
            assert!(!selected.contains(&other.join("Library")));
            let empty = upstream_exclusion_paths(&db_path, &roots, &[]);
            assert!(
                empty.is_empty(),
                "existing directory alone is not exclusion authorization"
            );
            fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn upstream_selection_skips_aliases_missing_paths_and_outside_database() {
            use std::os::unix::fs::symlink;
            let temp = temp_dir("upstream-alias");
            fs::create_dir_all(&temp).unwrap();
            let temp = fs::canonicalize(temp).unwrap();
            let root = temp.join("root");
            let outside = temp.join("outside");
            fs::create_dir_all(root.join("visible")).unwrap();
            fs::create_dir_all(&outside).unwrap();
            symlink(root.join("visible"), root.join(".cache")).unwrap();
            symlink(&root, temp.join("alias")).unwrap();
            let db_path = outside.join("index.sqlite");
            let excludes = runtime_excludes(vec!["Library".into(), ".cache".into()], &db_path);
            assert!(
                upstream_exclusion_paths(&db_path, std::slice::from_ref(&root), &excludes)
                    .is_empty()
            );
            fs::create_dir_all(root.join("Library")).unwrap();
            assert_eq!(
                upstream_exclusion_paths(&db_path, &[root], &excludes),
                [temp.join("root/Library")]
            );
            assert!(
                upstream_exclusion_paths(&db_path, &[temp.join("alias")], &excludes).is_empty()
            );
            fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn upstream_api_failure_keeps_cf_lifetimes_and_callback_recovery_filter() {
            let mut stream = FSEventStreamGuard {
                stream: ptr::null_mut(),
                started: false,
                exclusions: None,
            };
            let paths = vec![PathBuf::from("/root/Library")];
            let mut called = false;
            assert!(!configure_upstream_exclusions_with(
                &mut stream,
                &paths,
                |_, array| {
                    called = true;
                    assert!(!array.is_null());
                    false
                }
            ));
            assert!(called && stream.exclusions.is_some());
            let filter = EventFilter::new(Path::new("/state/index.sqlite"), &["Library".into()]);
            assert!(!filter.accepts("/root/Library/a", 0));
            assert!(filter.accepts("/root/visible/a", 0));
            assert!(filter.accepts(
                "/root/Library/a",
                FLAG_USER_DROPPED | FLAG_MUST_SCAN_SUBDIRS
            ));
            assert!(!configure_upstream_exclusions_with(
                &mut stream,
                &[],
                |_, _| panic!("empty selection must not call API")
            ));
            assert!(!configure_upstream_exclusions_with(
                &mut stream,
                &vec![paths[0].clone(); 9],
                |_, _| panic!("SDK limit must be enforced")
            ));
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
        fn deferred_cleanup_retry_restamps_unchanged_rows_before_old_generation_delete() {
            use crate::model::{FileKind, FileRecord};
            let temp = temp_dir("watch-exact-deferred-generation");
            let root = temp.join("root");
            let failed = root.join("failed");
            let keep = failed.join("keep.txt");
            let stale = root.join("removed.txt");
            fs::create_dir_all(&failed).unwrap();
            fs::write(&keep, "unchanged content").unwrap();
            let db = Database::open(temp.join("index.sqlite")).unwrap();
            refresh_roots(&db, std::slice::from_ref(&root), &[]).unwrap();
            let before = db.all_records().unwrap();
            let old_keep = before
                .iter()
                .find(|r| r.path == keep.to_string_lossy())
                .unwrap()
                .clone();
            let generation = old_keep.indexed_at + 1;
            // Model an incomplete legacy scan: visited directories were
            // stamped, while the unreadable child's unchanged file was not.
            for mut row in before {
                if row.path != keep.to_string_lossy() {
                    row.indexed_at = generation;
                    db.upsert_file(&row).unwrap();
                }
            }
            db.upsert_file(&FileRecord::new(
                stale.to_string_lossy().into_owned(),
                FileKind::File,
                old_keep.indexed_at,
            ))
            .unwrap();
            let mut pending = PendingWork::default();
            pending
                .retry_paths
                .insert(failed.to_string_lossy().into_owned(), 0);
            pending.deferred_cleanups.insert(root.clone(), generation);
            pending.max_event_id = 42;
            let legacy = pending.needs_legacy_generation();
            assert!(legacy);
            process_pending(
                &mut pending,
                true,
                |path| {
                    if legacy {
                        refresh_path_legacy(&db, path, &[])
                    } else {
                        refresh_path(&db, path, &[])
                    }
                },
                || panic!("retry stays within the failed child"),
            );
            assert!(persist_checkpoint(&db, &mut pending));
            let after = db.all_records().unwrap();
            let kept = after
                .iter()
                .find(|r| r.path == keep.to_string_lossy())
                .unwrap();
            assert_eq!(kept.id, old_keep.id);
            assert!(kept.indexed_at > generation);
            assert!(!after.iter().any(|r| r.path == stale.to_string_lossy()));
            assert!(!pending.needs_legacy_generation());
            let mut fresh = PendingWork {
                full_recovery_obligation: true,
                rescan_roots: true,
                ..PendingWork::default()
            };
            assert!(
                !fresh.needs_legacy_generation(),
                "fresh root recovery is eligible for exact seen IDs"
            );
            fresh.full_recovery_generation = Some(generation);
            assert!(
                fresh.needs_legacy_generation(),
                "compressed old generation debt must still restamp rows"
            );
            drop(db);
            fs::remove_dir_all(temp).unwrap();
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
            assert!(line.contains("\"admitted_raw\":{\"must_scan\":10,\"user_dropped\":1,"));
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
        fn raw_counts_keep_middle_flags_and_delivery_ids_through_merge_and_clear() {
            let mut first = PendingWork::default();
            let mut second = PendingWork::default();
            for id in 0..10 {
                let flags = match id {
                    0 => FLAG_MUST_SCAN_SUBDIRS | FLAG_ROOT_CHANGED,
                    6 => FLAG_USER_DROPPED | FLAG_KERNEL_DROPPED,
                    7 => FLAG_IDS_WRAPPED,
                    _ => 0,
                };
                let target = if id < 5 { &mut first } else { &mut second };
                target.add(FsEvent {
                    path: format!("/private/secret-{id}"),
                    flags,
                    id,
                });
            }
            first.merge_incoming(second);
            assert_eq!(first.event_count, 10);
            assert!(first
                .first_events
                .iter()
                .flatten()
                .all(|e| e.flags & FLAG_USER_DROPPED == 0));
            assert_eq!(first.last_event.unwrap().flags, 0);
            assert_eq!(
                first.recovery_event.unwrap().flags,
                FLAG_MUST_SCAN_SUBDIRS | FLAG_ROOT_CHANGED
            );
            assert_eq!(
                first.raw_counts,
                RawEventCounts {
                    must_scan: 1,
                    user_dropped: 1,
                    kernel_dropped: 1,
                    ids_wrapped: 1,
                    root_changed: 1,
                    first_id: Some(0),
                    last_id: Some(9),
                }
            );
            let watermark = first.max_event_id;
            first.clear_diagnostics();
            assert_eq!(first.raw_counts, RawEventCounts::default());
            assert_eq!(first.event_count, 0);
            assert_eq!(first.max_event_id, watermark);
            assert!(
                first.has_work(),
                "clearing diagnostics must not clear recovery"
            );
        }

        #[test]
        fn callback_signal_counts_are_raw_lifetime_counts_not_batch_or() {
            let state = CallbackState {
                inbox: Arc::new(EventInbox::new(
                    vec![PathBuf::from("/fixture")],
                    RandomState::new(),
                )),
                filter: EventFilter::new(Path::new("/private/state/index.sqlite"), &[]),
            };
            let paths = (0..5)
                .map(|n| CString::new(format!("/fixture/{n}")).unwrap())
                .collect::<Vec<_>>();
            let mut pointers = paths.iter().map(|p| p.as_ptr()).collect::<Vec<_>>();
            let flags = [
                FLAG_MUST_SCAN_SUBDIRS,
                FLAG_USER_DROPPED,
                FLAG_KERNEL_DROPPED,
                FLAG_IDS_WRAPPED,
                FLAG_ROOT_CHANGED,
            ];
            let ids = [9, 10, 11, 0, 1];
            for expected in 1..=2 {
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
                let batch = state.inbox.take_batch(Duration::ZERO);
                assert_eq!(batch.pending.raw_counts.user_dropped, 1);
                let lifetime = state.filter.counters.callback_signal_counts();
                assert_eq!(lifetime.must_scan, expected);
                assert_eq!(lifetime.user_dropped, expected);
                assert_eq!(lifetime.kernel_dropped, expected);
                assert_eq!(lifetime.ids_wrapped, expected);
                assert_eq!(lifetime.root_changed, expected);
                assert_eq!((lifetime.first_id, lifetime.last_id), (Some(9), Some(1)));
            }
            // This read/swap is deliberately independent of lifetime counters.
            state.filter.counters.flags.swap(0, Ordering::Relaxed);
            assert_eq!(
                state.filter.counters.callback_signal_counts().user_dropped,
                2
            );
        }

        #[test]
        fn raw_signal_counts_saturate_without_wrapping() {
            let max = RawEventCounts {
                must_scan: u64::MAX,
                user_dropped: u64::MAX,
                kernel_dropped: u64::MAX,
                ids_wrapped: u64::MAX,
                root_changed: u64::MAX,
                first_id: Some(u64::MAX),
                last_id: Some(u64::MAX),
            };
            let mut counts = max;
            counts.record(0x2f, 0);
            let mut incoming = RawEventCounts::default();
            incoming.record(0x2f, 1);
            counts.merge(incoming);
            assert_eq!(counts.must_scan, u64::MAX);
            assert_eq!(counts.user_dropped, u64::MAX);
            assert_eq!(counts.kernel_dropped, u64::MAX);
            assert_eq!(counts.ids_wrapped, u64::MAX);
            assert_eq!(counts.root_changed, u64::MAX);
            assert_eq!((counts.first_id, counts.last_id), (Some(u64::MAX), Some(1)));
            let counters = FilterCounters::default();
            *counters.callback_signals.lock().unwrap() = max;
            counters.record_callback_signals(0x2f, 0);
            assert_eq!(counters.callback_signal_counts().user_dropped, u64::MAX);
        }

        #[test]
        fn startup_summary_is_private_bounded_and_survives_trigger_rotation() {
            use std::os::unix::fs::symlink;
            let temp = temp_dir("watch-startup");
            fs::create_dir_all(&temp).unwrap();
            let db_path = temp.join("secret-index.sqlite");
            let mut log = TriggerLog::new(&db_path);
            log.startup_summary(3, 7, false, 0x16).unwrap();
            let path = temp.join(STARTUP_FILE_NAME);
            let original = fs::read(&path).unwrap();
            let value = String::from_utf8(original.clone()).unwrap();
            assert!(value.contains("\"roots\":3,"));
            assert!(value.contains("\"upstream_requested\":7,"));
            assert!(value.contains("\"upstream_accepted\":false,"));
            assert!(value.contains("\"create_flags\":22,"));
            assert!(value.contains("\"latency_ms\":350}"));
            let session = format!("\"session\":\"{:016x}\"", log.session);
            assert!(value.contains(&session));
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(original.len() <= STARTUP_FILE_BYTES);
            assert!(!String::from_utf8_lossy(&original).contains("secret-index"));
            let pending = PendingWork::default();
            log.counters.record_callback_signals(FLAG_USER_DROPPED, 42);
            for _ in 0..3 {
                private_log_file(&log.path)
                    .unwrap()
                    .set_len(LOG_FILE_BYTES - 1)
                    .unwrap();
                log.record("test", "roots", &pending, None, LogInfo::default());
                let line = fs::read_to_string(&log.path).unwrap();
                assert!(line.contains("\"callback_signals\":{\"must_scan\":0,\"user_dropped\":1,"));
                assert!(line.contains(&session));
                assert!(fs::metadata(&log.path).unwrap().len() <= LOG_RECORD_BYTES as u64);
                assert_eq!(fs::read(&path).unwrap(), original);
            }
            let filter = EventFilter::new(&db_path, &[]);
            assert!(!filter.accepts(path.to_str().unwrap(), 0));
            assert!(!filter.accepts(path.with_extension("lock").to_str().unwrap(), 0));
            assert_eq!(filter.counters.received.load(Ordering::Relaxed), 0);
            assert!(write_startup_summary(&path, &vec![0; STARTUP_FILE_BYTES + 1]).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
            let victim = temp.join("keep");
            fs::write(&victim, "untouched").unwrap();
            fs::remove_file(&path).unwrap();
            symlink(&victim, &path).unwrap();
            assert!(log.startup_summary(1, 0, true, 0x16).is_err());
            fs::remove_file(&path).unwrap();
            fs::hard_link(&victim, &path).unwrap();
            assert!(log.startup_summary(1, 0, true, 0x16).is_err());
            assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
            fs::remove_file(&path).unwrap();
            fs::write(&path, vec![0; STARTUP_FILE_BYTES + 1]).unwrap();
            assert!(log.startup_summary(1, 0, true, 0x16).is_err());
            fs::remove_dir_all(temp).unwrap();
        }

        #[test]
        fn diagnostic_rate_limits_reserve_recovery_slots_and_report_suppression() {
            let temp = temp_dir("watch-diagnostic-rate");
            std::fs::create_dir_all(&temp).unwrap();
            let mut log = TriggerLog::new(&temp.join("index.sqlite"));
            log.counters.record_callback_signals(FLAG_USER_DROPPED, 17);
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
            assert_eq!(log.counters.callback_signal_counts().user_dropped, 1);
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
            log.counters.record_callback_signals(FLAG_USER_DROPPED, 29);
            assert!(log.startup_summary(1, 0, false, 0x16).is_err());
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
            assert!(std::fs::read_to_string(&log.path)
                .unwrap()
                .contains("\"callback_signals\":{\"must_scan\":0,\"user_dropped\":1,"));
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
