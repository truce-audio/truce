//! `cargo truce build` — produce CLAP bundles in `target/bundles/` without
//! installing.

use crate::{
    cargo_build, codesign_bundle, deployment_target, load_config, project_root,
    release_lib, PluginDef, Res,
};
use std::fs;
use std::process::Command;

pub(crate) fn cmd_build(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let mut plugin_filter: Option<String> = None;
    let mut dev_mode = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dev" => dev_mode = true,
            "-p" => {
                i += 1;
                plugin_filter = Some(args.get(i).cloned().ok_or("-p requires a suffix")?);
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    let plugins: Vec<&PluginDef> = if let Some(ref f) = plugin_filter {
        config.plugin.iter().filter(|p| p.suffix == *f).collect()
    } else {
        config.plugin.iter().collect()
    };

    if plugins.is_empty() {
        return Err("no matching plugins".into());
    }

    let bundles_dir = root.join("target/bundles");
    fs::create_dir_all(&bundles_dir)?;

    // Build CLAP + VST3 (default features)
    if dev_mode {
        eprintln!("Building (dev mode)...");
        for p in &plugins {
            cargo_build(&[], &["-p", &p.crate_name, "--features", "dev"], dt)?;
        }
        // Also build debug dylibs
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("--workspace");
        cmd.env("MACOSX_DEPLOYMENT_TARGET", dt);
        cmd.status()?;
    } else {
        eprintln!("Building...");
        cargo_build(&[], &[], dt)?;
    }

    // Create CLAP bundles
    for p in &plugins {
        let src = release_lib(&root, &p.dylib_stem());
        if !src.exists() {
            continue;
        }

        #[cfg(target_os = "macos")]
        {
            let clap_dir = bundles_dir.join(format!("{}.clap/Contents/MacOS", p.name));
            fs::create_dir_all(&clap_dir)?;
            fs::copy(&src, clap_dir.join(&p.name))?;
            let bundle = bundles_dir.join(format!("{}.clap", p.name));
            codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
            eprintln!("  CLAP: {}", bundle.display());
        }

        #[cfg(not(target_os = "macos"))]
        {
            let dst = bundles_dir.join(format!("{}.clap", p.name));
            fs::copy(&src, &dst)?;
            eprintln!("  CLAP: {}", dst.display());
        }
    }

    eprintln!("Bundles in {}", bundles_dir.display());
    Ok(())
}
