//! `cargo truce uninstall` - remove plugin bundles for the current project,
//! or with `--stale` evict vendor-matching bundles no longer in `truce.toml`.

#[cfg(target_os = "macos")]
use crate::Config;
use crate::external::ExternalFormat;
use crate::install_scope::{InstallScope, note_once, set_cli_install_scope};
use crate::{PluginDef, Res, confirm_prompt, dirs, load_config};
// `run_sudo` is macOS-only (Linux is always per-user, Windows uses
// per-process UAC elevation rather than per-command sudo).
#[cfg(target_os = "macos")]
use crate::run_sudo;
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fs;
#[cfg(windows)]
use std::os::windows::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use truce_utils::shell_sidecar::sidecar_path;

/// The extension `install` gives a VST2 bundle on this platform.
const VST2_EXT: &str = if cfg!(target_os = "macos") {
    "vst"
} else if cfg!(target_os = "windows") {
    "dll"
} else {
    "so"
};

struct RemoveTarget {
    format: &'static str,
    path: PathBuf,
    needs_sudo: bool,
}

#[cfg(target_os = "macos")]
fn unregister_au3(config: &Config, plugin: &PluginDef, app_path: &Path) {
    // Match the vendor-rooted ids install registers (see `reset_au`);
    // a `com.` prefix would miss any vendor id not starting with it.
    let vid = config.vendor.id.as_str();
    for pattern in [
        format!("{vid}.{}.v3.ext", plugin.bundle_id),
        format!("{vid}.{}.au", plugin.bundle_id),
    ] {
        let _ = Command::new("pluginkit")
            .args(["-e", "ignore", "-i", &pattern])
            .output();
        let _ = Command::new("pluginkit")
            .args(["-r", "-i", &pattern])
            .output();
    }
    // `lsregister -u ""` interprets the empty string as the current
    // directory and unregisters whatever app-bundle the CWD happens to
    // be - alarming if the user invoked `cargo truce uninstall` from inside
    // some other `.app`. Skip the call instead.
    if let Some(app_path_str) = app_path.to_str() {
        let _ = Command::new(
            "/System/Library/Frameworks/CoreServices.framework/\
             Frameworks/LaunchServices.framework/Support/lsregister",
        )
        .args(["-u", app_path_str])
        .output();
    }
}

fn clear_au_caches() {
    // No HOME (or USERPROFILE on Windows) → skip the per-user cache
    // sweep silently. The system-wide `killall AudioComponentRegistrar`
    // below still runs; AU caches in $HOME just don't exist for a user
    // whose env we can't resolve.
    if let Some(home) = dirs::home_dir() {
        for dir in [
            home.join("Library/Caches/AudioUnitCache"),
            home.join(
                "Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache",
            ),
            home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
            home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
        ] {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    let _ = Command::new("killall")
        .args(["-9", "AudioComponentRegistrar"])
        .output();
}

/// The distinct install roots an external format occupies across
/// `scopes`, each paired with whether writing there needs `sudo`.
///
/// Deduped by path on purpose: a helper that declares no system
/// directory gets its user root back for *both* scopes, so scanning
/// per-scope would visit the same directory twice and report every
/// bundle in it as two targets. Deduping here is also what lets the
/// external paths stay free of the per-OS `cfg` branching the
/// compiled-in formats need - on Linux "system" simply isn't a
/// distinct place, and that falls out of the helper's own table.
fn external_roots(e: &ExternalFormat, scopes: &[InstallScope]) -> Vec<(PathBuf, bool)> {
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    for &s in scopes {
        let Some(dir) = e.dir(s) else { continue };
        if !roots.iter().any(|(d, _)| *d == dir) {
            roots.push((dir, s.needs_sudo()));
        }
    }
    roots
}

/// Whether a bundle or app name is the vendor's, by the convention truce
/// plugin names follow: the name starts with the vendor name as a whole
/// word (`Truce Gain.clap`, `truce-gain.lv2`, `Truce Gain (AUv3).app`).
///
/// A plain substring test let a short vendor name claim other vendors'
/// bundles - `Ab` matched `Fabulous.clap` and `Abyss.vst3`, `Audio`
/// matched `Acme Audio Verb.clap` - and `--stale` deletes what matches.
/// Anchoring at the start of the name and requiring a non-alphanumeric
/// character after the vendor rules those out. An empty `vendor_token`
/// matches nothing: a slug-cased format turns a vendor name with no ASCII
/// letters or digits into `""`, which every name starts with.
fn is_vendor_bundle(name: &str, vendor_token: &str) -> bool {
    !vendor_token.is_empty()
        && name
            .strip_prefix(vendor_token)
            .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()))
}

/// The vendor and the project's plugins spelled the way one format
/// spells them in bundle names, which is what `--stale` matches on disk.
/// The raw names stop matching once a format rewrites them: the built-in
/// formats replace reserved characters (`A/B Audio Dry/Wet` installs as
/// `A-B Audio Dry-Wet.clap`), LV2 slug-cases. Compared raw, the vendor
/// side finds nothing and the plugin side offers a live bundle for
/// deletion.
struct DiskNames {
    vendor: String,
    plugins: Vec<String>,
}

impl DiskNames {
    fn new(vendor: &str, plugins: &[PluginDef], spell: impl Fn(&str) -> String) -> Self {
        Self {
            vendor: spell(vendor),
            plugins: plugins.iter().map(|p| spell(&p.name)).collect(),
        }
    }

    /// The built-in formats' spelling, [`PluginDef::file_stem`]'s.
    fn builtin(vendor: &str, plugins: &[PluginDef]) -> Self {
        Self::new(vendor, plugins, truce_utils::safe_filename)
    }

    /// Whether `file_name`, found in one format's install directory, is
    /// a `--stale` candidate: a `.<ext>` bundle that [`is_vendor_bundle`]
    /// and whose stem names none of the project's plugins.
    fn is_stale(&self, file_name: &str, ext: &str) -> bool {
        if !is_vendor_bundle(file_name, &self.vendor) {
            return false;
        }
        let Some(stem) = file_name
            .strip_suffix(ext)
            .and_then(|rest| rest.strip_suffix('.'))
        else {
            return false;
        };
        !self.plugins.iter().any(|p| p == stem)
    }
}

/// `--stale -n <name>`: the name as each format spells it on disk, as
/// [`DiskNames`] does - `Truce Dry/Wet` is `Truce Dry-Wet.clap` for the
/// built-ins and `truce-dry-wet.lv2` for LV2 - lowercased.
fn name_filter_spellings(name: &str, externals: &[&ExternalFormat]) -> Vec<String> {
    std::iter::once(truce_utils::safe_filename(name))
        .chain(externals.iter().map(|e| e.spell(name)))
        .map(|s| s.to_lowercase())
        .collect()
}

/// Whether the bundle at `path` is the plugin named by one of
/// [`name_filter_spellings`], ignoring case, the AU v3 app suffix
/// (legacy ` v3`, current ` (AUv3)`) and the legacy `.standalone`.
fn is_named_bundle(path: &Path, spellings: &[String]) -> bool {
    let stem = path
        .file_stem()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let stem = stem
        .trim_end_matches(" v3")
        .trim_end_matches(" (auv3)")
        .trim_end_matches(".standalone");
    spellings.iter().any(|s| s == stem)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_uninstall(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    // Formats selected by a `--<name>` flag that names an installed
    // `truce-format-<name>` helper rather than a compiled-in format.
    let mut externals: Vec<&'static crate::external::ExternalFormat> = Vec::new();
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut standalone = false;
    let mut dry_run = false;
    let mut yes = false;
    let mut stale = false;
    let mut crate_filter: Option<String> = None;
    let mut name_filter: Option<String> = None;
    let mut cli_scope: Option<InstallScope> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--vst2" => vst2 = true,
            "--au2" => au2 = true,
            "--au3" => au3 = true,
            "--aax" => aax = true,
            "--standalone" => standalone = true,
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--stale" => stale = true,
            "--user" => set_cli_install_scope(&mut cli_scope, InstallScope::User)?,
            "--system" => set_cli_install_scope(&mut cli_scope, InstallScope::System)?,
            "--ask" => {
                return Err(
                    "--ask is not valid for `cargo truce uninstall` (no end user to prompt). \
                     Use --user or --system."
                        .into(),
                );
            }
            "-p" => {
                crate_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "-n" => {
                name_filter = Some(crate::util::arg_value(args, &mut i, "-n")?.to_string());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            // An unrecognised `--<name>` is a format flag before it is
            // a mistake: `--lv2` reaches this arm now and resolves to
            // the `truce-format-lv2` helper. `external::select` reports
            // the unknown-flag error when it isn't one.
            other => crate::external::select(other, &mut externals)?,
        }
        i += 1;
    }

    // Without an explicit scope flag, scan both user and system -
    // a dev who switched scopes mid-iteration may have stale copies
    // in the other half of the disk.
    let scopes_to_scan: Vec<InstallScope> = match cli_scope {
        Some(InstallScope::User) => vec![InstallScope::User],
        Some(InstallScope::System) => vec![InstallScope::System],
        None => vec![InstallScope::User, InstallScope::System],
    };
    // AAX, AU v3, and (on Windows) VST2 are always system-scope -
    // surface the same one-line note as `install` when `--user` was
    // explicitly requested for one of them.
    let user_explicit = matches!(cli_scope, Some(InstallScope::User));
    if user_explicit {
        if aax {
            note_once("AAX is system-only; ignoring --user");
        }
        if au3 && cfg!(target_os = "macos") {
            note_once("AU v3 is system-only; ignoring --user");
        }
        if vst2 && cfg!(target_os = "windows") {
            note_once("VST2 on Windows is system-only; ignoring --user");
        }
    }

    // Captured before the default-fill below so the post-loop sidecar
    // cleanup can tell "user passed no format flag → uninstall
    // everything for these plugins" apart from "user picked specific
    // formats → leave shell sidecars alone for the others".
    let all_formats_default =
        !clap && !vst3 && !vst2 && externals.is_empty() && !au2 && !au3 && !aax && !standalone;

    // Default: all formats if none specified.
    // `au3 = true` lands in a flag that's read only inside macOS-gated
    // blocks; the assignment-never-read warning on Linux/Windows is
    // intentional - keeping the flag uniform across platforms is more
    // readable than a per-platform `if`.
    #[allow(unused_assignments)]
    if all_formats_default {
        clap = true;
        vst3 = true;
        vst2 = true;
        externals = crate::external::for_uninstall()?;
        au2 = true;
        au3 = true;
        aax = true;
        standalone = true;
    }

    let vendor = &config.vendor.name;
    let builtin = DiskNames::builtin(vendor, &config.plugin);

    let mut targets: Vec<RemoveTarget> = Vec::new();
    // Collected in the non-stale branch below; used after the
    // bundle-removal loop to clean up `~/.truce/shell/<crate>.path`
    // sidecars when the user is uninstalling all formats for a
    // plugin. Empty for `--stale` (we only have display names there,
    // not crate names - sidecars stay).
    let mut crate_names_for_sidecar_cleanup: Vec<String> = Vec::new();

    if stale {
        // --stale: find vendor-matching bundles NOT in the current project
        let scan = |dir: &Path,
                    ext: &str,
                    format: &'static str,
                    names: &DiskNames,
                    needs_sudo: bool,
                    targets: &mut Vec<RemoveTarget>| {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    // Linux's scopes share one directory per format, so
                    // the second scope would list each bundle again.
                    let path = entry.path();
                    if !names.is_stale(&entry.file_name().to_string_lossy(), ext)
                        || targets.iter().any(|t| t.path == path)
                    {
                        continue;
                    }
                    targets.push(RemoveTarget {
                        format,
                        path,
                        needs_sudo,
                    });
                }
            }
        };

        let scan_system = scopes_to_scan.contains(&InstallScope::System);
        if clap {
            for s in &scopes_to_scan {
                scan(
                    &s.clap_dir(),
                    "clap",
                    "CLAP",
                    &builtin,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        if vst3 {
            for s in &scopes_to_scan {
                scan(
                    &s.vst3_dir(),
                    "vst3",
                    "VST3",
                    &builtin,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        if vst2 && !cfg!(target_os = "windows") {
            for s in &scopes_to_scan {
                scan(
                    &s.vst2_dir(),
                    VST2_EXT,
                    "VST2",
                    &builtin,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        } else if vst2 && scan_system {
            // Windows VST2 is always system-only - `vst2_dir()` returns
            // the same path for both scopes.
            scan(
                &InstallScope::System.vst2_dir(),
                VST2_EXT,
                "VST2",
                &builtin,
                InstallScope::System.needs_sudo(),
                &mut targets,
            );
        }
        for e in &externals {
            // Spell the needles the way this format spells its own
            // bundle names - LV2's are slug-cased (`Truce Analyzer` ->
            // `truce-analyzer.lv2`), so neither the raw vendor name nor
            // a raw display name would match on disk.
            let names = DiskNames::new(vendor, &config.plugin, |name| e.spell(name));
            if names.vendor.is_empty() {
                note_once(&format!(
                    "{}: vendor name {vendor:?} has no ASCII letters or digits, so it can't \
                     identify {} bundles by name; skipping {} for --stale",
                    e.label, e.label, e.label
                ));
                continue;
            }
            for (dir, needs_sudo) in external_roots(e, &scopes_to_scan) {
                scan(
                    &dir,
                    e.bundle_extension(),
                    &e.label,
                    &names,
                    needs_sudo,
                    &mut targets,
                );
            }
        }
        #[cfg(target_os = "macos")]
        if au2 {
            for s in &scopes_to_scan {
                scan(
                    &s.au_v2_dir(),
                    "component",
                    "AU v2",
                    &builtin,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        // AU v3 lives in `/Applications/...` only on macOS, so the
        // `--au3` removal scan is macOS-only. The flag is still parsed
        // on every platform so cross-platform CI scripts don't break;
        // it just no-ops on Linux / Windows.
        //
        // `--user` skips this scan: AU v3 has no user-scope install
        // path (the install-side note already explained that to the
        // user), so there's nothing for `--user` to clean up.
        #[cfg(target_os = "macos")]
        if au3 && scan_system {
            // Scan /Applications for vendor-matching v3 apps not in project.
            // Recognize truce AU v3 containers by bundle-name convention:
            // legacy "<name> v3.app" or the new default "<name> (AUv3).app".
            // A custom `au3_name` override may produce neither pattern - those
            // orphans can only be detected when the current config still
            // produces a recognizable name, so we compare against the current
            // bundle names as well.
            let known_au3_bundles: Vec<String> = config
                .plugin
                .iter()
                .map(|p| format!("{}.app", p.au3_app_name()))
                .collect();
            if let Ok(entries) = fs::read_dir("/Applications") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if !is_vendor_bundle(&name_str, &builtin.vendor) || !name_str.ends_with(".app")
                    {
                        continue;
                    }
                    let looks_like_au3 =
                        name_str.ends_with(" v3.app") || name_str.ends_with("(AUv3).app");
                    if !looks_like_au3 {
                        continue;
                    }
                    if known_au3_bundles
                        .iter()
                        .any(|k| k.as_str() == name_str.as_ref())
                    {
                        continue;
                    }
                    targets.push(RemoveTarget {
                        format: "AU v3",
                        path: entry.path(),
                        needs_sudo: true,
                    });
                }
            }
        }
        if aax
            && scan_system
            && let Some(aax_dir) = crate::util::aax_plugins_dir_if_supported()
        {
            scan(&aax_dir, "aaxplugin", "AAX", &builtin, true, &mut targets);
        }
        // `--stale --standalone` cleans up legacy `<Name>.standalone.app`
        // bundles only - the historical convention `cargo truce package`
        // used before the rename. The current `<Plugin>.app` layout
        // collides with arbitrary unrelated apps the user installed
        // from anywhere; vendor-string substring matching isn't enough
        // to confidently delete a `.app` from `/Applications`, so we
        // skip those here. Run with `--stale` + `-p` / `-n` for a
        // targeted sweep instead.
        #[cfg(target_os = "macos")]
        if standalone {
            let scan_legacy_standalone =
                |dir: &Path, needs_sudo: bool, targets: &mut Vec<RemoveTarget>| {
                    if let Ok(entries) = fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            if !builtin
                                .is_stale(&entry.file_name().to_string_lossy(), "standalone.app")
                            {
                                continue;
                            }
                            targets.push(RemoveTarget {
                                format: "Standalone",
                                path: entry.path(),
                                needs_sudo,
                            });
                        }
                    }
                };
            for s in &scopes_to_scan {
                scan_legacy_standalone(&s.standalone_dir(), s.needs_sudo(), &mut targets);
            }
        }

        // Apply -p (substring match on filename) or -n (exact display name match)
        if let Some(ref filter) = crate_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                t.path
                    .file_name()
                    .is_some_and(|f| f.to_string_lossy().to_lowercase().contains(&filter_lower))
            });
        } else if let Some(ref filter) = name_filter {
            let spellings = name_filter_spellings(filter, &externals);
            targets.retain(|t| is_named_bundle(&t.path, &spellings));
        }
    } else {
        // Normal mode: remove bundles for plugins in the project

        // Filter plugins by crate name (-p) or display name (-n)
        let plugins: Vec<&PluginDef> = if let Some(ref filter) = crate_filter {
            let matched: Vec<_> = config
                .plugin
                .iter()
                .filter(|p| p.crate_name == *filter)
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with crate name '{filter}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| format!("{} (-p {})", p.name, p.crate_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            let matched: Vec<_> = config
                .plugin
                .iter()
                .filter(|p| p.name.to_lowercase() == filter_lower)
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with name '{filter}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| format!("\"{}\" (-p {})", p.name, p.crate_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else {
            config.plugin.iter().collect()
        };

        // Only when every copy goes: `--user` / `--system` leave the
        // other scope's bundles installed, and `--user` skips the
        // system-only formats (AAX, AU v3, Windows VST2) altogether.
        if all_formats_default && cli_scope.is_none() {
            crate_names_for_sidecar_cleanup.extend(plugins.iter().map(|p| p.crate_name.clone()));
        }

        let scan_system = scopes_to_scan.contains(&InstallScope::System);
        let push_if_exists =
            |format: &'static str, path: PathBuf, needs_sudo: bool, targets: &mut Vec<_>| {
                if path.exists() && !targets.iter().any(|t: &RemoveTarget| t.path == path) {
                    targets.push(RemoveTarget {
                        format,
                        path,
                        needs_sudo,
                    });
                }
            };
        for p in &plugins {
            if clap {
                for s in &scopes_to_scan {
                    let path = s.clap_dir().join(format!("{}.clap", p.file_stem()));
                    push_if_exists("CLAP", path, s.needs_sudo(), &mut targets);
                }
            }
            if vst3 {
                for s in &scopes_to_scan {
                    let path = s.vst3_dir().join(format!("{}.vst3", p.file_stem()));
                    push_if_exists("VST3", path, s.needs_sudo(), &mut targets);
                }
            }
            if vst2 {
                #[cfg(target_os = "macos")]
                {
                    for s in &scopes_to_scan {
                        let path = s.vst2_dir().join(format!("{}.{VST2_EXT}", p.file_stem()));
                        push_if_exists("VST2", path, s.needs_sudo(), &mut targets);
                    }
                }
                #[cfg(target_os = "windows")]
                if scan_system {
                    let s = InstallScope::System;
                    let path = s.vst2_dir().join(format!("{}.{VST2_EXT}", p.file_stem()));
                    push_if_exists("VST2", path, s.needs_sudo(), &mut targets);
                }
                #[cfg(target_os = "linux")]
                {
                    // Linux VST2 is `~/.vst/<name>.so` for both scopes.
                    let s = InstallScope::User;
                    let path = s.vst2_dir().join(format!("{}.{VST2_EXT}", p.file_stem()));
                    push_if_exists("VST2", path, s.needs_sudo(), &mut targets);
                }
            }
            for e in &externals {
                let bundle = e.bundle_name_for(p);
                for (dir, needs_sudo) in external_roots(e, &scopes_to_scan) {
                    push_if_exists(&e.label, dir.join(&bundle), needs_sudo, &mut targets);
                }
            }
            #[cfg(target_os = "macos")]
            if au2 {
                for s in &scopes_to_scan {
                    let path = s.au_v2_dir().join(format!("{}.component", p.file_stem()));
                    push_if_exists("AU v2", path, s.needs_sudo(), &mut targets);
                }
            }
            #[cfg(target_os = "macos")]
            if au3 && scan_system {
                let path = PathBuf::from(format!("/Applications/{}.app", p.au3_app_name()));
                push_if_exists("AU v3", path, true, &mut targets);
            }
            if aax
                && scan_system
                && let Some(aax_dir) = crate::util::aax_plugins_dir_if_supported()
            {
                let path = aax_dir.join(format!("{}.aaxplugin", p.file_stem()));
                push_if_exists("AAX", path, true, &mut targets);
            }
            if standalone {
                #[cfg(target_os = "macos")]
                {
                    for s in &scopes_to_scan {
                        // Current convention: plain `<Plugin>.app` so
                        // Spotlight / Launch Services index it as a
                        // regular application. The historical
                        // `<Plugin>.standalone.app` name is checked too
                        // for users upgrading from older installers.
                        let dir = s.standalone_dir();
                        push_if_exists(
                            "Standalone",
                            dir.join(format!("{}.app", p.file_stem())),
                            s.needs_sudo(),
                            &mut targets,
                        );
                        push_if_exists(
                            "Standalone",
                            dir.join(format!("{}.standalone.app", p.file_stem())),
                            s.needs_sudo(),
                            &mut targets,
                        );
                    }
                }
                // Linux / Windows standalone paths are handled by the
                // platform installer (a bare ELF under `~/.local/bin`
                // on Linux, `%PROGRAMFILES%\<Vendor>\<Plugin>\...exe`
                // on Windows). Uninstall there is the OS package
                // manager's responsibility - `cargo truce uninstall`
                // never put the file there in the first place.
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = p;
                }
            }
        }
    }

    if targets.is_empty() {
        eprintln!("No installed plugins found to remove.");
        return Ok(());
    }

    // Show summary
    eprintln!("The following plugins will be removed:\n");
    for t in &targets {
        eprintln!("  {:<5} {}", t.format, t.path.display());
    }
    eprintln!();

    if dry_run {
        eprintln!("Dry run - nothing was removed.");
        return Ok(());
    }

    if !yes && !confirm_prompt(&format!("Remove {} bundle(s)?", targets.len())) {
        eprintln!("Cancelled.");
        return Ok(());
    }

    // Remove bundles
    let mut removed_au = false;
    let mut errors = 0u32;
    let mut system_scope_failed = false;

    for t in &targets {
        // AU v3 special handling: unregister before deleting (macOS-only).
        #[cfg(target_os = "macos")]
        if t.format == "AU v3" {
            // Try to find a matching plugin def for precise unregistration
            let matched_plugin = config
                .plugin
                .iter()
                .find(|p| t.path == Path::new(&format!("/Applications/{}.app", p.au3_app_name())));
            if let Some(p) = matched_plugin {
                unregister_au3(&config, p, &t.path);
            } else if let Some(path_str) = t.path.to_str() {
                // Stale AU v3 - unregister by path only (lsregister).
                // Skip the call when the path can't be UTF-8'd: `lsregister
                // -u ""` would unregister whatever app the CWD happens to be.
                let _ = Command::new(
                    "/System/Library/Frameworks/CoreServices.framework/\
                     Frameworks/LaunchServices.framework/Support/lsregister",
                )
                .args(["-u", path_str])
                .output();
            }
            removed_au = true;
        }
        if t.format == "AU v2" {
            removed_au = true;
        }

        // `needs_sudo` picks `sudo` only on macOS: on Linux every
        // scope is per-user, and on Windows the cargo-truce process
        // is either elevated (direct fs ops succeed) or it isn't
        // (the OS error from `remove_*` surfaces unchanged, and the
        // summary says to elevate).
        let result: Res = {
            #[cfg(target_os = "macos")]
            {
                if t.needs_sudo {
                    run_sudo("rm", &[OsStr::new("-rf"), t.path.as_os_str()])
                } else {
                    remove_bundle(&t.path).map_err(Into::into)
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                remove_bundle(&t.path).map_err(Into::into)
            }
        };

        let name = t.path.file_name().unwrap_or_default().to_string_lossy();
        match result {
            Ok(()) => eprintln!("  \u{2713} {:<5} {}", t.format, name),
            Err(e) => {
                eprintln!("  \u{2717} {:<5} {} ({})", t.format, name, e);
                errors += 1;
                system_scope_failed |= t.needs_sudo;
            }
        }
    }

    // Clear AU caches if any AU bundles were removed
    if removed_au {
        clear_au_caches();
        eprintln!("\nCleared AU caches.");
    }

    // Clean up `~/.truce/shell/<crate>.path` sidecars for plugins
    // whose every bundle is being uninstalled. Skipped on partial
    // uninstalls (`--clap`, `-p` etc. without all formats, or `--user`
    // / `--system`) since the sidecar is shared across format wrappers
    // and scopes, and removing it would break shell-mode for any
    // still-installed copies. Skipped on `--stale` because we don't
    // have crate names there, and when any bundle failed to go: those
    // bundles are still installed and still read the sidecar.
    let sidecar_crates = if errors == 0 {
        crate_names_for_sidecar_cleanup.as_slice()
    } else {
        &[]
    };
    for crate_name in sidecar_crates {
        if let Some(path) = sidecar_path(crate_name)
            && path.exists()
        {
            match fs::remove_file(&path) {
                Ok(()) => eprintln!("  \u{2713} sidecar {}", path.display()),
                Err(e) => eprintln!("  \u{2717} sidecar {} ({})", path.display(), e),
            }
        }
    }

    if errors > 0 {
        eprintln!();
        return Err(format!(
            "{errors} bundle(s) could not be removed. {}",
            removal_hint(system_scope_failed)
        )
        .into());
    }
    eprintln!("\nDone. Restart your DAW to rescan.");
    Ok(())
}

/// Remove one bundle. One already gone counts as removed, so uninstall
/// stays idempotent: a bundle removed after it was listed doesn't fail
/// the run.
fn remove_bundle(path: &Path) -> std::io::Result<()> {
    match remove_existing_bundle(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Remove a directory, a single file (Windows VST2 `.dll`, Linux `.so`),
/// or a link to either, which loses only the link. Picking the call by
/// kind keeps the error the user sees the real one; trying `remove_file`
/// after a failed `remove_dir_all` replaced it with the file call's
/// error.
fn remove_existing_bundle(path: &Path) -> std::io::Result<()> {
    let kind = fs::symlink_metadata(path)?.file_type();
    if kind.is_dir() {
        fs::remove_dir_all(path)
    } else if is_directory_link(kind) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// A Windows directory symlink or junction, which only `remove_dir`
/// removes (`remove_file` fails on it). Unix removes every symlink with
/// `remove_file`.
#[cfg(windows)]
fn is_directory_link(kind: fs::FileType) -> bool {
    kind.is_symlink_dir()
}

#[cfg(not(windows))]
fn is_directory_link(_kind: fs::FileType) -> bool {
    false
}

/// What to suggest when bundles couldn't be removed. Windows elevation
/// is per process, so a system folder there needs the whole command run
/// from an Administrator prompt; macOS already removed system bundles
/// through `sudo`, and Linux installs are per-user.
fn removal_hint(system_scope_failed: bool) -> &'static str {
    if cfg!(target_os = "windows") && system_scope_failed {
        "Removing from system folders needs an Administrator command prompt."
    } else {
        "Check the folders' permissions."
    }
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce uninstall [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax]
                             [--<helper format>]
                             [--standalone] [--user|--system] [-p <crate>] [-n <name>]
                             [--stale] [--dry-run] [--yes]

Uninstall plugin bundles for this project. Default: all formats,
all plugins, both user + system scopes. Asks for confirmation. AAX and
AU v3 are always system-scope - `--user` skips them.

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --vst2           VST2 only
  --lv2            LV2 only (needs the `truce-format-lv2` helper on PATH;
                   any other installed `truce-format-<name>` helper takes
                   a `--<name>` flag the same way)
  --au2            AU v2 only (.component, macOS only)
  --au3            AU v3 only (.app, macOS only)
  --aax            AAX only
  --standalone     Standalone host app only (.app, macOS only)
  --user           Only uninstall from per-user directories.
  --system         Only uninstall from system directories.
  -p <crate>       Filter by cargo crate name.
  -n <name>        Filter by display name.
  --stale          Uninstall vendor bundles NOT in the current project
                   (bundles whose name starts with the vendor name).
  --dry-run        Show what would be uninstalled without deleting.
  --yes, -y        Skip confirmation prompt.
  -h, --help       Show this message"
    );
}

#[cfg(test)]
mod tests {
    use super::{DiskNames, is_named_bundle, is_vendor_bundle, name_filter_spellings};
    use crate::config::test_plugin;
    use std::path::Path;

    /// Names already spelled as on disk, for the matching rules alone.
    fn names(vendor: &str, plugins: &[&str]) -> DiskNames {
        DiskNames {
            vendor: vendor.into(),
            plugins: plugins.iter().map(|&p| p.into()).collect(),
        }
    }

    /// A short vendor name must not claim other vendors' bundles that
    /// merely contain it - `--stale` deletes what it matches.
    #[test]
    fn a_vendor_name_inside_another_name_is_not_a_match() {
        for other in [
            "Fabulous.clap",
            "Abyss Verb.vst3",
            "Acme Ab Verb.clap",
            "fab-verb.lv2",
        ] {
            assert!(!is_vendor_bundle(other, "Ab"), "{other}");
            assert!(!names("Ab", &[]).is_stale(other, "clap"), "{other}");
        }
        assert!(!is_vendor_bundle("Acme Audio Verb.clap", "Audio"));
    }

    /// The vendor's own spellings across formats still match.
    #[test]
    fn a_name_led_by_the_vendor_word_is_a_match() {
        assert!(is_vendor_bundle("Ab Verb.clap", "Ab"));
        assert!(is_vendor_bundle("ab-verb.lv2", "ab"));
        assert!(is_vendor_bundle("Truce Gain (AUv3).app", "Truce"));
        assert!(is_vendor_bundle("Truce.clap", "Truce"));
    }

    #[test]
    fn a_vendor_bundle_not_in_the_project_is_stale() {
        assert!(names("truce", &["truce-gain"]).is_stale("truce-old.lv2", "lv2"));
    }

    #[test]
    fn a_project_plugin_is_not_stale() {
        assert!(!names("truce", &["truce-gain"]).is_stale("truce-gain.lv2", "lv2"));
    }

    /// The built-in formats' names are spelled as install writes their
    /// bundles: a reserved character in the plugin's name must not make
    /// its live bundle look stale, nor one in the vendor's hide the
    /// vendor's stale bundles.
    #[test]
    fn builtin_names_match_the_bundles_install_writes() {
        let plugin = test_plugin("A/B Audio Dry/Wet", "drywet");
        let names = DiskNames::builtin("A/B Audio", std::slice::from_ref(&plugin));
        let installed = format!("{}.aaxplugin", plugin.file_stem());
        assert_eq!(installed, "A-B Audio Dry-Wet.aaxplugin");
        assert!(!names.is_stale(&installed, "aaxplugin"));
        assert!(names.is_stale("A-B Audio Old.aaxplugin", "aaxplugin"));
    }

    /// `-n` takes the display name; the bundles carry it as install
    /// spelled it.
    #[test]
    fn the_name_filter_matches_bundles_by_their_on_disk_names() {
        let spellings = name_filter_spellings("Truce Dry/Wet", &[]);
        for bundle in [
            "Truce Dry-Wet.clap",
            "truce dry-wet.vst3",
            "Truce Dry-Wet (AUv3).app",
            "Truce Dry-Wet.standalone.app",
        ] {
            assert!(is_named_bundle(Path::new(bundle), &spellings), "{bundle}");
        }
        assert!(!is_named_bundle(Path::new("Truce Dry.clap"), &spellings));
    }

    #[test]
    fn another_vendors_bundle_is_not_stale() {
        assert!(!names("truce", &[]).is_stale("acme-verb.lv2", "lv2"));
    }

    /// The trap: a vendor name with no ASCII letters or digits slugs to
    /// "", and `contains("")` would make every bundle on disk stale.
    #[test]
    fn an_empty_vendor_token_matches_nothing() {
        assert!(!names("", &[]).is_stale("acme-verb.lv2", "lv2"));
        assert!(!names("", &[]).is_stale("anything", "lv2"));
    }

    /// The scan walks a whole install directory, which can hold more
    /// than bundles of the format it's looking for.
    #[test]
    fn a_file_without_the_format_extension_is_not_stale() {
        assert!(!names("truce", &[]).is_stale("truce-old.lv2.bak", "lv2"));
        assert!(!names("truce", &[]).is_stale("truce-notes.txt", "lv2"));
        // The extension is matched whole, not as a bare suffix.
        assert!(!names("truce", &[]).is_stale("truce-oldlv2", "lv2"));
    }
}
