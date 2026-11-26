//! Out-of-tree plug-in formats: `truce-format-<name>` helper binaries.
//!
//! `cargo-truce` learns about a format it was not compiled against the
//! way cargo learns about subcommands - a binary on `PATH`, discovered
//! lazily, speaking JSON on stdout. The eight built-in formats keep
//! their typed fast paths; this module is what lets the format enums
//! carry one more arm without the tool knowing what the format is.
//!
//! Two rules shape the protocol, and both are worth stating because
//! each rules out a shorter design:
//!
//! - **`describe` is data, not behavior.** Everything the install and
//!   installer-generator paths need - bundle suffix, per-OS install
//!   dirs, the macOS `productbuild` choice row, the Inno Setup
//!   component spec - is a field on [`ExternalFormat`]. A helper is
//!   spawned only for work that genuinely needs it: render a bundle
//!   ([`stage`]), encode or decode a preset. Asking the helper "where
//!   do you install?" on every path would turn `doctor` and `status`,
//!   which only want to print a table, into a fan of subprocesses.
//! - **The helper never sees `truce.toml`.** It declares the keys it
//!   reads in `config_keys` and gets a filtered view of just those
//!   (see [`write_config_view`]). Handing over the whole file would be
//!   less code here and a much worse contract: every key a helper
//!   happened to read would become load-bearing by accident, and
//!   `truce.toml` could never be reorganized without breaking formats
//!   we cannot see.
//!
//! Discovered formats are leaked into `'static` ([`describe`]). That is
//! deliberate rather than lazy: `Format` / `PkgFormat` are `Copy` and
//! appear in `const` tables (`doctor::PATH_FORMATS_MACOS`), so the
//! external arm has to be a `&'static` handle or every one of those
//! would have to grow a lifetime. Discovery happens at most once per
//! format per process and the data is read until exit, so the leak is
//! bounded by the number of installed helpers.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::CargoTruceError;

/// Protocol version this build speaks. A helper announcing anything
/// else is refused rather than guessed at - the whole point of the
/// metadata tree carrying `schema = 1` is that the seam versions by
/// bump, not in place.
pub(crate) const SCHEMA: u32 = 1;

/// The `truce-format-` binary-name prefix, mirroring cargo's
/// `cargo-<subcommand>` convention.
const PREFIX: &str = "truce-format-";

/// Format tokens this tool still names itself, even though their
/// implementations live in helper binaries.
///
/// Keeping the list is what lets `--lv2` stay a flag rather than
/// degrade to `--format lv2`, lets `doctor` print "LV2: helper not
/// installed" instead of silently omitting the row, and lets a missing
/// helper produce [`missing`]'s install hint rather than "unknown
/// format". A format never built into truce needs no entry here - it is
/// reachable by name through `--format <name>`.
pub(crate) const KNOWN: &[&str] = &["lv2"];

/// Every [`KNOWN`] format whose helper is actually installed.
///
/// A helper that is absent is skipped silently - this drives default
/// format selection and the `doctor` / `status` tables, where "you
/// don't have it" is not an error. A helper that is present but
/// broken still errors, via [`describe`].
pub(crate) fn installed() -> Result<Vec<&'static ExternalFormat>, CargoTruceError> {
    KNOWN
        .iter()
        .filter_map(|name| describe(name).transpose())
        .collect()
}

/// Where truce 6.x put each [`KNOWN`] format's bundles while it still
/// compiled the format in: enough of a `describe` reply to find them.
///
/// Frozen - it records what 6.x did, which no helper release changes.
/// Read only through [`for_uninstall`].
const LEGACY_LAYOUTS: &[(&str, &str)] = &[(
    "lv2",
    r#"{
        "schema": 1,
        "format": "lv2",
        "label": "LV2",
        "cargo_feature": "lv2",
        "bundle_suffix": ".lv2",
        "bundle_name": "slug",
        "install_dir": {
            "macos": { "user": "~/Library/Audio/Plug-Ins/LV2",
                       "system": "/Library/Audio/Plug-Ins/LV2" },
            "linux": { "user": "~/.lv2" },
            "windows": { "user": "%APPDATA%/LV2",
                         "system": "%COMMONPROGRAMFILES%/LV2" }
        }
    }"#,
)];

/// The external formats `cargo truce uninstall` covers when no format
/// was named: each [`KNOWN`] format's installed helper, or - when the
/// helper is missing - the layout truce 6.x installed that format with.
///
/// Uninstall is the one command that has to work without the helper. A
/// dev who upgraded past the move of these formats out of the truce repo
/// and never installed the helper still has 6.x bundles on disk, and
/// skipping them would leave them behind with no sign they exist. A
/// legacy handle declares no capabilities and has no helper behind it,
/// so no verb is ever sent to one; uninstall reads only its layout.
///
/// # Errors
///
/// A helper on `PATH` answered `describe` with something unreadable.
pub(crate) fn for_uninstall() -> Result<Vec<&'static ExternalFormat>, CargoTruceError> {
    KNOWN
        .iter()
        .map(|name| Ok(describe(name)?.unwrap_or_else(|| legacy_layout(name))))
        .collect()
}

/// The [`LEGACY_LAYOUTS`] entry for a [`KNOWN`] format, as a handle.
/// Leaked like [`describe`]'s, and for the same reason; uninstall builds
/// at most one per format per process.
fn legacy_layout(name: &str) -> &'static ExternalFormat {
    let (_, json) = LEGACY_LAYOUTS
        .iter()
        .find(|(n, _)| *n == name)
        .expect("every KNOWN format has a LEGACY_LAYOUTS entry");
    let mut layout: ExternalFormat =
        serde_json::from_str(json).expect("LEGACY_LAYOUTS entries parse");
    layout.fill_defaults();
    Box::leak(Box::new(layout))
}

/// One optional thing a helper can do beyond existing. Capabilities are
/// declared rather than probed so a missing one is a clear error at the
/// point of use ("LV2 does not support …") instead of a failed exec
/// three layers down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Capability {
    /// Can turn a built dylib into an installable bundle (`stage`).
    Stage,
    /// `stage` also writes the plugin's factory presets into the
    /// bundle, given `--presets`.
    Presets,
    /// Implements `preset-encode` / `preset-decode`, so
    /// `cargo truce preset convert` / `pull` / `pack` can round-trip
    /// this format's native preset files.
    PresetCodec,
}

impl Capability {
    /// The wire token, as a helper spells it in `describe`. Lives here
    /// beside the `rename_all` that defines the spelling so a
    /// diagnostic can echo back exactly what it was sent.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Presets => "presets",
            Self::PresetCodec => "preset-codec",
        }
    }
}

/// Where a format's bundles live on one OS. `system` is absent for a
/// format with no machine-wide location (Linux LV2 is `~/.lv2` and
/// nothing else), which is also what tells [`crate::format::Format::dir`]
/// to ignore the requested scope.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ScopedDir {
    pub(crate) user: String,
    #[serde(default)]
    pub(crate) system: Option<String>,
}

/// Something a helper declares once per OS. Only [`here`](Self::here)
/// is ever read, but all three are parsed and kept so `doctor` can
/// print the paths for a host it isn't running on.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PerOs<T> {
    #[serde(default)]
    pub(crate) macos: T,
    #[serde(default)]
    pub(crate) linux: T,
    #[serde(default)]
    pub(crate) windows: T,
}

impl<T> PerOs<T> {
    /// This host's entry.
    pub(crate) fn here(&self) -> &T {
        if cfg!(target_os = "macos") {
            &self.macos
        } else if cfg!(target_os = "windows") {
            &self.windows
        } else {
            &self.linux
        }
    }
}

/// How a bundle's base name derives from the plugin's display name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BundleName {
    /// `truce_utils::slugify` - lowercase, hyphenated. LV2 needs this
    /// because the slug is baked into the TTL's binary URI.
    Slug,
    /// The plugin's `file_stem()`, as CLAP / VST3 / VST2 use.
    FileStem,
}

/// The macOS `pkgbuild` / `productbuild` row for a format, absent when
/// the format isn't packageable into a `.pkg`.
///
/// Parsed on every host so a `describe` reply stays one shape
/// everywhere; only the macOS packager reads it, hence the gate.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct MacosPkg {
    pub(crate) extension: String,
    pub(crate) install_location: String,
    pub(crate) pkg_id_suffix: String,
    /// False when `pkgbuild` should recurse with `--root` rather than
    /// treat the directory as a native bundle with `--component`.
    pub(crate) is_native_bundle: bool,
    pub(crate) choice_description: String,
}

/// A format described by a helper binary: the parsed `describe` output
/// plus the path we found it at.
///
/// Compared and hashed by [`format`](Self::format) alone - two handles
/// naming the same format are the same format, whatever else drifted.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExternalFormat {
    pub(crate) schema: u32,
    /// The lowercase token users type (`lv2`), matching the helper's
    /// binary suffix.
    pub(crate) format: String,
    /// Display name for logs and installer UI (`LV2`).
    pub(crate) label: String,
    /// The plugin crate's Cargo feature that pulls in this format's
    /// runtime wrapper.
    pub(crate) cargo_feature: String,
    /// Dylib name suffix the plugin builds under. Filled in by
    /// [`describe`] with `_<format>` when the helper omits it, which is
    /// what every built-in format does - so readers can treat it as
    /// always present.
    #[serde(default)]
    dylib_suffix: Option<String>,
    /// Bundle directory extension, leading dot included (`.lv2`).
    pub(crate) bundle_suffix: String,
    #[serde(default = "default_bundle_name")]
    pub(crate) bundle_name: BundleName,
    /// Install roots, unexpanded. Kept as written so the strings stay
    /// printable in `doctor` output on a host they don't describe.
    pub(crate) install_dir: PerOs<Option<ScopedDir>>,
    /// Read only through [`require_macos_pkg`](Self::require_macos_pkg),
    /// which the macOS packager owns.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) macos_pkg: Option<MacosPkg>,
    /// Native preset file extension, without the dot (`ttl`). Absent
    /// for a format with no on-disk preset container.
    #[serde(default)]
    pub(crate) preset_extension: Option<String>,
    /// Directories `cargo truce preset pull` sweeps for host-saved user
    /// presets. Same expansion as `install_dir`.
    #[serde(default)]
    pub(crate) preset_scan_dirs: PerOs<Vec<String>>,
    #[serde(default)]
    pub(crate) capabilities: Vec<Capability>,
    /// The `truce.toml` keys this helper reads. Only these are written
    /// into its config view; see [`write_config_view`].
    #[serde(default)]
    pub(crate) config_keys: Vec<String>,
    /// Resolved helper path. Filled in by [`describe`] rather than sent
    /// by the helper, which has no business naming itself.
    #[serde(skip)]
    helper: PathBuf,
}

fn default_bundle_name() -> BundleName {
    BundleName::FileStem
}

impl PartialEq for ExternalFormat {
    fn eq(&self, other: &Self) -> bool {
        self.format == other.format
    }
}

impl Eq for ExternalFormat {}

impl ExternalFormat {
    /// Resolve the defaults that depend on other fields, which serde's
    /// per-field `default` can't see. Called once, by [`describe`],
    /// before the handle is published - so every read site can treat
    /// these as always present.
    fn fill_defaults(&mut self) {
        if self.dylib_suffix.is_none() {
            self.dylib_suffix = Some(format!("_{}", self.format));
        }
    }

    /// Dylib name suffix (`_lv2`). Always set; see [`fill_defaults`](Self::fill_defaults).
    pub(crate) fn dylib_suffix(&self) -> &str {
        self.dylib_suffix
            .as_deref()
            .expect("fill_defaults ran during describe")
    }

    pub(crate) fn has(&self, c: Capability) -> bool {
        self.capabilities.contains(&c)
    }

    /// The format's `pkgbuild` / `productbuild` row.
    ///
    /// Callers reach this only from the macOS packaging pipeline, which
    /// drops externals without one (a format that doesn't ship in a
    /// `.pkg`) before it gets this far - see
    /// `package::macos::resolve_formats`.
    #[cfg(target_os = "macos")]
    pub(crate) fn require_macos_pkg(&self) -> &MacosPkg {
        self.macos_pkg
            .as_ref()
            .expect("resolve_formats drops externals with no macos_pkg row")
    }

    /// This host's `preset_scan_dirs`, expanded. A directory naming
    /// something that doesn't resolve here is dropped, same as
    /// [`dir`](Self::dir).
    pub(crate) fn preset_scan_dirs_here(&self) -> Vec<PathBuf> {
        self.preset_scan_dirs
            .here()
            .iter()
            .filter_map(|d| expand_dir(d))
            .collect()
    }

    /// This host's install roots, or `None` when the format declares
    /// none (a format that simply doesn't ship here).
    pub(crate) fn dirs_here(&self) -> Option<&ScopedDir> {
        self.install_dir.here().as_ref()
    }

    /// How this format spells a display name in a file name: LV2 wants
    /// the lowercase-hyphenated slug, because that is what its TTL's
    /// `lv2:binary` IRI resolves against and some hosts reject a
    /// bundle whose file names aren't URI-safe. Most formats keep the
    /// plugin's own file stem.
    ///
    /// Exposed on its own so a caller matching *existing* file names -
    /// `uninstall`'s orphan scan, say - can spell its needles the same
    /// way [`bundle_name_for`](Self::bundle_name_for) spelled them.
    pub(crate) fn spell(&self, name: &str) -> String {
        match self.bundle_name {
            BundleName::Slug => truce_utils::slugify(name),
            BundleName::FileStem => truce_utils::safe_filename(name),
        }
    }

    /// The bundle directory name for one plugin (`truce-gain.lv2`).
    pub(crate) fn bundle_name_for(&self, p: &crate::PluginDef) -> String {
        format!("{}{}", self.spell(&p.name), self.bundle_suffix)
    }

    /// The bundle extension without its leading dot (`lv2`), as the
    /// callers that compare against `Path::extension` need it.
    pub(crate) fn bundle_extension(&self) -> &str {
        self.bundle_suffix.trim_start_matches('.')
    }

    /// Where the helper answering for this format was found.
    ///
    /// Only diagnostics read it - every other caller goes through the
    /// verbs in this module rather than spawning the binary itself. It
    /// is worth printing because two helpers for one format can both be
    /// on `PATH`, and then which one won is the whole question.
    pub(crate) fn helper(&self) -> &Path {
        &self.helper
    }

    /// Install root for the requested scope on this host.
    ///
    /// A format with no `system` entry for this OS is scope-independent
    /// (Linux LV2 is `~/.lv2` whoever asks), so the user root answers
    /// both - matching what [`crate::install_scope`] already does for
    /// the built-in formats there.
    pub(crate) fn dir(&self, scope: crate::install_scope::InstallScope) -> Option<PathBuf> {
        let dirs = self.dirs_here()?;
        let raw = match scope {
            crate::install_scope::InstallScope::System => {
                dirs.system.as_deref().unwrap_or(&dirs.user)
            }
            crate::install_scope::InstallScope::User => &dirs.user,
        };
        expand_dir(raw)
    }
}

/// Expand a `describe` path: leading `~` is the user's home, `%VAR%` is
/// an environment variable. Returns `None` when something it names
/// doesn't resolve, so a helper describing a Windows path on Linux is
/// skipped rather than turned into a nonsense relative path.
pub(crate) fn expand_dir(raw: &str) -> Option<PathBuf> {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return Some(crate::dirs::home_dir()?.join(rest));
    }
    if let Some(rest) = raw.strip_prefix('%') {
        let (var, tail) = rest.split_once('%')?;
        let base = match var.to_ascii_uppercase().as_str() {
            // Resolved through the same helper the built-in Windows
            // paths use, so an external format lands in the same place
            // as VST3 rather than wherever the env happens to point.
            #[cfg(target_os = "windows")]
            "COMMONPROGRAMFILES" => crate::common_program_files(),
            other => PathBuf::from(std::env::var_os(other)?),
        };
        let tail = tail.trim_start_matches(['/', '\\']);
        return Some(if tail.is_empty() {
            base
        } else {
            base.join(tail)
        });
    }
    Some(PathBuf::from(raw))
}

/// Discovered helpers, keyed by format token. Leaked into `'static` on
/// first lookup so the format enums can stay `Copy`; see the module doc.
static REGISTRY: OnceLock<Mutex<BTreeMap<String, Option<&'static ExternalFormat>>>> =
    OnceLock::new();

/// Look up the helper for `name`, running its `describe` once per
/// process.
///
/// `Ok(None)` means no such helper is installed, which callers turn into
/// their own message - [`missing`] is the one for a format the user asked
/// for by name. An installed-but-broken helper is an `Err`: a helper that
/// exists and misbehaves is a bug to report, not a format to skip.
pub(crate) fn describe(name: &str) -> Result<Option<&'static ExternalFormat>, CargoTruceError> {
    let registry = REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut cache = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hit) = cache.get(name) {
        return Ok(*hit);
    }

    let Some(helper) = crate::find_on_path(&format!("{PREFIX}{name}")) else {
        cache.insert(name.to_string(), None);
        return Ok(None);
    };
    let described = run_describe(&helper, name)?;
    let leaked: &'static ExternalFormat = Box::leak(Box::new(described));
    cache.insert(name.to_string(), Some(leaked));
    Ok(Some(leaked))
}

fn run_describe(helper: &Path, name: &str) -> Result<ExternalFormat, CargoTruceError> {
    let out = Command::new(helper)
        .arg("describe")
        .output()
        .map_err(|e| format!("running `{} describe`: {e}", helper.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`{} describe` failed: {}",
            helper.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    let mut parsed: ExternalFormat = serde_json::from_slice(&out.stdout).map_err(|e| {
        format!(
            "`{} describe` is not valid describe JSON: {e}",
            helper.display()
        )
    })?;
    if parsed.schema != SCHEMA {
        return Err(format!(
            "{} speaks format schema {} but this cargo-truce speaks {SCHEMA}. \
             Update whichever is older.",
            helper.display(),
            parsed.schema
        )
        .into());
    }
    if parsed.format != name {
        return Err(format!(
            "{} describes itself as format {:?}, but its name says {name:?}",
            helper.display(),
            parsed.format
        )
        .into());
    }
    parsed.fill_defaults();
    parsed.helper = helper.to_path_buf();
    Ok(parsed)
}

/// The error for a format the user asked for whose helper isn't
/// installed. Names the crate to install rather than reporting the
/// format as unknown - it is not unknown, it is elsewhere.
pub(crate) fn missing(name: &str) -> CargoTruceError {
    format!(
        "{} support moved out of the truce repo; install its helper with \
         `cargo install {PREFIX}{name}` (it needs to be on PATH).",
        name.to_uppercase()
    )
    .into()
}

/// The installed helper whose native preset container carries `ext`
/// (no leading dot), if any.
///
/// The extension is all a file lying on disk offers for free, and each
/// helper declares its own, so container detection stays data-driven -
/// a table here would go stale the first time a format is added.
/// Restricted to helpers that actually claim
/// [`Capability::PresetCodec`]: a format with a preset extension but
/// no codec can't be asked to decode one.
///
/// # Errors
///
/// A helper on `PATH` answered `describe` with something unreadable.
pub(crate) fn by_preset_extension(
    ext: &str,
) -> Result<Option<&'static ExternalFormat>, CargoTruceError> {
    Ok(installed()?
        .into_iter()
        .find(|e| e.has(Capability::PresetCodec) && e.preset_extension.as_deref() == Some(ext)))
}

/// Read one CLI flag the caller didn't recognise as a format selector:
/// `--lv2` picks up the `truce-format-lv2` helper on `PATH`, and any
/// other installed helper answers to `--<its format>` the same way.
///
/// A flag naming a format this tool used to compile in ([`KNOWN`]) gets
/// [`missing`]'s actionable error instead of "unknown flag" - that is
/// the whole difference between a user who upgraded and a user who
/// typo'd.
///
/// # Errors
///
/// The flag names no installed helper, or a helper answered `describe`
/// with something unreadable.
pub(crate) fn select(
    flag: &str,
    selected: &mut Vec<&'static ExternalFormat>,
) -> Result<(), CargoTruceError> {
    let unknown = || CargoTruceError::from(format!("unknown flag: {flag}"));
    let name = flag
        .strip_prefix("--")
        .filter(|n| !n.is_empty())
        .ok_or_else(unknown)?;
    match describe(name)? {
        Some(fmt) => {
            if !selected.contains(&fmt) {
                selected.push(fmt);
            }
            Ok(())
        }
        None if KNOWN.contains(&name) => Err(missing(name)),
        None => Err(unknown()),
    }
}

/// The external formats to act on when the user named no format at all:
/// every installed helper whose cargo feature the project declares, so
/// a flagless `build` / `install` keeps covering LV2 for a project that
/// builds it.
///
/// A declared format with no helper installed is reported through
/// [`crate::log_skip`] rather than silently dropped - that is exactly
/// the state a project is in right after upgrading past the move of
/// these formats out of the truce repo.
///
/// # Errors
///
/// A helper on `PATH` answered `describe` with something unreadable.
pub(crate) fn defaults_for(
    available: &std::collections::HashSet<String>,
) -> Result<Vec<&'static ExternalFormat>, CargoTruceError> {
    for name in KNOWN {
        if available.contains(*name) && describe(name)?.is_none() {
            crate::log_skip(missing(name).to_string());
        }
    }
    Ok(installed()?
        .into_iter()
        .filter(|e| available.contains(&e.cargo_feature))
        .collect())
}

/// Write the helper's filtered config view to
/// `<meta>/<format>.config.toml` and return the path.
///
/// Carries the format-neutral plugin identity and port shape every
/// helper needs regardless (it would otherwise have to be passed as a
/// dozen flags), plus exactly the `truce.toml` keys the helper declared
/// in `config_keys` - nothing else, so the helper's config surface
/// stays an enumerable list a reader can check against the doc.
///
/// The `[plugin]` block spells its keys the way `truce.toml` does, so a
/// helper can deserialize it straight into a
/// [`truce_build::PluginDef`] rather than maintaining a parallel
/// struct. Two keys there are resolved rather than copied:
/// - `version` is the *effective* version
///   ([`truce_build::effective_plugin_version`]), not the
///   `[[plugin]] version` override, so a helper never re-derives the
///   workspace fallback.
/// - the `midi_*` keys are the raw overrides, left unresolved on
///   purpose: [`truce_build::midi_wiring`] turns them into port counts
///   and the helper calls it, so that resolution has exactly one
///   implementation instead of one per side of the seam.
pub(crate) fn write_config_view(
    fmt: &ExternalFormat,
    meta_dir: &Path,
    config: &crate::Config,
    p: &crate::PluginDef,
    version: &str,
) -> Result<PathBuf, CargoTruceError> {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "schema = {SCHEMA}");
    let _ = writeln!(out, "\n[plugin]");
    for (key, value) in [
        ("name", p.name.as_str()),
        ("bundle_id", p.bundle_id.as_str()),
        ("crate", p.crate_name.as_str()),
        ("version", version),
        ("category", p.category.as_str()),
    ] {
        let _ = writeln!(out, "{key} = \"{}\"", truce_build::toml_escape(value));
    }
    // Omitted when unset, matching `truce.toml`: `midi_wiring` reads
    // "absent" as "derive from the category" and `false` as "suppress
    // the port the category would have given", which are different.
    for (key, value) in [("midi_input", p.midi_input), ("midi_output", p.midi_output)] {
        if let Some(value) = value {
            let _ = writeln!(out, "{key} = {value}");
        }
    }
    for (key, value) in [
        ("midi_input_ports", p.midi_input_ports),
        ("midi_output_ports", p.midi_output_ports),
    ] {
        if let Some(value) = value {
            let _ = writeln!(out, "{key} = {value}");
        }
    }

    let _ = writeln!(out, "\n[vendor]");
    let _ = writeln!(
        out,
        "name = \"{}\"",
        truce_build::toml_escape(&config.vendor.name)
    );
    let _ = writeln!(
        out,
        "id = \"{}\"",
        truce_build::toml_escape(&config.vendor.id)
    );
    let _ = writeln!(
        out,
        "url = \"{}\"",
        truce_build::toml_escape(config.vendor.url.as_deref().unwrap_or(""))
    );

    if !fmt.config_keys.is_empty() {
        let _ = writeln!(out, "\n[config]");
        for key in &fmt.config_keys {
            match declared_key(key, p) {
                DeclaredValue::Set(value) => {
                    let _ = writeln!(out, "{key} = {value}");
                }
                DeclaredValue::Unset => {}
                DeclaredValue::Unknown => {
                    return Err(format!(
                        "{} declares config key {key:?}, which this cargo-truce does not \
                         know how to provide. Either the helper is newer than the tool or \
                         the key is a typo.",
                        fmt.label
                    )
                    .into());
                }
            }
        }
    }

    let path = meta_dir.join(format!("{}.config.toml", fmt.format));
    std::fs::create_dir_all(meta_dir)?;
    std::fs::write(&path, out)?;
    Ok(path)
}

/// What one declared `config_keys` entry resolves to. The two "no
/// value" cases are kept apart because they mean opposite things to the
/// caller: one is a key the helper may legitimately not get, the other
/// is a mismatch between helper and tool.
#[derive(Debug, PartialEq, Eq)]
enum DeclaredValue {
    /// The rendered TOML value to write for the key.
    Set(String),
    /// A known key that this plugin leaves unset, so it is omitted and
    /// the helper falls back to its own default.
    Unset,
    /// Not a key this cargo-truce knows how to provide at all.
    Unknown,
}

/// Render one declared `config_keys` entry.
fn declared_key(key: &str, p: &crate::PluginDef) -> DeclaredValue {
    /// `None` -> [`DeclaredValue::Unset`]: the shape every known key's
    /// lookup already has.
    fn known(value: Option<String>) -> DeclaredValue {
        value.map_or(DeclaredValue::Unset, DeclaredValue::Set)
    }

    match key {
        "lv2_name" => known(
            p.lv2_name
                .as_deref()
                .map(|v| format!("\"{}\"", truce_build::toml_escape(v))),
        ),
        "legacy_state.lv2_uris" => {
            let uris = p
                .legacy_state
                .as_ref()
                .map(|l| l.lv2_uris.as_slice())
                .unwrap_or_default();
            known((!uris.is_empty()).then(|| {
                let joined = uris
                    .iter()
                    .map(|u| format!("\"{}\"", truce_build::toml_escape(u)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{joined}]")
            }))
        }
        _ => DeclaredValue::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Invoking a helper
// ---------------------------------------------------------------------------

/// One factory preset in format-neutral terms: the canonical state
/// envelope plus its parameter values keyed by truce's own param ids.
///
/// Ids rather than the format's own port names, deliberately. The
/// helper already reads the metadata tree, so it can map ids onto
/// whatever vocabulary it speaks (`lv2:symbol`, say) - and that keeps
/// the mapping, which is format knowledge, out of this repo.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Preset {
    pub(crate) uuid: String,
    pub(crate) name: String,
    /// Display category, empty when uncategorized.
    pub(crate) category: String,
    /// The authored file's stem. A helper that writes one file per
    /// preset names it after this rather than after `name`, so a host
    /// persisting preset paths keeps resolving them across a rename.
    pub(crate) stem: String,
    /// The canonical state envelope, base64 (`STANDARD`).
    pub(crate) blob: String,
    /// Plain parameter values, in the envelope's own order - which is
    /// the params struct's declaration order, the same order the
    /// metadata tree lists them in.
    ///
    /// A list rather than a map because that order is the point: a
    /// helper rendering per-parameter entries wants them in the same
    /// sequence its port declarations are in, and a map keyed by the
    /// FNV id would silently sort them by hash instead.
    pub(crate) params: Vec<(u32, f64)>,
}

impl Preset {
    /// Build one from a decoded envelope. `blob` is the envelope bytes;
    /// `params` the `(id, value)` pairs already recovered from it, in
    /// the order they appeared there.
    pub(crate) fn new(
        uuid: &str,
        name: &str,
        category: &str,
        stem: &str,
        blob: &[u8],
        params: &[(u32, f64)],
    ) -> Self {
        use base64::Engine as _;
        Self {
            uuid: uuid.to_string(),
            name: name.to_string(),
            category: category.to_string(),
            stem: stem.to_string(),
            blob: base64::engine::general_purpose::STANDARD.encode(blob),
            params: params.to_vec(),
        }
    }
}

/// Everything a `stage` run needs. Paths are absolute; `out` is the
/// bundle directory itself, which we create empty before spawning the
/// helper so a format whose bundle is a plain directory doesn't have to
/// think about it.
#[derive(Serialize)]
struct StageRequest<'a> {
    schema: u32,
    /// The plugin's slice of the format-neutral metadata tree.
    meta: &'a Path,
    /// The helper's filtered `truce.toml` view, from
    /// [`write_config_view`].
    config: &'a Path,
    /// The built dylib to install into the bundle.
    dylib: &'a Path,
    /// The bundle directory to fill. Created empty for the helper,
    /// which may also create it itself - `stage` has to work when
    /// driven by hand.
    out: &'a Path,
    /// `"macos"` / `"linux"` / `"windows"` - the OS the bundle is *for*,
    /// which on a cross-compile is not the one we're running on.
    target_os: &'a str,
    /// Factory presets to write into the bundle. Empty means none;
    /// a helper without [`Capability::Presets`] is never sent any.
    presets: &'a [Preset],
}

/// What a `stage` run produced.
#[derive(Deserialize)]
pub(crate) struct Staged {
    schema: u32,
    /// Paths inside the bundle that need codesigning, in the order the
    /// helper wants them signed. Empty for a format with nothing
    /// Mach-O-shaped in it.
    ///
    /// The helper decides, rather than truce guessing from a `describe`
    /// field, because "what in here is a Mach-O" is exactly the kind of
    /// layout detail only the thing that laid it out knows.
    #[serde(default)]
    pub(crate) signable: Vec<PathBuf>,
}

/// Render one plugin's bundle into `request.out`.
pub(crate) fn stage(
    fmt: &ExternalFormat,
    meta: &Path,
    config: &Path,
    dylib: &Path,
    out: &Path,
    target_os: &str,
    presets: &[Preset],
) -> Result<Staged, CargoTruceError> {
    let request = StageRequest {
        schema: SCHEMA,
        meta,
        config,
        dylib,
        out,
        target_os,
        presets,
    };
    let answer = call(fmt, "stage", &to_json(&request)?)?;
    let staged: Staged = from_json(fmt, "stage", &answer)?;
    check_response_schema(fmt, "stage", staged.schema)?;
    Ok(staged)
}

#[derive(Serialize)]
struct EncodeRequest<'a> {
    schema: u32,
    meta: &'a Path,
    config: &'a Path,
    preset: &'a Preset,
}

/// Encode one preset into this format's native container, returning the
/// file's bytes. Callers write them wherever they need (a `.lv2` bundle,
/// a zip entry), so the helper never has to know about either.
pub(crate) fn preset_encode(
    fmt: &ExternalFormat,
    meta: &Path,
    config: &Path,
    preset: &Preset,
) -> Result<Vec<u8>, CargoTruceError> {
    let request = EncodeRequest {
        schema: SCHEMA,
        meta,
        config,
        preset,
    };
    call(fmt, "preset-encode", &to_json(&request)?)
}

/// The wire form of a `preset-decode` answer.
#[derive(Deserialize)]
struct DecodedWire {
    schema: u32,
    #[serde(default)]
    name: String,
    /// The canonical state envelope, base64 (`STANDARD`).
    blob: String,
}

/// What a `preset-decode` run recovered from a native preset file.
pub(crate) struct Decoded {
    /// Display name, empty when the container carries none (callers
    /// fall back to the file stem).
    pub(crate) name: String,
    /// The canonical state envelope.
    pub(crate) blob: Vec<u8>,
}

/// Decode a native preset file, or `Ok(None)` when the helper doesn't
/// recognise it - which is routine, since `preset pull` sweeps shared
/// directories full of other plugins' and other vendors' files.
///
/// This is the one verb that takes raw bytes on stdin rather than a JSON
/// request: there is nothing else to say, and base64-wrapping a whole
/// preset file to keep the shape symmetric would be worse than the
/// asymmetry.
pub(crate) fn preset_decode(
    fmt: &ExternalFormat,
    bytes: &[u8],
) -> Result<Option<Decoded>, CargoTruceError> {
    use base64::Engine as _;
    let answer = call(fmt, "preset-decode", bytes)?;
    // A helper signals "not mine" with empty stdout, so a file it can't
    // read doesn't have to be an error every caller then suppresses.
    if answer.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let wire: DecodedWire = from_json(fmt, "preset-decode", &answer)?;
    check_response_schema(fmt, "preset-decode", wire.schema)?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(&wire.blob)
        .map_err(|e| describe_err(fmt, "preset-decode", &format!("blob is not base64: {e}")))?;
    Ok(Some(Decoded {
        name: wire.name,
        blob,
    }))
}

/// Spawn `truce-format-<name> <verb>`, feed it `stdin`, and return its
/// stdout.
///
/// Stderr is inherited, as the helper protocol promises: a helper's
/// diagnostics reach the user as they happen, and a failure points at
/// them rather than quoting a buffered copy.
///
/// The request is written from its own thread while this one drains
/// stdout. A `stage` request inlines every factory preset as base64, so
/// it can outgrow the pipe buffer, and writing all of it before reading
/// would hang both processes as soon as the helper filled its stdout
/// before it had read its whole request.
fn call(fmt: &ExternalFormat, verb: &str, stdin: &[u8]) -> Result<Vec<u8>, CargoTruceError> {
    let mut child = Command::new(&fmt.helper)
        .arg(verb)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| describe_err(fmt, verb, &format!("could not run it: {e}")))?;
    let mut request = child.stdin.take().expect("stdin was piped");
    let (sent, out) = std::thread::scope(|s| {
        // Dropping `request` when the write finishes is the helper's EOF.
        let writer = s.spawn(move || request.write_all(stdin));
        let out = child.wait_with_output();
        (
            writer.join().expect("the request writer doesn't panic"),
            out,
        )
    });
    let out =
        out.map_err(|e| describe_err(fmt, verb, &format!("could not read its answer: {e}")))?;
    // A helper that exits early fails the write with a broken pipe, so its
    // exit status is checked first: that is the error worth reporting.
    if !out.status.success() {
        return Err(describe_err(
            fmt,
            verb,
            &format!("failed ({}); its own output is above", out.status),
        ));
    }
    // A helper may answer from part of its request and exit - `preset-decode`
    // saying "not mine" after the header - which closes the pipe under the
    // writer. Once it has exited successfully, that broken pipe is just the
    // rest of the request going unread, not a failure.
    if let Err(e) = sent
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(describe_err(
            fmt,
            verb,
            &format!("could not send it the request: {e}"),
        ));
    }
    Ok(out.stdout)
}

fn to_json<T: Serialize>(request: &T) -> Result<Vec<u8>, CargoTruceError> {
    serde_json::to_vec(request).map_err(|e| format!("building a format-helper request: {e}").into())
}

fn from_json<T: for<'de> Deserialize<'de>>(
    fmt: &ExternalFormat,
    verb: &str,
    bytes: &[u8],
) -> Result<T, CargoTruceError> {
    serde_json::from_slice(bytes)
        .map_err(|e| describe_err(fmt, verb, &format!("answered with invalid JSON: {e}")))
}

/// A response announcing a different protocol version than the
/// `describe` that got us here means the helper is internally
/// inconsistent, which is worth saying plainly rather than letting the
/// fields be read as if they meant what we think.
fn check_response_schema(
    fmt: &ExternalFormat,
    verb: &str,
    schema: u32,
) -> Result<(), CargoTruceError> {
    if schema == SCHEMA {
        return Ok(());
    }
    Err(describe_err(
        fmt,
        verb,
        &format!("answered with schema {schema}, but its `describe` said {SCHEMA}"),
    ))
}

fn describe_err(fmt: &ExternalFormat, verb: &str, detail: &str) -> CargoTruceError {
    format!("`{} {verb}`: {detail}", fmt.helper.display()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv2() -> ExternalFormat {
        serde_json::from_str(
            r#"{
                "schema": 1,
                "format": "lv2",
                "label": "LV2",
                "cargo_feature": "lv2",
                "bundle_suffix": ".lv2",
                "bundle_name": "slug",
                "install_dir": {
                    "macos": { "user": "~/Library/Audio/Plug-Ins/LV2",
                               "system": "/Library/Audio/Plug-Ins/LV2" },
                    "linux": { "user": "~/.lv2" },
                    "windows": { "user": "%APPDATA%/LV2",
                                 "system": "%COMMONPROGRAMFILES%/LV2" }
                },
                "preset_extension": "ttl",
                "preset_scan_dirs": {
                    "macos": ["~/.lv2"],
                    "linux": ["~/.lv2"]
                },
                "capabilities": ["stage", "presets", "preset-codec"],
                "config_keys": ["lv2_name", "legacy_state.lv2_uris"]
            }"#,
        )
        .expect("the documented describe output parses")
    }

    /// Uninstall falls back to these for a format whose helper is
    /// missing, so a format listed as once built in without one would
    /// panic there instead of finding its 6.x bundles.
    #[test]
    fn every_known_format_has_a_legacy_layout() {
        for name in KNOWN {
            let layout = legacy_layout(name);
            assert_eq!(layout.format, *name);
            assert!(
                layout.capabilities.is_empty(),
                "a legacy handle has no helper to send a verb to"
            );
        }
    }

    /// The 6.x layout is a fact about where 6.x put bundles; the LV2
    /// helper declares the same places today, which is what lets one
    /// uninstall path serve both.
    #[test]
    fn the_legacy_lv2_layout_matches_the_helpers() {
        let legacy = legacy_layout("lv2");
        let helper = lv2();
        assert_eq!(legacy.bundle_suffix, helper.bundle_suffix);
        assert_eq!(legacy.bundle_name, helper.bundle_name);
        let roots = |d: &PerOs<Option<ScopedDir>>| {
            [&d.macos, &d.linux, &d.windows]
                .map(|os| os.as_ref().map(|r| (r.user.clone(), r.system.clone())))
        };
        assert_eq!(roots(&legacy.install_dir), roots(&helper.install_dir));
    }

    /// A helper may answer without reading its whole request (`preset-decode`
    /// declining a file after its header). Its exit status decides the
    /// call; the broken pipe the unread rest causes doesn't.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_exits_without_reading_its_request_succeeds() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("truce-call-unread-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("truce-format-unread");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut fmt = lv2();
        fmt.helper = script;
        // Well past any pipe buffer, so the writer is still mid-request
        // when the helper exits.
        let answer = call(&fmt, "preset-decode", &vec![b'x'; 1 << 20]);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(answer.unwrap(), Vec::<u8>::new());
    }

    /// A `stage` request inlines every factory preset, so it can be far
    /// larger than a pipe buffer. A helper that writes its answer before
    /// it has read the whole request must not deadlock against us.
    #[cfg(unix)]
    #[test]
    fn a_large_request_to_a_helper_answering_early_does_not_deadlock() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("truce-call-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("truce-format-early");
        // 1 MiB of answer before reading a byte of the 4 MiB request:
        // both outgrow any pipe buffer.
        std::fs::write(
            &script,
            "#!/bin/sh\nhead -c 1048576 /dev/zero\ncat >/dev/null\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut fmt = lv2();
        fmt.helper = script;
        let request = vec![b'x'; 4 << 20];

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(call(&fmt, "stage", &request).map(|o| o.len())));
        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("call() deadlocked on a request larger than the pipe buffer");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(answer.unwrap(), 1 << 20);
    }

    /// The `describe` block in the extraction doc is the contract; if it
    /// stops deserializing, the doc and the tool have drifted.
    #[test]
    fn the_documented_describe_output_round_trips() {
        let f = lv2();
        assert_eq!(f.format, "lv2");
        assert_eq!(f.bundle_name, BundleName::Slug);
        assert!(f.has(Capability::PresetCodec));
        // Absent `system` is "this OS has no machine-wide location",
        // not "same as user".
        assert!(f.install_dir.linux.as_ref().unwrap().system.is_none());
        // An OS the helper says nothing about resolves to nothing,
        // rather than to a default that would be wrong everywhere.
        assert_eq!(f.preset_scan_dirs.windows, Vec::<String>::new());
    }

    /// An omitted `dylib_suffix` has to agree with what the built-in
    /// formats do, or the staging step looks for the wrong file.
    #[test]
    fn dylib_suffix_defaults_to_the_format_token() {
        let mut f = lv2();
        assert!(f.dylib_suffix.is_none(), "the sample omits it");
        f.fill_defaults();
        assert_eq!(f.dylib_suffix(), "_lv2");
    }

    #[test]
    fn a_declared_key_we_cannot_provide_is_an_error_not_a_default() {
        let mut f = lv2();
        f.config_keys = vec!["something_invented".into()];
        assert_eq!(
            declared_key("something_invented", &plugin()),
            DeclaredValue::Unknown
        );
        // ... while the two LV2 keys are both known.
        assert_ne!(declared_key("lv2_name", &plugin()), DeclaredValue::Unknown);
        assert_ne!(
            declared_key("legacy_state.lv2_uris", &plugin()),
            DeclaredValue::Unknown
        );
    }

    /// An unset key is omitted rather than written as an empty string,
    /// so the helper can tell "not configured" from "configured empty".
    #[test]
    fn an_unset_declared_key_is_omitted() {
        let mut p = plugin();
        assert_eq!(declared_key("lv2_name", &p), DeclaredValue::Unset);
        p.shared.lv2_name = Some("Custom Name".into());
        assert_eq!(
            declared_key("lv2_name", &p),
            DeclaredValue::Set("\"Custom Name\"".into())
        );
    }

    #[test]
    fn expansion_leaves_an_absolute_path_alone() {
        assert_eq!(
            expand_dir("/Library/Audio/Plug-Ins/LV2"),
            Some(PathBuf::from("/Library/Audio/Plug-Ins/LV2"))
        );
    }

    #[test]
    fn expansion_resolves_a_leading_tilde() {
        let home = crate::dirs::home_dir().expect("a home dir");
        assert_eq!(expand_dir("~/.lv2"), Some(home.join(".lv2")));
    }

    fn plugin() -> crate::PluginDef {
        crate::config::test_plugin("Gain", "gain")
    }
}
