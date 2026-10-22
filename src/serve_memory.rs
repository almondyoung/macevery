//! Bounded service-only allocation/VM evidence. The idle sampler never touches
//! the search lock or database; index counters are copied only after refreshes.
use crate::compact_index::{FragmentationStats, IndexMemoryBreakdown};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const FILE_NAME: &str = "macevery-serve-memory.jsonl";
const FILE_BYTES: u64 = 512 * 1024;
const RECORD_BYTES: usize = 4096;
const PENDING_RECORDS: usize = 16;
const INTERVAL: Duration = Duration::from_secs(60);

/// Cumulative traffic evidence, never query text. Loads are deliberately not a
/// transactional snapshot: concurrent admission/completion can straddle a row.
#[derive(Default)]
pub(crate) struct TrafficCounters {
    http_accepted: AtomicU64,
    http_completed: AtomicU64,
    http_rejected: AtomicU64,
    searches_started: AtomicU64,
    searches_completed: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TrafficSample {
    http_accepted: u64,
    http_completed: u64,
    http_rejected: u64,
    searches_started: u64,
    searches_completed: u64,
}

impl TrafficCounters {
    // accepted includes capacity-rejected TCP connections. completed counts an
    // admitted handler ending, including parse/auth/write errors and unwinding.
    pub(crate) fn http_accept(&self) {
        self.http_accepted.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn http_complete(&self) {
        self.http_completed.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn http_reject(&self) {
        self.http_rejected.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn search_start(&self) {
        self.searches_started.fetch_add(1, Ordering::Relaxed);
    }
    // Completion is the admitted search guard's end, not a successful result.
    pub(crate) fn search_complete(&self) {
        self.searches_completed.fetch_add(1, Ordering::Relaxed);
    }
    fn sample(&self) -> TrafficSample {
        TrafficSample {
            http_accepted: self.http_accepted.load(Ordering::Relaxed),
            http_completed: self.http_completed.load(Ordering::Relaxed),
            http_rejected: self.http_rejected.load(Ordering::Relaxed),
            searches_started: self.searches_started.load(Ordering::Relaxed),
            searches_completed: self.searches_completed.load(Ordering::Relaxed),
        }
    }
    #[cfg(test)]
    pub(crate) fn totals(&self) -> [u64; 5] {
        let sample = self.sample();
        [
            sample.http_accepted,
            sample.http_completed,
            sample.http_rejected,
            sample.searches_started,
            sample.searches_completed,
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub phase: &'static str,
    pub backend: &'static str,
    pub revision: Option<i64>,
    pub database_device: Option<u64>,
    pub database_inode: Option<u64>,
    pub database_token: Option<u64>,
    pub records: usize,
    pub physical_records: usize,
    pub capacity_bytes: u64,
    pub memory: IndexMemoryBreakdown,
    pub dead_records: usize,
    pub dead_string_bytes: usize,
    pub extension_growth_bytes: usize,
    pub load_attempts: u64,
    pub successful_loads: u64,
    pub incremental_attempts: u64,
    pub incremental_applies: u64,
    pub incremental_change_ids: u64,
    pub last_change_ids: usize,
    pub method: &'static str,
    pub fallback_reason: Option<&'static str>,
}

impl Snapshot {
    pub(crate) fn set_fragmentation(&mut self, value: FragmentationStats) {
        self.dead_records = value.dead_records;
        self.dead_string_bytes = value.dead_string_bytes;
        self.extension_growth_bytes = value.extension_growth_bytes;
        self.physical_records = self.records.saturating_add(value.dead_records);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct VmSample {
    resident: Option<u64>,
    internal: Option<u64>,
    compressed: Option<u64>,
    reusable: Option<u64>,
    footprint: Option<u64>,
    cpu_ns: Option<u64>,
    process_start_ticks: Option<u64>,
    uuid: [u8; 16],
    vm_error: Option<i32>,
    cpu_error: Option<i32>,
}

#[derive(Clone, Copy)]
struct Record {
    snapshot: Snapshot,
    vm: VmSample,
    at_ms: u128,
    snapshot_at_ms: u128,
    idle: bool,
    traffic: TrafficSample,
}

struct State {
    latest: Snapshot,
    latest_at_ms: u128,
    pending: VecDeque<Record>,
    capture_window: Instant,
    captures: u32,
    suppressed: u64,
    io_failures: u64,
    sequence: u64,
}

pub(crate) struct MemoryAudit {
    path: PathBuf,
    enabled: bool,
    session: u128,
    started: AtomicBool,
    traffic: Arc<TrafficCounters>,
    state: Mutex<State>,
}

impl MemoryAudit {
    pub(crate) fn new(db_path: &Path) -> Arc<Self> {
        let enabled = cfg!(target_os = "macos")
            && std::env::var("MACEVERY_SERVE_MEMORY_LOG").as_deref() != Ok("0");
        Arc::new(Self::at_path(
            db_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(FILE_NAME),
            enabled,
        ))
    }

    fn at_path(path: PathBuf, enabled: bool) -> Self {
        Self {
            path,
            enabled,
            session: epoch_ms(),
            started: AtomicBool::new(false),
            traffic: Arc::new(TrafficCounters::default()),
            state: Mutex::new(State {
                latest: Snapshot::default(),
                latest_at_ms: 0,
                pending: VecDeque::with_capacity(PENDING_RECORDS),
                capture_window: Instant::now(),
                captures: 0,
                suppressed: 0,
                io_failures: 0,
                sequence: 0,
            }),
        }
    }

    pub(crate) fn traffic(&self) -> Arc<TrafficCounters> {
        Arc::clone(&self.traffic)
    }

    #[cfg(test)]
    pub(crate) fn for_test(db_path: &Path) -> Arc<Self> {
        Arc::new(Self::at_path(
            db_path.with_extension("test-memory.jsonl"),
            true,
        ))
    }

    #[cfg(test)]
    pub(crate) fn pending_phases(&self) -> (Vec<&'static str>, u64) {
        let state = self.state.lock().unwrap();
        (
            state
                .pending
                .iter()
                .map(|record| record.snapshot.phase)
                .collect(),
            state.suppressed,
        )
    }

    /// Refreshes may call this; unchanged searches and /status do not. Coalesce
    /// storms to two captures per second (a before/after pair) and a fixed-size
    /// queue. No file I/O here. Suppression/error counters are cumulative.
    pub(crate) fn publish(&self, snapshot: Snapshot) {
        self.publish_at(snapshot, Instant::now());
    }

    /// Mark an in-flight load without spending its outcome's VM capture slot.
    /// Idle records still identify the current cached phase while loading.
    pub(crate) fn cache_only(&self, snapshot: Snapshot) {
        self.update_at(snapshot, Instant::now(), false);
    }

    fn publish_at(&self, snapshot: Snapshot, now: Instant) {
        self.update_at(snapshot, now, true);
    }

    fn update_at(&self, snapshot: Snapshot, now: Instant, capture_vm: bool) {
        if !self.enabled {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.latest = snapshot;
        state.latest_at_ms = epoch_ms();
        if !capture_vm {
            return;
        }
        if now.duration_since(state.capture_window) >= Duration::from_secs(1) {
            state.capture_window = now;
            state.captures = 0;
        }
        if state.captures >= 2 {
            state.suppressed = state.suppressed.saturating_add(1);
            return;
        }
        state.captures += 1;
        if state.pending.len() == PENDING_RECORDS {
            state.pending.pop_front();
            state.suppressed = state.suppressed.saturating_add(1);
        }
        let snapshot_at_ms = state.latest_at_ms;
        state.pending.push_back(Record {
            snapshot,
            vm: sample_self(),
            at_ms: epoch_ms(),
            snapshot_at_ms,
            idle: false,
            traffic: self.traffic.sample(),
        });
    }

    /// One service-owned weak timer, started only by serve(), never unit loads.
    /// It exits when the service state disappears and never takes its RwLock.
    pub(crate) fn start(self: &Arc<Self>) {
        if !self.enabled || self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("serve-memory".into())
            .spawn(move || loop {
                let Some(audit) = weak.upgrade() else { break };
                audit.flush();
                drop(audit);
                std::thread::sleep(INTERVAL);
            });
    }

    fn flush(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // Fixed stack storage avoids growing a second persistent queue.
        let mut records = [None; PENDING_RECORDS + 1];
        let mut count = 0;
        while let Some(record) = state.pending.pop_front() {
            records[count] = Some(record);
            count += 1;
        }
        records[count] = Some(Record {
            snapshot: state.latest,
            vm: sample_self(),
            at_ms: epoch_ms(),
            snapshot_at_ms: state.latest_at_ms,
            idle: true,
            traffic: self.traffic.sample(),
        });
        let mut sequence = state.sequence;
        let suppressed = state.suppressed;
        let io_failures = state.io_failures;
        drop(state);

        let total = count + 1;
        let mut written = 0;
        let result = (|| {
            for record in records.into_iter().flatten() {
                sequence = sequence.saturating_add(1);
                let line = self.line(record, sequence, suppressed, io_failures);
                append_record(&self.path, line.as_bytes())?;
                written += 1;
            }
            Ok::<_, io::Error>(())
        })();
        if let Ok(mut state) = self.state.lock() {
            state.sequence = sequence;
            if result.is_err() {
                state.io_failures = state.io_failures.saturating_add(1);
                state.suppressed = state.suppressed.saturating_add((total - written) as u64);
            }
        }
        // Best-effort evidence only: permission/rotation failures never change
        // search readiness, index publication, or create stderr retry storms.
    }

    fn line(&self, record: Record, sequence: u64, suppressed: u64, io_failures: u64) -> String {
        let s = record.snapshot;
        let m = s.memory;
        let v = record.vm;
        let t = record.traffic;
        let uuid = v
            .uuid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!(concat!(
            "{{\"v\":1,\"pid\":{},\"session_ms\":{},\"sequence\":{},\"at_ms\":{},\"snapshot_at_ms\":{},\"phase\":\"{}\",\"cached_phase\":\"{}\",\"backend\":\"{}\",",
            "\"loaded_revision\":{},\"database_device\":{},\"database_inode\":{},\"database_token\":{},\"records\":{},\"physical_records\":{},\"capacity_bytes\":{},",
            "\"record_capacity_bytes\":{},\"metadata_capacity_bytes\":{},\"string_capacity_bytes\":{},\"string_used_bytes\":{},\"extension_capacity_bytes\":{},\"container_bytes\":{},",
            "\"prefix_string_bytes\":{},\"prefix_entry_capacity_bytes\":{},\"prefix_lookup_capacity_bytes\":{},\"prefix_entries\":{},\"narrow_metadata_blocks\":{},\"wide_metadata_blocks\":{},\"live_metadata_escapes\":{},",
            "\"dead_records\":{},\"dead_string_bytes\":{},\"extension_growth_bytes\":{},\"load_attempts\":{},\"successful_loads\":{},\"incremental_attempts\":{},\"incremental_applies\":{},\"incremental_change_ids\":{},\"last_change_ids\":{},\"method\":\"{}\",\"fallback_reason\":{},",
            "\"resident_bytes\":{},\"internal_bytes\":{},\"compressed_bytes\":{},\"reusable_bytes\":{},\"footprint_bytes\":{},\"cpu_ns\":{},\"process_start_ticks\":{},\"process_uuid\":\"{}\",\"vm_error\":{},\"cpu_error\":{},\"http_accepted\":{},\"http_completed\":{},\"http_rejected\":{},\"searches_started\":{},\"searches_completed\":{},\"suppressed_records\":{},\"io_failures\":{}}}\n"),
            std::process::id(), self.session, sequence, record.at_ms, record.snapshot_at_ms, if record.idle { "idle" } else { s.phase }, s.phase, s.backend,
            optional(s.revision), optional(s.database_device), optional(s.database_inode), optional(s.database_token), s.records, s.physical_records, s.capacity_bytes,
            m.record_capacity_bytes, m.metadata_capacity_bytes, m.string_capacity_bytes, m.string_used_bytes, m.extension_capacity_bytes, m.container_bytes,
            m.prefix_string_bytes, m.prefix_entry_capacity_bytes, m.prefix_lookup_capacity_bytes, m.prefix_entries, m.narrow_metadata_blocks, m.wide_metadata_blocks, m.live_metadata_escapes,
            s.dead_records, s.dead_string_bytes, s.extension_growth_bytes, s.load_attempts, s.successful_loads, s.incremental_attempts, s.incremental_applies, s.incremental_change_ids, s.last_change_ids, s.method,
            s.fallback_reason.map(|value| format!("\"{value}\"")).unwrap_or_else(|| "null".into()),
            optional(v.resident), optional(v.internal), optional(v.compressed), optional(v.reusable), optional(v.footprint), optional(v.cpu_ns), optional(v.process_start_ticks), uuid,
            optional(v.vm_error), optional(v.cpu_error), t.http_accepted, t.http_completed, t.http_rejected, t.searches_started, t.searches_completed, suppressed, io_failures)
    }
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".into())
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
mod native {
    use super::VmSample;
    // macOS SDK mach/task_info.h TASK_VM_INFO_REV1_COUNT: the stable prefix
    // through phys_footprint is 152 bytes / 38 natural_t. Check the returned
    // count before exposing any field; do not pretend an unavailable API is 0.
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfoRev1 {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct RusageInfoV0 {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        pkg_idle_wkups: u64,
        interrupt_wkups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        phys_footprint: u64,
        proc_start_abstime: u64,
        proc_exit_abstime: u64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: i32, data: *mut i32, count: *mut u32) -> i32;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV0) -> i32;
    }
    pub(super) fn sample() -> VmSample {
        let mut out = VmSample::default();
        let mut vm = TaskVmInfoRev1::default();
        let expected = (std::mem::size_of::<TaskVmInfoRev1>() / 4) as u32;
        let mut count = expected;
        let code = unsafe {
            task_info(
                mach_task_self_,
                22,
                (&mut vm as *mut TaskVmInfoRev1).cast(),
                &mut count,
            )
        };
        if code == 0 && count >= expected {
            out.resident = Some(vm.resident_size);
            out.internal = Some(vm.internal);
            out.compressed = Some(vm.compressed);
            out.reusable = Some(vm.reusable);
            out.footprint = Some(vm.phys_footprint);
        } else {
            out.vm_error = Some(if code == 0 { -1 } else { code });
        }
        let mut usage = RusageInfoV0::default();
        let code = unsafe { proc_pid_rusage(std::process::id() as i32, 0, &mut usage) };
        let mut timebase = Timebase::default();
        let time_code = unsafe { mach_timebase_info(&mut timebase) };
        if code == 0 && time_code == 0 && timebase.denom != 0 {
            let ticks = u128::from(usage.user_time) + u128::from(usage.system_time);
            out.cpu_ns =
                u64::try_from(ticks * u128::from(timebase.numer) / u128::from(timebase.denom)).ok();
            out.process_start_ticks = Some(usage.proc_start_abstime);
            out.uuid = usage.uuid;
        } else {
            out.cpu_error = Some(if code != 0 {
                code
            } else if time_code != 0 {
                time_code
            } else {
                -1
            });
        }
        out
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn sdk_prefix_layout_and_self_metrics_are_valid() {
            assert_eq!(std::mem::size_of::<TaskVmInfoRev1>(), 152);
            assert_eq!(std::mem::offset_of!(TaskVmInfoRev1, resident_size), 16);
            assert_eq!(std::mem::offset_of!(TaskVmInfoRev1, internal), 48);
            assert_eq!(std::mem::offset_of!(TaskVmInfoRev1, reusable), 80);
            assert_eq!(std::mem::offset_of!(TaskVmInfoRev1, compressed), 120);
            assert_eq!(std::mem::offset_of!(TaskVmInfoRev1, phys_footprint), 144);
            assert_eq!(std::mem::size_of::<RusageInfoV0>(), 96);
            assert_eq!(std::mem::offset_of!(RusageInfoV0, proc_start_abstime), 80);
            let sample = sample();
            assert_eq!(sample.vm_error, None);
            assert_eq!(sample.cpu_error, None);
            assert!(sample.footprint.is_some_and(|bytes| bytes > 0));
            assert!(sample.process_start_ticks.is_some_and(|ticks| ticks > 0));
        }
    }
}

fn sample_self() -> VmSample {
    #[cfg(target_os = "macos")]
    {
        native::sample()
    }
    #[cfg(not(target_os = "macos"))]
    {
        VmSample {
            vm_error: Some(-1),
            cpu_error: Some(-1),
            ..VmSample::default()
        }
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "macos")]
    const NOFOLLOW: i32 = 0x100;
    #[cfg(not(target_os = "macos"))]
    const NOFOLLOW: i32 = 0x20000;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        // O_NONBLOCK also prevents an existing FIFO from hanging the timer
        // before its type can be rejected below (regular-file I/O is unchanged).
        .custom_flags(NOFOLLOW | 0x04)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory audit requires a private regular file",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(unix)]
fn append_record(path: &Path, line: &[u8]) -> io::Result<()> {
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    if line.len() > RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory audit record exceeds bound",
        ));
    }
    let lock = private_file(&path.with_extension("lock"))?;
    if lock.metadata()?.len() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory audit lock is not empty",
        ));
    }
    if unsafe { flock(lock.as_raw_fd(), 0x02 | 0x04) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = private_file(path)?;
    let metadata = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if named.dev() != metadata.dev()
        || named.ino() != metadata.ino()
        || !named.is_file()
        || named.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory audit path changed while opening",
        ));
    }
    let size = metadata.len();
    if size > FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pre-existing memory audit exceeds bound",
        ));
    }
    if size + line.len() as u64 > FILE_BYTES {
        let previous = path.with_extension("jsonl.1");
        match fs::symlink_metadata(&previous) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > FILE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unsafe memory audit rotation target",
                    ));
                }
                fs::remove_file(&previous)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        drop(file);
        fs::rename(path, previous)?;
        file = private_file(path)?;
    }
    file.write_all(line)
}

#[cfg(not(unix))]
fn append_record(_: &Path, _: &[u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "memory audit requires unix private files",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "macevery-memory-audit-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn refresh_storm_queue_and_disk_are_bounded_and_latest_cache_survives() {
        let root = fixture();
        let path = root.join(FILE_NAME);
        let audit = MemoryAudit::at_path(path.clone(), true);
        let begin = Instant::now();
        for i in 0..100 {
            audit.publish_at(
                Snapshot {
                    records: i,
                    ..Snapshot::default()
                },
                begin + Duration::from_secs(i as u64),
            );
        }
        assert_eq!(audit.state.lock().unwrap().pending.len(), PENDING_RECORDS);
        assert_eq!(audit.state.lock().unwrap().latest.records, 99);
        audit.flush();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("\"records\":99"));
        let line = audit.line(
            Record {
                snapshot: Snapshot::default(),
                vm: VmSample::default(),
                at_ms: 0,
                snapshot_at_ms: 0,
                idle: false,
                traffic: TrafficSample::default(),
            },
            1,
            0,
            0,
        );
        assert!(line.len() <= RECORD_BYTES);
        for _ in 0..800 {
            append_record(&path, line.as_bytes()).unwrap();
        }
        for file in [&path, &path.with_extension("jsonl.1")] {
            let metadata = fs::metadata(file).unwrap();
            assert!(metadata.len() <= FILE_BYTES);
            #[cfg(unix)]
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn coalescing_and_io_failure_do_not_mutate_latest_index_evidence() {
        let root = fixture();
        let path = root.join("missing").join(FILE_NAME);
        let audit = MemoryAudit::at_path(path, true);
        let now = Instant::now();
        audit.publish_at(
            Snapshot {
                records: 1,
                ..Snapshot::default()
            },
            now,
        );
        let latest = Snapshot {
            records: 2,
            ..Snapshot::default()
        };
        audit.publish_at(latest, now);
        audit.publish_at(latest, now);
        assert_eq!(audit.state.lock().unwrap().pending.len(), 2);
        audit.flush();
        let state = audit.state.lock().unwrap();
        assert_eq!(state.latest, latest);
        assert_eq!(state.io_failures, 1);
        assert!(state.suppressed >= 2);
        drop(state);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fast_full_load_keeps_before_and_outcome_captures() {
        let root = fixture();
        for outcome in ["full_after", "full_failed"] {
            let audit = MemoryAudit::at_path(root.join(FILE_NAME), true);
            let now = Instant::now();
            audit.publish_at(
                Snapshot {
                    phase: "full_before",
                    ..Snapshot::default()
                },
                now,
            );
            audit.update_at(
                Snapshot {
                    phase: "full_loading",
                    ..Snapshot::default()
                },
                now,
                false,
            );
            {
                let state = audit.state.lock().unwrap();
                assert_eq!(state.latest.phase, "full_loading");
                assert_eq!(state.pending.len(), 1);
                assert_eq!(state.captures, 1);
            }
            audit.publish_at(
                Snapshot {
                    phase: outcome,
                    ..Snapshot::default()
                },
                now,
            );
            let state = audit.state.lock().unwrap();
            assert_eq!(state.latest.phase, outcome);
            assert_eq!(state.pending.len(), 2);
            assert_eq!(state.pending[0].snapshot.phase, "full_before");
            assert_eq!(state.pending[1].snapshot.phase, outcome);
            assert_eq!(state.captures, 2);
            assert_eq!(state.suppressed, 0);
            assert!(state.pending[0].snapshot_at_ms <= state.pending[1].snapshot_at_ms);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn links_and_unsafe_rotation_are_rejected_without_modifying_target() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let target = root.join("target");
        fs::write(&target, b"preserved").unwrap();
        let path = root.join(FILE_NAME);
        symlink(&target, &path).unwrap();
        assert!(append_record(&path, b"x\n").is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(append_record(&path, b"x\n").is_err());
        fs::remove_file(&path).unwrap();
        private_file(&path).unwrap().set_len(FILE_BYTES).unwrap();
        symlink(&target, path.with_extension("jsonl.1")).unwrap();
        assert!(append_record(&path, b"x\n").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"preserved");
        assert!(append_record(&path, &vec![0; RECORD_BYTES + 1]).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
