//! Per-format cdylib build helper, shared by `cargo truce build` and
//! `cargo truce install`.
//!
//! Both commands run an identical sequence for every selected format:
//!
//! 1. Skip on unsupported platforms (AU is macOS-only, AAX is macOS /
//!    Windows) with a single `log_skip` line.
//! 2. For AAX, gate on a configured SDK path (project-wide check, not
//!    per-plugin — emit one skip line and bypass the cargo build loop
//!    when missing).
//! 3. One `cargo build -p a -p b -p c …` per format batch with the
//!    format's feature set. No per-plugin env vars: per-format display
//!    names travel with `PluginInfo` (baked by `truce::plugin_info!`)
//!    and AU class names are registered at runtime via `objc2`.
//! 4. Copy the produced `lib<stem>.<dylib-ext>` to a format-suffixed
//!    path (`<stem>_clap`, `<stem>_vst3`, …) so the next format build
//!    doesn't overwrite the previous one (every plugin's cdylib lands
//!    at the same canonical cargo path).
//! 5. For AAX, also call `emit_aax_bundle` to assemble the `.aaxplugin`
//!    that the install / package paths consume.

use crate::util::fs_ctx;
use crate::{Config, PluginDef, Res, cargo_build, release_lib_for_target};
use std::path::Path;

/// One of the per-format cdylib targets the build / install pipelines
/// produce. Encodes the cargo feature flag, the format-suffix used in
/// the workspace target dir, and the platform-gate behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildFormat {
    Clap,
    Vst3,
    Vst2,
    Lv2,
    Au2,
    Aax,
}

impl BuildFormat {
    /// Cargo feature flag passed to `cargo build --features <feature>`.
    fn feature(self) -> &'static str {
        match self {
            BuildFormat::Clap => "clap",
            BuildFormat::Vst3 => "vst3",
            BuildFormat::Vst2 => "vst2",
            BuildFormat::Lv2 => "lv2",
            BuildFormat::Au2 => "au",
            BuildFormat::Aax => "aax",
        }
    }

    /// Human-facing name used in the "Building <label>..." banner.
    fn label(self) -> &'static str {
        match self {
            BuildFormat::Clap => "CLAP",
            BuildFormat::Vst3 => "VST3",
            BuildFormat::Vst2 => "VST2",
            BuildFormat::Lv2 => "LV2",
            BuildFormat::Au2 => "AU v2",
            BuildFormat::Aax => "AAX",
        }
    }

    /// Format-suffix appended to the dylib stem on copy. Keeps each
    /// format's binary distinct in `target/<profile>/` so subsequent
    /// per-format builds don't overwrite earlier ones.
    fn dylib_suffix(self) -> &'static str {
        match self {
            BuildFormat::Clap => "_clap",
            BuildFormat::Vst3 => "_vst3",
            BuildFormat::Vst2 => "_vst2",
            BuildFormat::Lv2 => "_lv2",
            BuildFormat::Au2 => "_au",
            BuildFormat::Aax => "_aax",
        }
    }
}

/// Returns a skip-reason string if AAX cannot be built on this host —
/// either the platform isn't supported (Linux) or the SDK isn't
/// configured (mac/Windows without `AAX_SDK_PATH` set in
/// `.cargo/config.toml`'s `[env]` table or the shell env). `None`
/// means AAX is buildable.
// On Linux this always returns `Some(...)` (AAX isn't supported), but
// callers consume an `Option<String>` so they can render "skipped"
// uniformly across platforms.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(clippy::unnecessary_wraps)
)]
fn aax_skip_reason(_config: &Config) -> Option<String> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some("AAX: not supported on this platform. Use macOS or Windows to build AAX.".to_string())
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        if crate::resolve_aax_sdk_path().is_some() {
            return None;
        }
        Some(
            "AAX: SDK not configured. Set AAX_SDK_PATH in .cargo/config.toml [env]."
                .to_string(),
        )
    }
}

/// Build cdylibs for one format across `plugins`. Centralizes the
/// per-format banner, env-var assembly, cargo build, copy-to-suffix,
/// and (for AAX) `emit_aax_bundle` step that `cargo truce build` and
/// `cargo truce install` both used to inline six times each.
///
/// Platform gates:
/// - `Au2`: macOS only. Other platforms emit `crate::log_skip` and
///   return `Ok(())` so callers don't need cfg blocks at the call site.
/// - `Aax`: macOS / Windows only. Linux emits `log_skip`.
/// - `Aax` SDK: macOS / Windows with no SDK configured emits one
///   project-wide `log_skip` and skips the build loop entirely.
///
/// `extra_features` are appended to the format's own feature (used by
/// shell-mode builds to add `"shell"`); empty otherwise.
///
/// `target` selects the cargo `--target <triple>`; `None` means "build
/// for the host" and outputs land at `target/release/`. With a target
/// set, outputs land at `target/<triple>/release/` and downstream
/// stagers must read from there too.
pub(crate) fn build_format_dylibs(
    format: BuildFormat,
    plugins: &[&PluginDef],
    extra_features: &[&str],
    config: &Config,
    root: &Path,
    deployment_target: &str,
    target: Option<&str>,
) -> Res {
    // Platform / SDK gates first — every gate emits a single skip line
    // and exits cleanly, so the caller's "if format_selected { build }"
    // doesn't need its own cfg arms.
    match format {
        BuildFormat::Au2 => {
            #[cfg(not(target_os = "macos"))]
            {
                crate::log_skip(
                    "AU v2: not supported on this platform. Audio Unit is macOS-only.".to_string(),
                );
                return Ok(());
            }
        }
        BuildFormat::Aax => {
            if let Some(reason) = aax_skip_reason(config) {
                crate::log_skip(reason);
                return Ok(());
            }
        }
        _ => {}
    }

    // Build banner. Mirrors the per-format pre-loop log line each
    // command used to emit; the shell-mode label gets the extra-feature
    // list parenthesised (e.g. "Building CLAP (shell)...").
    if extra_features.is_empty() {
        crate::vprintln!("Building {}...", format.label());
    } else {
        let extras = extra_features.join(" + ");
        crate::vprintln!("Building {} ({extras})...", format.label());
    }

    let mut format_features: Vec<&str> = vec![format.feature()];
    format_features.extend_from_slice(extra_features);
    let combined = format_features.join(",");

    // No per-plugin or per-format env vars left — display-name
    // overrides moved into `PluginInfo` (`truce::plugin_info!`), and
    // the v2/v3 distinction in `truce-au`'s name resolution went
    // away once the v3 host's display name was traced to the appex
    // plist rather than the framework dylib. Every plugin in the
    // batch can share one cargo invocation with empty env.
    let env_pairs: &[(&str, &str)] = &[];

    // Single `cargo build -p a -p b -p c …` for every plugin in this
    // format batch. Two wins: cargo pays the dep-graph-resolve and
    // process-startup cost once instead of N times, and codegen for
    // the leaf example crates parallelises across the plugins (cargo
    // can't parallelise across separate invocations).
    let mut cargo_args: Vec<String> = Vec::with_capacity(plugins.len() * 2 + 5);
    for p in plugins {
        cargo_args.push("-p".into());
        cargo_args.push(p.crate_name.clone());
    }
    cargo_args.push("--no-default-features".into());
    cargo_args.push("--features".into());
    cargo_args.push(combined.clone());
    if let Some(t) = target {
        cargo_args.push("--target".into());
        cargo_args.push(t.into());
    }
    let cargo_arg_refs: Vec<&str> = cargo_args.iter().map(String::as_str).collect();
    cargo_build(env_pairs, &cargo_arg_refs, deployment_target)?;

    // Post-build per-plugin staging: copy the produced `.dylib` to
    // its format-suffixed name and (for AAX) assemble the
    // `.aaxplugin` bundle. Cheap I/O, kept as a separate pass so
    // the cargo invocation above doesn't have to know about it.
    for p in plugins {
        let src = release_lib_for_target(root, &p.dylib_stem(), target);
        let dst = release_lib_for_target(
            root,
            &format!("{}{}", p.dylib_stem(), format.dylib_suffix()),
            target,
        );
        // CLAP / VST3 historically guarded the copy with `if src.exists()`
        // because a feature-flagged plugin can legitimately produce no
        // output for a format it doesn't support; preserve that for
        // every format so the loop is uniformly tolerant.
        if src.exists() {
            fs_ctx::copy(&src, &dst)?;
        }

        // AAX additionally assembles the `.aaxplugin` bundle in
        // `target/bundles/` here — both install (which then copies the
        // bundle to /Library/...) and build (which leaves it in
        // `target/bundles/`) want the bundle assembled.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if format == BuildFormat::Aax {
            crate::commands::install::aax::emit_aax_bundle(root, p, config, false)?;
        }
    }

    Ok(())
}

/// Build the per-plugin "logic" dylib (the cdylib the shell-mode shell
/// dlopens at runtime). Profile is `release` by default; `--debug`
/// flips it to cargo's debug profile; custom profiles fall through to
/// `cargo build --profile <name>`. Scoped per-plugin so a fresh
/// checkout doesn't rebuild every framework crate.
///
/// After every successful build, writes the sidecar that the shell
/// binary reads at runtime to find this dylib — see
/// [`write_shell_sidecar`].
pub(crate) fn build_logic_dylibs(
    plugins: &[&PluginDef],
    logic_profile: &str,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] deployment_target: &str,
) -> Res {
    use std::process::Command;

    let root = crate::project_root();
    for p in plugins {
        crate::vprintln!(
            "Building {} logic dylib for {}...",
            logic_profile,
            p.crate_name
        );
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("-p").arg(&p.crate_name);
        match logic_profile {
            "debug" => {} // cargo default
            "release" => {
                cmd.arg("--release");
            }
            other => {
                cmd.arg("--profile").arg(other);
            }
        }
        #[cfg(target_os = "macos")]
        cmd.env("MACOSX_DEPLOYMENT_TARGET", deployment_target);
        if let Some(wrapper) = crate::util::sccache_wrapper() {
            cmd.env("RUSTC_WRAPPER", wrapper);
        }
        let status = cmd.status()?;
        if !status.success() {
            return Err(format!("{logic_profile} build of {} failed", p.crate_name).into());
        }

        write_shell_sidecar(&root, &p.crate_name, logic_profile)?;
    }
    Ok(())
}

/// Resolve and write `~/.truce/shell/<crate>.path` so the installed
/// shell binary (loaded by the DAW) can find the logic dylib at
/// runtime. Writes the absolute, canonicalized path of the logic
/// dylib so the runtime read site doesn't have to re-resolve
/// `CARGO_TARGET_DIR` / `[build].target-dir` from a context that
/// lacks those signals.
///
/// Atomic write: lands the contents at a `<sidecar>.tmp.<pid>`
/// sibling and renames it into place. A `^C` or power loss between
/// the temp write and the rename leaves the prior sidecar intact;
/// the half-written temp file is harmless and gets overwritten on
/// the next build.
fn write_shell_sidecar(root: &std::path::Path, crate_name: &str, logic_profile: &str) -> Res {
    use std::fs;

    let stem = crate_name.replace('-', "_");
    let dylib_path = truce_build::target_dir(root)
        .join(logic_profile)
        .join(crate::util::shared_lib_name(&stem));
    let canonical = dylib_path.canonicalize().unwrap_or(dylib_path);

    let sidecar =
        truce_utils::shell_sidecar::sidecar_path(crate_name).ok_or_else(|| -> crate::BoxErr {
            "could not resolve $HOME (or %USERPROFILE% on Windows) for the \
         shell sidecar — the runtime needs $HOME to locate the logic \
         dylib without it"
                .into()
        })?;
    if let Some(parent) = sidecar.parent() {
        fs::create_dir_all(parent).map_err(|e| -> crate::BoxErr {
            format!("failed to create {}: {e}", parent.display()).into()
        })?;
    }
    let tmp = sidecar.with_extension(format!("path.tmp.{}", std::process::id()));
    fs::write(&tmp, format!("{}\n", canonical.display())).map_err(|e| -> crate::BoxErr {
        format!("failed to write shell sidecar {}: {e}", tmp.display()).into()
    })?;
    // `fs::rename` is atomic on POSIX (rename(2)) and on Windows
    // (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`). Same parent
    // directory guarantees same filesystem.
    fs::rename(&tmp, &sidecar).map_err(|e| -> crate::BoxErr {
        let _ = fs::remove_file(&tmp);
        format!(
            "failed to rename {} -> {}: {e}",
            tmp.display(),
            sidecar.display()
        )
        .into()
    })?;
    crate::vprintln!(
        "Wrote shell sidecar {} -> {}",
        sidecar.display(),
        canonical.display(),
    );
    Ok(())
}
