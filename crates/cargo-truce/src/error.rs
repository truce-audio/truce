//! Typed error enum for cargo-truce.
//!
//! Replaces the previous `Box<dyn Error>` blanket so callers can
//! pattern-match on the failure mode (missing tool, codesign failure,
//! manifest parse error, etc.) instead of string-grepping the
//! `Display` output. The catch-all `Other` variant carries any
//! still-stringly-typed errors from `Err("…".into())` sites the
//! codebase hasn't migrated yet - `From<String>` and `From<&str>`
//! conversions keep `?` propagation transparent during the migration.

use std::io;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CargoTruceError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("codesign failed: {0}")]
    Codesign(String),

    #[error("required tool not found: {0}")]
    MissingTool(String),

    #[error("{0}")]
    Other(String),
}

impl From<String> for CargoTruceError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for CargoTruceError {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

impl From<Box<dyn std::error::Error>> for CargoTruceError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for CargoTruceError {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::Other(e.to_string())
    }
}
