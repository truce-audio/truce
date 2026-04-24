//! Generic helpers shared across commands: paths, sub-process invocation,
//! signing, and Visual Studio / CMake / Ninja location.
//!
//! Functions here have no per-command flavor — anything that's specific
//! to install, package, or doctor lives next to the command that uses it.

use crate::BoxErr;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path-aware wrappers around `std::fs`. `io::Error` alone doesn't include
/// the path that triggered it, so a bare `fs::copy(src, dst)?` on a root-owned
/// leftover surfaces as "Permission denied (os error 13)" with no hint at
/// which file the user needs to fix. These wrappers bubble the path up.
pub(crate) mod fs_ctx {
    use crate::BoxErr;
    use std::fs;
    use std::path::Path;

    pub(crate) fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<u64, BoxErr> {
        let (from, to) = (from.as_ref(), to.as_ref());
        fs::copy(from, to)
            .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()).into())
    }

    pub(crate) fn create_dir_all(path: impl AsRef<Path>) -> Result<(), BoxErr> {
        let path = path.as_ref();
        fs::create_dir_all(path).map_err(|e| format!("mkdir -p {}: {e}", path.display()).into())
    }

    pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<(), BoxErr> {
        let path = path.as_ref();
        fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()).into())
    }

    /// Write only if the target file is missing or its bytes differ. On a
    /// no-op, the file's mtime stays put — important for tools like cmake
    /// that rebuild based on mtime comparisons.
    pub(crate) fn write_if_changed(
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> Result<bool, BoxErr> {
        let path = path.as_ref();
        let new = contents.as_ref();
        if let Ok(existing) = fs::read(path) {
            if existing == new {
                return Ok(false);
            }
        }
        fs::write(path, new)
            .map_err(|e| -> BoxErr { format!("write {}: {e}", path.display()).into() })?;
        Ok(true)
    }
}

/// Return the platform-specific shared library filename for a given stem.
/// macOS: `lib{stem}.dylib`, Windows: `{stem}.dll`, Linux: `lib{stem}.so`
pub(crate) fn shared_lib_name(stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else if cfg!(target_os = "linux") {
        format!("lib{stem}.so")
    } else {
        format!("lib{stem}.dylib")
    }
}

/// Return `target/release/{shared_lib_name}` for a plugin.
pub(crate) fn release_lib(root: &Path, stem: &str) -> PathBuf {
    root.join("target/release").join(shared_lib_name(stem))
}

/// Return the release-mode library path for a specific cargo target triple,
/// or the default `target/release/` when `target` is `None`.
pub(crate) fn release_lib_for_target(root: &Path, stem: &str, target: Option<&str>) -> PathBuf {
    match target {
        Some(t) => root
            .join("target")
            .join(t)
            .join("release")
            .join(shared_lib_name(stem)),
        None => release_lib(root, stem),
    }
}

/// Return the Windows `%COMMONPROGRAMFILES%` directory (typically `C:\Program Files\Common Files`).
#[cfg(target_os = "windows")]
pub(crate) fn common_program_files() -> PathBuf {
    if let Ok(v) = env::var("CommonProgramFiles") {
        PathBuf::from(v)
    } else {
        PathBuf::from(r"C:\Program Files\Common Files")
    }
}

/// Return the Windows `%PROGRAMFILES%` directory (typically `C:\Program Files`).
#[cfg(target_os = "windows")]
pub(crate) fn program_files() -> PathBuf {
    if let Ok(v) = env::var("ProgramFiles") {
        PathBuf::from(v)
    } else {
        PathBuf::from(r"C:\Program Files")
    }
}

/// Read the version from Cargo.toml.
/// Checks `[workspace.package] version` first, then `[package] version`.
pub(crate) fn read_workspace_version(root: &Path) -> Option<String> {
    let content = fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let doc: toml::Table = content.parse().ok()?;
    // Workspace layout: [workspace.package] version
    if let Some(v) = doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
    {
        return Some(v.to_string());
    }
    // Single crate: [package] version
    doc.get("package")?
        .get("version")?
        .as_str()
        .map(|s| s.to_string())
}

/// Read the default features from the project's Cargo.toml.
pub(crate) fn detect_default_features() -> std::collections::HashSet<String> {
    let root = project_root();
    if let Ok(content) = fs::read_to_string(root.join("Cargo.toml")) {
        if let Ok(doc) = content.parse::<toml::Table>() {
            if let Some(toml::Value::Table(feat)) = doc.get("features") {
                if let Some(toml::Value::Array(defaults)) = feat.get("default") {
                    return defaults
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
            }
        }
    }
    // Fallback: assume all formats (workspace with multiple crates)
    ["clap", "vst3", "vst2", "lv2", "au", "aax"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub(crate) fn project_root() -> PathBuf {
    // Walk up from the current directory looking for truce.toml.
    // This works from both `cargo xtask` (workspace) and `cargo truce`
    // (globally installed binary run from any project directory).
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();
    loop {
        if dir.join("truce.toml").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    // Fallback: CARGO_MANIFEST_DIR (works inside `cargo xtask`)
    if let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") {
        let p = Path::new(&manifest).parent().unwrap().to_path_buf();
        if p.join("truce.toml").exists() {
            return p;
        }
    }
    cwd
}

pub(crate) fn run_sudo(cmd: &str, args: &[&str]) -> crate::Res {
    let status = Command::new("sudo").arg(cmd).args(args).status()?;
    if !status.success() {
        return Err(format!("sudo {cmd} failed with {status}").into());
    }
    Ok(())
}

/// Sudo variant that swallows stdout + stderr. Intended for
/// fire-and-forget cleanup like `killall -9 pkd` where non-zero
/// exit ("No matching processes were found") is expected noise
/// on clean systems and shouldn't clutter the install log.
pub(crate) fn run_sudo_silent(cmd: &str, args: &[&str]) {
    use std::process::Stdio;
    let _ = Command::new("sudo")
        .arg(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub(crate) fn run_quiet(cmd: &str, args: &[&str]) -> std::result::Result<String, BoxErr> {
    let output = Command::new(cmd).args(args).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Whether the signing identity is a real Developer ID (not ad-hoc).
pub(crate) fn is_production_identity(identity: &str) -> bool {
    identity != "-"
}

/// Return the project-local temp directory (`target/tmp/`), creating it if needed.
pub(crate) fn tmp_dir() -> PathBuf {
    let dir = project_root().join("target/tmp");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Write entitlements.plist to a temp file and return its path.
pub(crate) fn write_entitlements_plist() -> PathBuf {
    let path = tmp_dir().join("entitlements.plist");
    let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
</dict>
</plist>"#;
    let _ = fs::write(&path, content);
    path
}

/// Code-sign a bundle. When `identity` is a Developer ID, adds hardened
/// runtime, timestamp, and entitlements (required for notarization).
/// When ad-hoc (`"-"`), performs a simple ad-hoc sign.
/// If `use_sudo` is true the codesign command runs via sudo.
pub(crate) fn codesign_bundle(bundle: &str, identity: &str, use_sudo: bool) -> crate::Res {
    let production = is_production_identity(identity);
    let entitlements = write_entitlements_plist();
    let ent_path = entitlements.to_str().unwrap();

    let mut args: Vec<&str> = vec!["--force", "--deep", "--sign", identity];
    if production {
        args.extend_from_slice(&["--options", "runtime", "--timestamp"]);
        args.extend_from_slice(&["--entitlements", ent_path]);
    }
    args.push(bundle);

    if use_sudo {
        run_sudo("codesign", &args)?;
    } else {
        let status = Command::new("codesign").args(&args).status()?;
        if !status.success() {
            return Err(format!("codesign failed for {bundle}").into());
        }
    }

    // Verify signature
    if production {
        let verify_args = ["--verify", "--strict", bundle];
        if use_sudo {
            run_sudo("codesign", &verify_args)?;
        } else {
            let status = Command::new("codesign").args(verify_args).status()?;
            if !status.success() {
                return Err(format!("codesign verification failed for {bundle}").into());
            }
        }
    }

    Ok(())
}

/// PACE / iLok wraptool, the canonical macOS install path. Eden 5 ships under
/// `Versions/5/`; `Current` is a stable symlink Eden maintains across version
/// bumps. Users who symlinked `wraptool` onto `$PATH` are picked up first.
#[cfg(not(target_os = "windows"))]
pub(crate) fn locate_wraptool_macos() -> Option<PathBuf> {
    if let Ok(p) = which_unix("wraptool") {
        return Some(p);
    }
    for canonical in [
        "/Applications/PACEAntiPiracy/Eden/Fusion/Current/bin/wraptool",
        "/Applications/PACEAntiPiracy/Eden/Fusion/Versions/5/bin/wraptool",
    ] {
        let p = PathBuf::from(canonical);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn which_unix(name: &str) -> std::result::Result<PathBuf, std::io::Error> {
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
        name.to_string(),
    ))
}

/// PACE-sign an AAX bundle on macOS. No-ops cleanly when wraptool isn't
/// installed or `PACE_ACCOUNT` / `PACE_SIGN_ID` aren't set — Pro Tools
/// Developer loads unsigned AAX, retail rejects with `-14013` → `-7054`.
///
/// Must run **after** Apple codesign on the bundle: PACE wraps the binary
/// and `--dsigharden` re-signs with hardened-runtime + secure timestamp,
/// which is what notarization wants. Apple-signing afterwards would be
/// detected as PACE tampering at load time.
///
/// Must be the **last** step that touches the bundle. PACE 2.4+ inserts a
/// symlink for backwards compatibility; `cp -r` (and most copy helpers
/// without `-H`) convert it to a regular file and break the digital seal.
#[cfg(not(target_os = "windows"))]
pub(crate) fn pace_sign_aax_macos(bundle: &Path) -> crate::Res {
    let Some(wraptool) = locate_wraptool_macos() else {
        eprintln!(
            "    wraptool not found — AAX bundle is unsigned for PACE. \
             Pro Tools Developer will load it; retail Pro Tools won't."
        );
        return Ok(());
    };
    let Ok(account) = std::env::var("PACE_ACCOUNT") else {
        eprintln!("    PACE_ACCOUNT not set — skipping PACE signing.");
        return Ok(());
    };
    let Ok(signid) = std::env::var("PACE_SIGN_ID") else {
        eprintln!("    PACE_SIGN_ID not set — skipping PACE signing.");
        return Ok(());
    };

    eprintln!("    wraptool: PACE-signing {}", bundle.display());
    let bundle_str = bundle
        .to_str()
        .ok_or("AAX bundle path is not valid UTF-8")?;
    let status = Command::new(&wraptool)
        .args([
            "sign",
            "--account",
            &account,
            "--signid",
            &signid,
            "--allowsigningservice",
            "--dsigharden",
            "--dsig1-compat",
            "off",
            "--in",
            bundle_str,
            "--out",
            bundle_str,
        ])
        .status()?;
    if !status.success() {
        return Err("wraptool failed".into());
    }
    Ok(())
}

/// Return true if `rustup` reports `triple` among its installed targets.
/// Used by `doctor` to surface cross-compile readiness.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn rustup_has_target(triple: &str) -> bool {
    installed_rustup_targets().map_or(false, |set| set.contains(triple))
}

/// Query `rustup target list --installed` once per process and cache
/// the result. Returns `None` when rustup itself isn't on PATH —
/// callers decide how to handle that (usually: surface a clear error
/// before invoking cargo with `--target`).
fn installed_rustup_targets() -> Option<&'static std::collections::HashSet<String>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<std::collections::HashSet<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let out = Command::new("rustup")
                .args(["target", "list", "--installed"])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            Some(
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        })
        .as_ref()
}

/// Ensure `rustup` has `triple` installed, adding it if missing. Errors
/// with a clear message when rustup itself isn't on PATH (the common
/// case is a Homebrew `cargo` shadowing rustup's shim; see the
/// `build-install-split.md` doc for the recovery steps).
pub(crate) fn ensure_rustup_target(triple: &str) -> crate::Res {
    let installed = match installed_rustup_targets() {
        Some(s) => s,
        None => {
            return Err(format!(
                "rustup not available — can't verify target `{triple}` is installed. \
                 Either `rustup` isn't on PATH, or `cargo` is resolving to a non-rustup \
                 toolchain (e.g. Homebrew's). Install rustup from https://rustup.rs and \
                 make sure `which cargo` points at `~/.cargo/bin/cargo`."
            )
            .into());
        }
    };
    if installed.contains(triple) {
        return Ok(());
    }
    eprintln!("rustup: installing target {triple}...");
    let status = Command::new("rustup")
        .args(["target", "add", triple])
        .status()?;
    if !status.success() {
        return Err(format!("`rustup target add {triple}` failed").into());
    }
    Ok(())
}

#[allow(unused_variables)]
pub(crate) fn cargo_build(
    env_vars: &[(&str, &str)],
    extra_args: &[&str],
    deployment_target: &str,
) -> crate::Res {
    // If the caller passed `--target <triple>`, make sure rustup has
    // it installed before firing cargo. Catches the common "cross-arch
    // build fails with E0463 can't find crate for core" failure mode.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let mut it = extra_args.iter();
        while let Some(a) = it.next() {
            if *a == "--target" {
                if let Some(triple) = it.next() {
                    ensure_rustup_target(triple)?;
                }
            } else if let Some(triple) = a.strip_prefix("--target=") {
                ensure_rustup_target(triple)?;
            }
        }
    }

    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("--release");
    #[cfg(target_os = "macos")]
    cmd.env("MACOSX_DEPLOYMENT_TARGET", deployment_target);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err("cargo build failed".into());
    }
    Ok(())
}

/// Apple architecture. Used by both AU v3 install and `cargo truce package`
/// to drive per-arch cargo builds and lipo into universal binaries. Defined
/// unconditionally so cross-platform codepaths can reference it without a
/// cfg matrix — only the macOS arms actually touch lipo/xcodebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MacArch {
    X86_64,
    Arm64,
}

impl MacArch {
    pub(crate) fn triple(self) -> &'static str {
        match self {
            MacArch::X86_64 => "x86_64-apple-darwin",
            MacArch::Arm64 => "aarch64-apple-darwin",
        }
    }

    pub(crate) fn host() -> Self {
        if cfg!(target_arch = "aarch64") {
            MacArch::Arm64
        } else {
            MacArch::X86_64
        }
    }
}

/// Combine per-arch dylibs into a single (fat) Mach-O at `output`.
///
/// Single-arch inputs are copied through; the output path matches the legacy
/// non-universal layout (`target/release/...`) so the per-format stage
/// functions don't need to know whether the build was universal.
pub(crate) fn lipo_into(inputs: &[PathBuf], output: &Path) -> crate::Res {
    if inputs.is_empty() {
        return Err("lipo_into: no inputs".into());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    if inputs.len() == 1 {
        // No fattening needed — just copy to the canonical location so
        // downstream stage code reads from the same path in both modes.
        fs::copy(&inputs[0], output)?;
        return Ok(());
    }
    let mut cmd = Command::new("lipo");
    cmd.arg("-create");
    for i in inputs {
        cmd.arg(i);
    }
    cmd.arg("-output").arg(output);
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!(
            "lipo -create failed combining {} slices into {}",
            inputs.len(),
            output.display()
        )
        .into());
    }
    Ok(())
}

/// Run a cargo release build for a specific Apple arch. Adds
/// `--target <triple>` to the caller's args so output lands under
/// `target/{triple}/release/` without colliding with other arches.
pub(crate) fn cargo_build_for_arch(
    env_vars: &[(&str, &str)],
    base_args: &[&str],
    arch: MacArch,
    dt: &str,
) -> crate::Res {
    let mut args: Vec<String> = vec!["--target".into(), arch.triple().into()];
    for a in base_args {
        args.push((*a).into());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    cargo_build(env_vars, &arg_refs, dt)
}

/// Recursive copy that preserves symlinks (critical for macOS .framework
/// bundles) and creates the destination tree.
#[allow(dead_code)]
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> crate::Res {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        // Preserve symlinks (critical for macOS .framework bundles)
        #[cfg(unix)]
        if ft.is_symlink() {
            let target = fs::read_link(&src_path)?;
            let _ = fs::remove_file(&dst_path);
            std::os::unix::fs::symlink(&target, &dst_path)?;
            continue;
        }
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Extract the team ID from a signing identity string like
/// `"Developer ID Application: Name (TEAMID)"`.
pub(crate) fn extract_team_id(sign_id: &str) -> String {
    if let Some(start) = sign_id.rfind('(') {
        if let Some(end) = sign_id.rfind(')') {
            return sign_id[start + 1..end].to_string();
        }
    }
    String::new()
}

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

/// Locate `cmake.exe`. Tries `%PATH%` first, then the CMake that ships with
/// Visual Studio's "C++ CMake tools" component, then the standalone installer
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

/// Locate `ninja.exe`. Same strategy as cmake — the VS CMake component bundles
/// Ninja next to it, so that's the most common path on machines that have VS
/// with "C++ CMake tools" installed.
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
            .map(|l| l.trim())
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
    if vcvars.exists() {
        Some(vcvars)
    } else {
        None
    }
}

/// Interactive `[y/N]` prompt that returns `true` only on an explicit yes.
pub(crate) fn confirm_prompt(message: &str) -> bool {
    eprint!("{message} [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    matches!(input.trim(), "y" | "Y" | "yes" | "YES")
}

/// Print a "tool present" line for `cargo truce doctor`. Runs the command
/// with `args` and shows the first stdout line as the version, or "not found"
/// when the command can't be executed.
pub(crate) fn check_cmd(cmd: &str, args: &[&str], label: &str) {
    match Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() => {
            let ver = String::from_utf8_lossy(&o.stdout);
            let first_line = ver.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                eprintln!("    ✅ {label}");
            } else {
                eprintln!("    ✅ {label}: {first_line}");
            }
        }
        Ok(_) => eprintln!("    ✅ {label}"),
        Err(_) => eprintln!("    ❌ {label}: not found"),
    }
}
