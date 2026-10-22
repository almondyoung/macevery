use crate::db::{Database, FileUpsert};
use crate::error::Result;
use crate::model::{FileKind, FileRecord};
use std::borrow::Cow;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct IndexRequest {
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub rebuild: bool,
}

#[derive(Clone, Debug, Default)]
pub struct IndexSummary {
    pub indexed: u64,
    /// Outer files INSERT/UPDATE rows, including generation-only changes.
    pub rows_written: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub warnings: Vec<String>,
    pub retry_paths: Vec<PathBuf>,
    pub deferred_cleanups: Vec<(PathBuf, i64)>,
}

pub fn index_paths(db: &Database, request: &IndexRequest) -> Result<IndexSummary> {
    if request.rebuild {
        db.clear_index()?;
    }

    let roots = request
        .roots
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    db.set_roots(&roots)?;
    db.set_excludes(&request.excludes)?;

    let mut summary = IndexSummary::default();
    let matcher = ExcludeMatcher::new(&request.excludes);
    db.begin()?;
    let result = (|| {
        let indexed_at = now_index_generation();
        let mut upsert = db.prepare_file_upsert()?;
        for root in &request.roots {
            scan_path(&mut upsert, root, &matcher, indexed_at, &mut summary)?;
        }
        drop(upsert);
        if !request.rebuild {
            cleanup_stale_if_complete(db, &request.roots, indexed_at, &mut summary)?;
        }
        db.set_meta("last_indexed_at", &now_epoch().to_string())?;
        Ok(())
    })();

    finish_write(db, result)?;

    Ok(summary)
}

pub fn refresh_path(db: &Database, path: &Path, excludes: &[String]) -> Result<IndexSummary> {
    let mut summary = IndexSummary::default();
    let matcher = ExcludeMatcher::new(excludes);
    if matcher.is_excluded(path) {
        return Ok(summary);
    }
    // Permission and transient I/O errors are not evidence of deletion.
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(err) => return Err(err.into()),
    };
    db.begin()?;
    let result = (|| {
        let path_text = path.to_string_lossy().to_string();
        let indexed_at = now_index_generation();
        let previous_kind = db.kind_for_path(&path_text)?;
        if let Some(metadata) = metadata {
            let kind = classify(path, &metadata);
            let should_cleanup_children = metadata.is_dir() && kind != FileKind::Symlink;
            if matches!(previous_kind, Some(FileKind::Directory | FileKind::App))
                && !should_cleanup_children
            {
                db.delete_path_prefix(&path_text)?;
            }
            let mut upsert = db.prepare_file_upsert()?;
            scan_path(&mut upsert, path, &matcher, indexed_at, &mut summary)?;
            drop(upsert);
            if should_cleanup_children {
                cleanup_stale_if_complete(db, &[path.to_path_buf()], indexed_at, &mut summary)?;
            }
        } else {
            // NotFound/NotADirectory proves this entire namespace is absent.
            // Coalesced subtree events may arrive after its parent row vanished.
            db.delete_path_prefix(&path_text)?;
        }
        Ok(())
    })();

    finish_write(db, result)?;

    Ok(summary)
}

pub fn refresh_roots(
    db: &Database,
    roots: &[PathBuf],
    excludes: &[String],
) -> Result<IndexSummary> {
    let mut summary = IndexSummary::default();
    let matcher = ExcludeMatcher::new(excludes);
    db.begin()?;
    let result = (|| {
        let indexed_at = now_index_generation();
        let mut upsert = db.prepare_file_upsert()?;
        for root in roots {
            scan_path(&mut upsert, root, &matcher, indexed_at, &mut summary)?;
        }
        drop(upsert);
        cleanup_stale_if_complete(db, roots, indexed_at, &mut summary)?;
        db.set_meta("last_indexed_at", &now_epoch().to_string())?;
        Ok(())
    })();

    finish_write(db, result)?;

    Ok(summary)
}

const MAX_SCAN_DEPTH: u32 = 256;

fn finish_write(db: &Database, result: Result<()>) -> Result<()> {
    let result = result.and_then(|()| db.commit());
    if result.is_err() {
        // COMMIT failures can leave an active SQLite transaction too.
        let _ = db.rollback();
    }
    result
}

fn scan_path(
    upsert: &mut FileUpsert<'_>,
    path: &Path,
    excludes: &ExcludeMatcher,
    indexed_at: i64,
    summary: &mut IndexSummary,
) -> Result<()> {
    scan_path_inner(upsert, path, excludes, indexed_at, summary, 0)
}

fn scan_path_inner(
    upsert: &mut FileUpsert<'_>,
    path: &Path,
    excludes: &ExcludeMatcher,
    indexed_at: i64,
    summary: &mut IndexSummary,
    depth: u32,
) -> Result<()> {
    if depth > MAX_SCAN_DEPTH {
        summary.retry_paths.push(path.to_path_buf());
        summary.warnings.push(format!(
            "{}: exceeded max depth {MAX_SCAN_DEPTH}",
            path.to_string_lossy()
        ));
        return Ok(());
    }

    if excludes.is_excluded(path) {
        return Ok(());
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => {
            summary.retry_paths.push(path.to_path_buf());
            summary
                .warnings
                .push(format!("{}: {}", path.to_string_lossy(), err));
            return Ok(());
        }
    };

    let kind = classify(path, &metadata);
    let mut record = FileRecord::new(path.to_string_lossy().to_string(), kind.clone(), indexed_at);
    record.size = if metadata.is_file() {
        Some(metadata.len() as i64)
    } else {
        None
    };
    record.mtime = Some(metadata.mtime());
    record.ctime = Some(metadata.ctime());
    record.dev = Some(metadata.dev() as i64);
    record.inode = Some(metadata.ino() as i64);
    if upsert.upsert(&record)? {
        summary.rows_written += 1;
    } else {
        summary.unchanged += 1;
    }
    summary.indexed += 1;

    if metadata.is_dir() && kind != FileKind::Symlink {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) => {
                summary.retry_paths.push(path.to_path_buf());
                summary
                    .warnings
                    .push(format!("{}: {}", path.to_string_lossy(), err));
                return Ok(());
            }
        };
        for entry in entries {
            match entry {
                Ok(entry) => scan_path_inner(
                    upsert,
                    &entry.path(),
                    excludes,
                    indexed_at,
                    summary,
                    depth + 1,
                )?,
                Err(err) => {
                    summary.retry_paths.push(path.to_path_buf());
                    summary
                        .warnings
                        .push(format!("{}: {}", path.to_string_lossy(), err));
                }
            }
        }
    }

    Ok(())
}

fn cleanup_stale_if_complete(
    db: &Database,
    roots: &[PathBuf],
    indexed_at: i64,
    summary: &mut IndexSummary,
) -> Result<()> {
    if summary.warnings.is_empty() {
        summary.deleted += db.delete_stale_under_roots(roots, indexed_at)?;
    } else {
        summary
            .deferred_cleanups
            .extend(roots.iter().cloned().map(|root| (root, indexed_at)));
        summary
            .warnings
            .push("stale cleanup deferred until incomplete subtrees recover".to_string());
    }
    Ok(())
}

fn classify(path: &Path, metadata: &fs::Metadata) -> FileKind {
    if metadata.file_type().is_symlink() {
        FileKind::Symlink
    } else if metadata.is_dir()
        && path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case("app"))
            .unwrap_or(false)
    {
        FileKind::App
    } else if metadata.is_dir() {
        FileKind::Directory
    } else if metadata.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExcludeMatcher {
    // Group suffixes by their first component. The callback and scanner can
    // reject a path without allocating a Vec for every filesystem event.
    patterns: std::collections::HashMap<String, Vec<Vec<String>>>,
}

impl ExcludeMatcher {
    pub(crate) fn new(excludes: &[String]) -> Self {
        let mut patterns = std::collections::HashMap::<String, Vec<Vec<String>>>::new();
        for pattern in excludes {
            let mut parts = pattern.trim().split('/').filter(|part| !part.is_empty());
            if let Some(first) = parts.next() {
                let suffix = parts.map(str::to_string).collect::<Vec<_>>();
                let choices = patterns.entry(first.to_string()).or_default();
                if !choices.contains(&suffix) {
                    choices.push(suffix);
                }
            }
        }
        Self { patterns }
    }

    pub(crate) fn is_excluded(&self, path: &Path) -> bool {
        let mut components = path.components().filter_map(component_text);
        while let Some(first) = components.next() {
            if let Some(suffixes) = self.patterns.get(first.as_ref()) {
                for suffix in suffixes {
                    let mut tail = components.clone();
                    if suffix
                        .iter()
                        .all(|expected| tail.next().is_some_and(|value| value.as_ref() == expected))
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
fn should_exclude(path: &Path, excludes: &[String]) -> bool {
    ExcludeMatcher::new(excludes).is_excluded(path)
}

fn component_text(component: Component<'_>) -> Option<Cow<'_, str>> {
    match component {
        Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy()),
        Component::Normal(value) => Some(value.to_string_lossy()),
        Component::CurDir => Some(Cow::Borrowed(".")),
        Component::ParentDir => Some(Cow::Borrowed("..")),
        Component::RootDir => None,
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

fn now_index_generation() -> i64 {
    static LAST_GENERATION: AtomicI64 = AtomicI64::new(0);
    let wall_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    next_index_generation(&LAST_GENERATION, wall_now)
}

fn next_index_generation(last: &AtomicI64, wall_now: i64) -> i64 {
    // Deferred cleanup must never see a later scan as older after a clock rollback.
    // This orders this process's scans; persisted history from other processes is
    // still wall-clock based, as before.
    let previous = last
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
            Some(wall_now.max(previous.saturating_add(1)))
        })
        .expect("generation update always returns Some");
    wall_now.max(previous.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::{index_paths, refresh_path, refresh_roots, should_exclude, IndexRequest};
    use crate::db::Database;
    use crate::model::FileKind;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn compiled_excludes_preserve_component_window_semantics() {
        use super::{component_text, ExcludeMatcher};
        use std::os::unix::ffi::OsStringExt;
        let excludes = [
            "",
            " / ",
            ".git",
            "Library/Caches",
            "Library/Mail",
            "a/a/b",
            "a/b",
            "./x",
            "../x",
            "重复/目录",
            " bad�/x ",
            "//a///b//",
        ]
        .map(str::to_string);
        let matcher = ExcludeMatcher::new(&excludes);
        // The pre-optimization component-window contract is the oracle. Include
        // overlapping starts, relative components, lookalikes and lossy paths.
        let mut paths = [
            "/",
            "",
            "./x",
            "../x",
            "/x/a/a/b/c",
            "a/a/a/b",
            "a/b-old",
            "a/b",
            "/Library/Caches/x",
            "Library/Mail",
            "Library/Caches-old/x",
            "/重复/目录/x",
            "/重复/目录外/x",
            "/foo/.git/x",
            "/foo/.github/x",
        ]
        .map(PathBuf::from)
        .to_vec();
        paths.push(PathBuf::from(std::ffi::OsString::from_vec(
            b"/foo/bad\xff/x".to_vec(),
        )));
        for path in paths {
            let components = path
                .components()
                .filter_map(component_text)
                .collect::<Vec<_>>();
            let expected = excludes.iter().any(|pattern| {
                let parts = pattern
                    .trim()
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>();
                !parts.is_empty()
                    && components.windows(parts.len()).any(|window| {
                        window
                            .iter()
                            .zip(&parts)
                            .all(|(value, part)| value.as_ref() == *part)
                    })
            });
            assert_eq!(matcher.is_excluded(&path), expected, "{}", path.display());
        }
    }

    #[test]
    fn clock_rollback_keeps_recovered_rows_newer_than_deferred_cleanup() {
        use super::next_index_generation;
        use crate::model::FileRecord;
        use std::sync::atomic::AtomicI64;
        let clock = AtomicI64::new(0);
        let deferred_generation = next_index_generation(&clock, 100);
        let recovered_generation = next_index_generation(&clock, 90);
        assert_eq!(deferred_generation, 100);
        assert_eq!(recovered_generation, 101);
        assert_eq!(next_index_generation(&clock, 90), 102);
        assert_eq!(next_index_generation(&clock, 200), 200);

        let temp = unique_temp_dir("macevery-clock-rollback");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        for (path, generation) in [
            ("/root/stale.txt", 1),
            ("/root/recovered.txt", recovered_generation),
        ] {
            db.upsert_file(&FileRecord::new(
                path.to_string(),
                FileKind::File,
                generation,
            ))
            .unwrap();
        }
        assert_eq!(
            db.delete_stale_under_roots(&[PathBuf::from("/root")], deferred_generation)
                .unwrap(),
            1
        );
        assert!(db.kind_for_path("/root/stale.txt").unwrap().is_none());
        assert!(db.kind_for_path("/root/recovered.txt").unwrap().is_some());
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn excludes_components_and_suffixes() {
        let excludes = vec!["node_modules".to_string(), "Library/Caches".to_string()];
        assert!(should_exclude(
            Path::new("/tmp/project/node_modules/pkg"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/Users/me/Library/Caches"),
            &excludes
        ));
        assert!(!should_exclude(Path::new("/tmp/project/src"), &excludes));
    }

    #[test]
    fn exclude_does_not_match_partial_names() {
        let excludes = vec!["build".to_string()];
        assert!(should_exclude(Path::new("/tmp/project/build"), &excludes));
        assert!(should_exclude(
            Path::new("/tmp/project/build/output/file.o"),
            &excludes
        ));
        assert!(!should_exclude(Path::new("/tmp/rebuild"), &excludes));
        assert!(!should_exclude(
            Path::new("/tmp/project/rebuilder"),
            &excludes
        ));
        assert!(!should_exclude(
            Path::new("/tmp/project/prebuild/src"),
            &excludes
        ));
    }

    #[test]
    fn exclude_multi_segment_pattern() {
        let excludes = vec!["Library/Caches".to_string(), ".cargo/registry".to_string()];
        assert!(should_exclude(
            Path::new("/Users/me/Library/Caches"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/Users/me/Library/Caches/com.apple.foo"),
            &excludes
        ));
        assert!(!should_exclude(
            Path::new("/Users/me/Library/Preferences"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/Users/me/.cargo/registry/crates"),
            &excludes
        ));
        assert!(!should_exclude(
            Path::new("/Users/me/.cargo/bin/tool"),
            &excludes
        ));
    }

    #[test]
    fn exclude_multi_segment_pattern_matches_relative_roots() {
        let excludes = vec!["Library/Caches".to_string()];
        assert!(should_exclude(Path::new("Library/Caches"), &excludes));
        assert!(should_exclude(
            Path::new("project/Library/Caches/com.apple.foo"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/Library/Caches/"),
            &["/Library/Caches/".to_string()]
        ));
        assert!(!should_exclude(Path::new("Library/Caches-old"), &excludes));
        assert!(!should_exclude(
            Path::new("SomeLibrary/Caches/file"),
            &excludes
        ));
    }

    #[test]
    fn exclude_common_defaults() {
        let excludes = vec![
            "node_modules".to_string(),
            ".git".to_string(),
            "target".to_string(),
            "DerivedData".to_string(),
            ".DS_Store".to_string(),
        ];
        assert!(should_exclude(
            Path::new("/home/user/project/node_modules/lodash"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/home/user/project/.git/objects"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/home/user/rust/target/release"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/home/user/ios/DerivedData/Build"),
            &excludes
        ));
        assert!(should_exclude(
            Path::new("/home/user/folder/.DS_Store"),
            &excludes
        ));
        assert!(!should_exclude(
            Path::new("/home/user/project/src/main.rs"),
            &excludes
        ));
        assert!(!should_exclude(
            Path::new("/home/user/targets/readme.md"),
            &excludes
        ));
    }

    #[test]
    fn refresh_roots_removes_stale_deleted_entries() {
        let temp = unique_temp_dir("macevery-stale-test");
        let root = temp.join("root");
        let file = root.join("gone.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&file, "hello").unwrap();

        let db = Database::open(temp.join("index.sqlite")).unwrap();
        let file_path = file.to_string_lossy().to_string();
        index_paths(
            &db,
            &IndexRequest {
                roots: vec![root.clone()],
                excludes: Vec::new(),
                rebuild: true,
            },
        )
        .unwrap();
        assert!(db
            .all_records()
            .unwrap()
            .iter()
            .any(|record| record.path == file_path));

        fs::remove_file(&file).unwrap();
        let summary = refresh_roots(&db, &[root], &[]).unwrap();
        assert!(summary.deleted > 0);
        assert!(!db
            .all_records()
            .unwrap()
            .iter()
            .any(|record| record.path == file_path));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn refresh_path_replacing_directory_with_file_removes_old_children() {
        let temp = unique_temp_dir("macevery-replace-dir-test");
        let root = temp.join("root");
        let replaced = root.join("replaced");
        let child = replaced.join("child.txt");
        fs::create_dir_all(&replaced).unwrap();
        fs::write(&child, "hello").unwrap();

        let db = Database::open(temp.join("index.sqlite")).unwrap();
        let replaced_path = replaced.to_string_lossy().to_string();
        let child_path = child.to_string_lossy().to_string();
        index_paths(
            &db,
            &IndexRequest {
                roots: vec![root.clone()],
                excludes: Vec::new(),
                rebuild: true,
            },
        )
        .unwrap();
        assert!(db
            .all_records()
            .unwrap()
            .iter()
            .any(|record| record.path == child_path));

        fs::remove_dir_all(&replaced).unwrap();
        fs::write(&replaced, "now a file").unwrap();
        refresh_path(&db, &replaced, &[]).unwrap();

        let records = db.all_records().unwrap();
        assert!(records
            .iter()
            .any(|record| record.path == replaced_path && record.kind == FileKind::File));
        assert!(!records.iter().any(|record| record.path == child_path));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn leaf_event_removes_stale_subtree_when_ancestor_is_now_a_file() {
        let temp = unique_temp_dir("macevery-replaced-ancestor");
        let root = temp.join("root");
        let parent = root.join("parent");
        let child = parent.join("child-dir");
        let grandchild = child.join("gone.txt");
        fs::create_dir_all(&child).unwrap();
        fs::write(&grandchild, "old").unwrap();
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        index_paths(
            &db,
            &IndexRequest {
                roots: vec![root],
                excludes: vec![],
                rebuild: true,
            },
        )
        .unwrap();
        fs::remove_dir_all(&parent).unwrap();
        fs::write(&parent, "now a file").unwrap();
        refresh_path(&db, &child, &[]).unwrap();
        assert!(db
            .kind_for_path(&child.to_string_lossy())
            .unwrap()
            .is_none());
        assert!(db
            .kind_for_path(&grandchild.to_string_lossy())
            .unwrap()
            .is_none());
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn excluded_refresh_does_not_open_a_transaction_or_change_records() {
        let temp = unique_temp_dir("macevery-excluded-refresh");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        let excluded = temp.join("project/.git/objects");
        db.begin().unwrap();
        let summary = refresh_path(&db, &excluded, &[".git".to_string()]).unwrap();
        assert_eq!(summary.indexed, 0);
        assert_eq!(summary.deleted, 0);
        // A nested BEGIN or COMMIT in refresh_path would fail this contract.
        db.rollback().unwrap();
        assert!(db.all_records().unwrap().is_empty());
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn metadata_error_keeps_indexed_directory_and_children() {
        use crate::model::FileRecord;
        use std::os::unix::fs::symlink;
        let temp = unique_temp_dir("macevery-metadata-error");
        fs::create_dir_all(&temp).unwrap();
        let loop_path = temp.join("loop");
        symlink("loop", &loop_path).unwrap();
        let path = loop_path.join("indexed-dir");
        let child = path.join("keep.txt");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        for (path, kind) in [(&path, FileKind::Directory), (&child, FileKind::File)] {
            db.upsert_file(&FileRecord::new(
                path.to_string_lossy().into_owned(),
                kind,
                1,
            ))
            .unwrap();
        }
        // ELOOP is deterministic even when tests run with elevated privileges.
        assert!(refresh_path(&db, &path, &[]).is_err());
        assert_eq!(db.all_records().unwrap().len(), 2);
        fs::remove_file(loop_path).unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(&child, "restored").unwrap();
        assert!(refresh_path(&db, &path, &[]).unwrap().warnings.is_empty());
        assert_eq!(db.all_records().unwrap().len(), 2);
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn missing_parent_row_still_cleans_deleted_descendants_only_in_that_namespace() {
        use crate::model::FileRecord;
        let temp = unique_temp_dir("macevery-missing-parent");
        let gone = temp.join("root/gone");
        let orphan = gone.join("old-child.txt");
        let neighbor = temp.join("root/gone-neighbor/keep.txt");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        for path in [&orphan, &neighbor] {
            db.upsert_file(&FileRecord::new(
                path.to_string_lossy().into_owned(),
                FileKind::File,
                1,
            ))
            .unwrap();
        }
        let neighbor_before = db
            .all_records()
            .unwrap()
            .into_iter()
            .find(|record| record.path == neighbor.to_string_lossy())
            .unwrap();
        refresh_path(&db, &gone, &[]).unwrap();
        assert_eq!(db.all_records().unwrap(), [neighbor_before]);
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn one_transaction_writer_reuses_bindings_and_preserves_generation_writes() {
        use super::{finish_write, scan_path, ExcludeMatcher, IndexSummary};
        let temp = unique_temp_dir("macevery-scan-writer");
        let root = temp.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "one").unwrap();
        fs::write(root.join("two"), "longer").unwrap();
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        let matcher = ExcludeMatcher::new(&[]);
        let mut summary = IndexSummary::default();
        let generation = super::now_index_generation();
        db.begin().unwrap();
        let mut writer = db.prepare_file_upsert().unwrap();
        scan_path(&mut writer, &root, &matcher, generation, &mut summary).unwrap();
        let first_stamp = db.content_stamp().unwrap();
        scan_path(&mut writer, &root, &matcher, generation, &mut summary).unwrap();
        assert_eq!(
            db.content_stamp().unwrap(),
            first_stamp,
            "identical generation/fields really skip files writes"
        );
        assert_eq!(
            (summary.indexed, summary.rows_written, summary.unchanged),
            (6, 3, 3)
        );
        drop(writer);
        finish_write(&db, Ok(())).unwrap();
        let before = db.all_records().unwrap();
        let summary = refresh_path(&db, &root, &[]).unwrap();
        assert_eq!(
            (summary.indexed, summary.rows_written, summary.unchanged),
            (3, 3, 0)
        );
        assert_eq!(
            db.content_stamp().unwrap().revision,
            first_stamp.revision + 3
        );
        let after = db.all_records().unwrap();
        for (before, after) in before.iter().zip(&after) {
            assert_eq!(before.id, after.id);
            assert!(after.indexed_at > before.indexed_at);
            let mut expected = before.clone();
            expected.indexed_at = after.indexed_at;
            assert_eq!(&expected, after);
        }
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn scan_write_error_rolls_back_files_and_journal_and_releases_transaction() {
        use super::finish_write;
        use crate::error::MacEveryError;
        use crate::model::FileRecord;
        let temp = unique_temp_dir("macevery-scan-rollback");
        let db = Database::open(temp.join("index.sqlite")).unwrap();
        let before = db.content_stamp().unwrap();
        db.begin().unwrap();
        db.upsert_file(&FileRecord::new(
            "/rollback/file".into(),
            FileKind::File,
            10,
        ))
        .unwrap();
        assert!(finish_write(
            &db,
            Err(MacEveryError::Cli("injected scan write failure".into()))
        )
        .is_err());
        assert_eq!(db.content_stamp().unwrap(), before);
        assert!(db.all_records().unwrap().is_empty());
        db.begin().unwrap();
        db.rollback().unwrap();
        drop(db);
        fs::remove_dir_all(temp).unwrap();
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
