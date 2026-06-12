use crate::db::Database;
use crate::error::Result;
use crate::model::{FileKind, FileRecord};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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
    pub warnings: Vec<String>,
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
    db.begin()?;
    let result = (|| {
        let indexed_at = now_epoch();
        for root in &request.roots {
            scan_path(db, root, &request.excludes, indexed_at, &mut summary)?;
        }
        db.set_meta("last_indexed_at", &indexed_at.to_string())?;
        Ok(())
    })();

    match result {
        Ok(()) => db.commit()?,
        Err(err) => {
            let _ = db.rollback();
            return Err(err);
        }
    }

    Ok(summary)
}

pub fn refresh_path(db: &Database, path: &Path, excludes: &[String]) -> Result<IndexSummary> {
    let mut summary = IndexSummary::default();
    db.begin()?;
    let result = (|| {
        let path_text = path.to_string_lossy().to_string();
        if path.exists() || fs::symlink_metadata(path).is_ok() {
            scan_path(db, path, excludes, now_epoch(), &mut summary)?;
        } else {
            db.delete_path_prefix(&path_text)?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => db.commit()?,
        Err(err) => {
            let _ = db.rollback();
            return Err(err);
        }
    }

    Ok(summary)
}

pub fn refresh_roots(
    db: &Database,
    roots: &[PathBuf],
    excludes: &[String],
) -> Result<IndexSummary> {
    let mut summary = IndexSummary::default();
    db.begin()?;
    let result = (|| {
        let indexed_at = now_epoch();
        for root in roots {
            scan_path(db, root, excludes, indexed_at, &mut summary)?;
        }
        db.set_meta("last_indexed_at", &indexed_at.to_string())?;
        Ok(())
    })();

    match result {
        Ok(()) => db.commit()?,
        Err(err) => {
            let _ = db.rollback();
            return Err(err);
        }
    }

    Ok(summary)
}

fn scan_path(
    db: &Database,
    path: &Path,
    excludes: &[String],
    indexed_at: i64,
    summary: &mut IndexSummary,
) -> Result<()> {
    if should_exclude(path, excludes) {
        return Ok(());
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => {
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
    db.upsert_file(&record)?;
    summary.indexed += 1;

    if metadata.is_dir() && kind != FileKind::Symlink {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) => {
                summary
                    .warnings
                    .push(format!("{}: {}", path.to_string_lossy(), err));
                return Ok(());
            }
        };
        for entry in entries {
            match entry {
                Ok(entry) => scan_path(db, &entry.path(), excludes, indexed_at, summary)?,
                Err(err) => summary
                    .warnings
                    .push(format!("{}: {}", path.to_string_lossy(), err)),
            }
        }
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

fn should_exclude(path: &Path, excludes: &[String]) -> bool {
    let path_text = path.to_string_lossy();
    for pattern in excludes {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        if path.file_name().and_then(|value| value.to_str()) == Some(pattern) {
            return true;
        }
        if path_text.ends_with(pattern) || path_text.contains(&format!("/{pattern}/")) {
            return true;
        }
    }
    false
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::should_exclude;
    use std::path::Path;

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
}
