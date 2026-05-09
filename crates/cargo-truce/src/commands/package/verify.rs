//! Post-build artifact sanity checks.
//!
//! Each platform's installer pipeline ends by producing a single file:
//! `.pkg` (macOS), `.exe` (Windows), `.tar.gz` (Linux). A whole class of
//! bugs leaves these files *technically valid* but with no payload —
//! e.g. a malformed Distribution.xml that drops every `<pkg-ref>`
//! produces a 2 KB metadata-only `.pkg` that opens in Installer.app
//! and reports "can't find the data needed for installation". The build
//! pipeline itself has no idea anything went wrong.
//!
//! These helpers run once per artifact, right after the platform tool
//! produces it, and bail loudly if the artifact is implausibly small or
//! missing expected payload entries.
//!
//! Cheap by design: a stat call plus one `pkgutil --expand` /
//! `tar -tzf` read. Worth the milliseconds against the alternative
//! of silently shipping a broken installer to end users.

use crate::Res;
use std::fs;
use std::path::Path;

/// Hard floor below which any installer is definitely empty / missing
/// payload. Inno Setup's bootstrap is ~700 KB on its own; productbuild's
/// metadata wrapper around an empty distribution is ~3 KB; tar.gz of
/// nothing-but-install.sh is ~1 KB. 50 KB sits well above all of those
/// failure modes and well below any real installer.
const MIN_INSTALLER_BYTES: u64 = 50_000;

/// Assert the artifact exists on disk and is over the empty-installer
/// floor. Catches the broadest class of "I produced nothing useful"
/// regressions across all three platforms.
pub(crate) fn assert_min_size(artifact: &Path) -> Res {
    let size = fs::metadata(artifact)
        .map_err(|e| format!("missing produced artifact {}: {e}", artifact.display()))?
        .len();
    if size < MIN_INSTALLER_BYTES {
        return Err(format!(
            "produced installer is only {} bytes (under {} byte floor): {}\n\
             This usually means the build pipeline succeeded but the \
             payload was dropped. Check the staging dir for missing \
             components or a malformed distribution / .iss / install.sh.",
            size,
            MIN_INSTALLER_BYTES,
            artifact.display()
        )
        .into());
    }
    Ok(())
}

/// Expand a macOS `.pkg` and assert every named component package is
/// present with a non-empty Payload. Catches the productbuild bug
/// where a malformed Distribution.xml (e.g. nested `<choice>` elements)
/// silently drops `<pkg-ref>` references — productbuild reports
/// success but the resulting `.pkg` contains only a Distribution file.
///
/// `expected_components` lists the component-package filenames each
/// caller already knows it staged (e.g. `["Truce Tremolo-CLAP.pkg",
/// "Truce Tremolo-VST3.pkg"]`).
#[cfg(target_os = "macos")]
pub(crate) fn assert_pkg_contains_components(pkg: &Path, expected_components: &[String]) -> Res {
    use std::process::Command;

    assert_min_size(pkg)?;

    let scratch = crate::tmp_dir().join(format!(
        "verify-pkg-{}",
        pkg.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("artifact")
    ));
    let _ = fs::remove_dir_all(&scratch);

    let status = Command::new("pkgutil")
        .args([
            "--expand",
            pkg.to_str().unwrap(),
            scratch.to_str().unwrap(),
        ])
        .status()?;
    if !status.success() {
        return Err(format!("pkgutil --expand failed on {}", pkg.display()).into());
    }

    let mut missing = Vec::new();
    for name in expected_components {
        let component_dir = scratch.join(name);
        if !component_dir.is_dir() {
            missing.push(name.clone());
            continue;
        }
        let payload = component_dir.join("Payload");
        if !payload.exists() {
            missing.push(format!("{name} (missing Payload)"));
        }
    }
    let _ = fs::remove_dir_all(&scratch);

    if !missing.is_empty() {
        return Err(format!(
            "{} is missing {} expected component(s): {}\n\
             productbuild reported success but the payload was dropped — \
             check the suite distribution.xml for nested <choice> elements \
             or unresolved pkg-ref filenames.",
            pkg.display(),
            missing.len(),
            missing.join(", "),
        )
        .into());
    }
    Ok(())
}

/// Read a `.tar.gz` member listing and assert each substring appears
/// at least once. Catches the case where `tar` reports success but the
/// archive is missing per-plugin payload directories — e.g. a staging
/// step that silently failed for one plugin.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn assert_tarball_contains(tarball: &Path, expected_substrings: &[&str]) -> Res {
    use std::process::Command;

    assert_min_size(tarball)?;

    let output = Command::new("tar")
        .args(["-tzf", tarball.to_str().unwrap()])
        .output()?;
    if !output.status.success() {
        return Err(format!("tar -tzf failed on {}", tarball.display()).into());
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let members: Vec<&str> = listing.lines().collect();

    let mut missing = Vec::new();
    for needle in expected_substrings {
        if !members.iter().any(|m| m.contains(needle)) {
            missing.push(*needle);
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "{} is missing {} expected entry/entries: {}\n\
             The tar step succeeded but staging produced no matching \
             files — check stage_plugin_payload for silent failures.",
            tarball.display(),
            missing.len(),
            missing.join(", "),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes a fixed-size junk file under the project's tmp dir so the
    /// test cleans up under `cargo clean` and never touches `/tmp`.
    fn write_dummy(name: &str, bytes: usize) -> std::path::PathBuf {
        let dir = crate::tmp_dir().join("verify-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
        path
    }

    #[test]
    fn min_size_rejects_tiny_file() {
        let path = write_dummy("tiny.bin", 1024);
        let err = assert_min_size(&path).expect_err("1 KB should fail the min-size check");
        let msg = err.to_string();
        assert!(
            msg.contains("under") && msg.contains("byte floor"),
            "got: {msg}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn min_size_accepts_normal_file() {
        let path = write_dummy("normal.bin", 100_000);
        assert!(assert_min_size(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn min_size_reports_missing_artifact() {
        let path = crate::tmp_dir()
            .join("verify-tests")
            .join("does-not-exist.bin");
        let err = assert_min_size(&path).expect_err("nonexistent path should error");
        assert!(
            err.to_string().contains("missing produced artifact"),
            "got: {err}"
        );
    }
}
