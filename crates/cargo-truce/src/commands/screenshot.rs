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
//! - `--out <path>` — output path (CWD-relative, or absolute).
//!   Required. The CLI never picks a path on the author's behalf.
//! - `--state <path>` — load a `.pluginstate` blob before rendering.
//!   Path is CWD-relative or absolute.
//! - `--check` — diff against the existing baseline; exit non-zero
//!   on regression. Strict pixel match — every host gates the same
//!   way, so cross-OS rasterizer drift will fail. Bake your
//!   baselines on whichever host you gate from.
//! - `--debug` — cargo dev profile (faster compile).

use crate::{Res, cargo_build, cargo_build_debug, deployment_target, load_config, project_root};
use std::path::{Path, PathBuf};

/// FFI signature emitted by `truce::plugin!`'s `__truce_screenshot`.
/// `(state_ptr, state_len, out_path_ptr, out_path_len, scale) -> u32`
/// — 0 on success, non-zero on failure (logged to stderr by the
/// plugin). `scale` is the render scale (default 2.0); `<= 0` falls
/// back to [`truce_core::screenshot::DEFAULT_SCREENSHOT_SCALE`] inside
/// the plugin.
///
/// **Must stay byte-identical to the `__truce_screenshot` definition in
/// `crates/truce/src/plugin_macro.rs`.** This typedef is what the CLI
/// casts the dlopen'd symbol to; the cdylib has no link-time signature
/// to cross-check against, so a mismatch (extra arg, reordered args,
/// return-type change) becomes silent UB at the first call rather than
/// a build failure. Update both sides together.
type ScreenshotFn = unsafe extern "C" fn(*const u8, usize, *const u8, usize, f64) -> u32;

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_screenshot(args: &[String]) -> Res {
    let mut plugin_filter: Option<String> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut state_path: Option<PathBuf> = None;
    let mut check_mode = false;
    let mut debug = false;
    // `0.0` is the FFI sentinel for "use the plugin's default
    // screenshot scale". Override via `--scale <f64>` to pin a
    // specific value; tests that opt out of the default via
    // `ScreenshotTest::scale` should pass the same value here when
    // re-baking their reference PNG.
    let mut scale: f64 = 0.0;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "--out" => {
                out_path = Some(PathBuf::from(crate::util::arg_value(
                    args, &mut i, "--out",
                )?));
            }
            "--state" => {
                state_path = Some(PathBuf::from(crate::util::arg_value(
                    args, &mut i, "--state",
                )?));
            }
            "--scale" => {
                let raw = crate::util::arg_value(args, &mut i, "--scale")?;
                scale = raw
                    .parse::<f64>()
                    .map_err(|e| format!("--scale: {raw:?} is not a valid f64: {e}"))?;
                if !scale.is_finite() || scale <= 0.0 {
                    return Err(format!("--scale: must be finite and > 0 (got {scale})").into());
                }
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

    let out_path = out_path.ok_or(
        "--out <path> is required. The screenshot CLI doesn't pick \
         an output path on your behalf; supply one explicitly.",
    )?;

    let config = load_config()?;
    let plugins = super::pick_plugins(&config, plugin_filter.as_deref())?;

    if plugins.is_empty() {
        return Err("no plugins in truce.toml".into());
    }

    if plugins.len() > 1 {
        return Err(
            "multi-plugin truce.toml: pass -p <crate> to pick which plugin to screenshot \
             (each plugin needs its own --out path; the CLI doesn't guess)"
                .into(),
        );
    }

    let dt = &deployment_target();
    let root = project_root();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Resolve --out / --state now that we know they're set.
    let resolved_out = if out_path.is_absolute() {
        out_path.clone()
    } else {
        cwd.join(&out_path)
    };
    let state_bytes: Option<Vec<u8>> = state_path
        .as_ref()
        .map(|p| {
            let resolved = if p.is_absolute() {
                p.clone()
            } else {
                cwd.join(p)
            };
            std::fs::read(&resolved)
                .map_err(|e| format!("--state: failed to read {}: {e}", resolved.display()))
        })
        .transpose()?;

    let plugin = plugins[0];

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
        // Render to <target>/screenshots/ for diffing; never overwrite
        // the committed baseline in --check mode. Use the basename
        // of the supplied --out so multiple `--check` invocations
        // don't trample each other in the workspace target dir.
        // `target_dir` honours `CARGO_TARGET_DIR` and the workspace's
        // `.cargo/config.toml`'s `[build].target-dir` so the artifact
        // landing path tracks where cargo actually builds.
        let render_dir = truce_build::target_dir(&root).join("screenshots");
        let fallback_name = format!("{}.png", plugin.crate_name);
        let render_path = render_dir.join(
            resolved_out
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(&fallback_name)),
        );
        unsafe { call_screenshot(&lib_path, state_bytes.as_deref(), &render_path, scale)? };
        check_against_reference(&render_path, &resolved_out, &plugin.crate_name)?;
    } else {
        unsafe { call_screenshot(&lib_path, state_bytes.as_deref(), &resolved_out, scale)? };
        eprintln!("Wrote {}", resolved_out.display());
    }
    Ok(())
}

/// Resolve `target/{release,debug}/lib<crate>.<ext>` for the host
/// platform. Cargo replaces `-` with `_` in the crate name when
/// forming the shared-library filename.
fn cdylib_path(root: &Path, crate_name: &str, debug: bool) -> PathBuf {
    let normalized = crate_name.replace('-', "_");
    let profile_dir = if debug { "debug" } else { "release" };
    let dir = truce_build::target_dir(root).join(profile_dir);
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
    scale: f64,
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
        let rc = screenshot(
            state_ptr,
            state_len,
            path_bytes.as_ptr(),
            path_bytes.len(),
            scale,
        );
        if rc != 0 {
            return Err(format!("__truce_screenshot returned non-zero ({rc})").into());
        }
        Ok(())
    }
}

/// `--check`: diff the just-rendered PNG (at `render_path`) against
/// the committed baseline (at `ref_path`). Strict pixel match — any
/// difference fails the check.
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

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce screenshot --out <path> [-p <crate>]
                              [--state <path.pluginstate>] [--check]
                              [--scale <f64>] [--debug]

Render a plugin's editor headlessly and save a PNG. The CLI is
self-contained — works on any crate built with `truce::plugin!`,
no test code required.

Required:
  --out <path>     Output path (CWD-relative or absolute). The CLI
                   never picks a path on your behalf.

Options:
  -p <crate>       Plugin crate name. Required for multi-plugin
                   projects (each plugin gets its own --out path).
  --state <path>   Load a `.pluginstate` blob (the file format the
                   standalone host's Cmd+S / Ctrl+S writes) before
                   rendering. CWD-relative or absolute.
  --scale <f64>    Render scale. Defaults to the plugin's
                   `DEFAULT_SCREENSHOT_SCALE` (currently 2.0) so
                   reference PNGs render at identical dimensions on
                   every host. Override only if a specific test
                   bakes its baseline at a different scale via
                   `ScreenshotTest::scale`.
  --check          Diff against the existing baseline at <path>;
                   exit non-zero on regression. Strict pixel match —
                   bake the baseline on the host you gate from.
  --debug          Cargo dev profile (faster compile). Default is release."
    );
}
