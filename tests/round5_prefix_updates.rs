//! Public-API regressions for shared original-path storage across incremental updates.
//! These deliberate transaction waves complement the fixed-seed mixed-DML property
//! test: final records and ordered queries must agree with one pinned DB snapshot.
use macevery::compact_index::{ApplyOutcome, CompactIndex};
use macevery::db::{ContentStamp, Database, FileChangeRead};
use macevery::model::{FileKind, FileRecord, SearchOptions};
use macevery::search::search_records;
use macevery::sqlite::{Connection, Step};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "macevery-prefix-updates-{}-{nanos}-{}",
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

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Write {
    Insert,
    Replace,
    Update(i64),
    UpdateReplace(i64),
}

fn write(connection: &Connection, row: &FileRecord, lower: Option<&str>, mode: Write) {
    let sql = match mode {
        Write::Insert => "INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13);",
        Write::Replace => "INSERT OR REPLACE INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13);",
        Write::Update(_) => "UPDATE files SET id=?1,path=?2,path_lower=?3,basename=?4,basename_lower=?5,ext_lower=?6,kind=?7,size=?8,mtime=?9,ctime=?10,dev=?11,inode=?12,indexed_at=?13 WHERE id=?14;",
        Write::UpdateReplace(_) => "UPDATE OR REPLACE files SET id=?1,path=?2,path_lower=?3,basename=?4,basename_lower=?5,ext_lower=?6,kind=?7,size=?8,mtime=?9,ctime=?10,dev=?11,inode=?12,indexed_at=?13 WHERE id=?14;",
    };
    let mut statement = connection.prepare(sql).unwrap();
    statement.bind_i64(1, Some(row.id)).unwrap();
    for (column, value) in [
        (2, Some(row.path.as_str())),
        (3, lower),
        (4, Some(row.basename.as_str())),
        (5, Some(row.basename_lower.as_str())),
        (6, row.ext_lower.as_deref()),
        (7, Some(row.kind.as_str())),
    ] {
        statement.bind_text(column, value).unwrap();
    }
    for (column, value) in [
        (8, row.size),
        (9, row.mtime),
        (10, row.ctime),
        (11, row.dev),
        (12, row.inode),
        (13, Some(row.indexed_at)),
    ] {
        statement.bind_i64(column, value).unwrap();
    }
    if let Write::Update(old) | Write::UpdateReplace(old) = mode {
        statement.bind_i64(14, Some(old)).unwrap();
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

fn row(id: i64, path: String) -> FileRecord {
    let kind = match id.rem_euclid(5) {
        0 => FileKind::Directory,
        1 => FileKind::File,
        2 => FileKind::Symlink,
        3 => FileKind::App,
        _ => FileKind::Other,
    };
    let mut row = FileRecord::new(path, kind, i64::MAX.wrapping_sub(id));
    row.id = id;
    row.mtime = Some(42);
    row.size = if id % 3 == 0 { None } else { Some(i64::MIN) };
    row.ctime = Some(if id % 2 == 0 { i64::MAX } else { i64::MIN });
    row.dev = if id % 4 == 0 { None } else { Some(-1) };
    row.inode = Some(if id % 2 == 0 { 0 } else { i64::MAX });
    row.ext_lower = match id % 3 {
        0 => None,
        1 => Some(String::new()),
        _ => Some("txt".into()),
    };
    // Equal lowered basename, score, mtime and byte length in the shared group
    // force original-path lexical ordering at the TopK boundary.
    row.basename_lower = "tie-target".into();
    row
}

fn complete_records(index: &CompactIndex) -> Vec<FileRecord> {
    let mut records = index
        .search(&SearchOptions {
            query: "*".into(),
            limit: usize::MAX,
            ..Default::default()
        })
        .into_iter()
        .map(|hit| hit.record)
        .collect::<Vec<_>>();
    records.sort_by_key(|row| row.id);
    records
}

fn verify_batch(
    database: &Database,
    index: &mut CompactIndex,
    loaded: &mut ContentStamp,
    label: &str,
) -> bool {
    let previous = complete_records(index);
    let previous_capacity = index.memory_usage_bytes();
    database.begin_read().unwrap();
    let (target, changes) = match database.file_changes_since(loaded, 4096, 4096).unwrap() {
        FileChangeRead::Available { target, changes } => (target, changes),
        other => panic!("{label}: unexpected journal fallback: {other:?}"),
    };
    let reloaded = match index.apply_changes(&changes).unwrap() {
        ApplyOutcome::Applied => false,
        ApplyOutcome::NeedsFullReload(_) => {
            assert_eq!(
                complete_records(index),
                previous,
                "{label}: half-applied records"
            );
            assert_eq!(
                index.memory_usage_bytes(),
                previous_capacity,
                "{label}: fallback changed capacity"
            );
            *index = CompactIndex::load(database).unwrap();
            true
        }
    };
    let records = database.all_records().unwrap();
    let fresh = CompactIndex::load(database).unwrap();
    assert_eq!(complete_records(index), records, "{label}: complete fields");
    assert_eq!(index.len(), records.len(), "{label}: live count");
    let cases = [
        "",
        "*",
        "t",
        "ti",
        "tie",
        "tie-",
        "tie-t",
        "tie-ta",
        "tie-tar",
        "*Report*",
        "~tgt",
        "Shared",
        "path:Shared",
        "!path:Retired",
        "ext:txt !kind:dir",
        "kind:file",
        "报告",
        "İ",
        "ος",
        "é",
        "e\u{301}",
        "\0",
        "*\0*",
        "__missing_prefix_probe__",
    ];
    for query in cases {
        for limit in [1, 3, 7, records.len() + 1] {
            for path_only in [false, true] {
                let options = SearchOptions {
                    query: query.into(),
                    path_only,
                    limit,
                    ..Default::default()
                };
                let expected = search_records(&records, &options);
                assert_eq!(index.search(&options), expected, "{label}: {options:?}");
                assert_eq!(
                    fresh.search(&options),
                    expected,
                    "{label}: fresh {options:?}"
                );
            }
        }
    }
    database.commit().unwrap();
    *loaded = target;
    reloaded
}

#[test]
fn shared_prefix_deletion_revival_rename_and_unicode_keep_full_results() {
    let fixture = Fixture::new();
    let database = Database::open(fixture.database()).unwrap();
    let writer = Connection::open_existing(&fixture.database()).unwrap();
    writer
        .execute_batch("PRAGMA recursive_triggers=OFF; BEGIN IMMEDIATE;")
        .unwrap();
    let parent = "/VeryLongSharedParent/ProjectAlpha/Archive/SharedFolder";
    let shared = (1..=40)
        .map(|id| {
            let mut row = row(id, format!("{parent}/Report-{id:03}.txt"));
            if id == 17 {
                // This cannot be represented by a slice into the row's short suffix.
                row.basename = row.path.clone();
            } else if id == 18 {
                row.basename = "independent-original-basename-报告-Σ".into();
            }
            row
        })
        .collect::<Vec<_>>();
    for row in &shared {
        write(&writer, row, Some(&row.path_lower), Write::Insert);
    }
    let unusual = [
        "/Shared/ΟΣ/Report-Σ.txt",
        "/Shared/İ/Report-İ.txt",
        "/Shared/E\u{301}/Report-é.txt",
        "/Shared/É/Report-e\u{301}.txt",
        "/ABC/zxxx",
        "/ABC/x/aa",
        "/ABC/yyzz",
        "/ABC/x/bb",
        "/AB/zxxxx",
        "/AB/x/aaa",
        "/AB/yxxxx",
        "/AB/x/bbb",
        "/all/lower/shared/report.txt",
        "/all/lower/shared/another.txt",
        "/",
        "",
        "relative/Shared/",
        "/Shared//Double/Slash.txt",
    ];
    for (offset, path) in unusual.into_iter().enumerate() {
        let mut row = row(100 + offset as i64, path.into());
        row.basename = if offset % 3 == 0 {
            row.path.clone()
        } else {
            row.basename
        };
        let lower = if offset % 4 == 0 {
            None
        } else if offset % 4 == 1 {
            Some("")
        } else if offset % 4 == 2 {
            Some("/custom/Shared/ος/报告/tie-target")
        } else {
            Some(row.path_lower.as_str())
        };
        write(&writer, &row, lower, Write::Insert);
    }
    // SQLite can contain embedded NUL although bind_text deliberately rejects
    // NUL input. Construct it in SQL and compare the public read semantics.
    writer.execute_batch("INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,mtime,indexed_at) VALUES
        (201,'/Shared/Nul'||char(0)||'Parent/Report-A.txt','/shared/nul'||char(0)||'parent/report-a.txt','independent'||char(0)||'basename','tie-target',NULL,'file',42,1),
        (202,'/Shared/Nul'||char(0)||'Parent/Report-B.txt','/custom/'||char(0)||'lower', '/Shared/Nul'||char(0)||'Parent/Report-B.txt','tie-target','','file',42,2),
        (203,'/Shared/NulSuffix/Report'||char(0)||'A.txt',NULL,'not-a-suffix','tie-target','txt','file',42,3);").unwrap();
    writer.execute_batch("COMMIT;").unwrap();
    database.begin_read().unwrap();
    let mut stamp = database.content_stamp().unwrap();
    let mut index = CompactIndex::load(&database).unwrap();
    database.commit().unwrap();
    assert!(!verify_batch(&database, &mut index, &mut stamp, "initial"));

    // Leave just one live owner of the shared original prefix, then restore
    // tombstones with exactly the same strings in a later transaction.
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    for row in shared.iter().take(39) {
        delete(&writer, row.id);
    }
    writer.execute_batch("COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "delete shared owners"
    ));
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    for row in shared.iter().take(20) {
        write(&writer, row, Some(&row.path_lower), Write::Insert);
    }
    writer.execute_batch("COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "same-string revival"
    ));

    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    for original in shared.iter().skip(20).take(19) {
        let mut changed = original.clone();
        changed.path = format!(
            "/DifferentSharedParent/资料/İ/Revived-{:03}.txt",
            changed.id
        );
        changed.basename = changed.path.clone();
        changed.basename_lower = "independent-ος-报告-tie-target".into();
        write(&writer, &changed, None, Write::Insert);
    }
    writer.execute_batch("COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "different-string revival"
    ));

    // Repeated moves switch shared/unique/raw-like layouts without changing IDs.
    for wave in 0..8 {
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        for id in 1..=12 {
            let mut changed = shared[(id - 1) as usize].clone();
            changed.path = match wave % 4 {
                0 => format!("/SharedWave-{wave}/ΟΣ/Report-{id:03}.txt"),
                1 => format!("/all/lower/wave-{wave}/report-{id:03}.txt"),
                2 => format!("/UniqueParent-{wave}-{id}/Shared/İ/Report.txt"),
                _ => changed.path.clone(),
            };
            changed.basename = if id % 2 == 0 {
                changed.path.clone()
            } else {
                format!("unrelated-basename-{wave}-{id}-报告")
            };
            changed.size = if wave % 2 == 0 { None } else { Some(i64::MAX) };
            changed.ctime = Some(i64::MIN + wave);
            changed.indexed_at = i64::MIN + wave;
            let lower = if id % 3 == 0 {
                Some("/custom/lower/tie-target/ος")
            } else {
                None
            };
            write(&writer, &changed, lower, Write::Update(id));
        }
        writer.execute_batch("COMMIT;").unwrap();
        assert!(!verify_batch(
            &database,
            &mut index,
            &mut stamp,
            &format!("rename wave {wave}")
        ));
    }

    // A new high ID appends; replacing it by the old ID must revive the old
    // slot and remove the hidden OR REPLACE victim in the final-state delta.
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let mut moved = shared[0].clone();
    moved.id = 3000;
    write(&writer, &moved, Some(&moved.path_lower), Write::Update(1));
    writer.execute_batch("COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "high primary-key move"
    ));
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    write(
        &writer,
        &shared[0],
        Some(&shared[0].path_lower),
        Write::Replace,
    );
    writer.execute_batch("COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "replace revives old ID"
    ));

    // This intentionally requires the public clean-fallback contract: a never
    // seen low ID follows an ordinary update with a new shared parent.
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let updated = row(2, "/BrandNewPlannedPrefix/Shared/Report.txt".into());
    write(&writer, &updated, None, Write::Update(2));
    let low = row(50, "/LowIdSharedPrefix/Shared/Report.txt".into());
    write(&writer, &low, None, Write::Insert);
    writer.execute_batch("COMMIT;").unwrap();
    assert!(verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "low-ID clean fallback"
    ));

    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let victim_path = database
        .all_records()
        .unwrap()
        .into_iter()
        .find(|row| row.id == 7)
        .unwrap()
        .path;
    let replacement = row(5, victim_path);
    write(
        &writer,
        &replacement,
        Some("/custom/Shared/Unicode-ος"),
        Write::UpdateReplace(50),
    );
    writer.execute_batch("COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "ID and shared-path victims"
    ));

    writer.execute_batch("BEGIN IMMEDIATE; DELETE FROM files WHERE id=201;
        UPDATE files SET path='/Shared/OtherNul'||char(0)||'Parent/Report-B.txt',path_lower='',basename='/Shared/OtherNul'||char(0)||'Parent/Report-B.txt' WHERE id=202; COMMIT;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "NUL prefix delete and rename"
    ));
    writer.execute_batch("INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,mtime,indexed_at)
        VALUES(201,'/Shared/Nul'||char(0)||'Parent/Report-A.txt',NULL,'a-new-independent-basename','tie-target','file',42,4);").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "NUL shared-prefix revival"
    ));

    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let rolled_back = row(5000, "/RolledBackPrefix/Shared/Report.txt".into());
    write(&writer, &rolled_back, None, Write::Insert);
    delete(&writer, 40);
    writer.execute_batch("ROLLBACK;").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "rollback leaves prefix state"
    ));
    database.set_meta("watch_last_event_id", "5000").unwrap();
    assert!(!verify_batch(
        &database,
        &mut index,
        &mut stamp,
        "checkpoint has no file delta"
    ));
}
