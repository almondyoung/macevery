use crate::error::{MacEveryError, Result};
use std::process::Command;

pub fn open_path(path: &str) -> Result<()> {
    let status = Command::new("open").arg("--").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(MacEveryError::Cli(format!("open failed for {path}")))
    }
}

pub fn reveal_path(path: &str) -> Result<()> {
    let status = Command::new("open")
        .arg("-R")
        .arg("--")
        .arg(path)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(MacEveryError::Cli(format!(
            "Finder reveal failed for {path}"
        )))
    }
}
