//! `cargo truce package` - build, sign, and create installers.
//!
//! Top-level dispatch + the format-flag parsing shared between the macOS
//! `.pkg` pipeline (`macos.rs`), the Linux tarball pipeline (`linux.rs`),
//! and the Windows Inno Setup pipeline (`windows.rs`).

use crate::CargoTruceError;
#[cfg(target_os = "macos")]
use crate::PluginDef;
use crate::Res;

pub(crate) mod assemble;
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
    pub(crate) fn want_suite(&self, name: &str, bundle_id: &str) -> bool {
        if self.no_suite {
            return false;
        }
        if self.only_suites.is_empty() {
            return true;
        }
        // Accept either the display name or the bundle_id slug - the
        // slug is what users naturally type (and what filenames use).
        self.only_suites.iter().any(|s| s == name || s == bundle_id)
    }

    /// Error if a `--suite <x>` matches no declared suite by name or
    /// `bundle_id`, so a typo / wrong identifier fails loudly instead of
    /// silently producing no suite installer.
    pub(crate) fn validate_against(
        &self,
        declared: &[crate::config::SuiteDef],
    ) -> Result<(), CargoTruceError> {
        let unknown: Vec<&str> = self
            .only_suites
            .iter()
            .filter(|req| {
                !declared
                    .iter()
                    .any(|s| &s.name == *req || &s.bundle_id == *req)
            })
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let available = if declared.is_empty() {
            "(none declared in truce.toml)".to_string()
        } else {
            declared
                .iter()
                .map(|s| format!("{} (\"{}\")", s.bundle_id, s.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Err(format!("--suite: no suite matches {unknown:?}. Available suites: {available}").into())
    }
}

/// Strip the suite-selection flags out of `args` before the
/// per-platform parser sees them. Returns the parsed selection plus a
/// new `args` vector with those flags removed.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
/// Pull `--features <list>` occurrences out of the package args and
/// return `(features, remaining_args)`. Kept separate from the per-OS
/// arg parsers so they never see the flag; the features flow to the
/// builds via the invocation global instead.
/// Pull the invocation-global feature flags out of the raw args: the
/// `--features` list and whether `--no-default-features` was passed.
/// Returns them plus the remaining args for the per-OS parsers.
fn extract_features_arg(
    args: &[String],
) -> Result<(Vec<String>, bool, Vec<String>), CargoTruceError> {
    let mut features = Vec::new();
    let mut no_default_features = false;
    let mut rest = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--features" {
            let value = crate::util::arg_value(args, &mut i, "--features")?;
            features.extend(crate::parse_extra_features(value)?);
        } else if args[i] == "--no-default-features" {
            no_default_features = true;
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    Ok((features, no_default_features, rest))
}

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
#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// A format packaged through its `truce-format-<name>` helper.
    External(&'static crate::external::ExternalFormat),
}

impl std::str::FromStr for PkgFormat {
    type Err = CargoTruceError;

    /// Parse one `--formats` token. A token that isn't a compiled-in
    /// format is looked up as a helper, so `--formats lv2` keeps
    /// working and `--formats <anything-installed>` starts working -
    /// with [`crate::external::missing`]'s install hint for a
    /// [`crate::external::KNOWN`] format whose helper is absent.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "clap" => Ok(PkgFormat::Clap),
            "vst3" => Ok(PkgFormat::Vst3),
            "vst2" => Ok(PkgFormat::Vst2),
            "au2" => Ok(PkgFormat::Au2),
            "au3" => Ok(PkgFormat::Au3),
            "aax" => Ok(PkgFormat::Aax),
            "standalone" => Ok(PkgFormat::Standalone),
            other => match crate::external::describe(other)? {
                Some(e) => Ok(PkgFormat::External(e)),
                None if crate::external::KNOWN.contains(&other) => {
                    Err(crate::external::missing(other))
                }
                None => Err(format!("unknown format: {other}").into()),
            },
        }
    }
}

/// One format's `pkgbuild` / `productbuild` row - the macOS packaging
/// pipeline's whole view of a format. One row per compiled-in
/// `PkgFormat` variant in [`PKG_FORMAT_META`]; [`PkgFormat::External`]
/// supplies the same fields from its `describe` output's `macos_pkg`
/// table, so adding a format out-of-tree needs no row here.
///
/// Every field is `&'static str` on both paths: the rows below are
/// literals, and an `ExternalFormat` handle is itself `'static`, so its
/// `String` fields borrow for exactly as long.
///
/// The rows compile everywhere - they're harmless string literals on
/// Linux / Windows - but every method that reads one is gated on
/// `cfg(target_os = "macos")`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy)]
struct PkgFormatMeta {
    pkg_id_suffix: &'static str,
    extension: &'static str,
    install_location: &'static str,
    is_native_bundle: bool,
    choice_description: &'static str,
}

// Read only by the macOS-gated `PkgFormat::meta` below; the table
// itself is plain data and stays compiled everywhere so a typo in it
// fails on any host.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const PKG_FORMAT_META: [(PkgFormat, PkgFormatMeta); 7] = [
    (
        PkgFormat::Clap,
        PkgFormatMeta {
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
            pkg_id_suffix: "vst2",
            extension: "vst",
            install_location: "/Library/Audio/Plug-Ins/VST/",
            is_native_bundle: false,
            choice_description: "Legacy - for hosts without VST3 support",
        },
    ),
    (
        PkgFormat::Au2,
        PkgFormatMeta {
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

    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Clap => "CLAP",
            Self::Vst3 => "VST3",
            Self::Vst2 => "VST2",
            Self::Au2 => "AU2",
            Self::Au3 => "AU3",
            Self::Aax => "AAX",
            Self::Standalone => "Standalone",
            Self::External(e) => &e.label,
        }
    }

    /// The cdylib build this format's bundle is staged from. `None` for
    /// the two formats that aren't one plugin cdylib per bundle: AU v3
    /// wraps the AU v2 build in an appex, and the standalone host is a
    /// binary rather than a plug-in.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn build_format(self) -> Option<crate::commands::build_dylibs::BuildFormat> {
        use crate::commands::build_dylibs::BuildFormat;
        match self {
            Self::Clap => Some(BuildFormat::Clap),
            Self::Vst3 => Some(BuildFormat::Vst3),
            Self::Vst2 => Some(BuildFormat::Vst2),
            Self::Au2 => Some(BuildFormat::Au2),
            Self::Aax => Some(BuildFormat::Aax),
            Self::External(e) => Some(BuildFormat::External(e)),
            Self::Au3 | Self::Standalone => None,
        }
    }
}

// macOS-only `pkgbuild` / `productbuild` plumbing. Windows packaging
// drives Inno Setup directly and doesn't need any of this.
#[cfg(target_os = "macos")]
impl PkgFormat {
    /// This format's `pkgbuild` row, from the static table for a
    /// compiled-in format and from `describe` for an external one.
    ///
    /// `resolve_formats` drops an external format with no `macos_pkg`
    /// table before packaging gets here, so both paths always have a
    /// row to hand back.
    fn meta(self) -> PkgFormatMeta {
        if let Self::External(e) = self {
            let pkg = e.require_macos_pkg();
            return PkgFormatMeta {
                pkg_id_suffix: &pkg.pkg_id_suffix,
                extension: &pkg.extension,
                install_location: &pkg.install_location,
                is_native_bundle: pkg.is_native_bundle,
                choice_description: &pkg.choice_description,
            };
        }
        PKG_FORMAT_META
            .iter()
            .find(|(f, _)| *f == self)
            .expect("PKG_FORMAT_META is exhaustive over the compiled-in PkgFormats")
            .1
    }

    pub(crate) fn extension(self) -> &'static str {
        self.meta().extension
    }

    pub(crate) fn install_location(self) -> &'static str {
        self.meta().install_location
    }

    pub(crate) fn pkg_id_suffix(self) -> &'static str {
        self.meta().pkg_id_suffix
    }

    /// Whether pkgbuild recognizes this as a native macOS bundle type.
    /// If false, we use --root instead of --component.
    pub(crate) fn is_native_bundle(self) -> bool {
        self.meta().is_native_bundle
    }

    /// Bundle directory name for a given plugin.
    pub(crate) fn bundle_name(self, plugin: &PluginDef) -> String {
        match self {
            PkgFormat::Au3 => format!("{}.app", plugin.au3_app_name()),
            // Plain `<Plugin>.app` so Spotlight / Launch Services index
            // it as a regular application. The historical
            // `<Plugin>.standalone.app` extension confused some indexing
            // paths and the bundle would not appear in Spotlight search.
            // AU v3 resolves to the same `<Plugin>.app` (its app *is* the
            // standalone host with the appex), so `resolve_formats` drops
            // the separate Standalone format whenever AU v3 is present -
            // they never coexist at this path.
            PkgFormat::Standalone => format!("{}.app", plugin.file_stem()),
            // An external format names its own bundles: LV2's have to be
            // the spec's lowercase-hyphenated slug, since that's what the
            // TTL's binary URI resolves against.
            PkgFormat::External(e) => e.bundle_name_for(plugin),
            _ => format!("{}.{}", plugin.file_stem(), self.extension()),
        }
    }

    pub(crate) fn choice_description(self) -> &'static str {
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
    // Pull `--features` out first and set the invocation global, so it
    // reaches every build the fan-out spawns (iOS, per-OS, universal)
    // via `apply_extra_features` without each per-OS parser handling it.
    let (user_features, no_default_features, args) = extract_features_arg(args)?;
    crate::set_extra_features(user_features);
    // `--no-default-features` opts out of re-adding each plugin's
    // non-format default features (e.g. `ara`) to the per-format builds.
    crate::set_no_default_features(no_default_features);
    let (assemble_from, args) = assemble::extract(&args)?;
    // iOS short-circuit: AU v3 inside an `.ipa` is the only viable
    // iOS distribution shape and doesn't share any of the macOS /
    // Windows / Linux packaging pipeline (no productbuild, no Inno
    // Setup, no tarball). Handle it as a thin pass-through before
    // the platform dispatch.
    if args.iter().any(|a| a == "--ios") {
        if assemble_from.is_some() {
            return Err("--assemble-from doesn't apply to --ios packaging".into());
        }
        #[cfg(target_os = "macos")]
        {
            return package_ios(&args);
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Err("--ios packaging requires macOS (Xcode-only).".into());
        }
    }
    let (selection, args) = extract_suite_selection(&args)?;
    // Fail loudly on an unknown `--suite <name>` rather than silently
    // building only the per-plugin installers. Only load config when a
    // suite was explicitly requested.
    if !selection.only_suites.is_empty() {
        selection.validate_against(&crate::load_config()?.suites)?;
    }
    let assemble_from = assemble_from.as_deref();
    #[cfg(target_os = "windows")]
    {
        windows::cmd_package_windows(&args, &selection, assemble_from)
    }
    #[cfg(target_os = "macos")]
    {
        macos::cmd_package_macos(&args, &selection, assemble_from)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        linux::cmd_package_linux(&args, &selection, assemble_from)
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
[--host-only|--universal] [--arch <x86_64|arm64>]

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
  --features <list>    Extra Cargo features for the plugin crate,
                       comma/space-separated. Additive, applied to every
                       underlying build. Format features are reserved -
                       use --formats.
  --no-default-features
                       Don't re-add the plugin's non-format default
                       features (e.g. ara) to each per-format build.

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
  --arch <x86_64|arm64>
                       Single-arch build of that architecture. The host
                       may be the other one. Not with --host-only.

Build invocation:
  --assemble-from <DIR>
                       Package bundles that already exist (e.g. signed
                       elsewhere) instead of building: no build, no
                       per-format staging, no bundle signing. Reads each
                       plugin's tree from `<DIR>/<bundle_id>/`, in the
                       layout `--no-installer` leaves under
                       `target/package/<os>/[<arch>/]plugin/`, out-of-bundle
                       factory presets included (never regenerated). Fails
                       if a selected plugin, format or shipped preset set
                       is missing. `<DIR>` is never modified; the installer
                       is still signed unless --no-sign.
  --no-installer       Windows / Linux: stage each plugin's tree and stop
                       (no .exe / tarball) - the input `--assemble-from`
                       reads.
  --no-build           Linux: skip the implicit `cargo truce build` and use
                       the existing `target/bundles/` manifest as-is.
                       Errors if no manifest is present.

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
        assert!(sel.want_suite("Studio", "studio"));
        assert_eq!(rest, [] as [std::string::String; 0]);
    }

    #[test]
    fn suite_filter_keeps_only_named_suites() {
        let (sel, rest) =
            extract_suite_selection(&s(&["--suite", "studio", "--suite", "free"])).unwrap();
        assert!(sel.want_suite("Studio", "studio"));
        assert!(sel.want_suite("Free", "free"));
        assert!(!sel.want_suite("MIDI Tools", "midi-tools"));
        assert_eq!(rest, [] as [std::string::String; 0]);
    }

    #[test]
    fn suite_filter_matches_bundle_id_or_name() {
        // `--suite` accepts the bundle_id slug or the display name.
        let (by_slug, _) = extract_suite_selection(&s(&["--suite", "truce-examples"])).unwrap();
        assert!(by_slug.want_suite("Truce Examples", "truce-examples"));
        let (by_name, _) = extract_suite_selection(&s(&["--suite", "Truce Examples"])).unwrap();
        assert!(by_name.want_suite("Truce Examples", "truce-examples"));
    }

    #[test]
    fn no_suite_drops_every_suite() {
        let (sel, _) = extract_suite_selection(&s(&["--no-suite"])).unwrap();
        assert!(!sel.want_suite("Studio", "studio"));
        assert!(sel.want_per_plugin());
    }

    #[test]
    fn no_per_plugin_keeps_suites() {
        let (sel, _) = extract_suite_selection(&s(&["--no-per-plugin"])).unwrap();
        assert!(!sel.want_per_plugin());
        assert!(sel.want_suite("Studio", "studio"));
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
        assert!(sel.want_suite("Studio", "studio"));
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
