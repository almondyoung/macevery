use crate::error::{MacEveryError, Result};
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use crate::db::Database;
    use crate::process_lifetime;
    use crate::scanner::{refresh_path, refresh_roots};
    use std::collections::BTreeMap;
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_void};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::thread;
    use std::time::Duration;

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

    struct CallbackState {
        sender: Sender<FsEvent>,
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
        let since_when = match decision {
            CheckpointDecision::UseCheckpoint(id) => id,
            CheckpointDecision::FullRescan => {
                eprintln!(
                    "watch: checkpoint invalid (stored={:?}, system={}), performing full rescan",
                    stored_event_id, current_system_id
                );
                let db = Database::open(db_path.clone())?;
                let summary = refresh_roots(&db, &roots, &excludes)?;
                db.set_meta(META_LAST_EVENT_ID, &current_system_id.to_string())?;
                eprintln!(
                    "watch: full rescan complete, {} entries, {} stale removed",
                    summary.indexed, summary.deleted
                );
                drop(db);
                current_system_id
            }
        };
        let (sender, receiver) = mpsc::channel::<FsEvent>();
        spawn_processor(db_path, roots.clone(), excludes, receiver);

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
            root_strings.push(cf_string);
            root_values.push(cf_string);
        }

        let paths = unsafe {
            CFArrayCreate(
                ptr::null(),
                root_values.as_ptr(),
                root_values.len() as CFIndex,
                ptr::null(),
            )
        };
        if paths.is_null() {
            return Err(MacEveryError::Cli(
                "failed to create FSEvents root array".to_string(),
            ));
        }

        let state = Box::new(CallbackState { sender });
        let mut context = FSEventStreamContext {
            version: 0,
            info: Box::into_raw(state).cast(),
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
                paths,
                since_when,
                0.35,
                flags,
            )
        };
        if stream.is_null() {
            return Err(MacEveryError::Cli(
                "failed to create FSEventStream".to_string(),
            ));
        }

        unsafe {
            FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        }
        if unsafe { FSEventStreamStart(stream) } == 0 {
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

        for value in root_strings {
            unsafe {
                CFRelease(value);
            }
        }
        unsafe {
            CFRelease(paths);
        }
        Ok(())
    }

    fn spawn_processor(
        db_path: PathBuf,
        roots: Vec<PathBuf>,
        excludes: Vec<String>,
        receiver: Receiver<FsEvent>,
    ) {
        thread::spawn(move || {
            let db_dir = db_path.parent().map(PathBuf::from);
            let db = match Database::open(db_path) {
                Ok(db) => db,
                Err(err) => {
                    eprintln!("watch: failed to open database: {err}");
                    return;
                }
            };

            loop {
                let Ok(first) = receiver.recv() else {
                    return;
                };
                let mut batch = vec![first];
                loop {
                    match receiver.recv_timeout(Duration::from_millis(150)) {
                        Ok(event) => batch.push(event),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                let max_event_id = batch.iter().map(|event| event.id).max().unwrap_or(0);
                let batch = batch
                    .into_iter()
                    .filter(|event| {
                        !is_ignored_event_path(&event.path, db.path(), db_dir.as_deref())
                    })
                    .collect::<Vec<_>>();
                if batch.is_empty() {
                    continue;
                }

                let action = batch_action(&batch);
                let processed = match action {
                    WatchAction::RescanRoots => match refresh_roots(&db, &roots, &excludes) {
                        Ok(summary) => {
                            eprintln!(
                                "watch: rescanned roots, {} entries, {} stale removed",
                                summary.indexed, summary.deleted
                            );
                            true
                        }
                        Err(err) => {
                            eprintln!("watch: root rescan failed: {err}");
                            false
                        }
                    },
                    WatchAction::RefreshPaths => {
                        let mut merged = BTreeMap::<String, FSEventStreamEventFlags>::new();
                        for event in &batch {
                            merged
                                .entry(event.path.clone())
                                .and_modify(|flags| *flags |= event.flags)
                                .or_insert(event.flags);
                        }

                        let paths = fold_refresh_paths(merged.keys().cloned().collect::<Vec<_>>());
                        let mut refreshed_paths = 0_u64;
                        let mut removed_paths = 0_u64;
                        let mut indexed_entries = 0_u64;
                        let mut deleted_entries = 0_u64;
                        let mut failed_paths = 0_u64;
                        let mut ok = true;
                        for path in paths {
                            match refresh_path(&db, &PathBuf::from(&path), &excludes) {
                                Ok(summary) => {
                                    if summary.indexed > 0 {
                                        refreshed_paths += 1;
                                        indexed_entries += summary.indexed;
                                        deleted_entries += summary.deleted;
                                    } else {
                                        removed_paths += 1;
                                    }
                                }
                                Err(err) => {
                                    eprintln!("watch: refresh failed for {path}: {err}");
                                    failed_paths += 1;
                                    ok = false;
                                }
                            }
                        }
                        let total_paths = refreshed_paths + removed_paths + failed_paths;
                        if total_paths > 0 {
                            eprintln!(
                                "watch: processed {total_paths} paths ({refreshed_paths} refreshed, {removed_paths} removed, {failed_paths} failed, {indexed_entries} entries, {deleted_entries} stale removed)"
                            );
                        }
                        ok
                    }
                };

                if processed && max_event_id > 0 {
                    if let Err(err) = db.set_meta(META_LAST_EVENT_ID, &max_event_id.to_string()) {
                        eprintln!("watch: failed to persist event checkpoint: {err}");
                    }
                }
            }
        });
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

    fn fold_refresh_paths(paths: Vec<String>) -> Vec<String> {
        let mut paths = paths;
        paths.sort();
        let mut folded = Vec::<String>::new();
        for path in paths {
            if folded
                .iter()
                .any(|parent| path_is_same_or_child(parent, &path))
            {
                continue;
            }
            folded.push(path);
        }
        folded
    }

    fn path_is_same_or_child(parent: &str, path: &str) -> bool {
        path == parent
            || path
                .strip_prefix(parent)
                .map(|rest| rest.starts_with('/'))
                .unwrap_or(false)
    }

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
            let path = CStr::from_ptr(path_ptr).to_string_lossy().to_string();
            let flags = *event_flags.add(index);
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
