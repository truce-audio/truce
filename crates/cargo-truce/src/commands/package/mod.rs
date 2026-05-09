//! `cargo truce package` — build, sign, and create installers.
//!
//! Top-level dispatch + the format-flag parsing shared between the macOS
//! `.pkg` pipeline (`macos.rs`) and the Windows Inno Setup pipeline
//! (`packaging_windows`).

use crate::BoxErr;
#[cfg(target_os = "macos")]
use crate::PluginDef;
use crate::Res;

pub(crate) mod stage;
pub(crate) mod verify;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

// Linux module always compiles so cross-platform CI catches compile
// errors on macOS / Windows runs. The dispatcher in `cmd_package`
// gates which `cmd_package_*` is invoked at runtime.
#[allow(dead_code)]
pub(crate) mod linux;

/// Composable selection flags shared across platforms. Parsed once at
/// top-level and passed down to the per-platform packagers so each
/// dispatch path applies them uniformly.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#[derive(Clone, Default, Debug)]
pub(crate) struct SuiteSelection {
    /// `--suite <name>` (repeatable). Empty = every declared suite.
    pub(crate) only_suites: Vec<String>,
    /// `--no-suite`: skip all suite installers.
    pub(crate) no_suite: bool,
    /// `--no-per-plugin`: skip per-plugin installers.
    pub(crate) no_per_plugin: bool,
}

impl SuiteSelection {
    pub(crate) fn want_per_plugin(&self) -> bool {
        !self.no_per_plugin
    }
    pub(crate) fn want_suite(&self, name: &str) -> bool {
        if self.no_suite {
            return false;
        }
        if self.only_suites.is_empty() {
            return true;
        }
        self.only_suites.iter().any(|s| s == name)
    }
}

/// Strip the suite-selection flags out of `args` before the
/// per-platform parser sees them. Returns the parsed selection plus a
/// new `args` vector with those flags removed.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn extract_suite_selection(
    args: &[String],
) -> Result<(SuiteSelection, Vec<String>), BoxErr> {
    let mut sel = SuiteSelection::default();
    let mut remaining = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--suite" => {
                i += 1;
                let v = args.get(i).ok_or("--suite requires a name")?;
                sel.only_suites.push(v.clone());
            }
            "--no-suite" => sel.no_suite = true,
            "--no-per-plugin" => sel.no_per_plugin = true,
            other => remaining.push(other.to_string()),
        }
        i += 1;
    }
    if sel.no_suite && !sel.only_suites.is_empty() {
        return Err("--no-suite and --suite <name> are contradictory".into());
    }
    if sel.no_suite && sel.no_per_plugin {
        return Err("--no-suite and --no-per-plugin together produce no output".into());
    }
    Ok((sel, remaining))
}

/// Parsed format flags for the package command.
/// Used by both `cmd_package_macos` and `packaging_windows::cmd_package`.
#[derive(Clone, PartialEq)]
pub(crate) enum PkgFormat {
    Clap,
    Vst3,
    Vst2,
    Au2,
    Au3,
    Aax,
    /// Standalone host application built from the plugin's
    /// `[features].standalone`. Installs to `/Applications/` on macOS,
    /// `%PROGRAMFILES%\<Vendor>\<Plugin>\` on Windows, `/usr/bin/` (or
    /// user equivalent) on Linux.
    Standalone,
}

impl std::str::FromStr for PkgFormat {
    type Err = BoxErr;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "clap" => Ok(PkgFormat::Clap),
            "vst3" => Ok(PkgFormat::Vst3),
            "vst2" => Ok(PkgFormat::Vst2),
            "au2" => Ok(PkgFormat::Au2),
            "au3" => Ok(PkgFormat::Au3),
            "aax" => Ok(PkgFormat::Aax),
            "standalone" => Ok(PkgFormat::Standalone),
            other => Err(format!("unknown format: {other}").into()),
        }
    }
}

impl PkgFormat {
    /// Comma-separated list parser. Each token is fed through
    /// [`PkgFormat::from_str`] (the `FromStr` impl above), so an
    /// unknown token surfaces a "unknown format: …" error rather
    /// than a generic parse failure.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn parse_list(s: &str) -> Result<Vec<PkgFormat>, BoxErr> {
        s.split(',').map(|t| t.trim().parse()).collect()
    }

    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn label(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "CLAP",
            PkgFormat::Vst3 => "VST3",
            PkgFormat::Vst2 => "VST2",
            PkgFormat::Au2 => "AU2",
            PkgFormat::Au3 => "AU3",
            PkgFormat::Aax => "AAX",
            PkgFormat::Standalone => "Standalone",
        }
    }

    /// Cargo feature flag name corresponding to this format.
    /// Used by feature-detection in `resolve_formats` to match against
    /// each plugin's `[features].default`. Linux pipeline plugs into
    /// this in a follow-up; for now only macOS/Windows reach for it.
    #[allow(dead_code)]
    pub(crate) fn feature_name(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst2",
            PkgFormat::Au2 | PkgFormat::Au3 => "au",
            PkgFormat::Aax => "aax",
            PkgFormat::Standalone => "standalone",
        }
    }
}

// macOS-only `pkgbuild` / `productbuild` plumbing — extensions,
// install paths, PkgID suffixes, AU3 `.app` naming. Windows packaging
// drives Inno Setup directly and doesn't need any of this.
#[cfg(target_os = "macos")]
impl PkgFormat {
    pub(crate) fn extension(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst",
            PkgFormat::Au2 => "component",
            PkgFormat::Au3 | PkgFormat::Standalone => "app",
            PkgFormat::Aax => "aaxplugin",
        }
    }

    pub(crate) fn install_location(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "/Library/Audio/Plug-Ins/CLAP/",
            PkgFormat::Vst3 => "/Library/Audio/Plug-Ins/VST3/",
            PkgFormat::Vst2 => "/Library/Audio/Plug-Ins/VST/",
            PkgFormat::Au2 => "/Library/Audio/Plug-Ins/Components/",
            PkgFormat::Au3 | PkgFormat::Standalone => "/Applications/",
            PkgFormat::Aax => "/Library/Application Support/Avid/Audio/Plug-Ins/",
        }
    }

    pub(crate) fn pkg_id_suffix(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst2",
            PkgFormat::Au2 => "au2",
            PkgFormat::Au3 => "au3",
            PkgFormat::Aax => "aax",
            PkgFormat::Standalone => "standalone",
        }
    }

    /// Whether pkgbuild recognizes this as a native macOS bundle type.
    /// If false, we use --root instead of --component.
    pub(crate) fn is_native_bundle(&self) -> bool {
        matches!(
            self,
            PkgFormat::Vst3 | PkgFormat::Au2 | PkgFormat::Au3 | PkgFormat::Standalone,
        )
    }

    /// Bundle directory name for a given plugin.
    pub(crate) fn bundle_name(&self, plugin: &PluginDef) -> String {
        match self {
            PkgFormat::Au3 => format!("{}.app", plugin.au3_app_name()),
            // Plain `<Plugin>.app` so Spotlight / Launch Services
            // index it as a regular application. The historical
            // `<Plugin>.standalone.app` extension confused some
            // indexing paths and the bundle would not appear in
            // Spotlight search. AU3 stays distinct via its `v3`
            // suffix (or the user's `au3_name` override), so there's
            // no `/Applications/` collision.
            PkgFormat::Standalone => format!("{}.app", plugin.name),
            _ => format!("{}.{}", plugin.name, self.extension()),
        }
    }

    pub(crate) fn choice_description(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "For Reaper, Bitwig",
            PkgFormat::Vst3 => "For Ableton, FL Studio, Reaper, Cubase",
            PkgFormat::Vst2 => "Legacy — for hosts without VST3 support",
            PkgFormat::Au2 => "For Logic Pro, GarageBand, Ableton",
            PkgFormat::Au3 => "Audio Unit v3 (appex)",
            PkgFormat::Aax => "For Pro Tools",
            PkgFormat::Standalone => "Run as a desktop app (no DAW required)",
        }
    }
}

// `args` is unused on platforms where the body falls through to the
// "not supported" Err branch — silence the unused-variable warning
// only on those targets.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(unused_variables)
)]
pub(crate) fn cmd_package(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    let (selection, args) = extract_suite_selection(args)?;
    #[cfg(target_os = "windows")]
    {
        crate::packaging_windows::cmd_package(&args, &selection)
    }
    #[cfg(target_os = "macos")]
    {
        macos::cmd_package_macos(&args, &selection)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        linux::cmd_package_linux(&args, &selection)
    }
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn _suppress_linux_unused() {
    // Reference linux::cmd_package_linux so the Linux pipeline gets
    // compile-checked on macOS / Windows builds without a runtime
    // path. Keeping the linux module always-compiled (and this fn
    // suppressed-dead-code) catches drift on the platform we don't
    // actually run on at dev time.
    let _ = linux::cmd_package_linux;
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce package [-p <crate>] [--suite <name>] \
[--no-suite|--no-per-plugin] [--formats <list>] \
[--user|--system|--ask] [--no-notarize] [--no-sign|--no-pace-sign] \
[--host-only|--universal]

Build, sign, and package plugins into installers:
  - macOS:   `target/dist/<Name>-<version>-<platform>.pkg` (productbuild)
  - Windows: `target/dist/<Name>-<version>-<platform>.exe` (Inno Setup)
  - Linux:   `target/dist/<Name>-<version>-<platform>.tar.gz` + install.sh

Suites: declare one or more `[[suite]]` entries in truce.toml to bundle
multiple plugins into a single installer per platform. With suites
declared, `cargo truce package` produces both per-plugin installers and
per-suite installers by default. See docs for the schema.

Selection (composable):
  -p <crate>           Package only this plugin crate's installer.
  --suite <name>       Package only this suite (repeatable).
  --no-suite           Skip suite installers.
  --no-per-plugin      Skip per-plugin installers.

Format selection:
  --formats <list>     Comma-separated subset
                       (clap,vst3,vst2,au2,au3,aax,standalone).
                       Default: every format in the plugin's
                       `[features].default`.

Install scope (where the resulting installer puts files at the end user's machine):
  --ask                End user picks at install time. Default.
  --user               User-scope. CLAP/VST3 land in user paths with no
                       admin prompt. System-only formats (AAX, AU v3, Windows
                       VST2) stay system-scope; the user sees one admin prompt.
  --system             Hard-lock to system paths.
  Override the default project-wide via `[packaging] preferred_scope` in truce.toml.

Signing / notarization (macOS / Windows):
  --no-notarize        Skip macOS notarization (still codesigns).
  --no-pace-sign       Skip PACE (AAX) signing — useful for non-Pro Tools
                       sanity checks. Apple codesign always runs on macOS.
  --no-sign            Synonym for --no-pace-sign on macOS.

Build target (macOS):
  --host-only          Single-arch build of the host. Default is universal.
  --universal          Explicit universal (no-op; same as default).

Build invocation (Linux):
  --no-build           Skip the implicit `cargo truce build` and use the
                       existing `target/bundles/` manifest as-is. Errors
                       if no manifest is present.

Misc:
  -h, --help           Show this message."
    );
}

#[cfg(test)]
mod selection_tests {
    use super::extract_suite_selection;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn empty_args_default_selection() {
        let (sel, rest) = extract_suite_selection(&[]).unwrap();
        assert!(sel.want_per_plugin());
        assert!(sel.want_suite("studio"));
        assert!(rest.is_empty());
    }

    #[test]
    fn suite_filter_keeps_only_named_suites() {
        let (sel, rest) =
            extract_suite_selection(&s(&["--suite", "studio", "--suite", "free"])).unwrap();
        assert!(sel.want_suite("studio"));
        assert!(sel.want_suite("free"));
        assert!(!sel.want_suite("midi-tools"));
        assert!(rest.is_empty());
    }

    #[test]
    fn no_suite_drops_every_suite() {
        let (sel, _) = extract_suite_selection(&s(&["--no-suite"])).unwrap();
        assert!(!sel.want_suite("studio"));
        assert!(sel.want_per_plugin());
    }

    #[test]
    fn no_per_plugin_keeps_suites() {
        let (sel, _) = extract_suite_selection(&s(&["--no-per-plugin"])).unwrap();
        assert!(!sel.want_per_plugin());
        assert!(sel.want_suite("studio"));
    }

    #[test]
    fn other_flags_pass_through() {
        let (sel, rest) = extract_suite_selection(&s(&[
            "-p",
            "truce-gain",
            "--suite",
            "studio",
            "--no-notarize",
        ]))
        .unwrap();
        assert_eq!(rest, s(&["-p", "truce-gain", "--no-notarize"]));
        assert!(sel.want_suite("studio"));
    }

    #[test]
    fn no_suite_and_no_per_plugin_together_errors() {
        let err = extract_suite_selection(&s(&["--no-suite", "--no-per-plugin"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no output"), "got: {err}");
    }

    #[test]
    fn no_suite_with_explicit_suite_errors() {
        let err = extract_suite_selection(&s(&["--no-suite", "--suite", "x"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("contradictory"), "got: {err}");
    }
}
