//! Standalone performance driver: rustc -O --edition=2021 --extern macevery=... .
//! Run through compare_round1.py: snapshot inputs must be isolated writable copies.
//! No allocation counter is installed; JSON encoding and digests are outside query timing.
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
    let plain = if dataset == "snapshot" {
        "code"
    } else {
        "source"
    };
    let suite = [
        ("broad", "*", false),
        ("plain", plain, false),
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
    let names = selected.split(',').collect::<Vec<_>>();
    assert!(
        names.iter().all(|name| suite.iter().any(|q| q.0 == *name)),
        "unknown query name"
    );
    for iteration in 0..rounds {
        for (id, query, path_only) in suite.iter().filter(|q| names.contains(&q.0)) {
            // Resolve relative dates before calling either unchanged public API.
            let options = SearchOptions {
                query: (*query).into(),
                limit,
                path_only: *path_only,
                ..Default::default()
            }
            .normalized_at(now);
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
        }
    }
    let final_usage = usage();
    emit(format!("{{\"event\":\"process_end\",\"cpu_ms\":{:.6},\"rss_bytes\":{},\"process_peak_rss_bytes\":{}}}", cpu_ms(&final_usage), optional_rss(rss()), peak_rss(&final_usage)));
}
