use std::fmt::{self, Display};
use std::io;

#[derive(Debug)]
pub enum MacEveryError {
    Cli(String),
    Io(io::Error),
    Sqlite(String),
}

impl Display for MacEveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(message) => write!(f, "{message}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Sqlite(message) => write!(f, "sqlite: {message}"),
        }
    }
}

impl std::error::Error for MacEveryError {}

impl From<io::Error> for MacEveryError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, MacEveryError>;
