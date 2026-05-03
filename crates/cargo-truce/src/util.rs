//! Generic helpers shared across commands: paths, sub-process invocation,
//! signing, and Visual Studio / CMake / Ninja location.
//!
//! Functions here have no per-command flavor — anything that's specific
//! to install, package, or doctor lives next to the command that uses it.

use crate::BoxErr;
use std::env;
use std::fs;
#[cfg(target_os = "macos")]
use std::io::Read;
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

    // Both used only by AAX template + AU v3 staging on macOS / Windows.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<(), BoxErr> {
        let path = path.as_ref();
        fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()).into())
    }

    /// Write only if the target file is missing or its bytes differ. On a
    /// no-op, the file's mtime stays put — important for tools like cmake
    /// that rebuild based on mtime comparisons.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn write_if_changed(
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> Result<bool, BoxErr> {
        let path = path.as_ref();
        let new = contents.as_ref();
        if let Ok(existing) = fs::read(path)
            && existing == new
        {
            return Ok(false);
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

/// Return the cargo target directory for `root`. Honors the
/// `CARGO_TARGET_DIR` env var when set (so test harnesses and CI can
/// share a build cache across crates) and falls back to
/// `<root>/target/`. Use this anywhere xtask reads or writes inside
/// the cargo target tree — never hard-code `<root>/target/`.
pub(crate) fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => root.join("target"),
    }
}

// Process-scoped active build profile. Drives both `cargo build`'s
// `--release` / `--profile <name>` flag selection and the
// `release_lib*` path resolvers (which read `target/<profile>/...`).
// Default is "release" so commands like `package` that never set
// the profile keep producing release artifacts.
//
// Recognised values:
//   - "release"  → `cargo build --release`,   `target/release/...`
//   - "debug"    → `cargo build`,             `target/debug/...`
//   - "shell"    → `cargo build --profile shell`, `target/shell/...`
//   - any other  → `cargo build --profile <name>`, `target/<name>/...`
static PROFILE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Set the active cargo profile by name. `"release"` / `"debug"` map
/// to cargo's built-in profiles; any other name maps to a custom
/// profile defined in the user's `Cargo.toml` (e.g. `[profile.shell]
/// inherits = "release"` for the shell-mode build).
pub(crate) fn set_build_profile(name: &str) {
    let mut g = PROFILE.lock().unwrap();
    g.clear();
    g.push_str(name);
}

/// Convenience wrapper for the common boolean-debug case. Equivalent
/// to `set_build_profile("debug")` / `set_build_profile("release")`.
pub(crate) fn set_debug_profile(debug: bool) {
    set_build_profile(if debug { "debug" } else { "release" });
}

/// Set a process-wide env var for downstream `cargo build` invocations
/// to inherit. Wraps the 2024-edition `unsafe std::env::set_var` so
/// the soundness comment lives in one place.
///
/// Soundness: `set_var` is `unsafe` because it mutates process-wide env
/// state without synchronization, and Rust can't see other threads
/// reading the same map (e.g. cpal / coreaudio threads on macOS,
/// allocator threads on Linux). `cargo truce` reaches this helper from
/// the install / build paths, both of which run only on the main
/// thread before any worker thread spawns — at the call site we hold
/// no concurrent reader. New callers must satisfy the same invariant.
pub(crate) fn set_build_env(key: &str, value: &str) {
    unsafe {
        std::env::set_var(key, value);
    }
}

/// Preflight check for `cargo truce install --shell` / `build --shell`:
/// the project's `Cargo.toml` (single-crate plugin or workspace root)
/// must declare a `[profile.shell]` table so `cargo build --profile
/// shell` resolves. Plugins scaffolded before 0.13.x predate the
/// custom profile and need a one-line addition.
///
/// Returns `Ok(())` when the profile is declared. Otherwise returns
/// an error string the caller can propagate; the message includes
/// the exact lines to add.
pub(crate) fn verify_shell_profile_declared() -> Result<(), BoxErr> {
    let cargo_toml = project_root().join("Cargo.toml");
    let content = fs::read_to_string(&cargo_toml).map_err(|e| -> BoxErr {
        format!("failed to read {}: {e}", cargo_toml.display()).into()
    })?;
    let doc: toml::Table = content.parse().map_err(|e| -> BoxErr {
        format!("failed to parse {}: {e}", cargo_toml.display()).into()
    })?;
    let has_profile_shell = doc
        .get("profile")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("shell"))
        .is_some();
    if has_profile_shell {
        return Ok(());
    }
    Err(format!(
        "--shell requires `[profile.shell]` in {}.\n\
         Add the following two lines and re-run:\n\
         \n\
             [profile.shell]\n\
             inherits = \"release\"\n\
         \n\
         (Plugins scaffolded with truce 0.13.x or later already include this.)",
        cargo_toml.display()
    )
    .into())
}

/// Read the active build profile name, defaulting to `"release"` when
/// no command has set one.
pub(crate) fn build_profile_name() -> String {
    let g = PROFILE.lock().unwrap();
    if g.is_empty() {
        "release".to_string()
    } else {
        g.clone()
    }
}

/// Whether the current xtask invocation is operating in debug mode.
/// Read by `cargo_build` so debug-flagged commands skip `--release`.
pub(crate) fn is_debug_profile() -> bool {
    build_profile_name() == "debug"
}

fn profile_subdir() -> String {
    build_profile_name()
}

/// Return `<target>/<profile>/{shared_lib_name}` for a plugin.
/// `<profile>` is `release` by default; commands that flip the active
/// profile (`--debug` → `"debug"`, shell-mode builds → `"shell"`)
/// move the resolution accordingly.
pub(crate) fn release_lib(root: &Path, stem: &str) -> PathBuf {
    target_dir(root)
        .join(profile_subdir())
        .join(shared_lib_name(stem))
}

/// Per-arch sibling of [`release_lib`]. `target` selects the triple
/// subdir cargo writes to (macOS universal, Windows x64+arm64); the
/// profile subdir tracks `release_lib`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn release_lib_for_target(root: &Path, stem: &str, target: Option<&str>) -> PathBuf {
    match target {
        Some(t) => target_dir(root)
            .join(t)
            .join(profile_subdir())
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
/// Only consumed by the package pipelines (macOS .pkg, Windows .exe).
#[cfg(any(target_os = "macos", target_os = "windows"))]
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

/// Resolve a plugin crate's `Cargo.toml` path via `cargo metadata`.
/// Used by `detect_default_features` to find the manifest in
/// workspace layouts where plugins live in arbitrary subdirectories.
fn locate_plugin_manifest(project_root: &Path, crate_name: &str) -> Option<PathBuf> {
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(project_root.join("Cargo.toml"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Cheap substring parse — avoids depending on serde_json here. We
    // only need `"name":"crate_name"` and the adjacent `"manifest_path"`.
    let text = String::from_utf8_lossy(&out.stdout);
    let name_needle = format!("\"name\":\"{crate_name}\"");
    let idx = text.find(&name_needle)?;
    // Find the enclosing package object by scanning for the nearest
    // `"manifest_path":"..."` within a bounded window.
    let window_end = (idx + 2048).min(text.len());
    let window_start = idx.saturating_sub(2048);
    let window = &text[window_start..window_end];
    let mp_marker = "\"manifest_path\":\"";
    let mp_idx = window.find(mp_marker)?;
    let rest = &window[mp_idx + mp_marker.len()..];
    let end = rest.find('"')?;
    let path = &rest[..end];
    Some(PathBuf::from(path))
}

/// Detect which format features to build when the user didn't pass
/// any `--clap` / `--vst3` / etc. flags.
///
/// Lookup order:
///
/// 1. **Root `Cargo.toml`'s `[features].default`** — the single-crate
///    layout (`cargo truce new` produces this). Most reliable signal.
/// 2. **Plugin crates listed in `truce.toml`** — the workspace layout
///    (`cargo truce new --workspace`). Reads each plugin's own
///    `[features].default` and returns the **union**, so `install`
///    tries the formats declared by at least one plugin and skips the
///    rest (vs. the old fall-through that tried *every* format and
///    errored for any plugin that didn't declare it).
pub(crate) fn detect_default_features() -> std::collections::HashSet<String> {
    let root = project_root();

    // Single-crate layout: root Cargo.toml has a `[features]` table.
    if let Ok(content) = fs::read_to_string(root.join("Cargo.toml"))
        && let Ok(doc) = content.parse::<toml::Table>()
        && let Some(toml::Value::Table(feat)) = doc.get("features")
        && let Some(toml::Value::Array(defaults)) = feat.get("default")
    {
        return defaults
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }

    // Workspace layout: iterate plugins from `truce.toml` and union
    // their declared default features.
    let mut union = std::collections::HashSet::new();
    if let Ok(config) = crate::load_config() {
        for p in &config.plugin {
            if let Some(manifest) = locate_plugin_manifest(&root, &p.crate_name)
                && let Ok(content) = fs::read_to_string(&manifest)
                && let Ok(doc) = content.parse::<toml::Table>()
                && let Some(toml::Value::Table(feat)) = doc.get("features")
                && let Some(toml::Value::Array(defaults)) = feat.get("default")
            {
                for v in defaults {
                    if let Some(s) = v.as_str() {
                        union.insert(s.to_string());
                    }
                }
            }
        }
    }
    if !union.is_empty() {
        return union;
    }

    // Last-ditch fallback: assume every format (legacy behavior, kept
    // so projects without truce.toml don't break).
    ["clap", "vst3", "vst2", "lv2", "au", "aax"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub(crate) fn project_root() -> PathBuf {
    // Walk up from the current directory looking for truce.toml. This
    // is what `cargo truce` does — the globally installed binary runs
    // from any project directory.
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
    // Fallback: CARGO_MANIFEST_DIR (works when invoked inside the
    // truce repo itself, e.g. for development).
    if let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") {
        let p = Path::new(&manifest).parent().unwrap().to_path_buf();
        if p.join("truce.toml").exists() {
            return p;
        }
    }
    cwd
}

pub(crate) fn run_sudo(cmd: &str, args: &[&str]) -> crate::Res {
    announce_sudo_once();
    let status = Command::new("sudo").arg(cmd).args(args).status()?;
    if !status.success() {
        return Err(format!("sudo {cmd} failed with {status}").into());
    }
    Ok(())
}

/// Print a one-line "why" before the first `sudo` call of the run, so the
/// user understands the password prompt that's about to appear. No-op on
/// subsequent calls — sudo's own cred cache covers the rest of the install.
fn announce_sudo_once() {
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "→ Installing to system plugin directories (/Library/Audio/Plug-Ins/, \
             /Library/Application Support/Avid/) — sudo required."
        );
    }
}

/// Process-global verbose flag. Set at the top of `cargo_truce::run`
/// from `-v` / `--verbose` and consulted by helpers that have output
/// worth gating (`codesign`'s "replacing existing signature", etc.).
static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn is_verbose() -> bool {
    VERBOSE.load(std::sync::atomic::Ordering::Relaxed)
}

/// `eprintln!` that's a no-op unless `--verbose` was passed. Use for
/// progress chatter (per-format build banners, per-bundle install
/// destinations) that's load-bearing during debugging but noise during
/// a normal multi-plugin install.
macro_rules! vprintln {
    ($($arg:tt)*) => {
        if $crate::util::is_verbose() {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use vprintln;

/// Per-process collector of produced bundle paths so the calling command
/// (`cmd_install` / `cmd_build`) can print a summary at the end (always
/// visible, regardless of verbose). Each per-format helper pushes one
/// line per bundle it writes.
static OUTPUTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Record an output destination + echo it under `--verbose`. Used by
/// `install_clap` / `install_vst3` / `stage_clap` / `stage_vst3` / etc.
pub(crate) fn log_output(line: String) {
    if is_verbose() {
        eprintln!("{line}");
    }
    if let Ok(mut v) = OUTPUTS.lock() {
        v.push(line);
    }
}

/// Drain the output log. Called once by the surrounding command at the
/// end so the summary prints exactly once and the static stays empty
/// between calls.
pub(crate) fn take_outputs() -> Vec<String> {
    OUTPUTS
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// Per-process collector of soft-skipped install reasons (e.g. AAX with
/// no SDK configured, AU v3 with ad-hoc signing). Same pattern as
/// `INSTALLED` but printed under a `Skipped:` header at the end of
/// `cmd_install` so the user sees what didn't make it.
static SKIPPED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Append a soft-skip reason. One line per (format, plugin) target —
/// callers should embed the plugin name in the message so the user
/// can match each skip to the corresponding `Installed:` row.
pub(crate) fn log_skip(line: String) {
    if is_verbose() {
        eprintln!("{line}");
    }
    if let Ok(mut v) = SKIPPED.lock() {
        v.push(line);
    }
}

pub(crate) fn take_skipped() -> Vec<String> {
    SKIPPED
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// Run `codesign` with the given args. Prints a one-line success or
/// failure summary per call (`  ✓ signed Truce Gain.vst3` / `  ✗ ...`).
/// In quiet mode, the `replacing existing signature` chatter and verify
/// output is captured and only printed on failure. `--verbose` inherits
/// stderr so everything surfaces.
///
/// Safe to redirect stderr even on the sudo path: `sudo` opens
/// `/dev/tty` for the password prompt, not stderr, so the prompt
/// stays visible to the user.
///
/// macOS-only: `codesign` is an Apple tool. CLAP / VST3 / LV2 on
/// Linux are unsigned `.so` files; Windows signs via `signtool` in
/// `packaging_windows`, not through here. The cross-platform
/// `codesign_bundle` wrapper short-circuits on non-macOS, so callers
/// never reach this function on other platforms.
#[cfg(target_os = "macos")]
pub(crate) fn run_codesign(args: &[&str], use_sudo: bool) -> crate::Res {
    use std::process::Stdio;
    let target = args.last().copied().unwrap_or("?");
    let target_label = std::path::Path::new(target)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.to_string());
    let is_verify = args.contains(&"--verify");
    let (verb_present, verb_past) = if is_verify {
        ("verify", "verified")
    } else {
        ("sign", "signed")
    };

    let mut cmd = if use_sudo {
        announce_sudo_once();
        let mut c = Command::new("sudo");
        c.arg("codesign");
        c
    } else {
        Command::new("codesign")
    };
    cmd.args(args);

    let (status, captured_stderr) = if is_verbose() {
        (cmd.status()?, String::new())
    } else {
        let output = cmd.stderr(Stdio::piped()).output()?;
        (
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    if status.success() {
        eprintln!("  ✓ {verb_past} {target_label}");
        Ok(())
    } else {
        if !captured_stderr.is_empty() {
            eprintln!("{captured_stderr}");
        }
        eprintln!("  ✗ failed to {verb_present} {target_label}");
        Err("codesign failed".into())
    }
}

/// Fire-and-forget cleanup helper. Intended for `killall -9 pkd` /
/// `killall -9 AudioComponentRegistrar` where non-zero exit
/// ("No matching processes were found") is expected noise on clean
/// systems and shouldn't clutter the install log. No sudo: both
/// daemons run in the user's launchd session, so the user can kill
/// their own processes. Only used by macOS-side AU v3 install +
/// `reset-au`.
#[cfg(target_os = "macos")]
pub(crate) fn run_silent(cmd: &str, args: &[&str]) {
    use std::process::Stdio;
    let _ = Command::new(cmd)
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
#[cfg(target_os = "macos")]
pub(crate) fn is_production_identity(identity: &str) -> bool {
    identity != "-"
}

/// Return the project-local temp directory (`<target>/tmp/`), creating it if needed.
pub(crate) fn tmp_dir() -> PathBuf {
    let dir = target_dir(&project_root()).join("tmp");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Write entitlements.plist to a temp file and return its path.
/// Only consumed by `codesign_bundle` on macOS.
#[cfg(target_os = "macos")]
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

/// Code-sign a bundle (or a single Mach-O). When `identity` is a
/// Developer ID, adds hardened runtime, timestamp, and entitlements
/// (required for notarization). When ad-hoc (`"-"`), performs a
/// simple ad-hoc sign.
/// If `use_sudo` is true the codesign command runs via sudo.
///
/// **Inside-out signing.** When `path` is a directory (a bundle),
/// every Mach-O in the bundle is enumerated and signed explicitly
/// before the bundle's outer seal is applied. This bypasses Apple's
/// `codesign --deep` traversal — which doesn't recurse into
/// `Contents/Resources/` for AAX (TDMw) and other non-app bundle
/// types, leaving inner dylibs with their linker-applied ad-hoc
/// signature and breaking notarization. Apple has been deprecating
/// `--deep` for years anyway; enumerate ourselves to be sure.
pub(crate) fn codesign_bundle(_bundle: &str, _identity: &str, _use_sudo: bool) -> crate::Res {
    // macOS-only: `codesign` is an Apple tool, and the entitlements plist
    // we write is consumed only by it. On Linux / Windows this is a no-op
    // so the cross-platform `stage_*` helpers can call us unconditionally.
    #[cfg(target_os = "macos")]
    {
        let production = is_production_identity(_identity);
        let entitlements = write_entitlements_plist();
        let ent_path = entitlements.to_str().unwrap();
        let bundle_path = Path::new(_bundle);

        let sign_one = |target: &str| -> crate::Res {
            let mut args: Vec<&str> = vec!["--force", "--sign", _identity];
            if production {
                args.extend_from_slice(&["--options", "runtime", "--timestamp"]);
                args.extend_from_slice(&["--entitlements", ent_path]);
            }
            args.push(target);
            run_codesign(&args, _use_sudo)
        };

        // Inside-out: sign each Mach-O in the bundle's tree before
        // sealing the bundle itself. For a single-file path, this
        // enumeration is empty and the path goes straight to the
        // bundle-level sign below.
        if bundle_path.is_dir() {
            let mach_os = enumerate_mach_os(bundle_path);
            for mach_o in &mach_os {
                let mach_o_str = mach_o.to_str().ok_or("Mach-O path is not UTF-8")?;
                sign_one(mach_o_str)?;
            }
        }

        // Bundle-level (or single-file) seal. With the inner Mach-Os
        // already signed inside-out, we don't need `--deep` here —
        // codesign will validate the inner signatures and stamp the
        // outer Info.plist seal.
        sign_one(_bundle)?;

        if production {
            run_codesign(&["--verify", "--strict", _bundle], _use_sudo)?;
        }
    }
    Ok(())
}

/// Detect a Mach-O file by its 4-byte magic. Catches 32 / 64-bit
/// thin Mach-O and FAT (universal) binaries in either endianness.
#[cfg(target_os = "macos")]
fn is_mach_o_file(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    if f.read_exact(&mut buf).is_err() {
        return false;
    }
    let magic_be = u32::from_be_bytes(buf);
    matches!(
        magic_be,
        0xFEEDFACE      // thin Mach-O 32-bit, BE
        | 0xFEEDFACF    // thin Mach-O 64-bit, BE
        | 0xCEFAEDFE    // thin Mach-O 32-bit, LE
        | 0xCFFAEDFE    // thin Mach-O 64-bit, LE
        | 0xCAFEBABE    // FAT/universal, BE
        | 0xBEBAFECA // FAT/universal, LE
    )
}

/// Walk a directory recursively and return every Mach-O file found.
/// Used by `codesign_bundle` to drive inside-out signing and by the
/// notarization-readiness check.
#[cfg(target_os = "macos")]
fn enumerate_mach_os(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_mach_os(dir, &mut out);
    out
}

#[cfg(target_os = "macos")]
fn walk_mach_os(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            walk_mach_os(&path, out);
        } else if metadata.is_file() && is_mach_o_file(&path) {
            out.push(path);
        }
    }
}

/// Verify that every Mach-O under `path` is signed for notarization:
///   - signed with a Developer ID Application Authority,
///   - has a secure timestamp,
///   - has the hardened runtime enabled.
///
/// These mirror the checks Apple's notarization service runs server-
/// side; running them locally before submission catches issues
/// (unsigned Mach-Os, missing `--timestamp`, missing
/// `--options runtime`, ad-hoc cert leakage) without a six-minute
/// round-trip to Apple's servers.
///
/// No-op when `identity` is ad-hoc — ad-hoc bundles are deliberately
/// not notarization-ready and the checks would all fail by design.
#[cfg(target_os = "macos")]
pub(crate) fn verify_signed_for_notarization(path: &Path, identity: &str) -> crate::Res {
    if !is_production_identity(identity) {
        return Ok(());
    }

    let mach_os = enumerate_mach_os(path);
    if mach_os.is_empty() {
        return Ok(());
    }

    let mut failures: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for mach_o in &mach_os {
        let issues = check_mach_o_signing(mach_o)?;
        if !issues.is_empty() {
            failures.push((mach_o.clone(), issues));
        }
    }

    if failures.is_empty() {
        return Ok(());
    }

    eprintln!();
    eprintln!(
        "{} Notarization-readiness check failed for {} Mach-O(s) under {}:",
        tag_fail(),
        failures.len(),
        path.display()
    );
    for (path, issues) in &failures {
        eprintln!("    {}", path.display());
        for issue in issues {
            eprintln!("      - {issue}");
        }
    }
    eprintln!();
    eprintln!(
        "These issues mirror Apple's notarization-server checks. \
         Submitting now would fail the same way, with a ~6-minute \
         round-trip per attempt."
    );
    Err("notarization-readiness check failed".into())
}

/// Inspect a single Mach-O via `codesign -d -vvvv` and return any
/// notarization-blocking issues. Empty Vec = passes.
#[cfg(target_os = "macos")]
fn check_mach_o_signing(path: &Path) -> Result<Vec<String>, BoxErr> {
    let path_str = path.to_str().ok_or("Mach-O path is not UTF-8")?;
    let output = Command::new("codesign")
        .args(["-d", "-vvvv", path_str])
        .output()?;
    // codesign writes its detail report to stderr.
    let report = String::from_utf8_lossy(&output.stderr);

    let mut issues = Vec::new();

    if report.contains("code object is not signed at all")
        || report.contains("is not signed at all")
    {
        issues.push("not signed".to_string());
        return Ok(issues);
    }

    if !report.contains("Authority=Developer ID Application:") {
        if report.contains("Signature=adhoc") {
            issues.push("ad-hoc signature (not a Developer ID cert)".to_string());
        } else {
            issues.push("not signed with a Developer ID Application certificate".to_string());
        }
    }

    // Timestamp line shows e.g. "Timestamp=Apr 28, 2026 at ..." or
    // "Signed Time=...". Absence (or "Timestamp=none") means no
    // secure timestamp.
    let has_timestamp = report
        .lines()
        .any(|l| l.starts_with("Timestamp=") && !l.contains("Timestamp=none"));
    if !has_timestamp {
        issues.push("missing secure timestamp (--timestamp)".to_string());
    }

    // Hardened runtime: codesign reports it on the CodeDirectory
    // flags line, e.g. "flags=0x10000(runtime)".
    if !report.contains("(runtime)") {
        issues.push("hardened runtime not enabled (--options runtime)".to_string());
    }

    Ok(issues)
}

/// PACE / iLok wraptool, the canonical macOS install path. Eden 5 ships under
/// `Versions/5/`; `Current` is a stable symlink Eden maintains across version
/// bumps. Users who symlinked `wraptool` onto `$PATH` are picked up first.
#[cfg(target_os = "macos")]
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

// Only `locate_wraptool_macos` calls this; gating to macOS keeps Linux
// from warning on the otherwise-cross-platform Unix `PATH` walker.
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
    installed_rustup_targets().is_some_and(|set| set.contains(triple))
}

/// Query `rustup target list --installed` once per process and cache
/// the result. Returns `None` when rustup itself isn't on PATH —
/// callers decide how to handle that (usually: surface a clear error
/// before invoking cargo with `--target`). Only used by the cross-arch
/// build paths (macOS universal Mach-O, Windows x64+arm64 installer);
/// no Linux caller today.
#[cfg(any(target_os = "macos", target_os = "windows"))]
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
/// `build-install-split.md` doc for the recovery steps). Same gating
/// rationale as [`installed_rustup_targets`].
#[cfg(any(target_os = "macos", target_os = "windows"))]
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
/// Run `cargo build` with the active profile. Release by default;
/// flips to dev when `set_debug_profile(true)` has been called — so
/// commands that accept `--debug` (`build`, `install`, `run`) pick
/// that up without each call site having to thread a flag through.
/// `package` never flips the flag, so shipped artifacts stay release.
pub(crate) fn cargo_build(
    env_vars: &[(&str, &str)],
    extra_args: &[&str],
    deployment_target: &str,
) -> crate::Res {
    cargo_build_with_profile(
        env_vars,
        extra_args,
        deployment_target,
        &build_profile_name(),
    )
}

/// Force a cargo dev-profile build regardless of the global profile
/// flag. Used by `cargo truce screenshot --debug`, which builds a
/// cdylib once and `dlopen`s it without touching the staging/install
/// paths that consult the global flag.
pub(crate) fn cargo_build_debug(
    env_vars: &[(&str, &str)],
    extra_args: &[&str],
    deployment_target: &str,
) -> crate::Res {
    cargo_build_with_profile(env_vars, extra_args, deployment_target, "debug")
}

/// Run `cargo build` with an explicit profile, regardless of the
/// process-global profile flag. `"release"` adds `--release`, `"debug"`
/// adds nothing (cargo's default), any other name adds `--profile <name>`
/// (so a custom `[profile.shell]` in the user's `Cargo.toml` works).
pub(crate) fn cargo_build_with_profile(
    env_vars: &[(&str, &str)],
    extra_args: &[&str],
    deployment_target: &str,
    profile: &str,
) -> crate::Res {
    cargo_build_inner(env_vars, extra_args, deployment_target, profile)
}

fn cargo_build_inner(
    env_vars: &[(&str, &str)],
    extra_args: &[&str],
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] deployment_target: &str,
    profile: &str,
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
    cmd.arg("build");
    match profile {
        "debug" => {} // cargo's default profile, no flag needed
        "release" => {
            cmd.arg("--release");
        }
        custom => {
            cmd.arg("--profile").arg(custom);
        }
    }
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
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MacArch {
    X86_64,
    Arm64,
}

#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
pub(crate) fn extract_team_id(sign_id: &str) -> String {
    if let Some(start) = sign_id.rfind('(')
        && let Some(end) = sign_id.rfind(')')
    {
        return sign_id[start + 1..end].to_string();
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
/// formatting and isn't on every minimal install — Server Core,
/// containers, sandboxed CI). Doing the PATH walk ourselves keeps
/// behavior identical across platforms.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    for dir in env::split_paths(&path) {
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

/// Locate `cl.exe` (the MSVC C/C++ compiler). Tries `%PATH%` first — that
/// only succeeds inside a Developer Command Prompt — then falls back to
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
    if vcvars.exists() { Some(vcvars) } else { None }
}

/// Interactive `[y/N]` prompt that returns `true` only on an explicit yes.
pub(crate) fn confirm_prompt(message: &str) -> bool {
    eprint!("{message} [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    matches!(input.trim(), "y" | "Y" | "yes" | "YES")
}

/// Status markers for `cargo truce doctor` output. Colored when stderr is a
/// terminal and `NO_COLOR` is unset; plain otherwise. All markers are 6 cols
/// wide so they line up regardless of whether color is active.
pub(crate) fn tag_ok() -> String {
    paint("[ OK ]", "\x1b[1;32m")
}
pub(crate) fn tag_fail() -> String {
    paint("[FAIL]", "\x1b[1;31m")
}
pub(crate) fn tag_warn() -> String {
    paint("[WARN]", "\x1b[1;33m")
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn tag_info() -> String {
    paint("[INFO]", "\x1b[1;36m")
}

fn paint(text: &str, ansi: &str) -> String {
    if doctor_use_color() {
        format!("{ansi}{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Cached check: `NO_COLOR` unset AND stderr is a tty. Decided once per
/// process — no need to re-stat the terminal on every line.
fn doctor_use_color() -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static USE: OnceLock<bool> = OnceLock::new();
    *USE.get_or_init(|| {
        if env::var_os("NO_COLOR").is_some() {
            return false;
        }
        std::io::stderr().is_terminal()
    })
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
                eprintln!("    {} {label}", tag_ok());
            } else {
                eprintln!("    {} {label}: {first_line}", tag_ok());
            }
        }
        Ok(_) => eprintln!("    {} {label}", tag_ok()),
        Err(_) => eprintln!("    {} {label}: not found", tag_fail()),
    }
}
