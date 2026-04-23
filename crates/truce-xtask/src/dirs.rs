//! Minimal `home_dir()` shim. We keep this in-tree to avoid pulling in
//! the `dirs` crate just for one lookup.

use std::path::PathBuf;

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
