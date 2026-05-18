//! `which`-style PATH walker plus Windows-side toolchain discovery for
//! `cmake.exe`, `ninja.exe`, `cl.exe`, and `vcvars64.bat`. Used by
//! `cargo truce doctor` (to surface tool availability) and the AAX
//! Windows builder (to drive a Developer-Command-Prompt-equivalent
//! environment from outside one).

use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::{env, fs};

/// Search for `name` (must include `.exe`) on `%PATH%`, returning the first
/// hit. Cross-platform equivalent of `where.exe`.
#[cfg(target_os = "windows")]
pub(crate) fn which_exe(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Locate `name` on `$PATH` (or `%PATH%` on Windows) without shelling
/// out to `which`. Returns the first matching file in the path
/// directory order, or `None` if not found.
///
/// On Windows, falls back to appending `.exe` when the bare name
/// doesn't hit so callers can pass either `"cl"` or `"cl.exe"` and get
/// the same answer.
///
/// Used by `cargo truce doctor` for tool checks. We can't call
/// `Command::new("which")` because Windows doesn't ship one (the
/// closest equivalent is `where.exe`, but it has different output
/// formatting and isn't on every minimal install - Server Core,
/// containers, sandboxed CI). Doing the PATH walk ourselves keeps
/// behavior identical across platforms.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let mut candidate = dir.join(name);
            if !ext.is_empty() {
                let mut s = candidate.into_os_string();
                s.push(ext);
                candidate = PathBuf::from(s);
            }
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locate `cmake.exe`. Tries `%PATH%` first, then the `CMake` that ships with
/// Visual Studio's "C++ `CMake` tools" component, then the standalone installer
/// default. Returns `None` if none are present.
#[cfg(target_os = "windows")]
pub(crate) fn locate_cmake() -> Option<PathBuf> {
    if let Some(p) = which_exe("cmake.exe") {
        return Some(p);
    }
    for vs_install in vs_install_paths() {
        let bundled =
            vs_install.join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    for c in [
        r"C:\Program Files\CMake\bin\cmake.exe",
        r"C:\Program Files (x86)\CMake\bin\cmake.exe",
    ] {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Locate `ninja.exe`. Same strategy as cmake - the VS `CMake` component bundles
/// Ninja next to it, so that's the most common path on machines that have VS
/// with "C++ `CMake` tools" installed.
#[cfg(target_os = "windows")]
pub(crate) fn locate_ninja() -> Option<PathBuf> {
    if let Some(p) = which_exe("ninja.exe") {
        return Some(p);
    }
    for vs_install in vs_install_paths() {
        let bundled =
            vs_install.join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    None
}

/// Locate `cl.exe` (the MSVC C/C++ compiler). Tries `%PATH%` first - that
/// only succeeds inside a Developer Command Prompt - then falls back to
/// scanning `VC\Tools\MSVC\<version>\bin\Hostx64\x64\cl.exe` under each VS
/// install reported by `vswhere.exe`. Returns the newest toolchain version
/// found across all VS installs.
#[cfg(target_os = "windows")]
pub(crate) fn locate_msvc_cl() -> Option<PathBuf> {
    if let Some(p) = which_exe("cl.exe") {
        return Some(p);
    }
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for vs_install in vs_install_paths() {
        let msvc_root = vs_install.join(r"VC\Tools\MSVC");
        let Ok(entries) = fs::read_dir(&msvc_root) else {
            continue;
        };
        for entry in entries.flatten() {
            let cl = entry.path().join(r"bin\Hostx64\x64\cl.exe");
            if cl.is_file() {
                let ver = entry.file_name().to_string_lossy().into_owned();
                candidates.push((ver, cl));
            }
        }
    }
    // Pick the highest version string. MSVC toolchain dirs are dotted numerics
    // (e.g. "14.50.35728"), so lexicographic compare on equal-length segments
    // is wrong, but in practice all entries share the same major and the minor
    // is two digits, so string compare picks the newest correctly here.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

/// Enumerate all VS installation roots known to `vswhere.exe`. Returned in
/// the order vswhere produces (latest first when called with `-latest`, or
/// all installs otherwise). We pass no filter here so we also pick up the old
/// VS 2022 install that's useful for CMake/Ninja even when its C++ workload
/// is broken.
#[cfg(target_os = "windows")]
pub(crate) fn vs_install_paths() -> Vec<PathBuf> {
    let vswhere =
        PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
    if !vswhere.exists() {
        return Vec::new();
    }
    let out = Command::new(&vswhere)
        .args(["-all", "-property", "installationPath", "-format", "value"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Locate `vcvars64.bat` via `vswhere.exe`. Returns `None` if VS isn't
/// installed with the C++ tools component.
#[cfg(target_os = "windows")]
pub(crate) fn locate_vcvars64() -> Option<PathBuf> {
    let vswhere =
        PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
    if !vswhere.exists() {
        return None;
    }
    let out = Command::new(&vswhere)
        .args([
            "-latest",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
            "-format",
            "value",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let install = String::from_utf8(out.stdout).ok()?;
    let install = install.trim();
    if install.is_empty() {
        return None;
    }
    let vcvars = PathBuf::from(install).join(r"VC\Auxiliary\Build\vcvars64.bat");
    vcvars.exists().then_some(vcvars)
}
