use std::env;
use std::path::PathBuf;

pub const APP_NAME: &str = "macevery";

pub fn default_db_path() -> PathBuf {
    if let Ok(path) = env::var("MACEVERY_DB") {
        return expand_tilde(&path);
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join(APP_NAME)
        .join("index.sqlite")
}

pub fn default_excludes() -> Vec<String> {
    [
        "node_modules",
        ".git",
        "target",
        "dist",
        "build",
        ".build",
        ".Trash",
        "Library",
        "Library/Caches",
        "Library/Developer",
        "Library/Application Support",
        "Library/Containers",
        "Library/Group Containers",
        "Library/Mail",
        "Library/Messages",
        ".cache",
        ".npm",
        ".cargo/registry",
        "DerivedData",
        ".DS_Store",
    ]
    .iter()
    .map(|value| value.to_string())
    .collect()
}

pub fn default_gui_roots() -> Vec<PathBuf> {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    vec![PathBuf::from(home), PathBuf::from("/Applications")]
}

pub fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".to_string()));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".to_string())).join(rest);
    }
    PathBuf::from(value)
}
