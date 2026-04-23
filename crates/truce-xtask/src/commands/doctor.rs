//! `cargo truce doctor` — environment diagnostics: Rust toolchain, code
//! signing tools, AAX SDK, installed plugins.

#![allow(unused_imports)]

use crate::config::read_cargo_config_env;
use crate::{
    check_cmd, dirs, load_config, project_root, resolve_aax_sdk_path,
    rustup_has_target, Res,
};
#[cfg(target_os = "macos")]
use crate::locate_wraptool_macos;
#[cfg(target_os = "windows")]
use crate::{common_program_files, locate_cmake, locate_ninja, packaging_windows, program_files};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// doctor — environment diagnostics
// ---------------------------------------------------------------------------

pub(crate) fn cmd_doctor() -> Res {
    eprintln!("truce doctor");
    eprintln!("─────────────────────────────────────────");
    eprintln!();

    // Toolchain
    eprintln!("  Toolchain");
    check_cmd("rustc", &["--version"], "rustc");
    check_cmd("cargo", &["--version"], "cargo");

    // Platform tools
    #[cfg(target_os = "macos")]
    {
        eprintln!();
        eprintln!("  macOS");
        check_cmd("xcode-select", &["-p"], "Xcode CLI tools");
        check_cmd("xcodebuild", &["-version"], "xcodebuild (AU v3)");
        check_cmd("codesign", &["--help"], "codesign");
        match locate_wraptool_macos() {
            Some(p) => eprintln!("    ✅ wraptool (PACE) at {}", p.display()),
            None => eprintln!(
                "    ℹ️  wraptool not found — only needed for signed AAX builds. \
                 Install Eden via the iLok License Manager, then optionally \
                 `sudo ln -s /Applications/PACEAntiPiracy/Eden/Fusion/Current/bin/wraptool /usr/local/bin/wraptool`"
            ),
        }

        // Universal packaging (default for `cargo truce package`) needs both
        // Apple Rust targets. Missing targets are a warning, not an error —
        // `--host-only` still works without them.
        let has_x64 = rustup_has_target("x86_64-apple-darwin");
        let has_arm = rustup_has_target("aarch64-apple-darwin");
        match (has_x64, has_arm) {
            (true, true) => eprintln!(
                "    ✅ Rust targets: x86_64-apple-darwin + aarch64-apple-darwin — `cargo truce package` will produce universal Mach-O binaries"
            ),
            (false, true) => eprintln!(
                "    ⚠️  Rust target x86_64-apple-darwin missing — run: rustup target add x86_64-apple-darwin (or pass `--host-only` to skip)"
            ),
            (true, false) => eprintln!(
                "    ⚠️  Rust target aarch64-apple-darwin missing — run: rustup target add aarch64-apple-darwin (or pass `--host-only` to skip)"
            ),
            (false, false) => eprintln!(
                "    ⚠️  No Apple Rust targets installed — run: rustup target add x86_64-apple-darwin aarch64-apple-darwin (or pass `--host-only` to skip)"
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        eprintln!();
        eprintln!("  Windows");
        match locate_cmake() {
            Some(p) => eprintln!("    ✅ cmake (AAX template build): {}", p.display()),
            None => eprintln!(
                "    ❌ cmake.exe not found — install cmake or VS \"C++ CMake tools\""
            ),
        }
        match locate_ninja() {
            Some(p) => eprintln!("    ✅ ninja (AAX template build): {}", p.display()),
            None => eprintln!(
                "    ❌ ninja.exe not found — install ninja or VS \"C++ CMake tools\""
            ),
        }

        eprintln!();
        eprintln!("  Packaging (Windows)");
        packaging_windows::doctor();
    }

    // Compilers
    eprintln!();
    eprintln!("  Compilers");
    #[cfg(not(target_os = "windows"))]
    {
        check_cmd("cc", &["--version"], "C compiler");
        check_cmd("c++", &["--version"], "C++ compiler (VST3 shim)");
    }
    #[cfg(target_os = "windows")]
    {
        check_cmd("cl", &["/?"], "MSVC compiler (run from Developer Command Prompt)");
    }

    // Validation tools
    eprintln!();
    eprintln!("  Validation Tools");
    check_cmd("auval", &["-h"], "auval");
    check_which_with_env("pluginval", Some("PLUGINVAL"));
    check_which_with_env("clap-validator", Some("CLAP_VALIDATOR"));

    // Configuration
    eprintln!();
    eprintln!("  Configuration");
    let root = project_root();
    let config = if root.join("truce.toml").exists() {
        match load_config() {
            Ok(c) => {
                eprintln!("    ✅ truce.toml: {} plugins configured", c.plugin.len());
                Some(c)
            }
            Err(e) => {
                eprintln!("    ❌ truce.toml parse error: {e}");
                None
            }
        }
    } else {
        eprintln!("    ❌ truce.toml not found");
        None
    };

    // AAX SDK
    eprintln!();
    eprintln!("  SDKs");
    let aax_sdk = config.as_ref().and_then(resolve_aax_sdk_path);
    match aax_sdk {
        Some(p) => eprintln!("    ✅ AAX SDK at {}", p.display()),
        None => {
            let hint = if cfg!(target_os = "windows") { "[windows].aax_sdk_path" } else { "[macos].aax_sdk_path" };
            eprintln!("    ⚠️  AAX SDK not configured (set {hint} in truce.toml or AAX_SDK_PATH env var)");
        }
    }

    // Installed plugins
    eprintln!();
    eprintln!("  Installed Plugins");
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().unwrap_or_default();
        count_plugins(&home.join("Library/Audio/Plug-Ins/CLAP"), "CLAP");
        count_plugins(&Path::new("/Library/Audio/Plug-Ins/VST3").to_path_buf(), "VST3");
        count_plugins(&Path::new("/Library/Audio/Plug-Ins/Components").to_path_buf(), "AU v2");
        count_plugins(&home.join("Library/Audio/Plug-Ins/VST"), "VST2");
    }
    #[cfg(target_os = "windows")]
    {
        let cpf = common_program_files();
        let pf = program_files();
        count_plugins(&cpf.join("CLAP"), "CLAP");
        count_plugins(&cpf.join("VST3"), "VST3");
        count_plugins(&pf.join("Steinberg").join("VstPlugins"), "VST2");
        count_plugins(&cpf.join("Avid").join("Audio").join("Plug-Ins"), "AAX");
    }
    if root.join("rust-toolchain.toml").exists() {
        eprintln!("    ✅ rust-toolchain.toml present");
    }

    eprintln!();
    eprintln!("─────────────────────────────────────────");
    Ok(())
}


/// Like `check_which`, but consults `env_var` (process env, then
/// `.cargo/config.toml` `[env]`) before falling back to `$PATH`. Lets users
/// point doctor at tools installed outside `$PATH` — useful for `.app`-bundled
/// binaries (pluginval) or sibling source checkouts (clap-validator).
fn check_which_with_env(name: &str, env_var: Option<&str>) {
    if let Some(var) = env_var {
        if let Some(path) = std::env::var(var)
            .ok()
            .or_else(|| read_cargo_config_env(var))
        {
            let p = PathBuf::from(&path);
            if p.is_file() {
                eprintln!("    ✅ {name}: {path} (via ${var})");
                return;
            }
            eprintln!(
                "    ⚠️  {name}: ${var}={path} but file not found — falling back to $PATH"
            );
        }
    }
    match Command::new("which").arg(name).output() {
        Ok(o) if o.status.success() => {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            eprintln!("    ✅ {name}: {path}");
        }
        _ => {
            let hint = env_var
                .map(|v| format!(" (or set ${v} in shell or .cargo/config.toml [env])"))
                .unwrap_or_default();
            eprintln!("    ⚠️  {name}: not found{hint}");
        }
    }
}

fn count_plugins(dir: &PathBuf, label: &str) {
    if dir.exists() {
        let count = fs::read_dir(dir)
            .map(|entries| entries.filter(|e| e.is_ok()).count())
            .unwrap_or(0);
        eprintln!("    {label}: {count} items in {}", dir.display());
    } else {
        eprintln!("    {label}: directory not found");
    }
}
