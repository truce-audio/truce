//! Link a plugin static archive into a macOS loadable bundle.
//!
//! Rust's `cdylib` link path produces `MH_DYLIB`, which `CFBundle`'s
//! loader (every JUCE-hosted VST3 host, pluginval, `DawDreamer`)
//! rejects. The cleanest fix is to skip the cdylib for macOS bundle
//! formats (VST3, CLAP, VST2) and instead link a Rust `staticlib`
//! through `clang -bundle` to produce a real `MH_BUNDLE`.
//!
//! AU v2 / AAX / Linux / Windows continue to use the cdylib path:
//! AU's component loader and AAX's `dlopen`-from-C++ shim are happy
//! with `MH_DYLIB`, and ELF / PE don't carry the bundle vs dylib
//! distinction.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::build::MacArch;

/// Symbols a CLAP bundle must export so the host can find the
/// descriptor table. The wrapper crate emits the symbol via
/// `#[no_mangle]`; clang `-bundle` would otherwise dead-strip it.
pub(crate) const CLAP_EXPORTS: &[&str] = &["_clap_entry"];

/// Symbols a macOS VST3 bundle must export. `GetPluginFactory` is the
/// factory entry; `BundleEntry`/`BundleExit` (plus the lower-cased
/// variants the SDK ships) get the dyld init/teardown callbacks. The
/// `ModuleEntry`/`ModuleExit` (Linux) and `InitDll`/`ExitDll` (Windows)
/// counterparts aren't defined on macOS in `truce-vst3`, so listing
/// them here would fail the link with "undefined exported symbol".
pub(crate) const VST3_EXPORTS: &[&str] = &[
    "_GetPluginFactory",
    "_BundleEntry",
    "_bundleEntry",
    "_BundleExit",
    "_bundleExit",
];

/// Symbols a VST2 bundle must export. `VSTPluginMain` is the modern
/// entry; `main_macho` is the legacy alias older Steinberg hosts
/// probe.
pub(crate) const VST2_EXPORTS: &[&str] = &["_VSTPluginMain", "_main_macho"];

/// Single source of truth for the "no staticlib emitted" error.
///
/// Plugins scaffolded before 0.44.0 ship `crate-type = ["cdylib",
/// "rlib"]`. The 0.44.0 macOS bundle-link pipeline reads
/// `lib<stem>.a` (a Rust `staticlib` archive) and feeds it to
/// `clang -bundle` to produce an `MH_BUNDLE`, so the missing
/// staticlib makes the install / package step fail. We surface the
/// exact one-line `Cargo.toml` fix here so plugin authors don't
/// have to hunt for it in release notes.
pub(crate) fn missing_staticlib_error(staticlib_path: &Path) -> String {
    format!(
        "macOS bundle link needs a Rust staticlib at\n  \
           {path}\n\
         but cargo didn't emit one.\n\
         \n\
         Starting with truce 0.44.0, macOS bundle formats (CLAP / VST3 / VST2) \
         are linked from `lib<stem>.a` via `clang -bundle`. Plugins scaffolded \
         before 0.44.0 only declared `[\"cdylib\", \"rlib\"]` and need to add \
         `\"staticlib\"` to the `crate-type` array in their plugin crate's \
         `Cargo.toml`.\n\
         \n\
         Exact change:\n\
         \n\
             # before\n\
             [lib]\n\
             crate-type = [\"cdylib\", \"rlib\"]\n\
         \n\
             # after\n\
             [lib]\n\
             crate-type = [\"cdylib\", \"staticlib\", \"rlib\"]\n\
         \n\
         Then re-run the failing command.",
        path = staticlib_path.display(),
    )
}

/// System frameworks the bundle needs at load time. Mirrors what the
/// equivalent cdylib build pulls in from `objc2-app-kit`,
/// `objc2-foundation`, `objc2-quartz-core`, `truce-gpu` (`Metal`),
/// `truce-au` shim (`AudioToolbox` / `AVFAudio` / `CoreAudio` /
/// `CoreMIDI`), and `core-graphics` deps.
///
/// Even though `-Wl,-undefined,dynamic_lookup` would let dyld resolve
/// these symbols when the host already has the frameworks mapped, a
/// non-DAW caller (e.g. `clap-validator`, a CLI) doesn't have `AppKit`
/// pre-loaded. Linking the frameworks here makes `LC_LOAD_DYLIB`
/// commands land in the bundle's Mach-O header, so dyld loads them
/// before symbol resolution kicks in - exactly what the cdylib path
/// does on its own.
const MACOS_PLUGIN_FRAMEWORKS: &[&str] = &[
    "AppKit",
    "Foundation",
    "CoreFoundation",
    "QuartzCore",
    "Metal",
    "AudioToolbox",
    "AVFAudio",
    "CoreAudio",
    "CoreMIDI",
    "CoreGraphics",
];

/// Link one or more per-arch Rust static archives into a single
/// macOS bundle binary at `out_bundle_bin`. Per-arch link via clang;
/// multi-arch is merged with `lipo`.
///
/// `exports` is the set of symbols clang must keep in the output
/// (e.g. [`CLAP_EXPORTS`] / [`VST3_EXPORTS`]). Everything else can be
/// dead-stripped. `-undefined dynamic_lookup` defers system framework
/// references (`CoreFoundation`, `AudioToolbox`, `AppKit`, ...) to the
/// host's dyld at load time; this is the standard pattern for macOS
/// audio plugins and avoids us re-declaring every framework the
/// staticlib's Rust deps would otherwise bring in via
/// `cargo:rustc-link-lib`.
pub(crate) fn link_macos_bundle(
    staticlibs: &[(MacArch, PathBuf)],
    exports: &[&str],
    deployment_target: &str,
    out_bundle_bin: &Path,
) -> crate::Res {
    if staticlibs.is_empty() {
        return Err("link_macos_bundle: no input static archives".into());
    }
    for (_, p) in staticlibs {
        if !p.exists() {
            return Err(
                format!("link_macos_bundle: missing static archive {}", p.display()).into(),
            );
        }
    }

    if let Some(parent) = out_bundle_bin.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if staticlibs.len() == 1 {
        let (arch, staticlib) = &staticlibs[0];
        return clang_bundle_single(*arch, staticlib, exports, deployment_target, out_bundle_bin);
    }

    // Multi-arch: link each slice next to the output, then `lipo` them.
    let mut per_arch_outputs: Vec<PathBuf> = Vec::with_capacity(staticlibs.len());
    for (arch, staticlib) in staticlibs {
        let slice_out = out_bundle_bin.with_extension(format!("{}-slice", arch.triple()));
        clang_bundle_single(*arch, staticlib, exports, deployment_target, &slice_out)?;
        per_arch_outputs.push(slice_out);
    }
    super::build::lipo_into(&per_arch_outputs, out_bundle_bin)?;
    for slice in &per_arch_outputs {
        let _ = std::fs::remove_file(slice);
    }
    Ok(())
}

fn clang_bundle_single(
    arch: MacArch,
    staticlib: &Path,
    exports: &[&str],
    deployment_target: &str,
    out: &Path,
) -> crate::Res {
    let arch_flag = match arch {
        MacArch::Arm64 => "arm64",
        MacArch::X86_64 => "x86_64",
    };
    let mut cmd = Command::new("clang");
    cmd.args([
        "-bundle",
        "-arch",
        arch_flag,
        // Clang spells the min-version flag as a single `=`-joined
        // token; the space-separated form gets parsed as `-m` + a
        // bare version string.
        &format!("-mmacosx-version-min={deployment_target}"),
        // Catch-all for any symbol we didn't explicitly link a
        // framework for (e.g. Rust deps that pull in obscure
        // CoreServices APIs). DAW hosts already have most system
        // frameworks mapped, so the deferred lookup succeeds at load
        // time. We still link the common framework set below so
        // non-DAW callers (`clap-validator`, headless test harnesses)
        // don't fail on `_NSFilenamesPboardType` and friends.
        "-Wl,-undefined,dynamic_lookup",
        // Pull every object from the archive so format-specific
        // entry points (declared with `#[no_mangle]` deep inside
        // truce-{clap,vst3,vst2}) aren't dead-stripped before we get
        // a chance to mark them exported below.
        "-Wl,-all_load",
    ]);
    for framework in MACOS_PLUGIN_FRAMEWORKS {
        cmd.args(["-framework", framework]);
    }
    // C++ runtime - truce-gpu pulls in wgpu/Metal which transitively
    // depends on libc++ symbols. libobjc + libSystem come implicitly
    // via clang's driver defaults; libc++ is the one extra runtime we
    // have to ask for by name.
    cmd.arg("-lc++");
    for sym in exports {
        cmd.arg(format!("-Wl,-exported_symbol,{sym}"));
    }
    // `-all_load` pulls every staticlib object into the link, then
    // `-dead_strip` removes everything not reachable from the
    // `-exported_symbol` roots. Without this the bundle ships every
    // monomorphization and dep the staticlib brought in - roughly
    // double the size of the equivalent cdylib (AU2 / AAX), whose
    // rustc-driven link gets `-dead_strip` for free on apple-darwin.
    cmd.arg("-Wl,-dead_strip");
    cmd.arg(staticlib);
    cmd.arg("-o").arg(out);

    let output = cmd.output().map_err(|e| -> crate::CargoTruceError {
        format!("invoking clang for bundle link: {e}").into()
    })?;
    if !output.status.success() {
        return Err(format!(
            "clang -bundle failed for {} ({arch_flag}):\n{}",
            staticlib.display(),
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }
    Ok(())
}
