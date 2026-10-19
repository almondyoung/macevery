//! Compaction is explicit and writes a fresh copy, never the configured source.
use macevery::db::Database;
use macevery::model::{FileKind, FileRecord};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "macevery-optimize-cli-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn run(source: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_macevery"))
        .env("MACEVERY_DB", source)
        .current_dir(source.parent().unwrap())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn optimize_requires_explicit_fresh_output_and_never_creates_missing_source() {
    let scratch = Scratch::new();
    let source = scratch.0.join("missing.sqlite");
    let target = scratch.0.join("copy.sqlite");
    for args in [
        vec!["optimize"],
        vec!["optimize", "--output"],
        vec!["optimize", "--wrong"],
        vec!["optimize", "--output", target.to_str().unwrap(), "--json"],
    ] {
        assert!(!run(&source, &args).status.success());
        assert!(!source.exists());
        assert!(!target.exists());
    }
    assert!(run(&source, &["optimize", "--help"]).status.success());
    assert!(!source.exists());
}

#[test]
fn optimize_copy_preserves_records_settings_and_supports_independent_updates() {
    let scratch = Scratch::new();
    let source = scratch.0.join("source.sqlite");
    let target = scratch.0.join("copy.sqlite");
    let db = Database::open(source.clone()).unwrap();
    db.set_roots(&["/source".into()]).unwrap();
    db.set_excludes(&["*.ignored".into()]).unwrap();
    let mut file = FileRecord::new("/source/code.rs".into(), FileKind::File, 1);
    file.size = Some(11);
    db.upsert_file(&file).unwrap();
    let records = db.all_records().unwrap();
    let stamp = db.content_stamp().unwrap();
    drop(db);
    let original = std::fs::read(&source).unwrap();
    let before_time = std::fs::metadata(&source).unwrap().modified().unwrap();
    let result = run(
        &source,
        &["optimize", "--output", target.to_str().unwrap(), "--json"],
    );
    assert!(result.status.success(), "{result:?}");
    assert!(result.stderr.is_empty());
    let json = String::from_utf8(result.stdout).unwrap();
    assert!(json.starts_with("{\"optimized_copy\":true,"));
    assert!(json.contains("\"verified_records\":1,"));
    assert!(json.ends_with("\"source_unchanged\":true}\n"));
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(
        std::fs::metadata(&source).unwrap().modified().unwrap(),
        before_time
    );
    let copy = Database::open(target.clone()).unwrap();
    assert_eq!(copy.all_records().unwrap(), records);
    let copied_stamp = copy.content_stamp().unwrap();
    assert_eq!(copied_stamp.database_id, stamp.database_id);
    assert_eq!(copied_stamp.revision, stamp.revision);
    assert_ne!(copied_stamp.file_identity, stamp.file_identity);
    assert_eq!(
        copy.roots_and_excludes().unwrap(),
        (vec!["/source".into()], vec!["*.ignored".into()])
    );
    file.size = Some(35);
    copy.upsert_file(&file).unwrap();
    assert_eq!(copy.all_records().unwrap()[0].size, Some(35));
    assert_eq!(
        Database::open_read_only(source.clone())
            .unwrap()
            .all_records()
            .unwrap()[0]
            .size,
        Some(11)
    );
    drop(copy);
    let target_before = std::fs::read(&target).unwrap();
    assert!(
        !run(&source, &["optimize", "--output", target.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(std::fs::read(&target).unwrap(), target_before);
}

#[test]
fn optimize_does_not_consume_flags_as_destination_names() {
    let scratch = Scratch::new();
    let source = scratch.0.join("source.sqlite");
    drop(Database::open(source.clone()).unwrap());
    let before = std::fs::read(&source).unwrap();
    for value in ["--json", "--help", ""] {
        assert!(!run(&source, &["optimize", "--output", value])
            .status
            .success());
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }
    assert!(!scratch.0.join("--json").exists());
    assert!(!scratch.0.join("--help").exists());
}
