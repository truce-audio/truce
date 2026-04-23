//! `cargo truce run` — build and launch a plugin's `--features standalone`
//! binary.

use crate::{cargo_build, deployment_target, load_config, project_root, Res};
use std::process::Command;

pub(crate) fn cmd_run(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let mut plugin_filter: Option<String> = None;
    let mut extra_args: Vec<String> = Vec::new();
    let mut past_separator = false;
    let mut i = 0;
    while i < args.len() {
        if past_separator {
            extra_args.push(args[i].clone());
        } else {
            match args[i].as_str() {
                "-p" => {
                    i += 1;
                    plugin_filter = Some(args.get(i).cloned().ok_or("-p requires a suffix")?);
                }
                "--" => past_separator = true,
                other => return Err(format!("unknown flag: {other}").into()),
            }
        }
        i += 1;
    }

    let plugin = if let Some(ref f) = plugin_filter {
        config.plugin.iter().find(|p| p.suffix == *f)
            .ok_or_else(|| format!("no plugin with suffix '{f}'"))?
    } else {
        config.plugin.first().ok_or("no plugins in truce.toml")?
    };

    // Build with standalone feature
    eprintln!("Building {} standalone...", plugin.name);
    cargo_build(
        &[],
        &["-p", &plugin.crate_name, "--features", "standalone"],
        dt,
    )?;

    // Find the standalone binary
    let bin_name = format!("{}-standalone", plugin.suffix);
    let bin_path = root.join(format!("target/release/{bin_name}"));
    if !bin_path.exists() {
        return Err(format!(
            "standalone binary not found at {}. \
             Does your plugin have a [[bin]] target named '{bin_name}'?",
            bin_path.display()
        ).into());
    }

    eprintln!("Running {}...", bin_path.display());
    let status = Command::new(&bin_path)
        .args(&extra_args)
        .status()?;

    if !status.success() {
        return Err(format!("{} exited with {status}", bin_path.display()).into());
    }
    Ok(())
}
