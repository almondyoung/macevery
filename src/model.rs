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
    pub ext_filters: Vec<String>,
    pub excluded_ext_filters: Vec<String>,
    pub kind: Option<FileKind>,
    pub kind_filters: Vec<FileKind>,
    pub excluded_kind_filters: Vec<FileKind>,
    pub path_filters: Vec<PathFilter>,
    pub excluded_path_filters: Vec<PathFilter>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub path_only: bool,
    pub fuzzy: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathFilterMode {
    Contains,
    Component,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathFilter {
    pub value: String,
    pub mode: PathFilterMode,
}

impl SearchOptions {
    pub fn normalized(&self) -> Self {
        self.normalized_at(now_epoch())
    }

    pub fn normalized_at(&self, now: i64) -> Self {
        let mut out = self.clone();
        let mut query_terms = Vec::new();
        let mut ext_filters = self.ext_filters.clone();
        let mut excluded_ext_filters = self.excluded_ext_filters.clone();
        let mut kind_filters = self.kind_filters.clone();
        let mut excluded_kind_filters = self.excluded_kind_filters.clone();
        let mut path_filters = self.path_filters.clone();
        let mut excluded_path_filters = self.excluded_path_filters.clone();

        if let Some(ext) = &self.ext {
            push_unique(&mut ext_filters, normalize_ext(ext));
        }
        if let Some(kind) = &self.kind {
            push_unique(&mut kind_filters, kind.clone());
        }

        for token in tokenize_query(&self.query) {
            if token.is_empty() {
                continue;
            }

            match parse_inline_filter(&token, now) {
                InlineFilter::Ext(exts) => {
                    for ext in exts {
                        push_unique(&mut ext_filters, ext);
                    }
                }
                InlineFilter::ExcludedExt(exts) => {
                    for ext in exts {
                        push_unique(&mut excluded_ext_filters, ext);
                    }
                }
                InlineFilter::Kind(kinds) => {
                    for kind in kinds {
                        push_unique(&mut kind_filters, kind);
                    }
                }
                InlineFilter::ExcludedKind(kinds) => {
                    for kind in kinds {
                        push_unique(&mut excluded_kind_filters, kind);
                    }
                }
                InlineFilter::Path(path) => push_unique(&mut path_filters, path),
                InlineFilter::ExcludedPath(path) => push_unique(&mut excluded_path_filters, path),
                InlineFilter::ModifiedAfter(after) => out.modified_after = Some(after),
                InlineFilter::ModifiedBefore(before) => out.modified_before = Some(before),
                InlineFilter::None => query_terms.push(token.to_string()),
            }
        }

        out.query = query_terms.join(" ");
        out.ext_filters = ext_filters;
        out.excluded_ext_filters = excluded_ext_filters;
        out.kind_filters = kind_filters;
        out.excluded_kind_filters = excluded_kind_filters;
        out.path_filters = path_filters;
        out.excluded_path_filters = excluded_path_filters;
        out
    }

    pub fn fuzzy_enabled(&self) -> bool {
        self.fuzzy
            || tokenize_query(&self.query)
                .into_iter()
                .next()
                .map(|token| token.starts_with('~') && token.len() > 1)
                .unwrap_or(false)
    }

    pub fn terms(&self) -> Vec<String> {
        tokenize_query(&self.query)
            .into_iter()
            .map(|value| value.strip_prefix('~').unwrap_or(&value).to_string())
            .filter(|value| !value.is_empty())
            .collect()
    }
}

enum InlineFilter {
    Ext(Vec<String>),
    ExcludedExt(Vec<String>),
    Kind(Vec<FileKind>),
    ExcludedKind(Vec<FileKind>),
    Path(PathFilter),
    ExcludedPath(PathFilter),
    ModifiedAfter(i64),
    ModifiedBefore(i64),
    None,
}

fn parse_inline_filter(token: &str, now: i64) -> InlineFilter {
    let (negated, token) = if let Some(rest) = token.strip_prefix('!') {
        (true, rest)
    } else if let Some(rest) = token.strip_prefix('-') {
        (true, rest)
    } else {
        (false, token)
    };

    let Some((key, value)) = token.split_once(':') else {
        return InlineFilter::None;
    };
    let value = value.trim();
    if value.is_empty() {
        return InlineFilter::None;
    }

    match key.to_lowercase().as_str() {
        "ext" | "extension" => match split_filter_values(value)
            .into_iter()
            .map(|value| normalize_ext(&value))
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
        {
            values if values.is_empty() => InlineFilter::None,
            values if negated => InlineFilter::ExcludedExt(values),
            values => InlineFilter::Ext(values),
        },
        "kind" | "type" => match split_filter_values(value)
            .into_iter()
            .filter_map(|value| FileKind::from_str(&value))
            .collect::<Vec<_>>()
        {
            values if values.is_empty() => InlineFilter::None,
            values if negated => InlineFilter::ExcludedKind(values),
            values => InlineFilter::Kind(values),
        },
        "path" | "in" => {
            let filter = PathFilter {
                value: value.to_lowercase(),
                mode: PathFilterMode::Contains,
            };
            if negated {
                InlineFilter::ExcludedPath(filter)
            } else {
                InlineFilter::Path(filter)
            }
        }
        "part" | "segment" | "component" | "parent" => {
            let filter = PathFilter {
                value: value.to_lowercase(),
                mode: PathFilterMode::Component,
            };
            if negated {
                InlineFilter::ExcludedPath(filter)
            } else {
                InlineFilter::Path(filter)
            }
        }
        "mtime" | "modified" => match parse_relative_time(value, now) {
            Some(value) if negated => InlineFilter::ModifiedBefore(value),
            Some(value) => InlineFilter::ModifiedAfter(value),
            None => InlineFilter::None,
        },
        _ => InlineFilter::None,
    }
}

fn tokenize_query(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in query.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }

    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn split_filter_values(value: &str) -> Vec<String> {
    value
        .split(['|', ','])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_ext(value: &str) -> String {
    value.trim().trim_start_matches('.').to_lowercase()
}

fn push_unique<T: Eq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
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
