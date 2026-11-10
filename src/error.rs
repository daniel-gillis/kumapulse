//! Error type shared by the data sources.
//!
//! Kept as a small enum with a `Display` impl instead of pulling in an error
//! crate: the exporter has four failure modes and they are all "tell the
//! operator what to fix".

use std::fmt;

#[derive(Debug)]
pub enum SourceError {
    /// The SQLite database could not be read (missing, locked, wrong schema).
    Database(String),
    /// The HTTP request to Uptime Kuma failed.
    Http(String),
    /// Uptime Kuma answered, but not in a shape this exporter understands.
    Parse(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(m) => write!(f, "database: {m}"),
            Self::Http(m) => write!(f, "http: {m}"),
            Self::Parse(m) => write!(f, "parse: {m}"),
        }
    }
}

impl std::error::Error for SourceError {}
