//! `cargo truce screenshot` — drive a plugin's editor headlessly
//! and save a PNG.
//!
//! Self-contained: works on any crate built with `truce::plugin!`,
//! whether or not it has any tests. The CLI dlopens the plugin's
//! cdylib, optionally loads a `.pluginstate` blob, calls the hidden
//! `__truce_screenshot` FFI, and writes the result to a path the
//! user picks (or a sensible default).
//!
//! Flags:
//! - `-p <crate>` — pick one plugin from a multi-plugin truce.toml.
//! - `--out <path>` — explicit output path (CWD-relative, or absolute).
//! - `--name <name>` — `<crate>/screenshots/<name>.png` shortcut.
//! - `--state <path>` — load a `.pluginstate` blob before rendering.
//!   Path is CWD-relative or absolute.
//! - `--check` — diff against the existing baseline; exit non-zero
//!   on regression. Same comparator semantics as the
//!   `truce_test::ScreenshotTest` runtime (pass-on-non-reference-OS
//!   via `TRUCE_SCREENSHOT_REFERENCE_OS`).
//! - `--debug` — cargo dev profile (faster compile).

use crate::{Res, cargo_build, cargo_build_debug, deployment_target, load_config, project_root};
use std::path::{Path, PathBuf};

/// FFI signature emitted by `truce::plugin!`'s `__truce_screenshot`.
/// `(state_ptr, state_len, out_path_ptr, out_path_len) -> u32` — 0
/// on success, non-zero on failure (logged to stderr by the plugin).
type ScreenshotFn = unsafe extern "C" fn(*const u8, usize, *const u8, usize) -> u32;

pub(crate) fn cmd_screenshot(args: &[String]) -> Res {
    let mut plugin_filter: Option<String> = None;
    let mut name_override: Option<String> = None;
    let mut out_override: Option<PathBuf> = None;
    let mut state_path: Option<PathBuf> = None;
    let mut check_mode = false;
    let mut debug = false;

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
            "--out" => {
                i += 1;
                out_override = Some(PathBuf::from(
                    args.get(i).cloned().ok_or("--out requires a path")?,
                ));
            }
            "--state" => {
                i += 1;
                state_path = Some(PathBuf::from(
                    args.get(i).cloned().ok_or("--state requires a path")?,
                ));
            }
            "--check" => check_mode = true,
            "--debug" => debug = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    if name_override.is_some() && out_override.is_some() {
        return Err("--name and --out are mutually exclusive (both pick the output path)".into());
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

    if (out_override.is_some() || name_override.is_some()) && plugins.len() > 1 {
        return Err(
            "--name / --out only make sense with a single plugin; pass -p <crate> to pick one"
                .into(),
        );
    }

    let dt = &deployment_target();
    let root = project_root();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Read state bytes once if --state was passed; same blob feeds
    // every plugin in a multi-plugin invocation.
    let state_bytes: Option<Vec<u8>> = state_path
        .as_ref()
        .map(|p| {
            let resolved = if p.is_absolute() { p.clone() } else { cwd.join(p) };
            std::fs::read(&resolved)
                .map_err(|e| format!("--state: failed to read {}: {e}", resolved.display()))
        })
        .transpose()?;

    for plugin in plugins {
        let crate_dir = plugin_crate_dir(&root, &plugin.crate_name)?;
        let out_path = resolve_out_path(
            &crate_dir,
            &cwd,
            &plugin.crate_name,
            out_override.as_deref(),
            name_override.as_deref(),
        );

        crate::vprintln!("Building {} cdylib...", plugin.name);
        let build_args = ["-p", &plugin.crate_name, "--no-default-features", "--lib"];
        if debug {
            cargo_build_debug(&[], &build_args, dt)?;
        } else {
            cargo_build(&[], &build_args, dt)?;
        }

        let lib_path = cdylib_path(&root, &plugin.crate_name, debug);
        if !lib_path.exists() {
            return Err(format!(
                "cdylib not found at {}. Plugin must declare \
                 `crate-type = [\"cdylib\", \"rlib\"]` in its [lib] section.",
                lib_path.display()
            )
            .into());
        }

        if check_mode {
            // Render to target/screenshots/ for diffing; never overwrite
            // the committed baseline in --check mode.
            let render_path = root
                .join("target")
                .join("screenshots")
                .join(format!("{}.png", plugin.crate_name));
            unsafe { call_screenshot(&lib_path, state_bytes.as_deref(), &render_path)? };
            check_against_reference(&render_path, &out_path, &plugin.crate_name)?;
        } else {
            unsafe { call_screenshot(&lib_path, state_bytes.as_deref(), &out_path)? };
            eprintln!("Wrote {}", out_path.display());
        }
    }
    Ok(())
}

/// Resolve the plugin crate's manifest dir from `truce.toml`. The
/// scaffold-shipped `truce.toml` carries each plugin's `crate =
/// "<crate_name>"`, but not the path — we resolve via cargo metadata
/// (cheap; the `project_root()` walk already runs).
fn plugin_crate_dir(root: &Path, crate_name: &str) -> Result<PathBuf, crate::BoxErr> {
    // Workspace projects: plugins live at <root>/plugins/<bundle_id>/.
    // Single-plugin projects: <root>/ IS the plugin crate.
    // Try both shapes by reading the candidate Cargo.toml for the
    // matching `name = "<crate_name>"`.
    let candidates = vec![
        root.to_path_buf(),
        root.join("plugins").join(crate_name),
        root.join(crate_name),
    ];
    for cand in &candidates {
        let toml = cand.join("Cargo.toml");
        if let Ok(s) = std::fs::read_to_string(&toml)
            && s.lines().any(|l| {
                l.trim_start().starts_with("name") && l.contains(&format!("\"{crate_name}\""))
            })
        {
            return Ok(cand.clone());
        }
    }
    // Workspace mode: scan plugins/* by reading each Cargo.toml.
    let plugins_dir = root.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let toml = p.join("Cargo.toml");
            if let Ok(s) = std::fs::read_to_string(&toml)
                && s.lines().any(|l| {
                    l.trim_start().starts_with("name") && l.contains(&format!("\"{crate_name}\""))
                })
            {
                return Ok(p);
            }
        }
    }
    Err(format!(
        "could not locate the manifest dir for crate '{crate_name}'. \
         Tried {} and plugins/*. Use `--out <path>` to set the output \
         path explicitly.",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
    .into())
}

/// Path resolution table for `--out` / `--name` / default.
fn resolve_out_path(
    crate_dir: &Path,
    cwd: &Path,
    crate_name: &str,
    out: Option<&Path>,
    name: Option<&str>,
) -> PathBuf {
    if let Some(p) = out {
        return if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    }
    let stem = name.unwrap_or(crate_name);
    crate_dir.join("screenshots").join(format!("{stem}.png"))
}

/// Resolve `target/{release,debug}/lib<crate>.<ext>` for the host
/// platform. Cargo replaces `-` with `_` in the crate name when
/// forming the shared-library filename.
fn cdylib_path(root: &Path, crate_name: &str, debug: bool) -> PathBuf {
    let normalized = crate_name.replace('-', "_");
    let profile_dir = if debug { "debug" } else { "release" };
    let dir = crate::target_dir(root).join(profile_dir);
    if cfg!(target_os = "macos") {
        dir.join(format!("lib{normalized}.dylib"))
    } else if cfg!(target_os = "windows") {
        dir.join(format!("{normalized}.dll"))
    } else {
        dir.join(format!("lib{normalized}.so"))
    }
}

/// dlopen the cdylib, look up `__truce_screenshot`, render with
/// optional `.pluginstate` bytes, write the PNG to `out_path`.
///
/// # Safety
/// The library at `lib_path` must export the symbol with the FFI
/// signature emitted by the `truce::plugin!` macro. Plugins built
/// from any in-tree truce version satisfy this.
unsafe fn call_screenshot(
    lib_path: &Path,
    state: Option<&[u8]>,
    out_path: &Path,
) -> Result<(), crate::BoxErr> {
    unsafe {
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

        let path_str = out_path.to_string_lossy();
        let path_bytes = path_str.as_bytes();
        let (state_ptr, state_len) = match state {
            Some(s) => (s.as_ptr(), s.len()),
            None => (std::ptr::null(), 0),
        };
        let rc = screenshot(state_ptr, state_len, path_bytes.as_ptr(), path_bytes.len());
        if rc != 0 {
            return Err(format!("__truce_screenshot returned non-zero ({rc})").into());
        }
        Ok(())
    }
}

/// `--check`: diff the just-rendered PNG (at `render_path`) against
/// the committed baseline (at `ref_path`). Mirrors the test
/// runtime's per-OS-pass-on-non-reference behavior so a green CI on
/// macOS doesn't go red on Linux just because the rasterizer drifts.
fn check_against_reference(render_path: &Path, ref_path: &Path, label: &str) -> Res {
    if !ref_path.exists() {
        return Err(format!(
            "no baseline at {} (rendered to {}). \
             Run `cargo truce screenshot` (without --check) to create one.",
            ref_path.display(),
            render_path.display()
        )
        .into());
    }

    let (cur, cw, ch) = load_png(render_path);
    let (refp, rw, rh) = load_png(ref_path);
    if (cw, ch) != (rw, rh) {
        return Err(format!(
            "{label}: GUI size changed: current {cw}x{ch}, reference {rw}x{rh}. \
             Delete {} and re-create it.",
            ref_path.display()
        )
        .into());
    }

    let diff_count = cur.iter().zip(refp.iter()).filter(|(a, b)| a != b).count();
    if diff_count == 0 {
        eprintln!("{label}: matches baseline ({})", ref_path.display());
        return Ok(());
    }

    if is_reference_platform() {
        Err(format!(
            "{label}: {diff_count} pixels differ from baseline.\n\
             Reference: {}\n\
             Current:   {}\n\
             Either fix the regression, or accept the new render with: cp '{}' '{}'",
            ref_path.display(),
            render_path.display(),
            render_path.display(),
            ref_path.display(),
        )
        .into())
    } else {
        eprintln!(
            "{label}: non-reference diff on {}: {diff_count} pixels differ vs {} \
             (informational; see TRUCE_SCREENSHOT_REFERENCE_OS).",
            std::env::consts::OS,
            ref_path.display(),
        );
        Ok(())
    }
}

/// Read an RGBA PNG from disk for `--check` comparison. Mirrors
/// `truce_core::screenshot::load_png` — duplicated here so the
/// CLI doesn't pull in the audio framework's transitive dep tree
/// just to decode a 1024×768 PNG.
fn load_png(path: &Path) -> (Vec<u8>, u32, u32) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .unwrap_or_else(|e| panic!("Failed to read PNG info: {e}"));
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("Failed to decode PNG frame: {e}"));
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}

/// Mirrors `truce_core::screenshot::is_reference_platform`. See that
/// fn for the rationale; same env-var contract.
fn is_reference_platform() -> bool {
    let target =
        std::env::var("TRUCE_SCREENSHOT_REFERENCE_OS").unwrap_or_else(|_| "macos".to_string());
    std::env::consts::OS == target
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce screenshot [-p <crate>] [--out <path> | --name <name>]
                              [--state <path.pluginstate>] [--check] [--debug]

Render a plugin's editor headlessly and save a PNG. The CLI is
self-contained — works on any crate built with `truce::plugin!`,
no test code required.

Output path:
  --out <path>     Explicit (CWD-relative or absolute).
  --name <name>    Shortcut for <crate>/screenshots/<name>.png.
  default          <crate>/screenshots/<crate>.png.

Options:
  -p <crate>       Plugin crate name (default: every plugin in truce.toml).
                   --out / --name require -p when a project has multiple plugins.
  --state <path>   Load a `.pluginstate` blob (the file format the
                   standalone host's Cmd+S / Ctrl+S writes) before
                   rendering. CWD-relative or absolute.
  --check          Diff against the existing baseline; exit non-zero
                   on regression. Honors TRUCE_SCREENSHOT_REFERENCE_OS
                   the same way the test runtime does.
  --debug          Cargo dev profile (faster compile). Default is release."
    );
}
