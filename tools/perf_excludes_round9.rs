// Compile with --cfg baseline_excludes or --cfg candidate_excludes. The
// generated adapter selects an unchanged production scanner.rs, not a copied
// matcher implementation. All paths/patterns and the matcher are built before
// counting/timing; the global allocator counts allocations in is_excluded only.
pub use macevery::{db, error, model};
use std::alloc::{GlobalAlloc, Layout, System};
use std::os::raw::{c_int, c_long};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
include!(env!("MACEVERY_SCANNER_ADAPTER"));

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Counter;
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, size)
    }
}
#[global_allocator]
static ALLOCATOR: Counter = Counter;
#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: c_long,
    usec: c_int,
    padding: c_int,
}
#[repr(C)]
#[derive(Default)]
struct Usage {
    user: Timeval,
    system: Timeval,
    maxrss: c_long,
    rest: [c_long; 13],
}
extern "C" {
    fn getrusage(who: c_int, usage: *mut Usage) -> c_int;
}
fn cpu() -> f64 {
    let mut u = Usage::default();
    assert_eq!(unsafe { getrusage(0, &mut u) }, 0);
    (u.user.sec + u.system.sec) as f64 + (u.user.usec + u.system.usec) as f64 / 1e6
}
fn main() {
    let patterns = [
        ".DS_Store",
        ".Trash",
        ".build",
        ".cache",
        ".cargo/registry",
        ".git",
        ".npm",
        "DerivedData",
        "Library",
        "Library/Application Support",
        "Library/Caches",
        "Library/Containers",
        "Library/Developer",
        "Library/Group Containers",
        "Library/Mail",
        "Library/Messages",
        "build",
        "dist",
        "node_modules",
        "target",
        ".local/share/macevery",
        ".codex",
        ".gemini",
        ".cursor",
        "/Users/fixture/.local/share/macevery",
    ]
    .map(str::to_string);
    let paths = [
        "/Users/fixture/pine/code/macevery/src/watch.rs",
        "/Users/fixture/pine/code/macevery/.git/objects/de/adbeef",
        "/Users/fixture/Library/Caches/com.fixture/cache",
        "/Users/fixture/pine/code/project/target/debug/incremental/object",
        "/Users/fixture/.codex/sessions/2026/10/22/log.jsonl",
        "/Users/fixture/.local/share/macevery/index.sqlite-wal",
        "/Users/fixture/Documents/code-notes.md",
        "/Users/fixture/project/Library-old/Caches/file",
        "/Applications/Code.app/Contents/MacOS/code",
        "/Users/fixture/.cargo/registry/src/package/lib.rs",
        "/Users/fixture/project/node_modules/pkg/index.js",
        "/Users/fixture/照片/示例.jpg",
    ]
    .map(PathBuf::from);
    let matcher = scanner::ExcludeMatcher::new(&patterns);
    let expected = [
        false, true, true, true, true, true, false, false, false, true, true, false,
    ];
    for (path, expected) in paths.iter().zip(expected) {
        assert_eq!(matcher.is_excluded(path), expected);
    }
    let loops = 100_000;
    let mut hits = 0u64;
    let before = cpu();
    let start = Instant::now();
    COUNTING.store(true, Ordering::Relaxed);
    for _ in 0..loops {
        for path in &paths {
            hits += std::hint::black_box(matcher.is_excluded(std::hint::black_box(path))) as u64;
        }
    }
    COUNTING.store(false, Ordering::Relaxed);
    let wall = start.elapsed().as_secs_f64();
    let elapsed_cpu = cpu() - before;
    assert_eq!(hits, loops * 7);
    println!("{{\"calls\":{},\"hits\":{},\"cpu_seconds\":{},\"wall_seconds\":{},\"allocation_calls\":{},\"allocated_bytes\":{}}}", loops*paths.len() as u64,hits,elapsed_cpu,wall,ALLOCS.load(Ordering::Relaxed),BYTES.load(Ordering::Relaxed));
}
