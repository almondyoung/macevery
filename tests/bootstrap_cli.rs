//! Startup JSON is a small, strict CLI interface; every database here is owned
//! by this test and isolated from the user's configured database.
use macevery::db::Database;
use macevery::model::{FileKind, FileRecord};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "macevery-bootstrap-cli-{}-{nanos}-{}",
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

fn run(database: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_macevery"))
        .env("MACEVERY_DB", database)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn bootstrap_json_creates_initializes_and_reports_the_exact_schema() {
    let scratch = Scratch::new();
    let database = scratch.database();
    let empty = run(&database, &["bootstrap", "--json"]);
    assert!(empty.status.success(), "{:?}", empty);
    assert!(empty.stderr.is_empty());
    assert_eq!(
        String::from_utf8(empty.stdout).unwrap(),
        format!(
            "{{\"db_path\":\"{}\",\"has_records\":false,\"roots\":[],\"excludes\":[],\"last_indexed_at\":null}}\n",
            database.display()
        )
    );
    let db = Database::open(database.clone()).unwrap();
    db.set_roots(&["/资料/\"quoted\"\\目录\n".into()]).unwrap();
    db.set_excludes(&["tab\tand\u{1}".into()]).unwrap();
    db.upsert_file(&FileRecord::new("/One.txt".into(), FileKind::File, 1))
        .unwrap();
    db.set_meta("last_indexed_at", &i64::MAX.to_string())
        .unwrap();
    drop(db);

    let populated = run(&database, &["bootstrap", "--json"]);
    assert!(populated.status.success(), "{:?}", populated);
    assert!(populated.stderr.is_empty());
    assert_eq!(
        String::from_utf8(populated.stdout).unwrap(),
        format!(
            "{{\"db_path\":\"{}\",\"has_records\":true,\"roots\":[\"/资料/\\\"quoted\\\"\\\\目录\\n\"],\"excludes\":[\"tab\\tand\\u0001\"],\"last_indexed_at\":{}}}\n",
            database.display(),
            i64::MAX
        )
    );

    // The existing status command still reports counts and its original schema.
    let status = run(&database, &["status", "--json"]);
    assert!(status.status.success(), "{:?}", status);
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("\"files\":1,\"dirs\":0,\"apps\":0,\"symlinks\":0,\"others\":0"));
    assert!(!status.contains("has_records"));
}

#[test]
fn bootstrap_invalid_arguments_fail_before_opening_a_database() {
    let scratch = Scratch::new();
    let database = scratch.database();
    for args in [
        vec!["bootstrap"],
        vec!["bootstrap", "--unknown"],
        vec!["bootstrap", "--json", "extra"],
        vec!["bootstrap", "--json=false"],
        vec!["bootstrap", "--json", "--json"],
    ] {
        let output = run(&database, &args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("bootstrap"));
        assert!(!database.exists(), "{args:?} created a database");
    }
    let help = run(&database, &["bootstrap", "--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8(help.stdout)
        .unwrap()
        .contains("macevery bootstrap --json"));
    assert!(!database.exists());
}

#[test]
fn bootstrap_corruption_returns_an_error_without_an_empty_json_response() {
    let scratch = Scratch::new();
    let database = scratch.database();
    let original = b"not a SQLite database";
    std::fs::write(&database, original).unwrap();
    let output = run(&database, &["bootstrap", "--json"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert_eq!(std::fs::read(database).unwrap(), original);
}
