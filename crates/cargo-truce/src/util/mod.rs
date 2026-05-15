//! Generic helpers shared across commands: paths, sub-process invocation,
//! signing, and Visual Studio / `CMake` / Ninja location.
//!
//! Functions here have no per-command flavor — anything that's specific
//! to install, package, or doctor lives next to the command that uses it.

use crate::BoxErr;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

mod build;
mod codesign;
mod locate;

#[cfg(target_os = "windows")]
pub(crate) use build::cargo_rustc_bin;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) use build::rustup_has_target;
#[cfg(target_os = "macos")]
pub(crate) use build::{MacArch, cargo_build_for_arch, cargo_build_multi_arch, lipo_into};
pub(crate) use build::{cargo_build, cargo_build_debug, sccache_wrapper};
pub(crate) use codesign::codesign_bundle;
#[cfg(target_os = "macos")]
pub(crate) use codesign::{
    is_production_identity, locate_wraptool_macos, pace_sign_aax_macos,
    verify_signed_for_notarization,
};
pub(crate) use locate::find_on_path;
#[cfg(target_os = "windows")]
pub(crate) use locate::{
    locate_cmake, locate_msvc_cl, locate_ninja, locate_vcvars64, vs_install_paths, which_exe,
};

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

/// Convert a path to `&str`, panicking with a clear message if
/// the path isn't valid UTF-8. The shell-out helpers in this
/// crate (`run`, `run_capture`, codesign argv assembly) take
/// `&[&str]` rather than `&[OsStr]` because every other arg in
/// those vecs is a literal; this is the standard way to thread
/// a path through. The panic is preferable to `to_string_lossy`
/// — passing a lossy path to `Command::arg` would silently
/// invoke a different binary than the caller named.
///
/// Today only the iOS install pipeline calls this; the gate is
/// `macos` to match, and widens to `any(target_os = "macos", …)`
/// when other shell-out sites fold in.
#[cfg(target_os = "macos")]
#[track_caller]
pub(crate) fn path_str(path: &Path) -> &str {
    path.to_str().unwrap_or_else(|| {
        panic!(
            "non-UTF-8 path can't be passed as a string: {}",
            path.display()
        )
    })
}

/// Consume the next CLI arg as the value for `flag`. Advances `*i`
/// past the consumed slot. Used by every per-subcommand arg loop in
/// `cargo-truce` (`build`/`install`/`uninstall`/`run`/`screenshot`/
/// `validate`/`package`/Windows-packaging) so the
/// "<flag> requires a value" error message stays uniform.
pub(crate) fn arg_value<'a>(
    args: &'a [String],
    i: &mut usize,
    flag: &str,
) -> Result<&'a str, BoxErr> {
    *i += 1;
    args.get(*i)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value").into())
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
// Each `cargo truce <command>` invocation sets the profile at most
// once (in arg parsing, before any build), then reads it many times.
// `OnceLock` matches that lifecycle: `set_build_profile` calls
// `OnceLock::set` (idempotent if the same profile is set twice — the
// second call's value is discarded), and reads never wait on a lock.
static PROFILE: OnceLock<String> = OnceLock::new();

/// Set the active cargo profile by name. `"release"` / `"debug"` map
/// to cargo's built-in profiles; any other name maps to a custom
/// profile defined in the user's `Cargo.toml` (e.g. `[profile.shell]
/// inherits = "release"` for the shell-mode build).
pub(crate) fn set_build_profile(name: &str) {
    PROFILE.get_or_init(|| name.to_string());
}

/// Convenience wrapper for the common boolean-debug case. Equivalent
/// to `set_build_profile("debug")` / `set_build_profile("release")`.
pub(crate) fn set_debug_profile(debug: bool) {
    set_build_profile(if debug { "debug" } else { "release" });
}

/// Preflight check for `cargo truce install --shell` / `build --shell`:
/// the project's `Cargo.toml` (single-crate plugin or workspace root)
/// must declare a `[profile.shell]` table so `cargo build --profile
/// shell` resolves.
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
         (Scaffolded plugins already include this.)",
        cargo_toml.display()
    )
    .into())
}

/// Read the active build profile name, defaulting to `"release"` when
/// no command has set one.
pub(crate) fn build_profile_name() -> String {
    PROFILE
        .get()
        .cloned()
        .unwrap_or_else(|| "release".to_string())
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
    truce_build::target_dir(root)
        .join(profile_subdir())
        .join(shared_lib_name(stem))
}

/// Per-target sibling of [`release_lib`]. `target` selects the triple
/// subdir cargo writes to (macOS universal, Windows x64+arm64, Linux
/// dual-arch); the profile subdir tracks `release_lib`.
pub(crate) fn release_lib_for_target(root: &Path, stem: &str, target: Option<&str>) -> PathBuf {
    match target {
        Some(t) => truce_build::target_dir(root)
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

/// Read the version from `Cargo.toml`.
/// Checks `[workspace.package] version` first, then `[package] version`.
/// Consumed by the package pipelines (macOS .pkg, Windows .exe, Linux
/// tarball).
///
/// # Errors
///
/// Returns `Err` when the manifest can't be read, parsed, or doesn't
/// declare a version anywhere — callers want the IO/parse case
/// distinguishable from the "no version key" case so the user can
/// fix the right thing.
pub(crate) fn read_workspace_version(root: &Path) -> Result<String, crate::BoxErr> {
    let path = root.join("Cargo.toml");
    let content = fs::read_to_string(&path)
        .map_err(|e| -> crate::BoxErr { format!("read {}: {e}", path.display()).into() })?;
    let doc: toml::Table = content
        .parse()
        .map_err(|e| -> crate::BoxErr { format!("parse {}: {e}", path.display()).into() })?;
    if let Some(v) = doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
    {
        return Ok(v.to_string());
    }
    if let Some(v) = doc
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
    {
        return Ok(v.to_string());
    }
    Err(format!(
        "{} has no version (expected [workspace.package] version or [package] version)",
        path.display()
    )
    .into())
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
    // only need `"name":"crate_name"` and the package's `"manifest_path"`.
    //
    // `cargo metadata` emits each package as
    // `{"name":..., "version":..., ..., "manifest_path":..., ...}` —
    // `manifest_path` is always *after* `name` within the same object,
    // and only appears at the package level (not in `dependencies` /
    // `targets`). So scanning forward from the matched `name` lands on
    // the right package's path. The earlier symmetric window scan
    // could see the *previous* package's `manifest_path` (which sits
    // right before the next `name` field) and silently return it.
    let text = String::from_utf8_lossy(&out.stdout);
    let name_needle = format!("\"name\":\"{crate_name}\"");
    let idx = text.find(&name_needle)?;
    let after = &text[idx + name_needle.len()..];
    let mp_marker = "\"manifest_path\":\"";
    let mp_idx = after.find(mp_marker)?;
    let rest = &after[mp_idx + mp_marker.len()..];
    let end = rest.find('"')?;
    Some(PathBuf::from(&rest[..end]))
}

/// Resolve the standalone binary's `[[bin]] name` from a plugin's
/// `Cargo.toml`. Returns the bare stem (no `.exe`).
///
/// Looks for a `[[bin]]` whose `required-features` contains
/// `"standalone"`; falls back to the only `[[bin]]` if exactly one is
/// declared. Returns `None` if no match — callers (`cargo truce run`)
/// then default to the scaffold convention `{crate_name}-standalone`,
/// which is also what the doc instructs hand-written plugins to use.
pub(crate) fn read_standalone_bin_name(crate_name: &str) -> Option<String> {
    let manifest = locate_plugin_manifest(&project_root(), crate_name)?;
    let content = fs::read_to_string(&manifest).ok()?;
    let doc: toml::Table = content.parse().ok()?;
    let bins = doc.get("bin")?.as_array()?;

    // Prefer the `standalone`-gated bin when there are multiple
    // `[[bin]]` entries (e.g. a plugin shipping both standalone +
    // shell-loader binaries).
    for bin in bins {
        let table = bin.as_table()?;
        let has_standalone = table
            .get("required-features")
            .and_then(toml::Value::as_array)
            .is_some_and(|arr| arr.iter().any(|x| x.as_str() == Some("standalone")));
        if has_standalone {
            return table.get("name")?.as_str().map(str::to_string);
        }
    }
    if bins.len() == 1 {
        return bins[0]
            .as_table()?
            .get("name")?
            .as_str()
            .map(str::to_string);
    }
    None
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
///    rest.
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
            .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
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
    union
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

pub(crate) fn run_sudo(cmd: &str, args: &[&OsStr]) -> crate::Res {
    announce_sudo_once();
    let status = Command::new("sudo").arg(cmd).args(args).status()?;
    if !status.success() {
        return Err(crate::CargoTruceError::Other(format!(
            "sudo {cmd} failed with {status}"
        )));
    }
    Ok(())
}

/// Print a one-line "why" before the first `sudo` call of the run, so the
/// user understands the password prompt that's about to appear. No-op on
/// subsequent calls — sudo's own cred cache covers the rest of the install.
fn announce_sudo_once() {
    static ANNOUNCED: AtomicBool = AtomicBool::new(false);
    if !ANNOUNCED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "→ Installing to system plugin directories (/Library/Audio/Plug-Ins/, \
             /Library/Application Support/Avid/) — sudo required."
        );
    }
}

/// Process-global verbose flag. Set at the top of `cargo_truce::run`
/// from `-v` / `--verbose` and consulted by helpers that have output
/// worth gating (`codesign`'s "replacing existing signature", etc.).
static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub(crate) fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
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
static OUTPUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

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
static SKIPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

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
pub(crate) fn run_codesign(args: &[&OsStr], use_sudo: bool) -> crate::Res {
    use std::process::Stdio;
    let target = args.last().copied().unwrap_or(OsStr::new("?"));
    let target_label = std::path::Path::new(target).file_name().map_or_else(
        || target.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let is_verify = args.iter().any(|a| *a == OsStr::new("--verify"));
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
        Err(crate::CargoTruceError::Codesign(format!(
            "failed to {verb_present} {target_label}"
        )))
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
pub(crate) fn run_silent(cmd: &str, args: &[&OsStr]) {
    use std::process::Stdio;
    let _ = Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// Gated to macOS: only `cmd_status` (macOS impl) shells out to `auval`.
#[cfg(target_os = "macos")]
pub(crate) fn run_quiet(cmd: &str, args: &[&OsStr]) -> std::result::Result<String, BoxErr> {
    let output = Command::new(cmd).args(args).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Return the project-local temp directory (`<target>/tmp/`), creating it if needed.
pub(crate) fn tmp_dir() -> PathBuf {
    let dir = truce_build::target_dir(&project_root()).join("tmp");
    let _ = fs::create_dir_all(&dir);
    dir
}

// Per-purpose subdirs under `tmp/`. Keeping `tmp/` from becoming a flat
// junk drawer of `aax_template/`, `entitlements.plist`, `*.bat`,
// `<id>_lv2_stage/`, `<id>_vst3.plist`, `verify-pkg-*/` … each shape
// gets its own subdir below. Helpers always create the dir lazily.

/// `tmp/manifests/` — short-lived plist / `.manifest` / `.json` config
/// files handed to platform tools (codesign, signtool, pkgbuild, etc).
/// Linux's tarball pipeline doesn't shell out to platform tools, so the
/// helper is gated to the platforms that actually consume it.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn tmp_manifests() -> PathBuf {
    let dir = tmp_dir().join("manifests");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// `tmp/scripts/` — generated `.bat` / shell driver scripts. Only the
/// Windows AAX builder shells out to `.bat` files today; if macOS ever
/// grows a similar driver this gate can widen.
#[cfg(target_os = "windows")]
pub(crate) fn tmp_scripts() -> PathBuf {
    let dir = tmp_dir().join("scripts");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// `tmp/verify/` — scratch dirs for post-build artifact verification
/// (pkgutil --expand targets, validator inputs).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn tmp_verify() -> PathBuf {
    let dir = tmp_dir().join("verify");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// `tmp/aax-template/` — Avid AAX C++ wrapper build directory.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn tmp_aax_template() -> PathBuf {
    let dir = tmp_dir().join("aax-template");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// `tmp/au-v3/<bundle_id>/` — per-plugin AU v3 framework + appex build root.
#[cfg(target_os = "macos")]
pub(crate) fn tmp_au_v3(bundle_id: &str) -> PathBuf {
    let dir = tmp_dir().join("au-v3").join(bundle_id);
    let _ = fs::create_dir_all(&dir);
    dir
}

/// `tmp/lv2/<bundle_id>/` — LV2 bundle staging directory.
pub(crate) fn tmp_lv2(bundle_id: &str) -> PathBuf {
    let dir = tmp_dir().join("lv2").join(bundle_id);
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Recursive copy that preserves symlinks (critical for macOS .framework
/// bundles) and creates the destination tree.
///
/// All callers (`commands::install::aax`, `commands::package::stage`,
/// `commands::package::macos`) live behind macOS / Windows cfgs, so
/// the function is genuinely dead on Linux — gate it the same way
/// instead of using `#[allow(dead_code)]`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
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
pub(crate) fn check_cmd(cmd: &str, args: &[&OsStr], label: &str) {
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
