//! Post-link DPI-aware manifest embed for the standalone host exe.
//!
//! Without an embedded application manifest declaring per-monitor v2
//! DPI awareness, Windows treats the standalone process as DPI-unaware
//! and the plugin editor renders blurry / wrong-sized on non-100%
//! displays. baseview does call `SetProcessDpiAwarenessContext` at
//! runtime, but that only gives PMA v1 and runs after the first HWND
//! is created — too late to influence initial sizing.
//!
//! Embedding here (rather than via a `build.rs` in the user's plugin
//! crate) keeps `truce-standalone` and the user's crate free of any
//! manifest plumbing — the truce framework owns the canonical DPI
//! policy in one place. Runs after the staged exe is in place, so
//! it's idempotent across repeated `cargo truce run` invocations.
//!
//! Embed strategy: prefer `mt.exe` from the Windows 10/11 SDK to write
//! a real `RT_MANIFEST` resource into the PE. If `mt.exe` isn't found,
//! drop a `<exe>.manifest` sidecar — Windows loads that automatically
//! when the exe launches, but it's brittle if the bare exe is ever
//! redistributed without the file alongside.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::fs_ctx;
use crate::{tmp_dir, Res};

/// Per-monitor v2 with v1 fallback for older Win10 builds. The
/// `supportedOS` Windows 10 GUID opts in to non-virtualized DPI APIs.
const MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="truce.standalone" version="1.0.0.0"/>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">True/PM</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2,PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

/// Embed the DPI-aware manifest into `exe`. Falls back to a sidecar
/// `<exe>.manifest` if `mt.exe` isn't available. Best-effort: warnings
/// are printed but the function doesn't fail the build.
pub(crate) fn embed_dpi_manifest(exe: &Path) -> Res {
    let manifest_path = tmp_dir().join("truce-standalone.manifest");
    fs_ctx::write(&manifest_path, MANIFEST_XML)?;

    if let Some(mt) = locate_mt_exe() {
        // `;#1` → resource id 1, the slot Windows reads for an exe's
        // application manifest at process start.
        let resource_arg = format!("-outputresource:{};#1", exe.display());
        let result = Command::new(&mt)
            .args([
                "-nologo",
                "-manifest",
                &manifest_path.display().to_string(),
                &resource_arg,
            ])
            .status();
        match result {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => eprintln!(
                "  warning: mt.exe exited with {s} embedding manifest into {} \
                 — falling back to sidecar",
                exe.display()
            ),
            Err(e) => {
                eprintln!("  warning: failed to invoke mt.exe ({e}) — falling back to sidecar")
            }
        }
    } else {
        eprintln!(
            "  note: mt.exe not found (install the Windows 10/11 SDK to embed \
             the DPI manifest); writing sidecar instead."
        );
    }

    fs_ctx::copy(&manifest_path, sidecar_path(exe))?;
    Ok(())
}

fn sidecar_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".manifest");
    PathBuf::from(s)
}

/// Locate `mt.exe`. Mirrors `locate_signtool` in `packaging_windows.rs`:
/// PATH first, then the highest-versioned subdir under the Win10 SDK.
fn locate_mt_exe() -> Option<PathBuf> {
    if let Ok(p) = which("mt.exe") {
        return Some(p);
    }
    let sdk_bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let entries = std::fs::read_dir(&sdk_bin).ok()?;
    let mut best: Option<PathBuf> = None;
    for e in entries.flatten() {
        let candidate = e.path().join(r"x64\mt.exe");
        if candidate.exists() {
            match &best {
                None => best = Some(candidate),
                Some(current) if candidate > *current => best = Some(candidate),
                _ => {}
            }
        }
    }
    best
}

fn which(name: &str) -> std::io::Result<PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PATH not set"))?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{name} not found"),
    ))
}
