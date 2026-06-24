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

#[cfg(target_os = "windows")]
mod msvc_env {
    //! Cross-arch MSVC env handling for `cargo build --target *-pc-windows-msvc`.
    //!
    //! Two mechanisms layered together so a mismatched developer shell
    //! never reaches `link.exe`:
    //!
    //! 1. **Preflight check** - parse `%LIB%` for the target arch's lib
    //!    directory. If it isn't there, we know `link.exe` would pull
    //!    the wrong-arch import libs and emit dozens of unresolved
    //!    externals (CRT `exp2`/`sinh`/…, plus a `LNK4272` machine-type
    //!    mismatch).
    //!
    //! 2. **vcvarsall subshell** - when the preflight fails but we can
    //!    locate `vcvarsall.bat`, wrap the cargo invocation in a temp
    //!    `.bat` that calls `vcvarsall.bat <arch>` first. The child
    //!    cmd.exe gets the right `LIB`/`INCLUDE`/`PATH` regardless of
    //!    the launching shell, so a dual-arch package build "just
    //!    works" from a plain PowerShell prompt. When `vcvarsall.bat`
    //!    isn't installed, we surface one actionable error instead of
    //!    letting cargo fan out to a 56-line `LNK1120` wall.
    use crate::locate_vcvarsall;
    use std::path::PathBuf;

    /// Lib subdirectory name MSVC uses for a Rust target triple's
    /// import libraries (`<vs>\VC\Tools\MSVC\<ver>\lib\<arch>\…` and
    /// `<Windows Kits>\10\lib\<ver>\um\<arch>\…`). Returns `None` for
    /// triples we don't try to manage - those fall through to cargo's
    /// normal behavior.
    pub(super) fn target_lib_arch(triple: &str) -> Option<&'static str> {
        match triple {
            "x86_64-pc-windows-msvc" => Some("x64"),
            "aarch64-pc-windows-msvc" => Some("arm64"),
            _ => None,
        }
    }

    /// Argument to pass to `vcvarsall.bat` to set the env for a given
    /// host + target combination. Only the host/target pairs truce
    /// supports are covered (x64 host, x64 or arm64 target; arm64
    /// host, arm64 target).
    pub(super) fn vcvarsall_arch_arg(host_arch: &str, target_arch: &str) -> Option<&'static str> {
        match (host_arch, target_arch) {
            ("x86_64", "x64") => Some("x64"),
            ("x86_64", "arm64") => Some("x64_arm64"),
            ("aarch64", "arm64") => Some("arm64"),
            ("aarch64", "x64") => Some("arm64_x64"),
            _ => None,
        }
    }

    /// Return the host CPU arch as a Rust-target-style short string
    /// (`"x86_64"` or `"aarch64"`). Used to pick the vcvarsall arg.
    pub(super) fn host_arch() -> &'static str {
        if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        }
    }

    /// True when at least one path in `lib_env` (`%LIB%`) has a path
    /// component exactly equal to `arch` - i.e. one of the lib dirs
    /// vcvars adds for that arch. Case-insensitive (Windows paths
    /// are), and matches on whole-component equality so `arm64`
    /// doesn't false-positive against `arm64ec`.
    pub(super) fn lib_env_has_arch(lib_env: &str, arch: &str) -> bool {
        let arch_lower = arch.to_lowercase();
        lib_env.split(';').any(|seg| {
            seg.split(['\\', '/'])
                .any(|c| c.eq_ignore_ascii_case(&arch_lower))
        })
    }

    /// Pretty-print the LIB env value for the error message, one path
    /// per line, indented. Empty/missing → `"(empty)"`.
    pub(super) fn format_lib_for_error(lib_env: &str) -> String {
        if lib_env.is_empty() {
            return "    (empty)".to_string();
        }
        lib_env
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| format!("    {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Outcome of deciding how to run a cargo invocation for one
    /// `--target *-pc-windows-msvc` triple. Returned by [`plan_for_target`].
    #[cfg_attr(test, derive(Debug))]
    pub(super) enum Plan {
        /// Current env already matches; invoke cargo directly.
        DirectOk,
        /// Env doesn't match but we can fix it via `vcvarsall.bat`.
        WrapVcvarsall {
            vcvarsall: PathBuf,
            arch_arg: &'static str,
        },
        /// Env doesn't match and we can't fix it; surface a clear error.
        Unfixable { message: String },
    }

    pub(super) fn plan_for_target(triple: &str) -> Option<Plan> {
        plan_for_target_with(
            triple,
            &std::env::var("LIB").unwrap_or_default(),
            host_arch(),
            locate_vcvarsall,
        )
    }

    /// Plan resolution with injected env + host + vcvars locator, so unit
    /// tests can drive every branch without touching the real
    /// `LIB`/`vswhere` environment.
    fn plan_for_target_with(
        triple: &str,
        lib_env: &str,
        host: &str,
        locator: fn() -> Option<PathBuf>,
    ) -> Option<Plan> {
        let arch = target_lib_arch(triple)?;
        if lib_env_has_arch(lib_env, arch) {
            return Some(Plan::DirectOk);
        }
        let Some(arch_arg) = vcvarsall_arch_arg(host, arch) else {
            return Some(Plan::Unfixable {
                message: format!(
                    "cargo-truce: building for `{triple}` from a `{host}` host \
                     isn't a supported vcvars combo. Launch a Developer shell \
                     that targets {arch} manually."
                ),
            });
        };
        if let Some(vcvarsall) = locator() {
            return Some(Plan::WrapVcvarsall {
                vcvarsall,
                arch_arg,
            });
        }
        Some(Plan::Unfixable {
            message: format!(
                "cargo-truce: building for `{triple}` but the current `%LIB%` \
                 doesn't contain an `{arch}` lib directory, and `vcvarsall.bat` \
                 isn't installed so we can't fix it automatically.\n\
                 \n\
                 Either:\n  \
                 - install \"MSVC v143 - VS 2022 C++ {arch_upper}/ARM64EC build tools\" \
                 via the VS Installer (`cargo truce doctor` will then see it), or\n  \
                 - launch a Developer PowerShell with the right arch and re-run:\n      \
                 `Launch-VsDevShell.ps1 -Arch {arch} -HostArch {host_for_msg}`\n\
                 \n\
                 Current LIB:\n{lib_lines}",
                arch_upper = arch.to_uppercase(),
                host_for_msg = if host == "aarch64" { "arm64" } else { "x64" },
                lib_lines = format_lib_for_error(lib_env),
            ),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn no_vcvars() -> Option<PathBuf> {
            None
        }

        // Signature has to match the `fn() -> Option<PathBuf>` locator
        // pointer plan_for_target_with takes, so the wrap is load-bearing.
        #[allow(clippy::unnecessary_wraps)]
        fn fake_vcvars() -> Option<PathBuf> {
            Some(PathBuf::from(r"C:\fake\vcvarsall.bat"))
        }

        #[test]
        fn lib_arch_for_supported_triples() {
            assert_eq!(target_lib_arch("x86_64-pc-windows-msvc"), Some("x64"));
            assert_eq!(target_lib_arch("aarch64-pc-windows-msvc"), Some("arm64"));
            assert_eq!(target_lib_arch("x86_64-apple-darwin"), None);
            assert_eq!(target_lib_arch("aarch64-unknown-linux-gnu"), None);
        }

        #[test]
        fn vcvarsall_arg_picks_cross_compile_combo() {
            assert_eq!(vcvarsall_arch_arg("x86_64", "x64"), Some("x64"));
            assert_eq!(vcvarsall_arch_arg("x86_64", "arm64"), Some("x64_arm64"));
            assert_eq!(vcvarsall_arch_arg("aarch64", "arm64"), Some("arm64"));
            assert_eq!(vcvarsall_arch_arg("aarch64", "x64"), Some("arm64_x64"));
            assert_eq!(vcvarsall_arch_arg("riscv64", "arm64"), None);
        }

        #[test]
        fn lib_env_arch_match_is_case_insensitive() {
            let lib = r"C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\um\ARM64;\
                        C:\VS\VC\Tools\MSVC\14.51\lib\arm64";
            assert!(lib_env_has_arch(lib, "arm64"));
            assert!(!lib_env_has_arch(lib, "x64"));
        }

        #[test]
        fn lib_env_arch_match_rejects_wrong_arch() {
            let lib = r"C:\Windows Kits\10\lib\10.0.26100.0\um\x86;C:\VS\lib\x86";
            assert!(!lib_env_has_arch(lib, "arm64"));
            assert!(!lib_env_has_arch(lib, "x64"));
        }

        #[test]
        fn plan_direct_when_lib_matches_target() {
            let lib = r"C:\Windows Kits\10\lib\10.0\um\arm64;C:\VS\lib\arm64";
            let plan = plan_for_target_with("aarch64-pc-windows-msvc", lib, "x86_64", no_vcvars);
            assert!(matches!(plan, Some(Plan::DirectOk)));
        }

        #[test]
        fn plan_wraps_in_vcvars_when_lib_mismatches_and_locator_finds_it() {
            let lib = r"C:\Windows Kits\10\lib\10.0\um\x86;C:\VS\lib\x86";
            let plan = plan_for_target_with("aarch64-pc-windows-msvc", lib, "x86_64", fake_vcvars);
            match plan {
                Some(Plan::WrapVcvarsall { arch_arg, .. }) => {
                    assert_eq!(arch_arg, "x64_arm64");
                }
                other => panic!("expected WrapVcvarsall, got {other:?}"),
            }
        }

        #[test]
        fn plan_unfixable_when_vcvars_missing() {
            let lib = r"C:\Windows Kits\10\lib\10.0\um\x86;C:\VS\lib\x86";
            let plan = plan_for_target_with("aarch64-pc-windows-msvc", lib, "x86_64", no_vcvars);
            match plan {
                Some(Plan::Unfixable { message }) => {
                    assert!(message.contains("aarch64-pc-windows-msvc"));
                    assert!(message.contains("vcvarsall.bat"));
                }
                other => panic!("expected Unfixable, got {other:?}"),
            }
        }

        #[test]
        fn plan_skips_non_windows_msvc_triples() {
            assert!(
                plan_for_target_with("x86_64-apple-darwin", "", "x86_64", fake_vcvars,).is_none()
            );
        }
    }
}

#[cfg(target_os = "windows")]
use crate::tmp_scripts;
#[cfg(target_os = "windows")]
use crate::util::fs_ctx;

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
    let targets = extract_target_triples(extra_args);
    for triple in &targets {
        // Catches the common "cross-arch build fails with E0463 can't
        // find crate for core" failure mode.
        ensure_rustup_target(triple)?;
    }

    #[cfg(target_os = "windows")]
    let msvc_plan = resolve_msvc_plan(&targets)?;

    let mut cmd = build_cargo_command(profile);
    #[cfg(target_os = "macos")]
    cmd.env("MACOSX_DEPLOYMENT_TARGET", deployment_target);
    apply_target_cpu(&mut cmd, &targets);
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

    #[cfg(target_os = "windows")]
    if let Some((vcvarsall, arch_arg)) = msvc_plan {
        return run_via_vcvarsall(&cmd, &vcvarsall, arch_arg, "cargo build failed");
    }

    let status = cmd.status()?;
    if !status.success() {
        return Err("cargo build failed".into());
    }
    Ok(())
}

fn build_cargo_command(profile: &str) -> Command {
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
    cmd
}

/// Walk the windows-msvc targets in this cargo invocation and decide
/// whether to invoke cargo directly, wrap it in a `vcvarsall.bat`
/// subshell, or bail with an actionable error. Returns the chosen
/// `(vcvarsall, arch_arg)` for the wrap case; `None` means "run cargo
/// directly" (either no windows-msvc target, or the env already
/// matches).
#[cfg(target_os = "windows")]
fn resolve_msvc_plan(
    targets: &[&str],
) -> Result<Option<(std::path::PathBuf, &'static str)>, crate::CargoTruceError> {
    use msvc_env::Plan;

    let mut wrap: Option<(std::path::PathBuf, &'static str)> = None;
    for triple in targets {
        let Some(plan) = msvc_env::plan_for_target(triple) else {
            continue;
        };
        match plan {
            Plan::DirectOk => {}
            Plan::WrapVcvarsall {
                vcvarsall,
                arch_arg,
            } => match &wrap {
                None => wrap = Some((vcvarsall, arch_arg)),
                Some((_, prior_arg)) if *prior_arg == arch_arg => {}
                Some((_, prior_arg)) => {
                    return Err(format!(
                        "cargo-truce: this cargo invocation mixes `--target` triples \
                         that need different MSVC envs (`{prior_arg}` and `{arch_arg}`). \
                         Each arch needs its own `vcvarsall.bat` call - split this \
                         into one `cargo build` per arch."
                    )
                    .into());
                }
            },
            Plan::Unfixable { message } => return Err(message.into()),
        }
    }
    Ok(wrap)
}

/// Build a `.bat` that calls `vcvarsall.bat <arch_arg>` and then the
/// cargo command, then execute it via `cmd /c`. Env vars set on `cargo`
/// are inherited by `cmd.exe` and survive the vcvars call (vcvars only
/// rewrites `LIB`/`INCLUDE`/`PATH`).
#[cfg(target_os = "windows")]
fn run_via_vcvarsall(
    cargo: &Command,
    vcvarsall: &std::path::Path,
    arch_arg: &str,
    failure_label: &str,
) -> crate::Res {
    use std::fmt::Write as _;

    let mut bat = String::from("@echo off\r\n");
    let _ = writeln!(
        bat,
        "call \"{}\" {arch_arg} >nul || exit /b 1\r",
        vcvarsall.display(),
    );
    bat.push_str(&quote_command_for_bat(cargo));
    bat.push_str("\r\n");

    let bat_path = tmp_scripts().join(format!("truce_cargo_{arch_arg}.bat"));
    fs_ctx::write(&bat_path, bat)?;

    let mut driver = Command::new("cmd");
    driver.arg("/c").arg(&bat_path);
    // Preserve every env var the caller set on `cargo` (sccache wrapper,
    // per-target RUSTFLAGS, user vars). vcvarsall rewrites only the
    // MSVC-toolchain env vars, so these pass through to the cargo child.
    for (k, v) in cargo.get_envs() {
        match v {
            Some(v) => driver.env(k, v),
            None => driver.env_remove(k),
        };
    }
    let status = driver.status()?;
    if !status.success() {
        return Err(failure_label.into());
    }
    Ok(())
}

/// Format a `Command` (program + args) as a single line suitable for a
/// `.bat` file. Each arg is wrapped in double quotes. We never embed
/// double-quotes in argv internally, so simple wrapping is sufficient
/// and avoids the cmd.exe quoting maze.
#[cfg(target_os = "windows")]
fn quote_command_for_bat(cmd: &Command) -> String {
    let mut out = String::new();
    out.push('"');
    out.push_str(&cmd.get_program().to_string_lossy());
    out.push('"');
    for arg in cmd.get_args() {
        out.push(' ');
        out.push('"');
        out.push_str(&arg.to_string_lossy());
        out.push('"');
    }
    out
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
    let targets = extract_target_triples(base_args);
    for triple in &targets {
        ensure_rustup_target(triple)?;
    }
    let msvc_plan = resolve_msvc_plan(&targets)?;

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
    apply_target_cpu(&mut cmd, &targets);
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
    if let Some((vcvarsall, arch_arg)) = msvc_plan {
        return run_via_vcvarsall(&cmd, &vcvarsall, arch_arg, "cargo rustc failed");
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err("cargo rustc failed".into());
    }
    Ok(())
}

/// Extract every `--target <triple>` / `--target=<triple>` value from a
/// flat arg vector, preserving order. Used to pre-fetch rustup targets
/// and to scope per-target `RUSTFLAGS`.
fn extract_target_triples<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == "--target" {
            if let Some(t) = it.next() {
                out.push(*t);
            }
        } else if let Some(t) = a.strip_prefix("--target=") {
            out.push(t);
        }
    }
    out
}

/// Apply the active [`crate::util::TargetCpu`] choice to a `cargo build`
/// command. When `--target <triple>` is in play we set
/// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` per target so a multi-arch
/// invocation (lipo universal builds) can give each slice the right
/// flag; otherwise we set plain `RUSTFLAGS` since cargo's
/// per-target rustflags don't apply to host builds that omit
/// `--target`.
fn apply_target_cpu(cmd: &mut Command, targets: &[&str]) {
    use crate::util::resolve_target_cpu;

    if targets.is_empty() {
        if let Some(cpu) = resolve_target_cpu(truce_build::host_triple()) {
            append_rustflags_env(cmd, "RUSTFLAGS", &format!("-C target-cpu={cpu}"));
        }
        return;
    }
    for triple in targets {
        let Some(cpu) = resolve_target_cpu(triple) else {
            continue;
        };
        let var = cargo_target_rustflags_var(triple);
        append_rustflags_env(cmd, &var, &format!("-C target-cpu={cpu}"));
    }
}

/// Build the cargo-recognised env var name for per-target rustflags:
/// `aarch64-apple-darwin` -> `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS`.
fn cargo_target_rustflags_var(triple: &str) -> String {
    let normalised: String = triple
        .chars()
        .map(|c| {
            if c == '-' || c == '.' {
                '_'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect();
    format!("CARGO_TARGET_{normalised}_RUSTFLAGS")
}

/// Append `flag` to env var `var` (space-separated). Reads the prior
/// value from the process env so a user-set `RUSTFLAGS` is preserved
/// instead of clobbered.
fn append_rustflags_env(cmd: &mut Command, var: &str, flag: &str) {
    let prior = std::env::var(var).unwrap_or_default();
    let combined = if prior.is_empty() {
        flag.to_string()
    } else {
        format!("{prior} {flag}")
    };
    cmd.env(var, combined);
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
    cargo_build_multi_arch_with_profile(archs, base_args, dt, &build_profile_name())
}

/// Like `cargo_build_multi_arch`, but with an explicit profile instead
/// of the process-global one. Used to build distribution artifacts
/// (the standalone host) in `release` even when the global profile is
/// `shell` or `debug` - those iteration profiles apply to the plugin
/// binary, not the host app that wraps it.
#[cfg(target_os = "macos")]
pub(crate) fn cargo_build_multi_arch_with_profile(
    archs: &[MacArch],
    base_args: &[&str],
    dt: &str,
    profile: &str,
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
    cargo_build_with_profile(&[], &arg_refs, dt, profile)
}
