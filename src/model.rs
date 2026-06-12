use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    App,
    Other,
}

impl FileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::Symlink => "symlink",
            Self::App => "app",
            Self::Other => "other",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value.to_lowercase().as_str() {
            "file" => Some(Self::File),
            "dir" | "dirs" | "directory" | "directories" | "folder" | "folders" => {
                Some(Self::Directory)
            }
            "symlink" | "link" => Some(Self::Symlink),
            "app" => Some(Self::App),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub path_lower: String,
    pub basename: String,
    pub basename_lower: String,
    pub ext_lower: Option<String>,
    pub kind: FileKind,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub indexed_at: i64,
}

impl FileRecord {
    pub fn new(path: String, kind: FileKind, indexed_at: i64) -> Self {
        let path_ref = Path::new(&path);
        let path_lower = path.to_lowercase();
        let basename = path_ref
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        let basename_lower = basename.to_lowercase();
        let ext_lower = path_ref
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_lowercase())
            .filter(|value| !value.is_empty());

        Self {
            id: 0,
            path,
            path_lower,
            basename,
            basename_lower,
            ext_lower,
            kind,
            size: None,
            mtime: None,
            ctime: None,
            dev: None,
            inode: None,
            indexed_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub record: FileRecord,
    pub score: i64,
}

#[derive(Clone, Debug, Default)]
pub struct SearchOptions {
    pub query: String,
    pub limit: usize,
    pub ext: Option<String>,
    pub kind: Option<FileKind>,
    pub path_filters: Vec<String>,
    pub modified_after: Option<i64>,
    pub path_only: bool,
    pub fuzzy: bool,
}

impl SearchOptions {
    pub fn normalized(&self) -> Self {
        self.normalized_at(now_epoch())
    }

    pub fn normalized_at(&self, now: i64) -> Self {
        let mut out = self.clone();
        let mut query_terms = Vec::new();

        for token in self.query.split_whitespace().map(str::trim) {
            if token.is_empty() {
                continue;
            }

            match parse_inline_filter(token, now) {
                InlineFilter::Ext(ext) => out.ext = Some(ext),
                InlineFilter::Kind(kind) => out.kind = Some(kind),
                InlineFilter::Path(path) => out.path_filters.push(path),
                InlineFilter::ModifiedAfter(after) => out.modified_after = Some(after),
                InlineFilter::None => query_terms.push(token.to_string()),
            }
        }

        out.query = query_terms.join(" ");
        out
    }

    pub fn fuzzy_enabled(&self) -> bool {
        self.fuzzy
            || self
                .query
                .split_whitespace()
                .next()
                .map(|token| token.starts_with('~') && token.len() > 1)
                .unwrap_or(false)
    }

    pub fn terms(&self) -> Vec<String> {
        self.query
            .split_whitespace()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.strip_prefix('~').unwrap_or(value))
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }
}

enum InlineFilter {
    Ext(String),
    Kind(FileKind),
    Path(String),
    ModifiedAfter(i64),
    None,
}

fn parse_inline_filter(token: &str, now: i64) -> InlineFilter {
    let Some((key, value)) = token.split_once(':') else {
        return InlineFilter::None;
    };
    let value = value.trim();
    if value.is_empty() {
        return InlineFilter::None;
    }

    match key.to_lowercase().as_str() {
        "ext" | "extension" => {
            let ext = value.trim_start_matches('.').to_lowercase();
            if ext.is_empty() {
                InlineFilter::None
            } else {
                InlineFilter::Ext(ext)
            }
        }
        "kind" | "type" => FileKind::from_str(value)
            .map(InlineFilter::Kind)
            .unwrap_or(InlineFilter::None),
        "path" | "in" => InlineFilter::Path(value.to_lowercase()),
        "mtime" | "modified" => parse_relative_time(value, now)
            .map(InlineFilter::ModifiedAfter)
            .unwrap_or(InlineFilter::None),
        _ => InlineFilter::None,
    }
}

fn parse_relative_time(value: &str, now: i64) -> Option<i64> {
    let value = value.trim().to_lowercase();
    let digits = value
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }

    let count = digits.parse::<i64>().ok()?;
    let unit = value[digits.len()..].trim();
    let seconds = match unit {
        "h" | "hour" | "hours" => 60 * 60,
        "" | "d" | "day" | "days" => 24 * 60 * 60,
        "w" | "week" | "weeks" => 7 * 24 * 60 * 60,
        "mo" | "month" | "months" => 30 * 24 * 60 * 60,
        "y" | "year" | "years" => 365 * 24 * 60 * 60,
        _ => return None,
    };
    Some(now.saturating_sub(count.saturating_mul(seconds)))
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, Default)]
pub struct IndexStats {
    pub files: i64,
    pub dirs: i64,
    pub symlinks: i64,
    pub apps: i64,
    pub others: i64,
    pub db_path: String,
    pub last_indexed_at: Option<i64>,
    pub roots: Vec<String>,
    pub excludes: Vec<String>,
}
