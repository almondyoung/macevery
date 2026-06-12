use std::path::Path;

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
        match value {
            "file" => Some(Self::File),
            "dir" | "directory" => Some(Self::Directory),
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
    pub path_only: bool,
    pub fuzzy: bool,
}

impl SearchOptions {
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
