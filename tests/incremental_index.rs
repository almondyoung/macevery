//! Fixed-seed public-API differential test for journal -> packed incremental state.
//! The scorer oracle is shared here deliberately: this proves incremental/fresh
//! equivalence. Independent legacy-scorer tests live in compact_index's tests.
use macevery::compact_index::{ApplyOutcome, CompactIndex};
use macevery::db::{ContentStamp, Database, FileChangeRead};
use macevery::model::{FileKind, FileRecord, PathFilter, PathFilterMode, SearchOptions};
use macevery::search::{search, search_records};
use macevery::sqlite::{Connection, Step};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SEED: u64 = 0xc37d_8a51_42f0_1609;
const INITIAL_ROWS: usize = 192;
const TRANSACTIONS: usize = 420;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "macevery-incremental-property-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }
    fn database(&self) -> PathBuf {
        self.0.join("index.sqlite")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn pick(&mut self, count: usize) -> usize {
        self.next() as usize % count
    }
    fn optional(&mut self) -> Option<i64> {
        [
            None,
            Some(i64::MIN),
            Some(i64::MAX),
            Some(-99),
            Some(0),
            Some(42),
        ][self.pick(6)]
    }
}

struct Row {
    id: i64,
    path: String,
    path_lower: Option<String>,
    basename: String,
    basename_lower: String,
    extension: Option<String>,
    kind: FileKind,
    size: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
    device: Option<i64>,
    inode: Option<i64>,
    generation: i64,
}
impl Row {
    fn generated(id: i64, serial: &mut u64, rng: &mut Rng) -> Self {
        *serial += 1;
        let number = *serial;
        let (directory, stem, suffix) = [
            ("Documents", "alpha-report", ".txt"),
            ("Reports", "report", ".PDF"),
            ("资料/Σ", "ΟΣ", ".doc"),
            ("emoji/🐾", "Ω-alpha", ".bin"),
            ("cache", "raw_%_\\", ""),
            ("Documents/子目录", "备忘录", ".pdf"),
        ][rng.pick(6)];
        let path = format!("/root/{directory}/{stem}-{number}{suffix}");
        let basename = if rng.pick(5) == 0 {
            format!("independent-Σ-{number}")
        } else {
            format!("{stem}-{number}{suffix}")
        };
        let path_lower = match rng.pick(4) {
            0 => None,
            1 => Some(String::new()),
            _ => Some(path.to_lowercase()),
        };
        let basename_lower = if rng.pick(7) == 0 {
            String::new()
        } else {
            basename.to_lowercase()
        };
        let extension = [
            None,
            Some(""),
            Some("txt"),
            Some("pdf"),
            Some("bin"),
            Some("doc"),
            Some("δ"),
        ][rng.pick(7)]
        .map(str::to_string);
        let kind = [
            FileKind::File,
            FileKind::Directory,
            FileKind::Symlink,
            FileKind::App,
            FileKind::Other,
        ][rng.pick(5)]
        .clone();
        // Empty-query's existing scorer overflows at i64::MIN mtime. Exercise all
        // nullable metadata and signed extrema elsewhere without changing that
        // separate pre-existing scoring boundary in this incremental regression.
        let mtime = [
            None,
            Some(-500),
            Some(0),
            Some(42),
            Some(8_999),
            Some(9_000),
            Some(i64::MAX),
        ][rng.pick(7)];
        Self {
            id,
            path,
            path_lower,
            basename,
            basename_lower,
            extension,
            kind,
            size: rng.optional(),
            mtime,
            ctime: rng.optional(),
            device: rng.optional(),
            inode: rng.optional(),
            generation: if rng.pick(4) == 0 {
                i64::MIN
            } else {
                number as i64
            },
        }
    }
}

fn write_row(connection: &Connection, row: &Row, mode: &str, previous_id: Option<i64>) {
    let sql = if previous_id.is_some() {
        "UPDATE files SET id=?1,path=?2,path_lower=?3,basename=?4,basename_lower=?5,ext_lower=?6,kind=?7,size=?8,mtime=?9,ctime=?10,dev=?11,inode=?12,indexed_at=?13 WHERE id=?14;".to_string()
    } else {
        format!("{mode} INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13);")
    };
    let mut statement = connection.prepare(&sql).unwrap();
    statement.bind_i64(1, Some(row.id)).unwrap();
    for (index, value) in [
        (2, Some(row.path.as_str())),
        (3, row.path_lower.as_deref()),
        (4, Some(row.basename.as_str())),
        (5, Some(row.basename_lower.as_str())),
        (6, row.extension.as_deref()),
        (7, Some(row.kind.as_str())),
    ] {
        statement.bind_text(index, value).unwrap();
    }
    for (index, value) in [
        (8, row.size),
        (9, row.mtime),
        (10, row.ctime),
        (11, row.device),
        (12, row.inode),
        (13, Some(row.generation)),
    ] {
        statement.bind_i64(index, value).unwrap();
    }
    if let Some(id) = previous_id {
        statement.bind_i64(14, Some(id)).unwrap();
    }
    assert!(matches!(statement.step().unwrap(), Step::Done));
}
fn delete(connection: &Connection, id: i64) {
    let mut statement = connection
        .prepare("DELETE FROM files WHERE id=?1;")
        .unwrap();
    statement.bind_i64(1, Some(id)).unwrap();
    assert!(matches!(statement.step().unwrap(), Step::Done));
}
fn scalar_update(connection: &Connection, id: i64, size: Option<i64>, mtime: Option<i64>) {
    let mut statement = connection
        .prepare("UPDATE files SET size=?1,mtime=?2,indexed_at=indexed_at+1 WHERE id=?3;")
        .unwrap();
    statement.bind_i64(1, size).unwrap();
    statement.bind_i64(2, mtime).unwrap();
    statement.bind_i64(3, Some(id)).unwrap();
    assert!(matches!(statement.step().unwrap(), Step::Done));
}
fn fresh_id(next: &mut i64) -> i64 {
    let id = *next;
    *next += 1;
    id
}

fn all_records(index: &CompactIndex, count: usize) -> Vec<FileRecord> {
    let mut records = index
        .search(&SearchOptions {
            limit: count + 1,
            ..SearchOptions::default()
        })
        .into_iter()
        .map(|result| result.record)
        .collect::<Vec<_>>();
    records.sort_by_key(|record| record.id);
    records
}
fn query_cases(turn: usize) -> Vec<SearchOptions> {
    let mut cases = vec![
        SearchOptions::default(),
        SearchOptions {
            query: "report".into(),
            ..SearchOptions::default()
        },
        SearchOptions {
            query: "*alpha*".into(),
            ..SearchOptions::default()
        },
        SearchOptions {
            query: "rpt".into(),
            fuzzy: true,
            ..SearchOptions::default()
        },
        SearchOptions {
            query: "root".into(),
            path_only: true,
            ..SearchOptions::default()
        },
        SearchOptions {
            query: "ext:pdf|txt !kind:dir".into(),
            ..SearchOptions::default()
        },
        SearchOptions {
            kind_filters: vec![FileKind::Directory, FileKind::App],
            ..SearchOptions::default()
        },
        SearchOptions {
            path_filters: vec![PathFilter {
                value: "Documents".into(),
                mode: PathFilterMode::Component,
            }],
            excluded_path_filters: vec![PathFilter {
                value: "cache".into(),
                mode: PathFilterMode::Contains,
            }],
            ..SearchOptions::default()
        },
        SearchOptions {
            modified_after: Some(-500),
            modified_before: Some(200),
            excluded_kind_filters: vec![FileKind::Symlink],
            ..SearchOptions::default()
        },
    ];
    for (ordinal, options) in cases.iter_mut().enumerate() {
        options.limit = [0, 1, 7, 31, 512][(turn + ordinal) % 5];
    }
    cases
}

#[test]
fn fixed_seed_incremental_mixed_transactions_match_same_snapshot_fresh_index() {
    let scratch = Scratch::new();
    let database = Database::open(scratch.database()).unwrap();
    let writer = Connection::open_existing(&scratch.database()).unwrap();
    writer
        .execute_batch("PRAGMA recursive_triggers=OFF; BEGIN IMMEDIATE;")
        .unwrap();
    let mut rng = Rng(SEED);
    let mut serial = 0;
    let mut next_id = 1;
    for _ in 0..INITIAL_ROWS {
        let row = Row::generated(fresh_id(&mut next_id), &mut serial, &mut rng);
        write_row(&writer, &row, "INSERT", None);
    }
    writer.execute_batch("COMMIT;").unwrap();
    database.begin_read().unwrap();
    let mut base: ContentStamp = database.content_stamp().unwrap();
    let mut index = CompactIndex::load(&database).unwrap();
    database.commit().unwrap();
    let mut deleted_ids = Vec::new();
    let mut operations = [0usize; 12];
    let (mut applied, mut full_fallbacks, mut changed_applies) = (0usize, 0usize, 0usize);

    for turn in 0..TRANSACTIONS {
        let rows = database.all_records().unwrap();
        assert!(
            rows.len() >= 3 && rows.len() <= 500,
            "fixture bounds at turn {turn}"
        );
        let op = turn % operations.len();
        operations[op] += 1;
        let target = &rows[rng.pick(rows.len())];
        let mut second = &rows[rng.pick(rows.len())];
        while second.id == target.id {
            second = &rows[rng.pick(rows.len())];
        }
        let mut third = &rows[rng.pick(rows.len())];
        while third.id == target.id || third.id == second.id {
            third = &rows[rng.pick(rows.len())];
        }
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        match op {
            0 => {
                let row = Row::generated(fresh_id(&mut next_id), &mut serial, &mut rng);
                write_row(&writer, &row, "INSERT", None);
            }
            1 => {
                let row = Row::generated(target.id, &mut serial, &mut rng);
                write_row(&writer, &row, "UPDATE", Some(target.id));
            }
            2 => {
                delete(&writer, target.id);
                deleted_ids.push(target.id);
            }
            3 => {
                let row = Row::generated(target.id, &mut serial, &mut rng);
                let mut stmt=writer.prepare("UPDATE files SET path=?1,path_lower=?2,basename=?3,basename_lower=?4 WHERE id=?5;").unwrap();
                stmt.bind_text(1, Some(&row.path)).unwrap();
                stmt.bind_text(2, row.path_lower.as_deref()).unwrap();
                stmt.bind_text(3, Some(&row.basename)).unwrap();
                stmt.bind_text(4, Some(&row.basename_lower)).unwrap();
                stmt.bind_i64(5, Some(target.id)).unwrap();
                assert!(matches!(stmt.step().unwrap(), Step::Done));
            }
            4 => {
                let id = if turn % 24 == 4 {
                    fresh_id(&mut next_id)
                } else {
                    second.id
                };
                let mut row = Row::generated(id, &mut serial, &mut rng);
                row.path = target.path.clone();
                row.path_lower = Some(row.path.to_lowercase());
                write_row(&writer, &row, "INSERT OR REPLACE", None);
                if id != target.id {
                    deleted_ids.push(target.id);
                }
            }
            5 => {
                let mut row = Row::generated(second.id, &mut serial, &mut rng);
                row.path = third.path.clone();
                row.path_lower = Some(row.path.to_lowercase());
                // UPDATE OR REPLACE can remove both the new-ID and new-path victims.
                let mut stmt=writer.prepare("UPDATE OR REPLACE files SET id=?1,path=?2,path_lower=?3,size=?4,mtime=?5 WHERE id=?6;").unwrap();
                stmt.bind_i64(1, Some(row.id)).unwrap();
                stmt.bind_text(2, Some(&row.path)).unwrap();
                stmt.bind_text(3, row.path_lower.as_deref()).unwrap();
                stmt.bind_i64(4, row.size).unwrap();
                stmt.bind_i64(5, row.mtime).unwrap();
                stmt.bind_i64(6, Some(target.id)).unwrap();
                assert!(matches!(stmt.step().unwrap(), Step::Done));
                deleted_ids.extend([target.id, third.id]);
            }
            6 => {
                let id = if turn % 36 == 18 {
                    -(serial as i64) - 1
                } else {
                    fresh_id(&mut next_id)
                };
                let row = Row::generated(id, &mut serial, &mut rng);
                write_row(&writer, &row, "UPDATE", Some(target.id));
                deleted_ids.push(target.id);
            }
            7 => {
                let reused = deleted_ids
                    .iter()
                    .rev()
                    .copied()
                    .find(|id| !rows.iter().any(|row| row.id == *id));
                let id = reused.unwrap_or_else(|| fresh_id(&mut next_id));
                let row = Row::generated(id, &mut serial, &mut rng);
                write_row(&writer, &row, "INSERT", None);
            }
            8 => {
                let id = fresh_id(&mut next_id);
                let mut row = Row::generated(id, &mut serial, &mut rng);
                row.path = target.path.clone();
                row.path_lower = Some(row.path.to_lowercase());
                write_row(&writer, &row, "INSERT OR REPLACE", None);
                delete(&writer, id);
                deleted_ids.extend([target.id, id]);
            }
            9 => {
                scalar_update(&writer, target.id, rng.optional(), Some(42));
                let row = Row::generated(fresh_id(&mut next_id), &mut serial, &mut rng);
                write_row(&writer, &row, "INSERT", None);
                delete(&writer, second.id);
            }
            10 => {
                let mut row = Row::generated(fresh_id(&mut next_id), &mut serial, &mut rng);
                row.path = target.path.clone();
                write_row(&writer, &row, "INSERT OR IGNORE", None);
                writer
                    .execute_batch("UPDATE files SET size=size,mtime=mtime;")
                    .unwrap();
            }
            11 => {
                scalar_update(&writer, target.id, rng.optional(), Some(8_999));
                let row = Row::generated(fresh_id(&mut next_id), &mut serial, &mut rng);
                write_row(&writer, &row, "INSERT", None);
                delete(&writer, second.id);
                deleted_ids.push(second.id);
            }
            _ => unreachable!(),
        }
        writer
            .execute_batch(if op == 9 { "ROLLBACK;" } else { "COMMIT;" })
            .unwrap();
        database.begin_read().unwrap();
        let delta = database.file_changes_since(&base, 4096, 4096).unwrap();
        let (target_stamp, changes) = match delta {
            FileChangeRead::Available { target, changes } => (target, changes),
            FileChangeRead::Fallback { reason } => {
                panic!("unexpected journal fallback seed={SEED:x} turn={turn} op={op}: {reason}")
            }
        };
        let previous = all_records(&index, rows.len());
        let outcome = index.apply_changes(&changes).unwrap_or_else(|error| {
            panic!("apply error seed={SEED:x} turn={turn} op={op}: {error}")
        });
        let expected = database.all_records().unwrap();
        let fresh = CompactIndex::load(&database).unwrap();
        match outcome {
            ApplyOutcome::Applied => {
                applied += 1;
                if !changes.is_empty() {
                    changed_applies += 1;
                }
            }
            ApplyOutcome::NeedsFullReload(_) => {
                full_fallbacks += 1;
                assert_eq!(
                    all_records(&index, rows.len()),
                    previous,
                    "fallback mutated old index at turn {turn}"
                );
                index = CompactIndex::load(&database).unwrap();
            }
        }
        database.commit().unwrap();
        assert_eq!(
            index.len(),
            expected.len(),
            "count seed={SEED:x} turn={turn} op={op}"
        );
        assert_eq!(
            all_records(&index, expected.len()),
            expected,
            "full records seed={SEED:x} turn={turn} op={op}"
        );
        for (category, options) in query_cases(turn).iter().enumerate() {
            let reference = search_records(&expected, options);
            assert_eq!(
                search(&database, options).unwrap(),
                reference,
                "SQLite/full-record oracle seed={SEED:x} turn={turn} op={op} category={category}"
            );
            assert_eq!(
                fresh.search(options),
                reference,
                "fresh oracle seed={SEED:x} turn={turn} category={category}"
            );
            assert_eq!(
                index.search(options),
                reference,
                "incremental seed={SEED:x} turn={turn} op={op} category={category}"
            );
        }
        if op == 9 || op == 10 {
            assert_eq!(
                target_stamp, base,
                "rollback/no-op changed stamp at turn {turn}"
            );
        }
        base = target_stamp;
    }
    assert!(operations.iter().all(|count| *count > 0));
    assert!(
        applied > 200 && changed_applies > 150,
        "insufficient incremental coverage: {applied}/{changed_applies}"
    );
    assert!(
        full_fallbacks > 5,
        "nonmonotonic ID cases did not exercise fallback"
    );
    println!("seed={SEED:x}, {TRANSACTIONS} transactions, {applied} applied, {changed_applies} changed, {full_fallbacks} clean fallbacks, operation counts={operations:?}");
}
