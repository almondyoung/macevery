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
    use std::path::PathBuf;
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
        let since_when = db
            .get_meta(META_LAST_EVENT_ID)?
            .and_then(|value| value.parse::<FSEventStreamEventId>().ok())
            .unwrap_or(SINCE_NOW);
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
        let excludes = stats.excludes;
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

                        let mut ok = true;
                        for path in merged.keys() {
                            match refresh_path(&db, &PathBuf::from(path), &excludes) {
                                Ok(summary) => {
                                    if summary.indexed > 0 {
                                        eprintln!(
                                            "watch: refreshed {path} ({} entries, {} stale removed)",
                                            summary.indexed, summary.deleted
                                        );
                                    } else {
                                        eprintln!("watch: removed {path}");
                                    }
                                }
                                Err(err) => {
                                    eprintln!("watch: refresh failed for {path}: {err}");
                                    ok = false;
                                }
                            }
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
