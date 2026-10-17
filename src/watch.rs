use crate::error::{MacEveryError, Result};
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use crate::db::Database;
    use crate::process_lifetime;
    use crate::scanner::{refresh_path, refresh_roots, ExcludeMatcher, IndexSummary};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_void};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::thread;
    use std::time::{Duration, Instant};

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
    const FLAG_ROOT_CHANGED: FSEventStreamEventFlags = 0x0000_0020;
    const META_LAST_EVENT_ID: &str = "watch_last_event_id";

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
    }

    impl EventFilter {
        fn new(db_path: &Path, excludes: &[String]) -> Self {
            Self {
                db_path: db_path.to_path_buf(),
                db_dir: db_path.parent().map(PathBuf::from),
                excludes: ExcludeMatcher::new(excludes),
            }
        }

        fn accepts(&self, path: &str, flags: FSEventStreamEventFlags) -> bool {
            // Lost-event and root-change notifications must survive exclusions.
            action_for_flags(flags) == WatchAction::RescanRoots
                || (!is_ignored_event_path(path, &self.db_path, self.db_dir.as_deref())
                    && !self.excludes.is_excluded(Path::new(path)))
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
    }

    impl PendingWork {
        fn add(&mut self, event: FsEvent) {
            self.max_event_id = self.max_event_id.max(event.id);
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

        fn has_retries(&self) -> bool {
            self.retry_roots || !self.retry_paths.is_empty()
        }

        fn has_work(&self) -> bool {
            self.rescan_roots || !self.paths.is_empty() || self.has_retries()
        }

        fn has_pending(&self) -> bool {
            self.has_work() || self.max_event_id > 0 || !self.deferred_cleanups.is_empty()
        }
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
        let stats = db.stats()?;
        let stored_event_id = db
            .get_meta(META_LAST_EVENT_ID)?
            .and_then(|value| value.parse::<FSEventStreamEventId>().ok());
        drop(db);

        if stats.roots.is_empty() {
            return Err(MacEveryError::Cli(
                "no indexed roots found; run `macevery index --rebuild PATHS...` first".to_string(),
            ));
        }

        let roots = stats
            .roots
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let excludes = runtime_excludes(stats.excludes, &db_path);

        let current_system_id = unsafe { FSEventsGetCurrentEventId() };
        let decision = resolve_since_when(stored_event_id, current_system_id);
        let mut initial = PendingWork::default();
        let since_when = match decision {
            CheckpointDecision::UseCheckpoint(id) => id,
            CheckpointDecision::FullRescan => {
                eprintln!(
                    "watch: checkpoint invalid (stored={:?}, system={}), performing full rescan",
                    stored_event_id, current_system_id
                );
                let db = Database::open(db_path.clone())?;
                let summary = refresh_roots(&db, &roots, &excludes)?;
                let counts = (summary.indexed, summary.deleted);
                if summary.warnings.is_empty() {
                    db.set_meta(META_LAST_EVENT_ID, &current_system_id.to_string())?;
                } else {
                    log_scan_warnings("initial root rescan", &summary);
                    retain_incomplete(&mut initial, summary);
                    if !initial.has_work() {
                        initial.retry_roots = true;
                    }
                    initial.max_event_id = current_system_id;
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
        spawn_processor(db_path, roots.clone(), excludes, receiver, initial);

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
    ) {
        thread::spawn(move || {
            let db = match Database::open(db_path) {
                Ok(db) => db,
                Err(err) => {
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
                    receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
                };
                let mut disconnected = false;
                match first {
                    Ok(event) => {
                        pending.add(event);
                        let deadline = Instant::now() + MAX_BATCH_WAIT;
                        // Bound both latency and the number drained under constant writes.
                        for _ in 1..MAX_BATCH_EVENTS {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            match receiver.recv_timeout(BATCH_IDLE.min(remaining)) {
                                Ok(event) => pending.add(event),
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
                process_pending(
                    &mut pending,
                    retry_due,
                    |path| refresh_path(&db, path, &excludes),
                    || refresh_roots(&db, &roots, &excludes),
                );
                let complete = persist_checkpoint(&db, &mut pending);
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

    fn process_pending<F, R>(
        pending: &mut PendingWork,
        retry_due: bool,
        mut refresh: F,
        mut rescan: R,
    ) where
        F: FnMut(&Path) -> Result<IndexSummary>,
        R: FnMut() -> Result<IndexSummary>,
    {
        if retry_due {
            pending.paths.append(&mut pending.retry_paths);
            pending.rescan_roots |= std::mem::take(&mut pending.retry_roots);
        }
        if std::mem::take(&mut pending.rescan_roots) {
            match rescan() {
                Ok(summary) if summary.warnings.is_empty() => {
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
                    log_scan_warnings("root rescan", &summary);
                    pending.paths.clear();
                    pending.retry_paths.clear();
                    pending.retry_roots = summary.retry_paths.is_empty();
                    retain_incomplete(pending, summary);
                    return;
                }
                Err(err) => {
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
            let retry = match refresh(Path::new(&path)) {
                Ok(summary) if summary.warnings.is_empty() => {
                    indexed += summary.indexed;
                    deleted += summary.deleted;
                    pending
                        .retry_paths
                        .retain(|old, _| !Path::new(old).starts_with(&path));
                    false
                }
                Ok(summary) => {
                    log_scan_warnings(&path, &summary);
                    let retry_original = summary.retry_paths.is_empty();
                    retain_incomplete(pending, summary);
                    retry_original
                }
                Err(err) => {
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

    fn persist_checkpoint(db: &Database, pending: &mut PendingWork) -> bool {
        // A later successful batch cannot advance past an earlier failed path.
        if pending.has_work() {
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
                return false;
            }
            pending.deferred_cleanups.clear();
        }
        if pending.max_event_id > 0 {
            if let Err(err) = db.set_meta(META_LAST_EVENT_ID, &pending.max_event_id.to_string()) {
                eprintln!("watch: failed to persist event checkpoint: {err}; retry retained");
                return false;
            }
            pending.max_event_id = 0;
        }
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
        if flags
            & (FLAG_MUST_SCAN_SUBDIRS | FLAG_USER_DROPPED | FLAG_KERNEL_DROPPED | FLAG_ROOT_CHANGED)
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
                [4, 5, 6, 7]
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
                WatchAction::RescanRoots
            );
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
