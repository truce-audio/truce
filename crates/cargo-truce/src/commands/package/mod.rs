//! `cargo truce package` - build, sign, and create installers.
//!
//! Top-level dispatch + the format-flag parsing shared between the macOS
//! `.pkg` pipeline (`macos.rs`), the Linux tarball pipeline (`linux.rs`),
//! and the Windows Inno Setup pipeline (`windows.rs`).

use crate::CargoTruceError;
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

// Windows packager + its DPI/icon manifest helper. Gated on
// `windows` because both pull in `windows_sys` APIs that don't exist
// on other targets (unlike the pure-std `linux` module).
#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "windows")]
pub(crate) mod windows_manifest;

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
) -> Result<(SuiteSelection, Vec<String>), CargoTruceError> {
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
/// Used by both `cmd_package_macos` and `windows::cmd_package_windows`.
#[derive(Clone, PartialEq)]
pub(crate) enum PkgFormat {
    Clap,
    Vst3,
    Vst2,
    Lv2,
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
    type Err = CargoTruceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "clap" => Ok(PkgFormat::Clap),
            "vst3" => Ok(PkgFormat::Vst3),
            "vst2" => Ok(PkgFormat::Vst2),
            "lv2" => Ok(PkgFormat::Lv2),
            "au2" => Ok(PkgFormat::Au2),
            "au3" => Ok(PkgFormat::Au3),
            "aax" => Ok(PkgFormat::Aax),
            "standalone" => Ok(PkgFormat::Standalone),
            other => Err(format!("unknown format: {other}").into()),
        }
    }
}

/// Per-format static metadata. One row per `PkgFormat` variant; adding
/// a new format means adding one row to [`PKG_FORMAT_META`] plus a
/// matching enum variant + `FromStr` arm.
///
/// Cross-platform rows. `extension`, `install_location`, etc. are
/// macOS-specific values (the `pkgbuild` / `productbuild` pipeline);
/// they're harmless string literals on Linux / Windows and the methods
/// that read them are gated on `cfg(target_os = "macos")`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct PkgFormatMeta {
    label: &'static str,
    pkg_id_suffix: &'static str,
    extension: &'static str,
    install_location: &'static str,
    is_native_bundle: bool,
    choice_description: &'static str,
}

const PKG_FORMAT_META: [(PkgFormat, PkgFormatMeta); 8] = [
    (
        PkgFormat::Clap,
        PkgFormatMeta {
            label: "CLAP",
            pkg_id_suffix: "clap",
            extension: "clap",
            install_location: "/Library/Audio/Plug-Ins/CLAP/",
            // macOS CLAP is a loadable bundle (`Contents/MacOS/<name>`
            // + `Info.plist`) - pkgbuild gets the same component-plist
            // treatment as VST3 / AU so the installer pins it to the
            // declared install_location and upgrades by bundle ID.
            is_native_bundle: true,
            choice_description: "For Reaper, Bitwig",
        },
    ),
    (
        PkgFormat::Vst3,
        PkgFormatMeta {
            label: "VST3",
            pkg_id_suffix: "vst3",
            extension: "vst3",
            install_location: "/Library/Audio/Plug-Ins/VST3/",
            is_native_bundle: true,
            choice_description: "For Ableton, FL Studio, Reaper, Cubase",
        },
    ),
    (
        PkgFormat::Vst2,
        PkgFormatMeta {
            label: "VST2",
            pkg_id_suffix: "vst2",
            extension: "vst",
            install_location: "/Library/Audio/Plug-Ins/VST/",
            is_native_bundle: false,
            choice_description: "Legacy - for hosts without VST3 support",
        },
    ),
    (
        PkgFormat::Lv2,
        PkgFormatMeta {
            label: "LV2",
            pkg_id_suffix: "lv2",
            extension: "lv2",
            // macOS LV2 plugins live alongside the other formats in
            // the Audio Plug-Ins root; the bundle itself is a
            // directory with the `.lv2` extension. Reaper, Ardour,
            // Bitwig pick them up from here.
            install_location: "/Library/Audio/Plug-Ins/LV2/",
            // LV2 bundles are plain directories (not macOS-style
            // bundle blobs with `Info.plist`) - `pkgbuild` should
            // recurse into them like any other folder of files.
            is_native_bundle: false,
            choice_description: "For Ardour, Bitwig, Reaper, and Linux DAWs",
        },
    ),
    (
        PkgFormat::Au2,
        PkgFormatMeta {
            label: "AU2",
            pkg_id_suffix: "au2",
            extension: "component",
            install_location: "/Library/Audio/Plug-Ins/Components/",
            is_native_bundle: true,
            choice_description: "For Logic Pro, GarageBand, Ableton",
        },
    ),
    (
        PkgFormat::Au3,
        PkgFormatMeta {
            label: "AU3",
            pkg_id_suffix: "au3",
            extension: "app",
            install_location: "/Applications/",
            is_native_bundle: true,
            choice_description: "Audio Unit v3 (appex)",
        },
    ),
    (
        PkgFormat::Aax,
        PkgFormatMeta {
            label: "AAX",
            pkg_id_suffix: "aax",
            extension: "aaxplugin",
            install_location: "/Library/Application Support/Avid/Audio/Plug-Ins/",
            is_native_bundle: false,
            choice_description: "For Pro Tools",
        },
    ),
    (
        PkgFormat::Standalone,
        PkgFormatMeta {
            label: "Standalone",
            pkg_id_suffix: "standalone",
            extension: "app",
            install_location: "/Applications/",
            is_native_bundle: true,
            choice_description: "Run as a desktop app (no DAW required)",
        },
    ),
];

impl PkgFormat {
    /// Comma-separated list parser. Each token is fed through
    /// `PkgFormat::from_str` (the `FromStr` impl above), so an
    /// unknown token surfaces a "unknown format: …" error rather
    /// than a generic parse failure.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn parse_list(s: &str) -> Result<Vec<PkgFormat>, CargoTruceError> {
        s.split(',').map(|t| t.trim().parse()).collect()
    }

    fn meta(&self) -> &'static PkgFormatMeta {
        &PKG_FORMAT_META
            .iter()
            .find(|(f, _)| f == self)
            .expect("PKG_FORMAT_META is exhaustive over PkgFormat")
            .1
    }

    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn label(&self) -> &'static str {
        self.meta().label
    }
}

// macOS-only `pkgbuild` / `productbuild` plumbing. Windows packaging
// drives Inno Setup directly and doesn't need any of this.
#[cfg(target_os = "macos")]
impl PkgFormat {
    pub(crate) fn extension(&self) -> &'static str {
        self.meta().extension
    }

    pub(crate) fn install_location(&self) -> &'static str {
        self.meta().install_location
    }

    pub(crate) fn pkg_id_suffix(&self) -> &'static str {
        self.meta().pkg_id_suffix
    }

    /// Whether pkgbuild recognizes this as a native macOS bundle type.
    /// If false, we use --root instead of --component.
    pub(crate) fn is_native_bundle(&self) -> bool {
        self.meta().is_native_bundle
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
            PkgFormat::Standalone => format!("{}.app", plugin.file_stem()),
            // LV2 bundle names follow the spec's lowercase-hyphenated
            // convention (the same slug `derive(Params)` bakes into
            // `manifest.ttl` / `plugin.ttl`). Anything else and hosts
            // can't resolve the bundle from the TTL's binary URI.
            PkgFormat::Lv2 => format!("{}.lv2", stage::lv2_slug(&plugin.name)),
            _ => format!("{}.{}", plugin.file_stem(), self.extension()),
        }
    }

    pub(crate) fn choice_description(&self) -> &'static str {
        self.meta().choice_description
    }

    /// True for formats whose install destination can't be redirected
    /// into the user's home - AAX lives under
    /// `/Library/Application Support/Avid/...` where Pro Tools scans,
    /// AU v3 needs `/Applications/` for `LaunchServices` to register
    /// the appex, and a standalone `.app` belongs in `/Applications/`
    /// to show up in Launchpad. When the user picks "Install for me
    /// only" but selects one of these, the installer escalates
    /// (`auth="Root"` on the corresponding `<pkg-ref>`) so the
    /// component still lands in the right place rather than failing
    /// with a permission-denied shove.
    pub(crate) fn is_system_only_on_macos(&self) -> bool {
        matches!(
            self,
            PkgFormat::Aax | PkgFormat::Au3 | PkgFormat::Standalone
        )
    }
}

// `args` is unused on platforms where the body falls through to the
// "not supported" Err branch - silence the unused-variable warning
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
    // iOS short-circuit: AU v3 inside an `.ipa` is the only viable
    // iOS distribution shape and doesn't share any of the macOS /
    // Windows / Linux packaging pipeline (no productbuild, no Inno
    // Setup, no tarball). Handle it as a thin pass-through before
    // the platform dispatch.
    if args.iter().any(|a| a == "--ios") {
        #[cfg(target_os = "macos")]
        {
            return package_ios(args);
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Err("--ios packaging requires macOS (Xcode-only).".into());
        }
    }
    let (selection, args) = extract_suite_selection(args)?;
    #[cfg(target_os = "windows")]
    {
        windows::cmd_package_windows(&args, &selection)
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
fn package_ios(args: &[String]) -> Res {
    use crate::commands::install::au_ios;
    let mut plugin_filter: Option<&str> = None;
    let mut xcframework_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ios" => {}
            "--xcframework" => xcframework_only = true,
            "-p" => {
                i += 1;
                plugin_filter = args.get(i).map(String::as_str);
                if plugin_filter.is_none() {
                    return Err("-p needs a plugin name".into());
                }
            }
            other => return Err(format!("Unknown flag for --ios packaging: {other}").into()),
        }
        i += 1;
    }
    let root = crate::project_root();
    let config = crate::load_config()?;
    let plugins: Vec<&crate::PluginDef> = if let Some(s) = plugin_filter {
        let p = config
            .plugin
            .iter()
            .find(|p| p.crate_name == s || p.bundle_id == s)
            .ok_or_else(|| -> crate::CargoTruceError {
                format!("No plugin with crate name or bundle id '{s}'.").into()
            })?;
        vec![p]
    } else {
        config.plugin.iter().collect()
    };

    // Print each "Packaged: ..." line as the plugin finishes rather
    // than batching them at the end - gives the user feedback during
    // multi-plugin runs (each ipa is minutes of cargo build), and
    // avoids any flush race between the final eprintln batch and
    // process exit.
    for p in plugins {
        let path = if xcframework_only {
            au_ios::build_xcframework(&root, p)?
        } else {
            au_ios::package_ipa(&root, p)?
        };
        eprintln!("Packaged: {}", path.display());
    }
    if !xcframework_only && let Some(team) = crate::ios_team_id() {
        eprintln!("Signed for team {team}.");
    }
    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce package [-p <crate>] [--suite <name>] \
[--no-suite|--no-per-plugin] [--formats <list>] \
[--user|--system|--ask] [--no-notarize] [--no-sign|--no-pace-sign] \
[--host-only|--universal]

Build, sign, and package plugins into installers. Per-plugin dist
filenames use the cargo crate name (e.g. `truce-example-gain`);
suite filenames use the suite's bundle_id. Same slug across
macOS / Windows / Linux:
  - macOS:   `target/dist/<crate>-<version>-macos.pkg` (productbuild)
  - Windows: `target/dist/<crate>-<version>-windows.exe` (Inno Setup)
  - Linux:   `target/dist/<crate>-<version>-linux-<arch>.tar.gz` + install.sh

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
  --no-pace-sign       Skip PACE (AAX) signing - useful for non-Pro Tools
                       sanity checks. Apple codesign always runs on macOS.
  --no-sign            Synonym for --no-pace-sign on macOS.

Build target (macOS):
  --host-only          Single-arch build of the host. Default is universal.
  --universal          Explicit universal (no-op; same as default).

Build invocation (Linux):
  --no-build           Skip the implicit `cargo truce build` and use the
                       existing `target/bundles/` manifest as-is. Errors
                       if no manifest is present.

Codegen tuning (all platforms):
  --target-cpu <value> Override the x86_64 default of `-C target-cpu=x86-64-v3`.
                       baseline|v2|v3|v4|native or any literal rustc
                       target-cpu name. See `cargo truce build --help`
                       for the full description.

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
