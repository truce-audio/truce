//! `cargo truce doctor` — environment diagnostics: Rust toolchain, code
//! signing tools, AAX SDK, installed plugins.

#![allow(unused_imports)]

use crate::config::read_cargo_config_env;
use crate::install_scope::InstallScope;
#[cfg(target_os = "macos")]
use crate::locate_wraptool_macos;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::rustup_has_target;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::tag_info;
use crate::{
    Res, check_cmd, dirs, load_config, project_root, resolve_aax_sdk_path, tag_fail, tag_ok,
    tag_warn,
};
#[cfg(target_os = "windows")]
use crate::{
    common_program_files, locate_cmake, locate_msvc_cl, locate_ninja, packaging_windows,
    program_files, which_exe,
};
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

    let root = project_root();

    // Toolchain
    eprintln!("  Toolchain");
    check_cmd("rustc", &["--version"], "rustc");
    check_cmd("cargo", &["--version"], "cargo");
    if root.join("rust-toolchain.toml").exists() {
        eprintln!("    {} rust-toolchain.toml present", tag_ok());
    }

    // Platform tools
    #[cfg(target_os = "macos")]
    {
        eprintln!();
        eprintln!("  macOS");
        check_cmd("xcode-select", &["-p"], "Xcode CLI tools");
        check_cmd("xcodebuild", &["-version"], "xcodebuild (AU v3)");
        check_cmd("codesign", &["--help"], "codesign");
        match locate_wraptool_macos() {
            Some(p) => eprintln!("    {} wraptool (PACE) at {}", tag_ok(), p.display()),
            None => eprintln!(
                "    {} wraptool not found — only needed for signed AAX builds. \
                 Install Eden via the iLok License Manager, then optionally \
                 `sudo ln -s /Applications/PACEAntiPiracy/Eden/Fusion/Current/bin/wraptool /usr/local/bin/wraptool`",
                tag_info()
            ),
        }

        // Universal packaging (default for `cargo truce package`) needs both
        // Apple Rust targets. Missing targets are a warning, not an error —
        // `--host-only` still works without them.
        let has_x64 = rustup_has_target("x86_64-apple-darwin");
        let has_arm = rustup_has_target("aarch64-apple-darwin");
        match (has_x64, has_arm) {
            (true, true) => eprintln!(
                "    {} Rust targets: x86_64-apple-darwin + aarch64-apple-darwin — `cargo truce package` will produce universal Mach-O binaries",
                tag_ok()
            ),
            (false, true) => eprintln!(
                "    {} Rust target x86_64-apple-darwin missing — run: rustup target add x86_64-apple-darwin (or pass `--host-only` to skip)",
                tag_warn()
            ),
            (true, false) => eprintln!(
                "    {} Rust target aarch64-apple-darwin missing — run: rustup target add aarch64-apple-darwin (or pass `--host-only` to skip)",
                tag_warn()
            ),
            (false, false) => eprintln!(
                "    {} No Apple Rust targets installed — run: rustup target add x86_64-apple-darwin aarch64-apple-darwin (or pass `--host-only` to skip)",
                tag_warn()
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        eprintln!();
        eprintln!("  Windows");
        match locate_cmake() {
            Some(p) => eprintln!(
                "    {} cmake (AAX template build): {}",
                tag_ok(),
                p.display()
            ),
            None => eprintln!(
                "    {} cmake.exe not found — install cmake or VS \"C++ CMake tools\"",
                tag_fail()
            ),
        }
        match locate_ninja() {
            Some(p) => eprintln!(
                "    {} ninja (AAX template build): {}",
                tag_ok(),
                p.display()
            ),
            None => eprintln!(
                "    {} ninja.exe not found — install ninja or VS \"C++ CMake tools\"",
                tag_fail()
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
        // `cl.exe` is only on PATH inside a Developer Command Prompt, but Rust
        // (cc-rs) and CMake both auto-discover MSVC via vswhere — so the bare
        // PATH check would falsely flag the tool as missing on a perfectly
        // working setup. Try PATH first, then fall back to vswhere.
        if which_exe("cl.exe").is_some() {
            check_cmd("cl", &["/?"], "MSVC compiler (in current PATH)");
        } else {
            match locate_msvc_cl() {
                Some(p) => eprintln!(
                    "    {} MSVC compiler at {} (not in PATH — Rust/CMake auto-discover it via vswhere)",
                    tag_ok(),
                    p.display()
                ),
                None => eprintln!(
                    "    {} MSVC compiler: not found — install VS \"Desktop development with C++\" workload",
                    tag_fail()
                ),
            }
        }
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
    let config = if root.join("truce.toml").exists() {
        match load_config() {
            Ok(c) => {
                eprintln!(
                    "    {} truce.toml: {} plugins configured",
                    tag_ok(),
                    c.plugin.len()
                );
                Some(c)
            }
            Err(e) => {
                eprintln!("    {} truce.toml parse error: {e}", tag_fail());
                None
            }
        }
    } else {
        eprintln!("    {} truce.toml not found", tag_fail());
        None
    };

    // AAX SDK
    eprintln!();
    eprintln!("  SDKs");
    let aax_sdk = config.as_ref().and_then(resolve_aax_sdk_path);
    match aax_sdk {
        Some(p) => eprintln!("    {} AAX SDK at {}", tag_ok(), p.display()),
        None => {
            let hint = if cfg!(target_os = "windows") {
                "[windows].aax_sdk_path"
            } else {
                "[macos].aax_sdk_path"
            };
            eprintln!(
                "    {} AAX SDK not configured (set {hint} in truce.toml or AAX_SDK_PATH env var)",
                tag_warn()
            );
        }
    }

    // Plugin install paths — both scopes side-by-side per format.
    // Helps a user who's confused about why a host is finding the
    // wrong copy of their plugin: when the same name appears under
    // both scopes, the host picks one and shadows the other.
    eprintln!();
    eprintln!("  Plugin install paths");
    show_scope_paths();

    eprintln!();
    eprintln!("─────────────────────────────────────────");
    Ok(())
}

#[derive(Clone, Copy)]
struct PathFormat {
    label: &'static str,
    /// File extension or bundle suffix. Used to count plug-ins in
    /// the directory and to detect cross-scope collisions in
    /// `cargo truce validate`.
    ext: &'static str,
}

const PATH_FORMATS_MACOS: &[PathFormat] = &[
    PathFormat {
        label: "CLAP",
        ext: "clap",
    },
    PathFormat {
        label: "VST3",
        ext: "vst3",
    },
    PathFormat {
        label: "VST2",
        ext: "vst",
    },
    PathFormat {
        label: "LV2",
        ext: "lv2",
    },
    PathFormat {
        label: "AU v2",
        ext: "component",
    },
];

const PATH_FORMATS_WINDOWS: &[PathFormat] = &[
    PathFormat {
        label: "CLAP",
        ext: "clap",
    },
    PathFormat {
        label: "VST3",
        ext: "vst3",
    },
    PathFormat {
        label: "VST2",
        ext: "dll",
    },
    PathFormat {
        label: "LV2",
        ext: "lv2",
    },
];

const PATH_FORMATS_LINUX: &[PathFormat] = &[
    PathFormat {
        label: "CLAP",
        ext: "clap",
    },
    PathFormat {
        label: "VST3",
        ext: "vst3",
    },
    PathFormat {
        label: "VST2",
        ext: "so",
    },
    PathFormat {
        label: "LV2",
        ext: "lv2",
    },
];

fn show_scope_paths() {
    let formats: &[PathFormat] = if cfg!(target_os = "macos") {
        PATH_FORMATS_MACOS
    } else if cfg!(target_os = "windows") {
        PATH_FORMATS_WINDOWS
    } else {
        PATH_FORMATS_LINUX
    };

    for f in formats {
        let user_path = scope_path_for(f.label, InstallScope::User);
        let system_path = scope_path_for(f.label, InstallScope::System);
        report_scope_line(f, "user", InstallScope::User, &user_path);
        // Linux's user and system dirs resolve to the same path —
        // skip the duplicate row to keep the matrix readable.
        if user_path != system_path {
            report_scope_line(f, "system", InstallScope::System, &system_path);
        }
    }

    // AAX is system-only; AU v3 is system-only and lives in
    // /Applications/. Both have a single canonical location.
    #[cfg(target_os = "macos")]
    {
        report_fixed(
            "AAX",
            "system",
            true,
            &PathBuf::from("/Library/Application Support/Avid/Audio/Plug-Ins"),
            "aaxplugin",
        );
        report_fixed(
            "AU v3",
            "system",
            true,
            &PathBuf::from("/Applications"),
            // /Applications/ also holds every non-plug-in Mac app,
            // so a count by ".app" would be meaningless — skip it.
            "",
        );
    }
    #[cfg(target_os = "windows")]
    {
        let aax_dir = common_program_files()
            .join("Avid")
            .join("Audio")
            .join("Plug-Ins");
        report_fixed("AAX", "system", true, &aax_dir, "aaxplugin");
    }
}

/// Resolve the same path the install / remove / package commands use
/// for `(format, scope)`. Falls through to user-scope on Linux for the
/// unsupported `system` arm (Linux is user-only).
fn scope_path_for(label: &str, scope: InstallScope) -> PathBuf {
    match label {
        "CLAP" => scope.clap_dir(),
        "VST3" => scope.vst3_dir(),
        "VST2" => scope.vst2_dir(),
        "LV2" => scope.lv2_dir(),
        #[cfg(target_os = "macos")]
        "AU v2" => scope.au_v2_dir(),
        _ => PathBuf::new(),
    }
}

fn report_scope_line(f: &PathFormat, scope_label: &str, scope: InstallScope, path: &Path) {
    let label = format!("{} {}:", f.label, scope_label);
    report_path_line(&label, scope.needs_sudo(), path, f.ext);
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn report_fixed(format_label: &str, scope_label: &str, needs_sudo: bool, path: &Path, ext: &str) {
    let label = format!("{} {}:", format_label, scope_label);
    report_path_line(&label, needs_sudo, path, ext);
}

fn report_path_line(label: &str, needs_sudo: bool, path: &Path, ext: &str) {
    if !path.exists() {
        eprintln!(
            "    {label:<14} {}{}— not present",
            display_path(path),
            spacer(path)
        );
        return;
    }
    let count_str = if ext.is_empty() {
        // Path is shared with non-plug-in content (e.g.
        // `/Applications/` for AU v3) — counting `.app`s would
        // include every Mac app on disk. Skip the count.
        String::new()
    } else {
        let count = count_bundles_with_ext(path, ext);
        let plural = if count == 1 { "" } else { "s" };
        format!(" ({count} plug-in{plural})")
    };
    let state = if needs_sudo {
        format!("{} needs sudo{count_str}", tag_warn())
    } else if path_is_writable(path) {
        format!("{} writable{count_str}", tag_ok())
    } else {
        format!("{} not writable{count_str}", tag_warn())
    };
    eprintln!(
        "    {label:<14} {}{}{state}",
        display_path(path),
        spacer(path)
    );
}

/// Tab-aligned spacer between the path and the state column.
/// Pads the path column to ~46 chars so the state markers line up
/// regardless of how long the resolved path is.
fn spacer(path: &Path) -> String {
    let width = display_path(path).chars().count();
    let target = 46usize;
    if width < target {
        " ".repeat(target - width)
    } else {
        "  ".to_string()
    }
}

/// Render `path` as `~/...` when it's under `$HOME`, otherwise as
/// the absolute path. Keeps the matrix readable on macOS / Linux
/// where the user-scope paths all begin with the home directory.
fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    path.display().to_string()
}

fn count_bundles_with_ext(dir: &Path, ext: &str) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let want = format!(".{ext}");
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .to_lowercase()
                .ends_with(&want.to_lowercase())
        })
        .count()
}

/// Probe writability by trying to create a tempfile in `dir`. Avoids
/// false positives from `metadata().permissions().readonly()` which
/// only reports the file mode, not the effective access — system
/// dirs on macOS are 0755 and would read as "writable" without
/// surfacing the sudo requirement.
fn path_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".truce-doctor-write-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Like `check_which`, but consults `env_var` (process env, then
/// `.cargo/config.toml` `[env]`) before falling back to `$PATH`. Lets users
/// point doctor at tools installed outside `$PATH` — useful for `.app`-bundled
/// binaries (pluginval) or sibling source checkouts (clap-validator).
fn check_which_with_env(name: &str, env_var: Option<&str>) {
    if let Some(var) = env_var
        && let Some(path) = std::env::var(var)
            .ok()
            .or_else(|| read_cargo_config_env(var))
        {
            let p = PathBuf::from(&path);
            if p.is_file() {
                eprintln!("    {} {name}: {path} (via ${var})", tag_ok());
                return;
            }
            eprintln!(
                "    {} {name}: ${var}={path} but file not found — falling back to $PATH",
                tag_warn()
            );
        }
    match Command::new("which").arg(name).output() {
        Ok(o) if o.status.success() => {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            eprintln!("    {} {name}: {path}", tag_ok());
        }
        _ => {
            let hint = env_var
                .map(|v| format!(" (or set ${v} in shell or .cargo/config.toml [env])"))
                .unwrap_or_default();
            eprintln!("    {} {name}: not found{hint}", tag_warn());
        }
    }
}
