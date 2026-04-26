//! `cargo truce screenshot` — drive a plugin's editor headlessly
//! and save a PNG.
//!
//! Usage: `cargo truce screenshot -p <crate> [--name <name>]`. With no
//! `-p`, screenshots every plugin in `truce.toml`.
//!
//! Implementation: every plugin built with `truce::plugin!` exports a
//! hidden `extern "C" fn __truce_screenshot(...)` symbol into its
//! cdylib (the same artifact the CLAP/VST3 wrappers use). This command
//! builds the cdylib, `dlopen`s it, and calls the symbol.

use crate::{cargo_build, deployment_target, load_config, project_root, Res};
use std::path::{Path, PathBuf};

/// Maximum byte length of a returned PNG path. 4 KiB is well over any
/// realistic filesystem path; if a path actually exceeds this, the
/// FFI export reports the needed length and we surface a truncation
/// error rather than silently writing a half-path.
const PATH_BUF_CAP: usize = 4096;

type ScreenshotFn = unsafe extern "C" fn(*const u8, usize, *mut u8, usize) -> usize;

pub(crate) fn cmd_screenshot(args: &[String]) -> Res {
    let mut plugin_filter: Option<String> = None;
    let mut name_override: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                plugin_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-p requires a plugin crate name")?,
                );
            }
            "--name" => {
                i += 1;
                name_override = Some(args.get(i).cloned().ok_or("--name requires a value")?);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    let config = load_config()?;
    let plugins: Vec<_> = match &plugin_filter {
        Some(f) => {
            let p = config
                .plugin
                .iter()
                .find(|p| p.crate_name == *f)
                .ok_or_else(|| {
                    format!(
                        "No plugin with crate name '{f}'. Available: {}",
                        config
                            .plugin
                            .iter()
                            .map(|p| p.crate_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            vec![p]
        }
        None => config.plugin.iter().collect(),
    };

    if plugins.is_empty() {
        return Err("no plugins in truce.toml".into());
    }

    let dt = &deployment_target();
    let root = project_root();

    for plugin in plugins {
        let default_name = format!("{}_screenshot", plugin.bundle_id);
        let name = name_override.as_deref().unwrap_or(&default_name);

        eprintln!("Building {} cdylib...", plugin.name);
        // No format features needed — the screenshot symbol is emitted
        // unconditionally by `truce::plugin!`. Skip CLAP/VST3/etc.
        // compilation for a faster build.
        cargo_build(
            &[],
            &["-p", &plugin.crate_name, "--no-default-features", "--lib"],
            dt,
        )?;

        let lib_path = cdylib_path(&root, &plugin.crate_name);
        if !lib_path.exists() {
            return Err(format!(
                "cdylib not found at {}. Plugin must declare \
                 `crate-type = [\"cdylib\", \"rlib\"]` in its [lib] section.",
                lib_path.display()
            )
            .into());
        }

        eprintln!("Rendering {} → {name}.png", plugin.name);
        let path = unsafe { call_screenshot(&lib_path, name)? };
        eprintln!("  → {path}");
    }
    Ok(())
}

/// Resolve `target/release/lib<crate>.<ext>` for the host platform.
/// Cargo replaces `-` with `_` in the crate name when forming the
/// shared-library filename.
fn cdylib_path(root: &Path, crate_name: &str) -> PathBuf {
    let normalized = crate_name.replace('-', "_");
    let release = root.join("target").join("release");
    if cfg!(target_os = "macos") {
        release.join(format!("lib{normalized}.dylib"))
    } else if cfg!(target_os = "windows") {
        release.join(format!("{normalized}.dll"))
    } else {
        release.join(format!("lib{normalized}.so"))
    }
}

/// dlopen the cdylib, look up `__truce_screenshot`, call it, and
/// return the saved PNG path as a `String`.
///
/// # Safety
/// The library at `lib_path` must export the symbol with the FFI
/// signature emitted by the `truce::plugin!` macro. Plugins built
/// from any in-tree truce version satisfy this.
unsafe fn call_screenshot(lib_path: &Path, name: &str) -> Result<String, crate::BoxErr> {
    let lib = libloading::Library::new(lib_path)
        .map_err(|e| format!("failed to dlopen {}: {e}", lib_path.display()))?;
    let screenshot: libloading::Symbol<ScreenshotFn> =
        lib.get(b"__truce_screenshot\0").map_err(|e| {
            format!(
                "{}: __truce_screenshot symbol not found ({e}). \
             Was this plugin built with `truce::plugin!{{ ... }}`?",
                lib_path.display()
            )
        })?;

    let name_bytes = name.as_bytes();
    let mut out_buf = vec![0u8; PATH_BUF_CAP];
    let written = screenshot(
        name_bytes.as_ptr(),
        name_bytes.len(),
        out_buf.as_mut_ptr(),
        out_buf.len(),
    );
    if written > out_buf.len() {
        return Err(format!(
            "screenshot path of {written} bytes exceeds the {PATH_BUF_CAP}-byte buffer"
        )
        .into());
    }
    out_buf.truncate(written);
    String::from_utf8(out_buf).map_err(|e| format!("non-UTF8 path returned: {e}").into())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce screenshot [-p <crate>] [--name <name>]

Render a plugin's editor headlessly and save the PNG to
target/screenshots/<name>.png.

Options:
  -p <crate>     plugin crate name (default: every plugin in truce.toml)
  --name <name>  output filename stem (default: <bundle_id>_screenshot)

Builds each plugin's cdylib (`cargo build --release --no-default-features
--lib`), dlopens it, and calls the `__truce_screenshot` symbol exported
by the `truce::plugin!` macro. No per-plugin scaffolding required."
    );
}
