//! `cargo truce screenshot` - drive a plugin's editor headlessly
//! and save a PNG.
//!
//! Self-contained: works on any crate built with `truce::plugin!`,
//! whether or not it has any tests. The CLI dlopens the plugin's
//! cdylib, optionally loads a `.pluginstate` blob, calls the hidden
//! `__truce_screenshot` FFI, and writes the result to a path the
//! user picks (or a sensible default).
//!
//! Flags:
//! - `-p <crate>` - pick one plugin from a multi-plugin truce.toml.
//! - `--out <path>` - output path (CWD-relative, or absolute).
//!   Required. The CLI never picks a path on the author's behalf.
//! - `--state <path>` - load a `.pluginstate` blob before rendering.
//!   Path is CWD-relative or absolute.
//! - `--check` - diff against the existing baseline; exit non-zero
//!   on regression. Strict pixel match - every host gates the same
//!   way, so cross-OS rasterizer drift will fail. Bake your
//!   baselines on whichever host you gate from.
//! - `--debug` - cargo dev profile (faster compile).

use crate::{Res, cargo_build, cargo_build_debug, deployment_target, load_config, project_root};
use std::path::{Path, PathBuf};
// `Command`, `Duration`, `Instant`, and `thread::sleep` are only
// used by the iOS / `simctl` paths which are themselves gated on
// macOS; matching the cfg here keeps non-macOS builds warning-free.
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::thread::sleep;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

/// FFI signature the CLI casts the dlopen'd `__truce_screenshot`
/// symbol to.
///
/// Arguments: `(state_ptr, state_len, out_path_ptr, out_path_len,
/// scale)`. Returns 0 on success, non-zero on failure (logged to
/// stderr by the plugin side). `scale` is the render scale
/// (default 2.0); `<= 0` falls back to
/// [`truce_core::screenshot::DEFAULT_SCREENSHOT_SCALE`] inside the
/// plugin.
///
/// The cdylib has no link-time signature to cross-check against, so
/// any mismatch with the plugin-side export becomes silent UB at the
/// first call rather than a build failure. Changing this signature
/// requires changing both the plugin-side emit and this typedef in
/// lock-step.
type ScreenshotFn = unsafe extern "C" fn(*const u8, usize, *const u8, usize, f64) -> u32;

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_screenshot(args: &[String]) -> Res {
    // iOS short-circuit: render via the booted simulator instead of
    // dlopen'ing a desktop cdylib. The simulator path captures the
    // *real* rendered editor (including the iOS BuiltinEditor's
    // CGImage blit + UIView compositing), which the desktop
    // `__truce_screenshot` path can't see. Useful for catching iOS-
    // specific regressions (scale factor, layer.contents swap, etc).
    if args.iter().any(|a| a == "--ios") {
        #[cfg(target_os = "macos")]
        {
            return cmd_screenshot_ios(args);
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Err("--ios screenshot requires macOS (Xcode + simctl).".into());
        }
    }
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
) -> Result<(), crate::CargoTruceError> {
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
/// the committed baseline (at `ref_path`). Strict pixel match - any
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
/// `truce_core::screenshot::load_png` - duplicated here so the
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
                              [--ios]

Render a plugin's editor headlessly and save a PNG. The CLI is
self-contained - works on any crate built with `truce::plugin!`,
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
                   exit non-zero on regression. Strict pixel match -
                   bake the baseline on the host you gate from.
  --debug          Cargo dev profile (faster compile). Default is release.
  --ios            Build + install on the booted iOS Simulator and capture the
                   simulator's rendered output via `xcrun simctl io screenshot`.
                   The desktop dlopen path doesn't see the iOS BuiltinEditor's
                   CGImage blit / UIView compositing, so this is what catches
                   iOS-specific render regressions.
  --crop-mode <m>  (--ios only) `editor` (default) crops to the plug-in editor's
                   region. `container` crops just the iOS status bar band off the
                   top, keeping the rest of the container chrome - use for
                   framework-level tests that gate on the container layout.
  --container-out <path>
                   (--ios only) Also emit a container-crop image at <path> from
                   the same captured screenshot. Cuts the install + launch
                   round-trip in half when both crops are wanted. Incompatible
                   with --check."
    );
}

#[cfg(target_os = "macos")]
fn cmd_screenshot_ios(args: &[String]) -> Res {
    let mut plugin_filter: Option<&str> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut container_out_path: Option<PathBuf> = None;
    let mut check_mode = false;
    let mut crop_mode = IosCropMode::Editor;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ios" => {}
            "--out" => {
                i += 1;
                out_path = args.get(i).map(PathBuf::from);
                if out_path.is_none() {
                    return Err("--out needs a path".into());
                }
            }
            "--container-out" => {
                i += 1;
                container_out_path = args.get(i).map(PathBuf::from);
                if container_out_path.is_none() {
                    return Err("--container-out needs a path".into());
                }
            }
            "-p" => {
                i += 1;
                plugin_filter = args.get(i).map(String::as_str);
            }
            "--check" => check_mode = true,
            "--crop-mode" => {
                i += 1;
                crop_mode = match args.get(i).map(String::as_str) {
                    Some("editor") => IosCropMode::Editor,
                    Some("container") => IosCropMode::Container,
                    Some(other) => {
                        return Err(format!(
                            "--crop-mode: expected `editor` or `container`, got `{other}`"
                        )
                        .into());
                    }
                    None => return Err("--crop-mode needs a value (editor|container)".into()),
                };
            }
            other => return Err(format!("unknown flag for --ios: {other}").into()),
        }
        i += 1;
    }
    let out_path = out_path.ok_or("--out <path> required")?;
    if container_out_path.is_some() && check_mode {
        return Err("--container-out is not supported in --check mode yet".into());
    }

    // Resolve plugin + drive the install pipeline so the simulator
    // has a freshly-built bundle to launch.
    let config = load_config()?;
    let p = crate::commands::pick_plugins(&config, plugin_filter)?
        .into_iter()
        .next()
        .ok_or("no plugin to screenshot")?;
    let root = project_root();
    // Restrict `UISupportedInterfaceOrientations` to one canonical
    // orientation (the plugin's first listed, or "portrait" by
    // default). Without this, the sim inherits whatever rotation a
    // previous launch left it in: a landscape-only plug-in earlier
    // in a CI loop will leave the sim in landscape, and any
    // portrait-supporting plug-in launched afterwards will render
    // in landscape too - making baseline dimensions order-dependent.
    // Locking the Info.plist forces iOS to rotate the sim to the
    // canonical orientation on container launch.
    let canonical_orientation = p
        .ios_orientations
        .as_ref()
        .and_then(|o| o.first().cloned())
        .unwrap_or_else(|| "portrait".to_string());
    crate::commands::install::au_ios::install_one_screenshot(
        &root,
        p,
        crate::commands::install::au_ios::IosTarget::Simulator,
        &[canonical_orientation],
    )?;
    // Full reverse-DNS bundle ID: `{vendor.id}.{bundle_id-suffix}`.
    // simctl looks up the installed app by its CFBundleIdentifier,
    // which `build_bundle` constructed the same way.
    let suffix = p.bundle_id.replace('_', "-");
    let bundle_id = format!("{}.{suffix}", config.vendor.id);
    // Delete any stale `_truce_editor_frame.json` so the poll below
    // gates on *this* launch's first paint, not a prior run's file.
    clear_editor_frame_json(&bundle_id);
    eprintln!("==> Launching {bundle_id} on booted simulator...");
    // Retry the launch on the cold-runner FrontBoard flake. The
    // `simctl_install` step already polls `simctl get_app_container`
    // to confirm `installd` has registered the bundle, but
    // SBMainWorkspace (the service `simctl launch` talks to via
    // FrontBoard) sometimes lags `installd` by a few hundred ms - the
    // app is reachable by container path before the workspace's
    // launch table sees it. Up to 5 attempts at 500ms each is well
    // within the observed sync window without slowing the happy path
    // (first attempt succeeds in the warm case).
    let mut launched_ok = false;
    let mut last_status = None;
    for attempt in 0..5 {
        let status = Command::new("xcrun")
            .args(["simctl", "launch", "booted", &bundle_id])
            .status()
            .map_err(|e| format!("simctl launch: {e}"))?;
        if status.success() {
            launched_ok = true;
            break;
        }
        last_status = Some(status);
        if attempt < 4 {
            eprintln!(
                "    simctl launch attempt {} failed; retrying after 500ms...",
                attempt + 1
            );
            sleep(Duration::from_millis(500));
        }
    }
    if !launched_ok {
        let status = last_status.expect("loop ran at least once");
        return Err(format!("simctl launch exited {status} after 5 attempts").into());
    }
    // Wait for the editor to actually paint before snapping the
    // screenshot. The editor writes `_truce_editor_frame.json` into
    // its Documents container on first layout — its appearance is
    // proof the editor has rendered at least one frame. Hard-fail
    // on timeout so the downstream "screenshot size mismatch" error
    // doesn't mask the real "editor never rendered" failure.
    wait_for_editor_frame_json(&bundle_id)?;
    // Small grace pause once the JSON appears: the file lands at
    // the end of `BuiltinEditor::open`'s first paint, but a host
    // sometimes drives one more frame for animated content. Cheap
    // insurance against per-DAW jitter; doesn't change the happy-
    // path latency materially.
    sleep(Duration::from_millis(250));
    let resolved_out = if out_path.is_absolute() {
        out_path.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&out_path)
    };
    if check_mode {
        // Render to <target>/screenshots/<basename> for diffing; never
        // overwrite the committed baseline in --check mode.
        let render_dir = truce_build::target_dir(&root).join("screenshots");
        std::fs::create_dir_all(&render_dir).ok();
        let basename = out_path.file_name().map_or_else(
            || std::ffi::OsString::from(format!("{}_ios.png", p.crate_name)),
            std::ffi::OsStr::to_os_string,
        );
        let render_path = render_dir.join(basename);
        capture_simctl_screenshot(&render_path)?;
        crop_for_mode(&render_path, &bundle_id, crop_mode);
        diff_simctl_screenshot(&render_path, &resolved_out)?;
        return Ok(());
    }
    capture_simctl_screenshot(&resolved_out)?;
    // `--container-out` reuses the just-captured framebuffer: copy
    // it to the second destination, then run `crop_for_mode` over
    // each copy independently. Saves one full install + launch +
    // sleep round-trip per crate in the screenshot bake loop.
    if let Some(container_out) = container_out_path {
        let container_resolved = if container_out.is_absolute() {
            container_out
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(container_out)
        };
        if let Some(parent) = container_resolved.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&resolved_out, &container_resolved).map_err(|e| {
            format!(
                "copy {} -> {}: {e}",
                resolved_out.display(),
                container_resolved.display()
            )
        })?;
        crop_for_mode(&container_resolved, &bundle_id, IosCropMode::Container);
    }
    crop_for_mode(&resolved_out, &bundle_id, crop_mode);
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Copy, Clone)]
enum IosCropMode {
    /// Crop down to just the plug-in editor's region. Default -
    /// per-plug-in tests that gate on the editor's visual output.
    Editor,
    /// Crop only the iOS status bar band (which holds the variable
    /// clock) off the top; keep the full container app chrome
    /// below. For framework-level tests of the container layout.
    Container,
}

#[cfg(target_os = "macos")]
fn crop_for_mode(png_path: &Path, bundle_id: &str, mode: IosCropMode) {
    let frame = match read_editor_frame_json(bundle_id) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: skipping screenshot trim ({e})");
            return;
        }
    };
    // `simctl io screenshot` always returns the physical framebuffer,
    // which is portrait on every iPhone. For a landscape-only plug-in
    // iOS still renders the UI in landscape - that means the captured
    // PNG has the UI rotated 90° inside a portrait canvas, and the
    // editor-frame coords (which the container wrote in the rendered
    // UI's coordinate space) land out of bounds against the un-rotated
    // PNG. Rotate the file in place so its orientation matches the
    // coord space before the crop step touches the dimensions.
    if let Err(e) = orient_to_ui(png_path, frame.orientation.as_deref()) {
        eprintln!("warning: orient {} skipped ({e})", png_path.display());
    }
    match mode {
        IosCropMode::Editor => crop_to_editor_frame(png_path, &frame),
        IosCropMode::Container => crop_to_container_chrome(png_path, &frame),
    }
}

/// Rotate `path` in place so the saved PNG matches the rendered-UI
/// orientation the container reported. `sips -r <degrees>` rotates
/// clockwise; we pick whichever rotation undoes iOS's rotation of
/// the UI inside the portrait framebuffer.
///
/// For `landscapeLeft` (home button on left): iOS draws the UI 90°
/// CCW inside the portrait framebuffer, so rotating the PNG 90° CW
/// (`sips -r 90`) returns it to upright. `landscapeRight` is the
/// mirror (270° CW = 90° CCW); `portraitUpsideDown` is 180°.
/// `portrait` and unknown orientations no-op.
#[cfg(target_os = "macos")]
fn orient_to_ui(path: &Path, orientation: Option<&str>) -> Result<(), crate::CargoTruceError> {
    let degrees = match orientation.unwrap_or("portrait") {
        "landscapeLeft" => "90",
        "landscapeRight" => "270",
        "portraitUpsideDown" => "180",
        _ => return Ok(()),
    };
    let out = Command::new("sips")
        .args(["-r", degrees, "--out"])
        .arg(path)
        .arg(path)
        .output()
        .map_err(|e| format!("sips: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sips -r {degrees} exited {} ({})",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        )
        .into());
    }
    Ok(())
}

/// Crop the simulator screenshot down to just the editor's region.
/// Reads the editor frame the container app wrote into its app
/// container's `Documents/_truce_editor_frame.json` on first paint,
/// trims the PNG in-place, and overwrites the source file. Failures
/// here are non-fatal - the untrimmed screenshot stays in place with
/// a warning, since cropping is a quality-of-life feature and the
/// underlying PNG is still useful.
#[cfg(target_os = "macos")]
fn crop_to_editor_frame(png_path: &Path, frame: &EditorFrame) {
    if let Err(e) = crop_png(png_path, frame.x, frame.y, frame.w, frame.h) {
        eprintln!("warning: failed to trim {} ({e})", png_path.display());
    }
}

/// Crop just the iOS status bar band off the top. The status bar
/// is where the variable clock lives - chopping that one band keeps
/// the rest of the container chrome (title, editor, button, status)
/// intact while making the diff stable across runs. Falls back to
/// leaving the screenshot untrimmed if the container didn't write
/// the safe-area inset (older builds, or layout still pending).
#[cfg(target_os = "macos")]
fn crop_to_container_chrome(png_path: &Path, frame: &EditorFrame) {
    let (src_w, src_h) = match png_size(png_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("warning: skipping container crop ({e})");
            return;
        }
    };
    let top = frame.safe_area_top_px.min(src_h);
    if top == 0 {
        return; // nothing to crop - leave untouched.
    }
    let height = src_h.saturating_sub(top);
    if let Err(e) = crop_png(png_path, 0, top, src_w, height) {
        eprintln!("warning: failed to trim {} ({e})", png_path.display());
    }
}

/// Cheap PNG dimensions probe (just the IHDR chunk). Pulling in
/// `png::Decoder` would be the same cost as `crop_png`'s read path,
/// but the chunk header is at a fixed offset so reading 24 bytes is
/// enough.
#[cfg(target_os = "macos")]
fn png_size(path: &Path) -> Result<(u32, u32), crate::CargoTruceError> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let reader = decoder
        .read_info()
        .map_err(|e| format!("read_info {}: {e}", path.display()))?;
    let info = reader.info();
    Ok((info.width, info.height))
}

#[cfg(target_os = "macos")]
struct EditorFrame {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// Safe-area top inset in physical pixels - the height of the
    /// iOS status bar band that contains the (variable) clock. Used
    /// by `--mode container` to crop just that band off so framework
    /// chrome screenshots stay stable across runs.
    safe_area_top_px: u32,
    /// Interface orientation the container was rendering in when it
    /// wrote the frame. `simctl io screenshot` always captures the
    /// portrait-physical framebuffer, so a landscape-only plug-in
    /// shows up rotated 90° inside a portrait PNG. `crop_for_mode`
    /// rotates the file to match this orientation before cropping
    /// so the frame coords (which are in the rendered UI's space)
    /// line up. `None` for older containers that didn't write it.
    orientation: Option<String>,
}

/// Resolve `Documents/_truce_editor_frame.json` inside the bundle's
/// data container, if simctl can find it right now. Returns `None`
/// when the container isn't registered yet (just-installed apps).
#[cfg(target_os = "macos")]
fn editor_frame_json_path(bundle_id: &str) -> Option<PathBuf> {
    let out = Command::new("xcrun")
        .args(["simctl", "get_app_container", "booted", bundle_id, "data"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let container = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if container.is_empty() {
        return None;
    }
    Some(Path::new(&container).join("Documents/_truce_editor_frame.json"))
}

/// Delete any leftover `_truce_editor_frame.json` from a prior
/// container run so the post-launch poll gates on *this* launch's
/// first paint. Best-effort — a missing file or missing container
/// is fine.
#[cfg(target_os = "macos")]
fn clear_editor_frame_json(bundle_id: &str) {
    if let Some(p) = editor_frame_json_path(bundle_id) {
        let _ = std::fs::remove_file(p);
    }
}

/// Poll for `_truce_editor_frame.json` to appear (and be non-empty)
/// after launch. The file is written at the end of the editor's
/// first paint pass on iOS, so its presence proves at least one
/// frame has rendered.
///
/// Errors when the editor doesn't render inside the timeout. The
/// alternative — silently proceeding — masks the failure as a
/// downstream "screenshot size mismatch" against the baseline.
#[cfg(target_os = "macos")]
fn wait_for_editor_frame_json(bundle_id: &str) -> Res {
    // 30 s covers the cold-runner GHA case (AUv3 host bring-up +
    // first frame can easily take 10–20 s on a fresh sim).
    const TIMEOUT: Duration = Duration::from_secs(30);
    const POLL_INTERVAL: Duration = Duration::from_millis(150);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // Resolve the container fresh each iteration: `installd`
        // sometimes returns the path slightly before the Documents
        // directory is populated.
        if let Some(frame_path) = editor_frame_json_path(bundle_id)
            && let Ok(meta) = std::fs::metadata(&frame_path)
            && meta.len() > 0
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "editor never rendered: timed out after {}s waiting for \
                 _truce_editor_frame.json in {bundle_id}'s container. The \
                 AUv3 plugin likely failed to instantiate or its first \
                 paint pass crashed. Check `xcrun simctl spawn booted log \
                 show --predicate 'subsystem == \"{bundle_id}\"'` for clues.",
                TIMEOUT.as_secs()
            )
            .into());
        }
        sleep(POLL_INTERVAL);
    }
}

#[cfg(target_os = "macos")]
fn read_editor_frame_json(bundle_id: &str) -> Result<EditorFrame, crate::CargoTruceError> {
    let out = Command::new("xcrun")
        .args(["simctl", "get_app_container", "booted", bundle_id, "data"])
        .output()
        .map_err(|e| format!("simctl get_app_container: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "simctl get_app_container exited {} ({})",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        )
        .into());
    }
    let container = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if container.is_empty() {
        return Err("simctl returned empty app container path".into());
    }
    let frame_path = Path::new(&container).join("Documents/_truce_editor_frame.json");
    let json = std::fs::read_to_string(&frame_path)
        .map_err(|e| format!("read {}: {e}", frame_path.display()))?;
    // Tiny hand-rolled parser - the file is a single-line object
    // with four / five integer fields; pulling in `serde_json` for
    // this is overkill.
    let pick = |key: &str| -> Result<u32, crate::CargoTruceError> {
        let needle = format!("\"{key}\":");
        let start = json
            .find(&needle)
            .ok_or_else(|| format!("frame JSON missing key {key}"))?
            + needle.len();
        let rest = &json[start..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest.len());
        rest[..end]
            .trim()
            .parse::<i64>()
            .map(|v| u32::try_from(v.max(0)).unwrap_or(0))
            .map_err(|e| format!("frame JSON {key}: {e}").into())
    };
    // Hand-rolled string-field reader for the new `orientation`
    // entry. Same shape as `pick` but parses a quoted value instead
    // of an integer. `None` when the field is absent (older
    // containers) so callers can fall back to "portrait".
    let pick_string = |key: &str| -> Option<String> {
        let needle = format!("\"{key}\":\"");
        let start = json.find(&needle)? + needle.len();
        let rest = &json[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    };
    Ok(EditorFrame {
        x: pick("x")?,
        y: pick("y")?,
        w: pick("w")?,
        h: pick("h")?,
        // Tolerate older containers that didn't write this field
        // yet - fall back to 0 so the editor-mode crop keeps working.
        safe_area_top_px: pick("safeAreaTopPx").unwrap_or(0),
        orientation: pick_string("orientation"),
    })
}

#[cfg(target_os = "macos")]
fn crop_png(path: &Path, x: u32, y: u32, w: u32, h: u32) -> Result<(), crate::CargoTruceError> {
    if w == 0 || h == 0 {
        return Err(format!("crop rect is zero-area ({w}×{h})").into());
    }
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("read_info {}: {e}", path.display()))?;
    let info = reader.info().clone();
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "unsupported bit depth {:?} (only 8-bit PNGs supported)",
            info.bit_depth
        )
        .into());
    }
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        _ => return Err(format!("unsupported color type {:?}", info.color_type).into()),
    };
    let src_w = info.width;
    let src_h = info.height;
    if x.saturating_add(w) > src_w || y.saturating_add(h) > src_h {
        return Err(
            format!("crop ({x},{y},{w}×{h}) out of bounds for {src_w}×{src_h} image").into(),
        );
    }
    let mut buf = vec![
        0u8;
        reader
            .output_buffer_size()
            .ok_or("png output_buffer_size returned None")?
    ];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("decode frame: {e}"))?;
    buf.truncate(frame.buffer_size());

    let row_bytes = (src_w as usize) * channels;
    let crop_row_bytes = (w as usize) * channels;
    let mut out_buf = Vec::with_capacity(crop_row_bytes * h as usize);
    for row in 0..h {
        let src_row = (y + row) as usize;
        let src_x = (x as usize) * channels;
        let off = src_row * row_bytes + src_x;
        out_buf.extend_from_slice(&buf[off..off + crop_row_bytes]);
    }

    let tmp = path.with_extension("png.tmp");
    {
        let out_file =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(out_file), w, h);
        encoder.set_color(if channels == 4 {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(&out_buf)
            .map_err(|e| format!("png write: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn capture_simctl_screenshot(out: &Path) -> Res {
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok();
    }
    let status = Command::new("xcrun")
        .args(["simctl", "io", "booted", "screenshot"])
        .arg(out)
        .status()
        .map_err(|e| format!("xcrun simctl io screenshot: {e}"))?;
    if !status.success() {
        return Err(format!("simctl io screenshot exited {status}").into());
    }
    eprintln!("Screenshot: {}", out.display());
    Ok(())
}

/// Per-channel tolerance for simulator render jitter. Pixels are
/// RGBA8; the simulator occasionally bumps a single channel by ±1
/// between captures even with identical content.
#[cfg(target_os = "macos")]
const SIMCTL_CHANNEL_TOLERANCE: u8 = 2;

/// Inverse of the diff-pixel count budget (~0.5% of the image).
/// Catches real layout regressions while ignoring background
/// rendering noise plus incidental text variance (the status label
/// transitions "Loading audio…" → "Ready" between consecutive
/// captures and covers a few thousand pixels on its own).
#[cfg(target_os = "macos")]
const SIMCTL_DIFF_BUDGET_DENOM: usize = 200;

#[cfg(target_os = "macos")]
fn diff_simctl_screenshot(render: &Path, baseline: &Path) -> Res {
    if !baseline.exists() {
        return Err(format!(
            "baseline not found at {}. Render saved at {}. \
             Accept with: cp {} {}",
            baseline.display(),
            render.display(),
            render.display(),
            baseline.display(),
        )
        .into());
    }
    // simctl io screenshot PNGs aren't byte-stable across captures
    // even when the rendered pixels are identical (PNG metadata
    // chunks, encoder settings drift). Decode both sides to raw
    // RGBA then compare pixels with a small per-channel tolerance
    // to absorb the simulator's render jitter. Hard size mismatch
    // is a real regression (someone changed the device or chrome
    // dimensions) so that still fails immediately.
    let (cur, cw, ch) = load_png(render);
    let (refp, rw, rh) = load_png(baseline);
    if (cw, ch) != (rw, rh) {
        return Err(format!(
            "iOS screenshot size changed: rendered {cw}x{ch}, baseline {rw}x{rh}. \
             Either the simulator device differs from the one that baked the \
             baseline, or the container layout actually changed. Re-bake with: \
             cp {} {}",
            render.display(),
            baseline.display(),
        )
        .into());
    }
    let pixel_count = (cw as usize) * (ch as usize);
    let diff_budget = pixel_count / SIMCTL_DIFF_BUDGET_DENOM;
    let diff_count = cur
        .chunks_exact(4)
        .zip(refp.chunks_exact(4))
        .filter(|(a, b)| {
            a.iter()
                .zip(b.iter())
                .any(|(av, bv)| av.abs_diff(*bv) > SIMCTL_CHANNEL_TOLERANCE)
        })
        .count();
    if diff_count <= diff_budget {
        eprintln!(
            "OK: {} matches {} (diff {} ≤ {})",
            render.display(),
            baseline.display(),
            diff_count,
            diff_budget,
        );
        return Ok(());
    }
    Err(format!(
        "iOS screenshot differs from baseline ({diff_count} of {pixel_count} pixels \
         exceed channel tolerance {SIMCTL_CHANNEL_TOLERANCE}, budget {diff_budget}).
  rendered: {}
  baseline: {}
  accept with: cp {} {}",
        render.display(),
        baseline.display(),
        render.display(),
        baseline.display(),
    )
    .into())
}
