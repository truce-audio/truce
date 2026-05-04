//! Per-user vs per-machine plug-in install scope.
//!
//! Every plug-in install path flows through [`InstallScope`]; the
//! developer picks scope via `--user` / `--system` on
//! `cargo truce install`, with `--ask` added for `cargo truce
//! package` ([`PkgScope`]). Three formats with platform-specific
//! constraints silently fall back to system scope when `--user`
//! isn't reliably supported (AAX, AU v3, Windows VST2). See
//! [`effective_scope`] for the fallback policy.

use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallScope {
    User,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Clap,
    Vst3,
    Vst2,
    Lv2,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Au2,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Au3,
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    Aax,
}

impl InstallScope {
    /// Default install scope for the current OS when no CLI flag is
    /// set. User on every platform: avoids the password prompt in
    /// the dev loop and matches indie-installer convention.
    pub(crate) fn os_default() -> Self {
        Self::User
    }

    /// True when writing to this scope's plug-in directory needs
    /// elevated privileges. Drives whether install copies wrap in
    /// `run_sudo` (macOS) / fail-with-hint (Windows). Linux is
    /// always user-scope so this is always `false`.
    pub(crate) fn needs_sudo(self) -> bool {
        match self {
            Self::User => false,
            Self::System => cfg!(target_os = "macos") || cfg!(target_os = "windows"),
        }
    }
}

/// Distribution-installer scope for `cargo truce package`. Adds
/// `Ask` to [`InstallScope`] for the indie-installer default that
/// lets the end user pick at install time (macOS `Installer.app`
/// destination page; Inno Setup "Choose installation mode" page).
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PkgScope {
    User,
    System,
    Ask,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl PkgScope {
    /// `cargo truce package` default when no flag and no
    /// `[packaging] preferred_scope` is set: ask the end user.
    /// Matches indie-installer convention (u-he, Valhalla, `FabFilter`).
    pub(crate) fn os_default() -> Self {
        Self::Ask
    }

    /// Parse the value side of `[packaging] preferred_scope = "..."`.
    /// `cargo truce install` has no toml override; only package
    /// supports it because the developer's choice at packaging time
    /// is the install-time UX an end user will see.
    pub(crate) fn parse_toml_value(s: &str) -> Result<Self, String> {
        match s {
            "user" => Ok(Self::User),
            "system" => Ok(Self::System),
            "ask" => Ok(Self::Ask),
            other => Err(format!(
                "[packaging] preferred_scope: unknown value {other:?} \
                 (expected \"user\", \"system\", or \"ask\")"
            )),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Ask => "ask",
        }
    }

    /// Suffix appended to `target/dist/<plugin>-<version>-<platform>`
    /// so a `--user` and `--system` build of the same plugin don't
    /// overwrite each other in `dist/`. `--ask` (the default)
    /// produces the unsuffixed filename so existing release artefacts
    /// keep their canonical name.
    pub(crate) fn dist_suffix(self) -> &'static str {
        match self {
            Self::User => "-user",
            Self::System => "-system",
            Self::Ask => "",
        }
    }
}

/// Resolve the requested scope for one format, applying the per-format
/// guardrails from `docs/internal/install-scope.md`. AAX, AU v3, and
/// (on Windows) VST2 silently fall back to system scope and return a
/// note string the caller prints exactly once per `cargo truce`
/// invocation via [`note_once`].
pub(crate) fn effective_scope(
    format: Format,
    requested: InstallScope,
) -> (InstallScope, Option<&'static str>) {
    if requested == InstallScope::System {
        return (InstallScope::System, None);
    }
    match format {
        Format::Aax => (
            InstallScope::System,
            Some("AAX is system-only; ignoring --user"),
        ),
        Format::Au3 => (
            InstallScope::System,
            Some("AU v3 is system-only; ignoring --user"),
        ),
        Format::Vst2 if cfg!(target_os = "windows") => (
            InstallScope::System,
            Some("VST2 on Windows is system-only; ignoring --user"),
        ),
        _ => (InstallScope::User, None),
    }
}

/// Set the CLI scope slot, rejecting a second flag with a different
/// value. `cargo truce install` and `cargo truce remove` both accept
/// `--user` / `--system` and need the same mutual-exclusion check;
/// the helper centralizes the error message so both sites stay in
/// sync.
pub(crate) fn set_cli_install_scope(
    slot: &mut Option<InstallScope>,
    want: InstallScope,
) -> crate::Res {
    if let Some(prev) = *slot
        && prev != want
    {
        return Err("--user and --system are mutually exclusive".into());
    }
    *slot = Some(want);
    Ok(())
}

/// Print a per-message fallback note exactly once per `cargo truce`
/// invocation. Keeps the install log readable when `--user` covers
/// multiple plugins or formats that all hit the same guardrail.
pub(crate) fn note_once(message: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut g = SEEN.lock().unwrap();
    if g.iter().any(|s| s == message) {
        return;
    }
    g.push(message.to_string());
    eprintln!("note: {message}");
}

// --- Per-format directory resolution ----------------------------------
//
// One impl block per OS. Every site that previously hard-coded
// `/Library/Audio/Plug-Ins/...` or `%COMMONPROGRAMFILES%\...` reads
// these instead, so toggling `--user` / `--system` rewrites the
// destination uniformly across formats.

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home() -> PathBuf {
    crate::dirs::home_dir().expect("HOME not set")
}

#[cfg(target_os = "windows")]
fn local_appdata() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .expect("LOCALAPPDATA env var not set")
}

#[cfg(target_os = "windows")]
fn appdata() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .expect("APPDATA env var not set")
}

#[cfg(target_os = "macos")]
impl InstallScope {
    pub(crate) fn clap_dir(self) -> PathBuf {
        match self {
            Self::User => home().join("Library/Audio/Plug-Ins/CLAP"),
            Self::System => PathBuf::from("/Library/Audio/Plug-Ins/CLAP"),
        }
    }
    pub(crate) fn vst3_dir(self) -> PathBuf {
        match self {
            Self::User => home().join("Library/Audio/Plug-Ins/VST3"),
            Self::System => PathBuf::from("/Library/Audio/Plug-Ins/VST3"),
        }
    }
    pub(crate) fn vst2_dir(self) -> PathBuf {
        match self {
            Self::User => home().join("Library/Audio/Plug-Ins/VST"),
            Self::System => PathBuf::from("/Library/Audio/Plug-Ins/VST"),
        }
    }
    pub(crate) fn lv2_dir(self) -> PathBuf {
        match self {
            Self::User => home().join("Library/Audio/Plug-Ins/LV2"),
            Self::System => PathBuf::from("/Library/Audio/Plug-Ins/LV2"),
        }
    }
    pub(crate) fn au_v2_dir(self) -> PathBuf {
        match self {
            Self::User => home().join("Library/Audio/Plug-Ins/Components"),
            Self::System => PathBuf::from("/Library/Audio/Plug-Ins/Components"),
        }
    }
}

#[cfg(target_os = "windows")]
impl InstallScope {
    pub(crate) fn clap_dir(self) -> PathBuf {
        match self {
            Self::User => local_appdata().join(r"Programs\Common\CLAP"),
            Self::System => crate::common_program_files().join("CLAP"),
        }
    }
    pub(crate) fn vst3_dir(self) -> PathBuf {
        match self {
            Self::User => local_appdata().join(r"Programs\Common\VST3"),
            Self::System => crate::common_program_files().join("VST3"),
        }
    }
    // `self` is unused — Windows VST2 has no per-scope split. Kept on
    // `&self` for shape-symmetry with `clap_dir` / `vst3_dir` / `lv2_dir`
    // so callers don't need a special case for VST2.
    #[allow(clippy::unused_self)]
    pub(crate) fn vst2_dir(self) -> PathBuf {
        // Windows VST2 falls back to system in `effective_scope` —
        // keep the user arm wired to the system path so an unfiltered
        // `--user` invocation still resolves to a real directory if
        // something bypasses `effective_scope`.
        crate::program_files().join("Steinberg").join("VstPlugins")
    }
    pub(crate) fn lv2_dir(self) -> PathBuf {
        match self {
            Self::User => appdata().join("LV2"),
            Self::System => crate::common_program_files().join("LV2"),
        }
    }
}

#[cfg(target_os = "linux")]
impl InstallScope {
    // Linux is user-scope only; `--system` is accepted for symmetry
    // with macOS / Windows but resolves to the same paths every host
    // already scans (`~/.clap`, `~/.vst3`, …).

    pub(crate) fn clap_dir(self) -> PathBuf {
        let _ = self;
        home().join(".clap")
    }
    pub(crate) fn vst3_dir(self) -> PathBuf {
        let _ = self;
        home().join(".vst3")
    }
    pub(crate) fn vst2_dir(self) -> PathBuf {
        let _ = self;
        home().join(".vst")
    }
    pub(crate) fn lv2_dir(self) -> PathBuf {
        let _ = self;
        home().join(".lv2")
    }
}
