//! `cargo truce build` — produce per-format bundles in `target/bundles/`
//! without installing.
//!
//! Every format flag (`--clap` / `--vst3` / `--vst2` / `--lv2` / `--au2`
//! / `--au3` / `--aax`) produces a self-contained, signed bundle in
//! `target/bundles/`; `cargo truce install` then copies those bundles
//! to system paths.

use super::build_dylibs::{BuildFormat, build_format_dylibs, build_logic_dylibs};
#[cfg(target_os = "macos")]
use crate::commands::package::stage::stage_au2;
use crate::commands::package::stage::{lv2_slug, stage_clap, stage_lv2, stage_vst2, stage_vst3};
use crate::util::fs_ctx;
use crate::{Res, deployment_target, detect_default_features, load_config, project_root};
use std::path::PathBuf;
use truce_build::{BundleEntry, BundleManifest};

struct TargetPlan<'a> {
    target: Option<&'a str>,
    stage_dir: PathBuf,
    triple: String,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_build(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut lv2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut shell_mode = false;
    let mut debug = false;
    let mut plugin_filter: Option<String> = None;
    let mut targets: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--vst2" => vst2 = true,
            "--lv2" => lv2 = true,
            "--au2" => au2 = true,
            "--au3" => au3 = true,
            "--aax" => aax = true,
            "--shell" => shell_mode = true,
            "--debug" => debug = true,
            "-p" => plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string()),
            "--target" => {
                targets.push(crate::util::arg_value(args, &mut i, "--target")?.to_string());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    // No format flags → enable every format in the project's default
    // features, mirroring `install`'s discovery rule.
    if !clap && !vst3 && !vst2 && !lv2 && !au2 && !au3 && !aax {
        let available = detect_default_features();
        clap = available.contains("clap");
        vst3 = available.contains("vst3");
        vst2 = available.contains("vst2");
        lv2 = available.contains("lv2");
        // AU is macOS-only at runtime, but flip the flags on every platform
        // so the build path can emit per-plugin skip lines for Linux /
        // Windows users with `"au"` in their `[features].default`.
        au2 = available.contains("au");
        au3 = available.contains("au");
        aax = available.contains("aax");
    }

    let plugins = super::pick_plugins(&config, plugin_filter.as_deref())?;
    if plugins.is_empty() {
        return Err("no matching plugins".into());
    }

    // In shell mode `--debug` selects the *logic* profile (the dylib
    // the shell dlopens at runtime). The shell itself goes to
    // `target/shell/` via the custom `[profile.shell]`; default logic
    // profile is release for better DSP perf, debug for fast iteration.
    let logic_profile = if debug { "debug" } else { "release" };

    if shell_mode {
        // Bail early if the user's Cargo.toml is missing
        // `[profile.shell]` — clearer than cargo's downstream
        // "profile `shell` is not declared" message.
        crate::verify_shell_profile_declared()?;
        crate::set_build_profile("shell");
    } else {
        crate::set_debug_profile(debug);
    }

    // AU v3 + shell is unreliable due to the appex sandbox. Same
    // warning as `cargo truce install --shell --au3`.
    if shell_mode && au3 && cfg!(target_os = "macos") {
        eprintln!(
            "note: AU v3 + --shell is unreliable. The appex sandbox blocks dlopen of \
             target/<profile>/lib<crate>.dylib, so hot-reload won't fire. Use --au2 \
             for hot-reload iteration."
        );
    }

    let root = project_root();
    let dt = &deployment_target();
    let bundles_dir = truce_build::target_dir(&root).join("bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;

    let extra_features: Vec<&str> = if shell_mode { vec!["shell"] } else { vec![] };

    // Per-target staging plan. When the user passes one or more
    // `--target <triple>` flags, each target builds + stages into
    // `target/bundles/<triple>/`. With no `--target`, we keep the
    // historical flat layout (`target/bundles/<filename>`) so existing
    // macOS / Windows workflows that inspect that directory don't
    // change shape.
    let target_plans: Vec<TargetPlan<'_>> = if targets.is_empty() {
        vec![TargetPlan {
            target: None,
            stage_dir: bundles_dir.clone(),
            triple: truce_build::host_triple().to_string(),
        }]
    } else {
        targets
            .iter()
            .map(|t| TargetPlan {
                target: Some(t.as_str()),
                stage_dir: bundles_dir.join(t),
                triple: t.clone(),
            })
            .collect()
    };

    // --- Build dylibs per format ---
    //
    // Each format gets its own cargo build with `--features {format}`.
    // Because every build overwrites `target/<triple>/release/lib{stem}.dylib`,
    // the helper immediately copies the output to a format-suffixed
    // path (`_clap`, `_vst3`, `_vst2`, ...) that the stage/install
    // steps read from. Platform gates (AU is macOS-only, AAX is
    // macOS/Windows + SDK-configured) live inside the helper.
    let format_selection: &[(bool, BuildFormat)] = &[
        (clap, BuildFormat::Clap),
        (vst3, BuildFormat::Vst3),
        (vst2, BuildFormat::Vst2),
        (lv2, BuildFormat::Lv2),
        (au2, BuildFormat::Au2),
        (aax, BuildFormat::Aax),
    ];

    let identity = crate::application_identity();
    let entry = |p: &crate::PluginDef, format: &str, filename: String| BundleEntry {
        plugin_crate: p.crate_name.clone(),
        plugin_name: p.name.clone(),
        plugin_bundle_id: p.bundle_id.clone(),
        format: format.to_string(),
        filename,
    };

    for plan in &target_plans {
        fs_ctx::create_dir_all(&plan.stage_dir)?;

        for &(selected, format) in format_selection {
            if selected {
                build_format_dylibs(
                    format,
                    &plugins,
                    &extra_features,
                    &config,
                    &root,
                    dt,
                    plan.target,
                )?;
            }
        }

        // Shell mode: also build the per-plugin logic dylibs the shells
        // dlopen at runtime. Host-only — shell relies on dlopen of a
        // freshly built binary, same constraint as LV2.
        if shell_mode && plan.target.is_none() {
            build_logic_dylibs(&plugins, logic_profile, dt)?;
        }

        // --- Stage each format's bundle into the per-target dir ---
        //
        // Each successful stage_* call appends a BundleEntry to
        // `produced`; we serialize the lot into `manifest.toml` at
        // the end so `cargo truce package` has an explicit list of
        // what to ship.
        let mut produced: Vec<BundleEntry> = Vec::new();
        for p in &plugins {
            if clap {
                stage_clap(&root, p, &config, &plan.stage_dir, &identity, plan.target)?;
                let filename = format!("{}.clap", p.name);
                crate::log_output(format!(
                    "CLAP: {}",
                    plan.stage_dir.join(&filename).display()
                ));
                produced.push(entry(p, "clap", filename));
            }
            if vst3 {
                stage_vst3(&root, p, &config, &plan.stage_dir, plan.target)?;
                let filename = format!("{}.vst3", p.name);
                crate::log_output(format!(
                    "VST3: {}",
                    plan.stage_dir.join(&filename).display()
                ));
                produced.push(entry(p, "vst3", filename));
            }
            if vst2 {
                // macOS produces a `.vst` directory bundle; Linux/Windows
                // get a bare `.so` / `.dll` since neither uses a bundle.
                let staged = stage_vst2(&root, p, &config, &plan.stage_dir, plan.target)?;
                crate::log_output(format!("VST2: {}", staged.display()));
                let filename = staged.file_name().map_or_else(
                    || format!("{}.vst", p.name),
                    |n| n.to_string_lossy().into_owned(),
                );
                produced.push(entry(p, "vst2", filename));
            }
            if lv2 {
                stage_lv2(
                    &root,
                    p,
                    &plan.stage_dir,
                    &crate::application_identity(),
                    plan.target,
                )?;
                let slug = lv2_slug(&p.name);
                let filename = format!("{slug}.lv2");
                crate::log_output(format!(
                    "LV2:  {}",
                    plan.stage_dir.join(&filename).display()
                ));
                produced.push(entry(p, "lv2", filename));
            }
            if au2 {
                #[cfg(target_os = "macos")]
                {
                    // AU2 is macOS-only and only fires for host targets;
                    // cross-target macOS builds (e.g. x86_64 from arm64)
                    // are still macOS-host so the existing helper works.
                    let _ = plan; // referenced via stage_dir below
                    stage_au2(&root, p, &config, &plan.stage_dir)?;
                    let filename = format!("{}.component", p.name);
                    crate::log_output(format!(
                        "AU:   {}",
                        plan.stage_dir.join(&filename).display()
                    ));
                    produced.push(entry(p, "au2", filename));
                }
                // AU is macOS-only; the build phase already log_skip'd
                // above for non-macOS, so nothing to do here.
            }
        }

        // AU v3 has its own driver that builds Rust-framework +
        // xcodebuild + codesign inside-out. Host arch only; the
        // universal flow lives in `cargo truce package`. AU v3 only
        // makes sense when this plan is the host build (no --target),
        // so we gate on `plan.target.is_none()`. Cross-target users
        // get a clear log_skip line per target.
        if au3 {
            if plan.target.is_some() {
                crate::log_skip(format!(
                    "AU v3 ({}): cross-target builds are unsupported (AU v3 wraps a \
                     macOS host xcodebuild step).",
                    plan.triple,
                ));
            } else {
                #[cfg(target_os = "macos")]
                {
                    use crate::{MacArch, extract_team_id};
                    // Same gate as install: ad-hoc / no-team-id makes
                    // AU v3 unbuildable. The "no team id" case is
                    // project-wide (signing identity isn't per-plugin),
                    // so emit one skip line and bypass the per-plugin
                    // loop.
                    let sign_id = crate::application_identity();
                    if extract_team_id(&sign_id).is_empty() {
                        crate::log_skip(
                            "AU v3: needs a Developer ID with team ID. \
                             Set TRUCE_SIGNING_IDENTITY in .cargo/config.toml \
                             [env] (e.g., \"Developer ID Application: Your Name (TEAMID)\"); \
                             ad-hoc signing (\"-\") is not supported for AU v3 appex bundles."
                                .to_string(),
                        );
                    } else {
                        crate::commands::install::au_v3::emit_au_v3_bundle(
                            &root,
                            &config,
                            &plugins,
                            &[MacArch::host()],
                            // `cargo truce build --au3` doesn't run AU2,
                            // so there's no AU artifact to reuse.
                            false,
                        )?;
                        for p in &plugins {
                            let filename = format!("{}.app", p.au3_app_name());
                            crate::log_output(format!(
                                "AU3:  {}",
                                plan.stage_dir.join(&filename).display()
                            ));
                            produced.push(entry(p, "au3", filename));
                        }
                    }
                }
                #[cfg(not(target_os = "macos"))]
                crate::log_skip(
                    "AU v3: not supported on this platform. Audio Unit is macOS-only.".to_string(),
                );
            }
        }

        // Persist the manifest for this target before printing
        // summaries so a post-build `cargo truce package` always sees
        // the latest entries. Merge with any existing manifest in the
        // same dir so partial builds (`--clap` then `--vst3`) accumulate.
        write_bundle_manifest(&plan.stage_dir, &plan.triple, shell_mode, debug, &produced)?;
    }

    let outputs = crate::take_outputs();
    if !outputs.is_empty() {
        eprintln!("\nBuilt:");
        for line in outputs {
            eprintln!("  {line}");
        }
    }
    let skipped = crate::take_skipped();
    if !skipped.is_empty() {
        eprintln!("\nSkipped:");
        for line in skipped {
            eprintln!("  {line}");
        }
    }
    eprintln!("\nBundles in {}", bundles_dir.display());
    Ok(())
}

fn write_bundle_manifest(
    bundles_dir: &std::path::Path,
    target_triple: &str,
    shell_mode: bool,
    debug: bool,
    produced: &[BundleEntry],
) -> Res {
    let profile = if shell_mode {
        "shell"
    } else if debug {
        "debug"
    } else {
        "release"
    };
    let mut next = BundleManifest::new(target_triple, profile);
    next.bundles = produced.to_vec();

    // Missing manifest is the common case (first build); start empty.
    // Corrupt/incompatible is treated the same — the manifest is a
    // derived artifact, not user data, so replacing beats failing.
    let mut manifest = match BundleManifest::load_if_present(bundles_dir) {
        Ok(Some(existing)) => existing,
        Ok(None) | Err(_) => BundleManifest::new(target_triple, profile),
    };
    manifest.merge(next);
    manifest.save(bundles_dir)?;
    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax]
                         [-p <crate>] [--target <triple>]... [--shell] [--debug]

Build per-format bundles into target/bundles/ without installing.
Defaults to release; pass --debug for the cargo dev profile.
Defaults match `install`: when no format flags are passed, every
format in the project's default Cargo features is built.

Pass --target <triple> (repeatable) to cross-build for a specific
cargo target; bundles land in target/bundles/<triple>/. Without
--target, the host build lands in target/bundles/ (flat layout, as
before).

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --vst2           VST2 only
  --lv2            LV2 only
  --au2            AU v2 only (.component, macOS only)
  --au3            AU v3 only (.appex inside .app, macOS only)
  --aax            AAX only (requires pre-built SDK + template)
  -p <crate>       Build only the plugin with this cargo crate name
  --target <triple>
                   Cargo target triple (e.g. aarch64-unknown-linux-gnu).
                   Repeatable. Outputs land at target/bundles/<triple>/.
                   Shell-mode is host-only; cross-target invocations
                   log_skip it.
  --shell          Build dynamic shells + per-plugin logic dylibs
  --debug          Cargo dev profile (faster compile, slower DSP)
  -h, --help       Show this message"
    );
}
