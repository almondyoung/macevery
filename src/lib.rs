pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod macos;
pub mod model;
pub mod scanner;
pub mod search;
pub mod server;
pub mod sqlite;
pub mod watch;

pub use error::{MacEveryError, Result};
