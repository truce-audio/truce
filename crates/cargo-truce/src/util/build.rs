//! Cargo build wrappers + cross-arch glue.
//!
//! Owns the rustup target check, profile selection, sccache discovery,
//! `lipo` driver, and the multi-arch build that fans out per Apple
//! architecture. Every cargo invocation that the `install`, `package`,
//! `build`, and `screenshot` commands fire goes through one of the
//! `cargo_build*` wrappers here.

#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::build_profile_name;

/// Return true if `rustup` reports `triple` among its installed targets.
/// Used by `doctor` to surface cross-compile readiness.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn rustup_has_target(triple: &str) -> bool {
    installed_rustup_targets().is_some_and(|set| set.contains(triple))
}

/// Query `rustup target list --installed` once per process and cache
/// the result. Returns `None` when rustup itself isn't on PATH -
/// callers decide how to handle that (usually: surface a clear error
/// before invoking cargo with `--target`). Used by every cross-arch
/// build path (macOS universal Mach-O, Windows x64+arm64 installer,
/// Linux `--target` flag).
fn installed_rustup_targets() -> Option<&'static std::collections::HashSet<String>> {
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
pub(crate) fn ensure_rustup_target(triple: &str) -> crate::Res {
    let Some(installed) = installed_rustup_targets() else {
        return Err(format!(
            "rustup not available - can't verify target `{triple}` is installed. \
             Either `rustup` isn't on PATH, or `cargo` is resolving to a non-rustup \
             toolchain (e.g. Homebrew's). Install rustup from https://rustup.rs and \
             make sure `which cargo` points at `~/.cargo/bin/cargo`."
        )
        .into());
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
/// flips to dev when `set_debug_profile(true)` has been called - so
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
    if let Some(wrapper) = sccache_wrapper() {
        // Cache rustc invocations at the input-hash level. Wins
        // every time cargo's fingerprint flips but the rustc inputs
        // (source + flags + env reachable via `env!`/`option_env!`)
        // are byte-identical - common on cross-arch / cross-feature
        // batches that touch leaf crates back to back.
        cmd.env("RUSTC_WRAPPER", wrapper);
    }
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

/// Like `cargo_build`, but invokes `cargo rustc --bin <name>` for one
/// target and forwards `link_args` after `--`. Use this when linker
/// flags must be scoped to a single bin: the trailing args only reach
/// the chosen target's final rustc invocation, not its dependencies.
///
/// `RUSTFLAGS` is the wrong tool here - it leaks onto every rustc
/// spawn cargo does for the build, including transitively-required
/// cdylib link steps that reject exe-only flags like `/SUBSYSTEM:WINDOWS`
/// (the cdylib has no `main`, so `link.exe` errors with `LNK2019`).
#[cfg(target_os = "windows")]
pub(crate) fn cargo_rustc_bin(
    env_vars: &[(&str, &str)],
    base_args: &[&str],
    package: &str,
    bin_name: &str,
    link_args: &[&str],
) -> crate::Res {
    {
        let mut it = base_args.iter();
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
    cmd.arg("rustc");
    match build_profile_name().as_str() {
        "debug" => {}
        "release" => {
            cmd.arg("--release");
        }
        custom => {
            cmd.arg("--profile").arg(custom);
        }
    }
    cmd.arg("-p").arg(package);
    cmd.arg("--bin").arg(bin_name);
    if let Some(wrapper) = sccache_wrapper() {
        cmd.env("RUSTC_WRAPPER", wrapper);
    }
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
    for arg in base_args {
        cmd.arg(arg);
    }
    if !link_args.is_empty() {
        cmd.arg("--");
        for a in link_args {
            cmd.arg(a);
        }
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err("cargo rustc failed".into());
    }
    Ok(())
}

/// Resolve a path to `sccache` if it's available and the user hasn't
/// pinned `RUSTC_WRAPPER` themselves. Returns `None` when sccache is
/// off the path (silent passthrough - no error, no log) or when the
/// user has already configured a wrapper they presumably prefer.
pub(crate) fn sccache_wrapper() -> Option<std::ffi::OsString> {
    // Respect any user-set wrapper - don't override their choice.
    // `TRUCE_DISABLE_SCCACHE=1` is the escape hatch when the user
    // wants cargo-truce to skip auto-wrapping for one invocation.
    if std::env::var_os("RUSTC_WRAPPER").is_some()
        || std::env::var_os("RUSTC_WORKSPACE_WRAPPER").is_some()
        || std::env::var_os("TRUCE_DISABLE_SCCACHE").is_some()
    {
        return None;
    }
    which("sccache")
}

/// Minimal `which`: walk `PATH` looking for an executable file with
/// `name`. Avoids pulling in the `which` crate just for this one use.
fn which(name: &str) -> Option<std::ffi::OsString> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
        {
            return Some(candidate.into_os_string());
        }
    }
    None
}

/// Apple architecture. Used by both AU v3 install and `cargo truce package`
/// to drive per-arch cargo builds and lipo into universal binaries. Defined
/// unconditionally so cross-platform codepaths can reference it without a
/// cfg matrix - only the macOS arms actually touch lipo/xcodebuild.
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
        // No fattening needed - just copy to the canonical location so
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
    let arg_refs: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
    cargo_build(env_vars, &arg_refs, dt)
}

/// Build for every Apple arch in `archs` in a single cargo invocation
/// by passing multiple `--target <triple>` flags. Cargo 1.64+ accepts
/// this and parallelizes codegen across targets internally - shared
/// `.rmeta` is computed once, target-specific codegen runs per-arch
/// inside the same process - so the user gets:
///
/// - One `target/.cargo-lock` acquisition (no inter-process lock
///   contention on the workspace lock file).
/// - One progress display, with cargo's normal terminal styling /
///   color / progress bar inherited.
/// - One dep-graph resolution + process startup cost amortized
///   across all arches.
///
/// Per-target outputs land at `target/<triple>/release/` exactly as
/// `cargo_build_for_arch` would deposit them.
#[cfg(target_os = "macos")]
pub(crate) fn cargo_build_multi_arch(
    archs: &[MacArch],
    base_args: &[&str],
    dt: &str,
) -> crate::Res {
    let mut args: Vec<String> = Vec::with_capacity(archs.len() * 2 + base_args.len());
    for arch in archs {
        args.push("--target".into());
        args.push(arch.triple().into());
    }
    for a in base_args {
        args.push((*a).into());
    }
    let arg_refs: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
    cargo_build(&[], &arg_refs, dt)
}
