//! `cargo truce package --assemble-from <DIR>`: package bundles that
//! already exist instead of building them.
//!
//! A release pipeline that signs bundles on a separate machine between
//! building and packaging (build -> sign -> assemble -> sign) needs the
//! installer made from exactly those bundles: rebuilding here would
//! replace the signed binaries with unsigned ones. So with the flag the
//! packagers skip the build and their per-format staging, copy each
//! plugin's staged tree out of `<DIR>`, check every selected format is
//! there, and run everything after staging as usual - presets, the
//! installer, installer signing and notarization.
//!
//! `<DIR>` is read-only to us: each plugin's tree is copied into a
//! staging directory of our own before anything (installer scripts,
//! component packages) is written next to it, so a signer's signatures
//! there survive byte for byte. The bundles themselves are packaged as
//! found, never re-signed.

use crate::{CargoTruceError, PluginDef};
use std::path::{Path, PathBuf};

/// Pull `--assemble-from <DIR>` out of the package args. Returns the
/// directory (made absolute, and checked to exist) and the remaining
/// args for the per-OS parsers.
pub(crate) fn extract(args: &[String]) -> Result<(Option<PathBuf>, Vec<String>), CargoTruceError> {
    let mut from = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--assemble-from" {
            let value = crate::util::arg_value(args, &mut i, "--assemble-from")?;
            if from.is_some() {
                return Err("--assemble-from given twice".into());
            }
            let dir = Path::new(value);
            if !dir.is_dir() {
                return Err(
                    format!("--assemble-from: {} is not a directory", dir.display()).into(),
                );
            }
            from = Some(std::path::absolute(dir)?);
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    Ok((from, rest))
}

/// `<from>/<bundle_id>`, the staged tree of one plugin, or an error naming
/// what's missing (the packagers fail rather than build it).
pub(crate) fn plugin_dir(from: &Path, p: &PluginDef) -> Result<PathBuf, CargoTruceError> {
    let dir = from.join(&p.bundle_id);
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!(
            "--assemble-from: no staged tree for {} at {}",
            p.name,
            dir.display()
        )
        .into())
    }
}

/// The error for a selected format missing from a plugin's staged tree.
pub(crate) fn missing(p: &PluginDef, format: &str, expected: &Path) -> CargoTruceError {
    format!(
        "--assemble-from: {} has no {format} bundle: expected {}",
        p.name,
        expected.display()
    )
    .into()
}

/// With `--assemble-from`, a plugin's out-of-bundle factory presets come
/// from its staged tree, resolved when the tree was staged - never
/// regenerated here, which would need the build's parameter metadata for
/// field-name keys. So a plugin that ships presets must have them there:
/// fail naming `expected` when it's absent, rather than package an
/// installer without them.
pub(crate) fn require_staged_presets(root: &Path, p: &PluginDef, expected: &Path) -> crate::Res {
    if expected.exists() || !ships_factory_presets(root, p) {
        return Ok(());
    }
    Err(format!(
        "--assemble-from: {} ships factory presets, but its staged tree has none at {}",
        p.name,
        expected.display()
    )
    .into())
}

/// Whether `p` has any authored `.preset` file.
fn ships_factory_presets(root: &Path, p: &PluginDef) -> bool {
    fn any_preset(dir: &Path) -> bool {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let path = e.path();
                if path.is_dir() {
                    any_preset(&path)
                } else {
                    path.extension().is_some_and(|x| x == "preset")
                }
            })
        })
    }
    crate::commands::install::presets::authored_presets_dir(root, p).is_some_and(|d| any_preset(&d))
}

/// Replace `staging` with a copy of `src`. Refuses when one contains the
/// other, which would either copy a tree into itself or delete the very
/// bundles being assembled.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn copy_into_staging(src: &Path, staging: &Path) -> crate::Res {
    ensure_disjoint(src, staging)?;
    let _ = std::fs::remove_dir_all(staging);
    crate::util::copy_dir_recursive(src, staging)
}

/// Refuse a `src` and `staging` where one contains the other, which
/// would copy a tree into itself or delete the bundles being assembled.
pub(crate) fn ensure_disjoint(src: &Path, staging: &Path) -> crate::Res {
    if overlaps(src, staging)? {
        return Err(format!(
            "--assemble-from: {} overlaps cargo-truce's own staging at {}; \
             pass a directory outside it",
            src.display(),
            staging.display()
        )
        .into());
    }
    Ok(())
}

/// Whether either path contains the other, comparing them as resolved on
/// disk (a missing `staging` resolves through its nearest existing
/// ancestor).
fn overlaps(src: &Path, staging: &Path) -> Result<bool, CargoTruceError> {
    let src = src.canonicalize()?;
    let staging = resolve_lenient(staging)?;
    Ok(src.starts_with(&staging) || staging.starts_with(&src))
}

/// `path` canonicalized, or - when it doesn't exist yet - its nearest
/// existing ancestor canonicalized with the rest appended.
fn resolve_lenient(path: &Path) -> Result<PathBuf, CargoTruceError> {
    let path = std::path::absolute(path)?;
    let mut tail = Vec::new();
    let mut cur = path.as_path();
    loop {
        if let Ok(resolved) = cur.canonicalize() {
            return Ok(tail.iter().rev().fold(resolved, |acc, part| acc.join(part)));
        }
        let (Some(parent), Some(name)) = (cur.parent(), cur.file_name()) else {
            return Ok(path);
        };
        tail.push(name.to_owned());
        cur = parent;
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use super::copy_into_staging;
    use super::{extract, overlaps};
    use std::fs;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("truce-assemble-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extract_takes_the_dir_and_leaves_the_rest() {
        let dir = scratch("extract");
        let (from, rest) = extract(&args(&[
            "-p",
            "gain",
            "--assemble-from",
            dir.to_str().unwrap(),
            "--no-sign",
        ]))
        .unwrap();
        assert_eq!(from.as_deref(), Some(dir.as_path()));
        assert_eq!(rest, args(&["-p", "gain", "--no-sign"]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extract_rejects_a_missing_dir() {
        let err = extract(&args(&["--assemble-from", "/no/such/dir"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn staging_inside_or_around_the_source_is_refused() {
        let dir = scratch("overlap");
        let src = dir.join("stage/gain");
        fs::create_dir_all(&src).unwrap();
        assert!(overlaps(&src, &src).unwrap());
        assert!(overlaps(&src, &src.join("not-yet")).unwrap());
        assert!(overlaps(&src, &dir).unwrap());
        assert!(!overlaps(&src, &dir.join("target/package/gain")).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The copy is the source byte for byte, and the source is untouched.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn copy_replaces_staging_and_leaves_the_source_alone() {
        let dir = scratch("copy");
        let src = dir.join("in/gain");
        fs::create_dir_all(src.join("clap/x64")).unwrap();
        fs::write(src.join("clap/x64/Gain.clap"), b"signed bytes").unwrap();
        let staging = dir.join("out/gain");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("stale.txt"), b"old").unwrap();

        copy_into_staging(&src, &staging).unwrap();

        assert_eq!(
            fs::read(staging.join("clap/x64/Gain.clap")).unwrap(),
            b"signed bytes"
        );
        assert!(!staging.join("stale.txt").exists());
        assert_eq!(
            fs::read(src.join("clap/x64/Gain.clap")).unwrap(),
            b"signed bytes"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
