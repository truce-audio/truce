//! `cargo truce build` — produce per-format bundles in `target/bundles/`
//! without installing.
//!
//! Every format flag (`--clap` / `--vst3` / `--vst2` / `--lv2` / `--au2`
//! / `--au3` / `--aax`) produces a self-contained, signed bundle in
//! `target/bundles/`; `cargo truce install` then copies those bundles
//! to system paths.

#[cfg(target_os = "macos")]
use crate::commands::package::stage::stage_au2;
use crate::commands::package::stage::{lv2_slug, stage_clap, stage_lv2, stage_vst2, stage_vst3};
use crate::util::fs_ctx;
use crate::{Res, deployment_target, detect_default_features, load_config, project_root};

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
            "-p" => {
                i += 1;
                plugin_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-p requires a plugin crate name")?,
                );
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
        // Drop the logic profile into the workspace's
        // `<target>/.truce-build-config` sidecar instead of process
        // env. `truce-build`'s build script reads the file (with
        // `cargo:rerun-if-changed`) and re-emits it as
        // `cargo:rustc-env=TRUCE_LOGIC_PROFILE=...` so the shell
        // binary's runtime `option_env!` lookup keeps working.
        // Replaces the env-chain the audit flagged as fragile.
        crate::write_hot_reload_config(&crate::project_root(), logic_profile)?;
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
    let bundles_dir = crate::target_dir(&root).join("bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;

    let extra_features: Vec<&str> = if shell_mode { vec!["shell"] } else { vec![] };

    // --- Build dylibs per format ---
    //
    // Each format gets its own cargo build with `--features {format}`.
    // Because every build overwrites `target/release/lib{stem}.dylib`,
    // the helper immediately copies the output to a format-suffixed
    // path (`_clap`, `_vst3`, `_vst2`, ...) that the stage/install
    // steps read from. Platform gates (AU is macOS-only, AAX is
    // macOS/Windows + SDK-configured) live inside the helper.
    use super::build_dylibs::{BuildFormat, build_format_dylibs, build_logic_dylibs};
    let format_selection: &[(bool, BuildFormat)] = &[
        (clap, BuildFormat::Clap),
        (vst3, BuildFormat::Vst3),
        (vst2, BuildFormat::Vst2),
        (lv2, BuildFormat::Lv2),
        (au2, BuildFormat::Au2),
        (aax, BuildFormat::Aax),
    ];
    for &(selected, format) in format_selection {
        if selected {
            build_format_dylibs(format, &plugins, &extra_features, &config, &root, dt)?;
        }
    }

    // Shell mode: also build the per-plugin logic dylibs the shells
    // dlopen at runtime. Scoped per-plugin — `--workspace` would
    // rebuild every example + framework crate.
    if shell_mode {
        build_logic_dylibs(&plugins, logic_profile, dt)?;
    }

    // --- Stage each format's bundle into target/bundles/ ---
    let identity = config.macos.application_identity();
    for p in &plugins {
        if clap {
            stage_clap(&root, p, &bundles_dir, identity)?;
            crate::log_output(format!(
                "CLAP: {}",
                bundles_dir.join(format!("{}.clap", p.name)).display()
            ));
        }
        if vst3 {
            stage_vst3(&root, p, &config, &bundles_dir)?;
            crate::log_output(format!(
                "VST3: {}",
                bundles_dir.join(format!("{}.vst3", p.name)).display()
            ));
        }
        if vst2 {
            // macOS produces a `.vst` directory bundle; Linux/Windows
            // get a bare `.so` / `.dll` since neither uses a bundle.
            let staged = stage_vst2(&root, p, &config, &bundles_dir)?;
            crate::log_output(format!("VST2: {}", staged.display()));
        }
        if lv2 {
            stage_lv2(&root, p, &bundles_dir)?;
            let slug = lv2_slug(&p.name);
            crate::log_output(format!(
                "LV2:  {}",
                bundles_dir.join(format!("{slug}.lv2")).display()
            ));
        }
        if au2 {
            #[cfg(target_os = "macos")]
            {
                stage_au2(&root, p, &config, &bundles_dir)?;
                crate::log_output(format!(
                    "AU:   {}",
                    bundles_dir.join(format!("{}.component", p.name)).display()
                ));
            }
            // AU is macOS-only; the build phase already log_skip'd above
            // for non-macOS, so nothing to do here.
        }
    }

    // AU v3 has its own driver that builds Rust-framework + xcodebuild
    // + codesign inside-out and writes directly to target/bundles/.
    // Host arch only; universal builds are reserved for `package`.
    // macOS-only; the function returns a clear error on other platforms.
    if au3 {
        #[cfg(target_os = "macos")]
        {
            use crate::{MacArch, extract_team_id};
            // Same gate as install: ad-hoc / no-team-id makes AU v3
            // unbuildable. The "no team id" case is project-wide
            // (signing identity isn't per-plugin), so emit one skip
            // line and bypass the per-plugin loop.
            let sign_id = config.macos.application_identity();
            if extract_team_id(sign_id).is_empty() {
                crate::log_skip(
                    "AU v3: needs a Developer ID with team ID. \
                     Set [macos.signing].application_identity in truce.toml \
                     (e.g., \"Developer ID Application: Your Name (TEAMID)\"); \
                     ad-hoc signing (\"-\") is not supported for AU v3 appex bundles."
                        .to_string(),
                );
            } else {
                crate::commands::install::au_v3::emit_au_v3_bundle(
                    &root,
                    &config,
                    &plugins,
                    &[MacArch::host()],
                )?;
                for p in &plugins {
                    crate::log_output(format!(
                        "AU3:  {}",
                        bundles_dir
                            .join(format!("{}.app", p.au3_app_name()))
                            .display()
                    ));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        crate::log_skip(
            "AU v3: not supported on this platform. Audio Unit is macOS-only.".to_string(),
        );
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

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax]
                         [-p <crate>] [--shell] [--debug]

Build per-format bundles into target/bundles/ without installing.
Defaults to release; pass --debug for the cargo dev profile.
Defaults match `install`: when no format flags are passed, every
format in the project's default Cargo features is built.

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --vst2           VST2 only
  --lv2            LV2 only
  --au2            AU v2 only (.component, macOS only)
  --au3            AU v3 only (.appex inside .app, macOS only)
  --aax            AAX only (requires pre-built SDK + template)
  -p <crate>       Build only the plugin with this cargo crate name
  --shell          Build dynamic shells + per-plugin logic dylibs
  --debug          Cargo dev profile (faster compile, slower DSP)
  -h, --help       Show this message"
    );
}
