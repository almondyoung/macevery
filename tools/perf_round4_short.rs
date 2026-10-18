//! Round-4 short-query companion to perf_round1.rs (public API only).
//! Frozen measurement/JSON mechanics derived from round-3 perf_round1.rs;
//! run with compare_round4_short.py, isolated snapshot clones only.
//! Query CPU excludes result JSON and diagnostics. RSS includes allocator/page
//! residency effects; footprint is a separate macOS accounting value.
use macevery::{
    compact_index::CompactIndex,
    db::Database,
    model::{FileKind, FileRecord, SearchOptions, SearchResult},
    search,
};
use std::{
    collections::HashMap,
    env,
    io::{self, Write},
    os::raw::{c_int, c_long},
    path::PathBuf,
    time::Instant,
};

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: c_long,
    #[cfg(target_os = "macos")]
    usec: c_int,
    #[cfg(target_os = "macos")]
    padding: c_int,
    #[cfg(not(target_os = "macos"))]
    usec: c_long,
}
#[repr(C)]
#[derive(Default)]
struct RUsage {
    user: Timeval,
    system: Timeval,
    maxrss: c_long,
    rest: [c_long; 13],
}
extern "C" {
    fn getrusage(who: c_int, usage: *mut RUsage) -> c_int;
}
#[cfg(target_os = "macos")]
#[link(name = "proc")]
extern "C" {
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buf: *mut std::ffi::c_void,
        size: c_int,
    ) -> c_int;
}
fn usage() -> RUsage {
    let mut out = RUsage::default();
    assert_eq!(unsafe { getrusage(0, &mut out) }, 0, "getrusage failed");
    out
}
fn cpu_ms(u: &RUsage) -> f64 {
    (u.user.sec + u.system.sec) as f64 * 1000.0
        + (u.user.usec as f64 + u.system.usec as f64) / 1000.0
}
fn peak_rss(u: &RUsage) -> u64 {
    #[cfg(target_os = "macos")]
    {
        u.maxrss as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        u.maxrss as u64 * 1024
    }
}
fn rss() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // macOS SDK sys/proc_info.h: PROC_PIDTASKINFO=4, 6 u64 + 12 i32;
        // pti_resident_size is the second u64, in bytes.
        let mut buf = [0u64; 12];
        let n = unsafe {
            proc_pidinfo(
                std::process::id() as c_int,
                4,
                0,
                buf.as_mut_ptr().cast(),
                96,
            )
        };
        if n == 96 {
            Some(buf[1])
        } else {
            None
        }
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn number(n: Option<i64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}
fn optional_rss(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}
fn emit(s: String) {
    println!("{s}");
    io::stdout().flush().unwrap();
}
fn results_json(results: &[SearchResult]) -> String {
    let mut rows = Vec::with_capacity(results.len());
    for r in results {
        let f = &r.record;
        rows.push(format!(concat!("{{\"score\":{},\"id\":{},\"path\":{},\"path_lower\":{},\"basename\":{},\"basename_lower\":{},",
            "\"ext_lower\":{},\"kind\":{},\"size\":{},\"mtime\":{},\"ctime\":{},\"dev\":{},\"inode\":{},\"indexed_at\":{}}}"),
            r.score, f.id, json_string(&f.path), json_string(&f.path_lower), json_string(&f.basename), json_string(&f.basename_lower),
            f.ext_lower.as_deref().map(json_string).unwrap_or_else(|| "null".into()), json_string(f.kind.as_str()),
            number(f.size), number(f.mtime), number(f.ctime), number(f.dev), number(f.inode), f.indexed_at));
    }
    format!("[{}]", rows.join(","))
}
fn fnv1a(s: &[u8]) -> u64 {
    s.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}
fn synthetic_record(i: usize, now: i64) -> FileRecord {
    let name = match i % 17 {
        0 => format!("report-{i:08}.pdf"),
        1 => format!("报告-{i:08}.PDF"),
        2 => format!("İstanbul-{i:08}.txt"),
        _ => format!("source-{i:08}.rs"),
    };
    let mut f = FileRecord::new(
        format!("/Users/perf/project-{}/{name}", i % 1024),
        FileKind::File,
        now,
    );
    f.id = i as i64 + 1;
    f.size = Some((i % 100_000) as i64);
    f.mtime = Some(now - (i % 32) as i64 * 86_400);
    f.ctime = Some(now - (i % 64) as i64 * 86_400);
    f.dev = Some(1);
    f.inode = Some(i as i64 + 100);
    f
}
enum Index {
    Memory(Vec<FileRecord>),
    Compact(CompactIndex),
}
impl Index {
    fn len(&self) -> usize {
        match self {
            Self::Memory(r) => r.len(),
            Self::Compact(i) => i.len(),
        }
    }
    fn search(&self, o: &SearchOptions) -> Vec<SearchResult> {
        match self {
            Self::Memory(r) => search::search_records(r, o),
            Self::Compact(i) => i.search(o),
        }
    }
}
// Same libproc footprint layout used by check_service_round2.py. This is
// optional diagnostics, not a replacement for RSS or packed capacity.
#[cfg(target_os = "macos")]
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
    physical_footprint: u64,
    proc_start_abstime: u64,
    proc_exit_abstime: u64,
}
#[cfg(target_os = "macos")]
#[link(name = "proc")]
extern "C" {
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut std::ffi::c_void) -> c_int;
}
fn footprint() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info = RusageInfoV0::default();
        (unsafe {
            proc_pid_rusage(
                std::process::id() as c_int,
                0,
                (&mut info as *mut RusageInfoV0).cast(),
            )
        } == 0)
            .then_some(info.physical_footprint)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
fn memory_event(phase: &str, index: &Index) {
    let capacity = match index {
        Index::Compact(index) => Some(index.memory_usage_bytes()),
        // The Vec path is retained only as a legacy driver compatibility option.
        // This round compares compact against compact, never against this Vec.
        Index::Memory(_) => None,
    };
    emit(format!("{{\"event\":\"memory\",\"phase\":{},\"index_capacity_bytes\":{},\"rss_bytes\":{},\"physical_footprint_bytes\":{}}}",
        json_string(phase), optional_rss(capacity), optional_rss(rss()), optional_rss(footprint())));
}
fn main() {
    let mut args = HashMap::new();
    let mut values = env::args().skip(1);
    while let Some(key) = values.next() {
        args.insert(key, values.next().expect("each --flag needs a value"));
    }
    let get = |key: &str, default: &str| args.get(key).cloned().unwrap_or_else(|| default.into());
    let dataset = get("--dataset", "synthetic");
    let backend = get("--backend", "memory");
    assert!(matches!(dataset.as_str(), "synthetic" | "snapshot"));
    assert!(matches!(backend.as_str(), "memory" | "compact"));
    let rows = get("--rows", "100000").parse::<usize>().unwrap();
    let rounds = get("--query-rounds", "1").parse::<usize>().unwrap();
    let now = get("--fixed-now", "1792195200").parse::<i64>().unwrap();
    let limit = get("--limit", "80").parse::<usize>().unwrap();
    let db_path = args.get("--db").map(PathBuf::from);
    emit(format!("{{\"event\":\"start\",\"pid\":{},\"dataset\":{},\"backend\":{},\"requested_rows\":{},\"fixed_now\":{},\"limit\":{}}}", std::process::id(), json_string(&dataset), json_string(&backend), rows, now, limit));
    if dataset == "synthetic" && backend == "compact" {
        let path = db_path
            .as_ref()
            .expect("synthetic compact requires isolated --db");
        assert!(!path.exists(), "synthetic database must not already exist");
        let db = Database::open(path.clone()).unwrap();
        db.begin().unwrap();
        for i in 0..rows {
            db.upsert_file(&synthetic_record(i, now)).unwrap();
        }
        db.commit().unwrap();
    }
    let rss_before_load = rss();
    emit(format!(
        "{{\"event\":\"load_start\",\"rss_before_bytes\":{}}}",
        optional_rss(rss_before_load)
    ));
    let load_cpu = cpu_ms(&usage());
    let started = Instant::now();
    let index = if dataset == "synthetic" && backend == "memory" {
        Index::Memory((0..rows).map(|i| synthetic_record(i, now)).collect())
    } else {
        let db = Database::open(db_path.expect("snapshot/compact requires isolated --db")).unwrap();
        match backend.as_str() {
            "memory" => Index::Memory(db.all_records().unwrap()),
            _ => Index::Compact(CompactIndex::load(&db).unwrap()),
        }
    };
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let load_usage = usage();
    emit(format!("{{\"event\":\"load_end\",\"records\":{},\"wall_ms\":{:.6},\"cpu_ms\":{:.6},\"rss_after_bytes\":{},\"process_peak_rss_bytes\":{}}}",
        index.len(), load_ms, cpu_ms(&load_usage) - load_cpu, optional_rss(rss()), peak_rss(&load_usage)));
    // IDs describe input UTF-8 byte lengths, not presumed hit counts. Rare
    // ASCII sequences can still occur in a real index; actual count is recorded.
    let suite = [
        ("common_1_a", "a", false),
        ("common_1_e", "e", false),
        ("rare_1_z", "z", false),
        ("common_2_in", "in", false),
        ("common_2_rs", "rs", false),
        ("rare_2_qz", "qz", false),
        ("common_3_src", "src", false),
        ("common_3_app", "app", false),
        ("rare_3_qzv", "qzv", false),
        ("common_4_code", "code", false),
        ("common_4_json", "json", false),
        ("rare_4_qzvx", "qzvx", false),
        ("common_5_cache", "cache", false),
        ("common_5_build", "build", false),
        ("rare_5_qzvxj", "qzvxj", false),
        ("common_6_report", "report", false),
        ("common_6_source", "source", false),
        ("rare_6_qzvxjk", "qzvxjk", false),
        ("common_7_project", "project", false),
        ("common_7_library", "library", false),
        ("rare_7_qzvxjkw", "qzvxjkw", false),
        ("unicode_3_han", "报", false),
        ("unicode_6_han", "报告", false),
        ("unicode_2_sigma", "Σ", false),
        ("unicode_2_dotted_i", "İ", false),
        ("unicode_4_emoji", "📷", false),
        ("unicode_5_accent", "café", false),
        // Optional impossible-in-native-path controls, never in the default
        // performance mix. Counts still establish absence rather than assume it.
        ("nohit_1_nul", "\0", false),
        ("nohit_2_nul", "\0\0", false),
        ("nohit_3_nul", "\0\0\0", false),
        ("nohit_4_nul", "\0\0\0\0", false),
        ("nohit_5_nul", "\0\0\0\0\0", false),
        ("nohit_6_nul", "\0\0\0\0\0\0", false),
        ("nohit_7_nul", "\0\0\0\0\0\0\0", false),
        // Original round-3 names remain available for bridge/regression runs.
        ("broad", "*", false),
        ("plain", "code", false),
        ("nohit", "__macevery_round1_no_hit_7fd6b329__", false),
        ("glob", "*report*", false),
        ("fuzzy", "~rpt", false),
        ("filters", "ext:rs|pdf !path:Library", false),
        ("mtime", "mtime:7d", false),
        ("unicode", "报告", false),
        ("path", "project", true),
    ];
    let selected = get(
        "--queries",
        "broad,plain,nohit,glob,fuzzy,filters,mtime,unicode,path",
    );
    // An empty selection performs only load accounting; preserve caller order
    // so separate fresh-process runs can isolate the first text-touch effect.
    let names = selected
        .split(',')
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    assert!(
        names.iter().all(|name| suite.iter().any(|q| q.0 == *name)),
        "unknown query name"
    );
    memory_event("after_load", &index);
    if names.is_empty() {
        // Existing wrapper requires a results event even in a load-only run.
        emit(
            "{\"event\":\"results\",\"id\":\"__load_only__\",\"iteration\":0,\"results\":[]}"
                .into(),
        );
    }
    for iteration in 0..rounds {
        for (id, query, path_only) in names
            .iter()
            .map(|name| suite.iter().find(|q| q.0 == *name).unwrap())
        {
            // Resolve relative dates before calling either unchanged public API.
            let options = SearchOptions {
                query: (*query).into(),
                limit,
                path_only: *path_only,
                ..Default::default()
            }
            .normalized_at(now);
            emit(format!("{{\"event\":\"query_input\",\"id\":{},\"iteration\":{},\"query\":{},\"input_utf8_bytes\":{},\"input_characters\":{},\"lowercase_utf8_bytes\":{},\"path_only\":{},\"fuzzy\":{}}}",
                json_string(id), iteration, json_string(query), query.len(), query.chars().count(), query.to_lowercase().len(), path_only, options.fuzzy));
            let before = usage();
            let rss_before = rss();
            emit(format!(
                "{{\"event\":\"query_start\",\"id\":{},\"iteration\":{},\"rss_before_bytes\":{}}}",
                json_string(id),
                iteration,
                optional_rss(rss_before)
            ));
            let cpu_started = cpu_ms(&usage());
            let started = Instant::now();
            let results = index.search(&options);
            let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
            let after = usage();
            emit(format!("{{\"event\":\"query_end\",\"id\":{},\"iteration\":{},\"query\":{},\"count\":{},\"wall_ms\":{:.6},\"cpu_ms\":{:.6},\"rss_before_bytes\":{},\"rss_after_bytes\":{},\"process_peak_rss_before_bytes\":{},\"process_peak_rss_bytes\":{}}}",
                json_string(id), iteration, json_string(query), results.len(), wall_ms, cpu_ms(&after) - cpu_started,
                optional_rss(rss_before), optional_rss(rss()), peak_rss(&before), peak_rss(&after)));
            let json = results_json(&results);
            emit(format!("{{\"event\":\"results\",\"id\":{},\"iteration\":{},\"fnv1a64\":\"{:016x}\",\"results\":{}}}", json_string(id), iteration, fnv1a(json.as_bytes()), json));
            std::hint::black_box(&results);
            drop(json);
            drop(results);
            memory_event(&format!("after_query_drop:{id}:{iteration}"), &index);
        }
    }
    let final_usage = usage();
    emit(format!("{{\"event\":\"process_end\",\"cpu_ms\":{:.6},\"rss_bytes\":{},\"process_peak_rss_bytes\":{}}}", cpu_ms(&final_usage), optional_rss(rss()), peak_rss(&final_usage)));
}
