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

use crate::format::Format;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallScope {
    User,
    System,
}

impl InstallScope {
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
impl std::str::FromStr for PkgScope {
    type Err = String;

    /// Parse the value side of `[packaging] preferred_scope = "..."`.
    /// `cargo truce install` has no toml override; only package
    /// supports it because the developer's choice at packaging time
    /// is the install-time UX an end user will see.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
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
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl PkgScope {
    /// `cargo truce package` default when no flag and no
    /// `[packaging] preferred_scope` is set: ask the end user.
    pub(crate) fn os_default() -> Self {
        Self::Ask
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

/// Resolve the per-format scope.
///
/// `requested` is the raw CLI choice: `Some` when the developer
/// passed `--user` / `--system`, `None` when they passed neither.
///
/// Two policies layered together:
/// - **Hard upgrade** - AAX, AU v3, and (on Windows) VST2 are
///   system-only. An explicit `--user` is downgraded to System with
///   a note (printed once per `cargo truce` invocation via
///   [`note_once`]).
/// - **Default selection** - when `requested` is `None`, picks the
///   per-(format, OS) default. Most combinations default to User to
///   keep the dev loop password-free; the exception is VST3 on
///   Windows, which defaults to System because that's the directory
///   every commercial host scans by convention (the per-user
///   `%LOCALAPPDATA%\Programs\Common\VST3` is supported but
///   uncommon outside developer machines).
pub(crate) fn effective_scope(
    format: Format,
    requested: Option<InstallScope>,
) -> (InstallScope, Option<&'static str>) {
    // Hard upgrades come first - they override both an explicit
    // `--user` and the per-format default.
    let hard_upgrade: Option<&'static str> = match format {
        Format::Aax => Some("AAX is system-only; ignoring --user"),
        Format::Au3 => Some("AU v3 is system-only; ignoring --user"),
        Format::Vst2 if cfg!(target_os = "windows") => {
            Some("VST2 on Windows is system-only; ignoring --user")
        }
        _ => None,
    };
    if let Some(msg) = hard_upgrade {
        // Note only fires when the user *asked* for User and got
        // overridden; staying silent when they passed --system or
        // nothing keeps the install log uncluttered.
        let note = if requested == Some(InstallScope::User) {
            Some(msg)
        } else {
            None
        };
        return (InstallScope::System, note);
    }

    if let Some(s) = requested {
        return (s, None);
    }

    // No explicit flag: per-(format, OS) default.
    if cfg!(target_os = "windows") && format == Format::Vst3 {
        return (
            InstallScope::System,
            Some("VST3 on Windows defaults to system scope; pass --user for per-user install"),
        );
    }
    (InstallScope::User, None)
}

/// Set the CLI scope slot, rejecting a second flag with a different
/// value. `cargo truce install` and `cargo truce uninstall` both accept
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
    use std::collections::HashSet;
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut g = SEEN.lock().unwrap();
    let seen = g.get_or_insert_with(HashSet::new);
    if seen.insert(message.to_string()) {
        eprintln!("note: {message}");
    }
}

// --- Per-format directory resolution ----------------------------------
//
// One impl block per OS. Toggling `--user` / `--system` rewrites the
// destination uniformly across formats because every install site
// reads through these helpers rather than hard-coding the path.

// Each of these unwraps a typed error from `crate::dirs`. Threading
// the `Result` out through every `_dir` method (and 30+ callers)
// would be a large refactor; the panic message is at least
// consistent with the `?`-form helper that other commands use.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home() -> PathBuf {
    crate::dirs::require_home_dir().expect("home directory required")
}

#[cfg(target_os = "windows")]
fn local_appdata() -> PathBuf {
    crate::dirs::require_local_appdata().expect("LOCALAPPDATA required")
}

#[cfg(target_os = "windows")]
fn appdata() -> PathBuf {
    crate::dirs::require_appdata().expect("APPDATA required")
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
    /// Directory the packager drops `<Plugin>.app` into for the
    /// standalone host. `Install for me only` relocates `/Applications`
    /// to `~/Applications`; `cargo truce package` lands the bundle in
    /// whichever the user picked.
    pub(crate) fn standalone_dir(self) -> PathBuf {
        match self {
            Self::User => home().join("Applications"),
            Self::System => PathBuf::from("/Applications"),
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
    // `self` is unused - Windows VST2 has no per-scope split. Kept on
    // `&self` for shape-symmetry with `clap_dir` / `vst3_dir` / `lv2_dir`
    // so callers don't need a special case for VST2.
    #[allow(clippy::unused_self)]
    pub(crate) fn vst2_dir(self) -> PathBuf {
        // Windows VST2 falls back to system in `effective_scope` -
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
