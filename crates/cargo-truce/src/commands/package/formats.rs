//! Which formats each plugin is packaged in.
//!
//! Every plugin gets its own list. Without `--formats` (or `[packaging]
//! formats` in truce.toml) that's the formats its own `[features].default`
//! turns on; with it, the requested formats the plugin declares a feature
//! for, a format it can't be built in being skipped with a note. One list
//! for the whole workspace would build every plugin in every plugin's
//! default formats - a plugin with no `standalone` feature then fails the
//! run.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::PkgFormat;
use crate::{CargoTruceError, PluginDef};

/// The formats of every selected plugin, worked out once per run.
pub(crate) struct PluginFormats {
    /// Every format at least one plugin gets, in the run's order.
    all: Vec<PkgFormat>,
    /// Each plugin's formats by `bundle_id`, in the run's order.
    by_plugin: HashMap<String, Vec<PkgFormat>>,
}

impl PluginFormats {
    /// Resolve `plugins`' formats. `requested` is `--formats` or
    /// `[packaging] formats`; `keep` is the packager's filter for what its
    /// OS can ship, applied once to the run's list (it may note what it
    /// drops), and each plugin's list is narrowed to what it keeps.
    pub(crate) fn resolve(
        root: &Path,
        plugins: &[&PluginDef],
        requested: Option<&[PkgFormat]>,
        keep: impl FnOnce(Vec<PkgFormat>) -> Result<Vec<PkgFormat>, CargoTruceError>,
    ) -> Result<Self, CargoTruceError> {
        match requested {
            Some(requested) => combine(&requested_lists(root, plugins, requested), requested, keep),
            None => combine(&default_lists(root, plugins)?, &COMPILED_IN, keep),
        }
    }

    /// Every format at least one selected plugin is packaged in.
    pub(crate) fn all(&self) -> &[PkgFormat] {
        &self.all
    }

    /// `p`'s formats. Empty for a plugin `resolve` wasn't given.
    pub(crate) fn of(&self, p: &PluginDef) -> &[PkgFormat] {
        self.by_plugin.get(&p.bundle_id).map_or(&[], Vec::as_slice)
    }

    /// Apply a per-plugin rule (one that depends on the plugin's other
    /// formats) to every plugin's list, and recompute the run's list.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn adjust_each(&mut self, rule: impl Fn(Vec<PkgFormat>) -> Vec<PkgFormat>) {
        for list in self.by_plugin.values_mut() {
            *list = rule(std::mem::take(list));
        }
        let by_plugin = &self.by_plugin;
        self.all
            .retain(|f| by_plugin.values().any(|l| l.contains(f)));
    }

    /// The plugins among `plugins` packaged in `format`.
    pub(crate) fn plugins_with<'a>(
        &self,
        plugins: &[&'a PluginDef],
        format: PkgFormat,
    ) -> Vec<&'a PluginDef> {
        plugins
            .iter()
            .copied()
            .filter(|p| self.of(p).contains(&format))
            .collect()
    }
}

/// The compiled-in formats in the order a run lists them; helper formats
/// follow.
const COMPILED_IN: [PkgFormat; 7] = [
    PkgFormat::Clap,
    PkgFormat::Vst3,
    PkgFormat::Vst2,
    PkgFormat::Au2,
    PkgFormat::Au3,
    PkgFormat::Aax,
    PkgFormat::Standalone,
];

/// The plugin crate's Cargo feature that builds `f`.
fn feature(f: PkgFormat) -> &'static str {
    match f {
        PkgFormat::Clap => "clap",
        PkgFormat::Vst3 => "vst3",
        PkgFormat::Vst2 => "vst2",
        PkgFormat::Au2 | PkgFormat::Au3 => "au",
        PkgFormat::Aax => "aax",
        PkgFormat::Standalone => "standalone",
        PkgFormat::External(e) => &e.cargo_feature,
    }
}

/// Each plugin's share of the `requested` formats: those it declares a
/// feature for. Notes each one skipped.
fn requested_lists<'a>(
    root: &Path,
    plugins: &[&'a PluginDef],
    requested: &[PkgFormat],
) -> Vec<(&'a PluginDef, Vec<PkgFormat>)> {
    plugins
        .iter()
        .map(|p| {
            let declared = crate::util::plugin_declared_features(root, &p.crate_name);
            let (kept, skipped) = select(requested, &declared, has_standalone_bin(p));
            for f in skipped {
                eprintln!(
                    "note: {} {}; packaging it without {}.",
                    p.name,
                    why_not(f),
                    f.label()
                );
            }
            (*p, kept)
        })
        .collect()
}

/// Each plugin's own default formats: those its `[features].default`
/// turns on, an installed helper format included.
fn default_lists<'a>(
    root: &Path,
    plugins: &[&'a PluginDef],
) -> Result<Vec<(&'a PluginDef, Vec<PkgFormat>)>, CargoTruceError> {
    let enabled: Vec<HashSet<String>> = plugins
        .iter()
        .map(|p| crate::util::plugin_enabled_default_features(root, &p.crate_name))
        .collect();
    // The installed helper formats among anyone's defaults; a missing
    // helper is noted once here.
    let union: HashSet<String> = enabled.iter().flatten().cloned().collect();
    let externals: Vec<PkgFormat> = crate::external::defaults_for(&union)?
        .into_iter()
        .map(PkgFormat::External)
        .collect();
    let candidates: Vec<PkgFormat> = COMPILED_IN.iter().copied().chain(externals).collect();
    Ok(plugins
        .iter()
        .zip(&enabled)
        .map(|(p, on)| (*p, select(&candidates, on, has_standalone_bin(p)).0))
        .collect())
}

fn has_standalone_bin(p: &PluginDef) -> bool {
    crate::read_standalone_bin_name(&p.crate_name).is_some()
}

/// Split `candidates` into the formats a plugin with `features` (and a
/// standalone `[[bin]]` or not) can be built in, and the rest.
fn select(
    candidates: &[PkgFormat],
    features: &HashSet<String>,
    standalone_bin: bool,
) -> (Vec<PkgFormat>, Vec<PkgFormat>) {
    candidates.iter().copied().partition(|&f| {
        features.contains(feature(f)) && (f != PkgFormat::Standalone || standalone_bin)
    })
}

/// Why a requested format was skipped for a plugin, for the note.
fn why_not(f: PkgFormat) -> String {
    if f == PkgFormat::Standalone {
        "has no standalone feature or no standalone [[bin]]".to_string()
    } else {
        format!("has no `{}` feature", feature(f))
    }
}

/// The run's list (every plugin's formats in `order`, helper formats
/// after, passed through `keep`) and each plugin's list narrowed to it.
/// Fails naming a plugin left with no format: its installer would be
/// empty.
fn combine(
    lists: &[(&PluginDef, Vec<PkgFormat>)],
    order: &[PkgFormat],
    keep: impl FnOnce(Vec<PkgFormat>) -> Result<Vec<PkgFormat>, CargoTruceError>,
) -> Result<PluginFormats, CargoTruceError> {
    let mut union: Vec<PkgFormat> = order
        .iter()
        .copied()
        .filter(|f| lists.iter().any(|(_, l)| l.contains(f)))
        .collect();
    for (_, l) in lists {
        for &f in l {
            if !union.contains(&f) {
                union.push(f);
            }
        }
    }
    let all = keep(union)?;
    let mut by_plugin = HashMap::new();
    for (p, l) in lists {
        let mine: Vec<PkgFormat> = all.iter().copied().filter(|f| l.contains(f)).collect();
        if mine.is_empty() {
            return Err(format!(
                "{} can't be built in any of the selected formats ({}); leave it out \
                 with -p or --suite, or pick formats it has",
                p.name,
                all.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")
            )
            .into());
        }
        by_plugin.insert(p.bundle_id.clone(), mine);
    }
    Ok(PluginFormats { all, by_plugin })
}

#[cfg(test)]
mod tests {
    use super::{PkgFormat, combine, select};
    use crate::config::test_plugin;
    use std::collections::HashSet;

    fn set(v: &[&str]) -> HashSet<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_plugin_gets_the_formats_its_features_build() {
        let all = [
            PkgFormat::Clap,
            PkgFormat::Vst3,
            PkgFormat::Au2,
            PkgFormat::Au3,
            PkgFormat::Standalone,
        ];
        let (kept, skipped) = select(&all, &set(&["clap", "au", "standalone"]), true);
        assert_eq!(
            kept,
            [
                PkgFormat::Clap,
                PkgFormat::Au2,
                PkgFormat::Au3,
                PkgFormat::Standalone
            ]
        );
        assert_eq!(skipped, [PkgFormat::Vst3]);
        // A standalone feature without a standalone [[bin]] builds nothing.
        let (kept, _) = select(&all, &set(&["clap", "standalone"]), false);
        assert_eq!(kept, [PkgFormat::Clap]);
    }

    #[test]
    fn each_plugin_keeps_its_own_list() {
        let synth = test_plugin("Synth", "synth");
        let envelope = test_plugin("Envelope", "envelope");
        let lists = vec![
            (
                &synth,
                vec![PkgFormat::Clap, PkgFormat::Vst3, PkgFormat::Standalone],
            ),
            (&envelope, vec![PkgFormat::Vst2, PkgFormat::Clap]),
        ];
        let order = [
            PkgFormat::Clap,
            PkgFormat::Vst3,
            PkgFormat::Vst2,
            PkgFormat::Standalone,
        ];
        let pf = combine(&lists, &order, Ok).unwrap();
        assert_eq!(pf.all(), order);
        assert_eq!(
            pf.of(&synth),
            [PkgFormat::Clap, PkgFormat::Vst3, PkgFormat::Standalone]
        );
        assert_eq!(pf.of(&envelope), [PkgFormat::Clap, PkgFormat::Vst2]);
        let with_standalone = pf.plugins_with(&[&synth, &envelope], PkgFormat::Standalone);
        assert_eq!(with_standalone.len(), 1);
        assert_eq!(with_standalone[0].bundle_id, "synth");

        // The OS filter narrows every plugin's list too.
        let no_standalone = combine(&lists, &order, |mut v| {
            v.retain(|f| *f != PkgFormat::Standalone);
            Ok(v)
        })
        .unwrap();
        assert_eq!(no_standalone.of(&synth), [PkgFormat::Clap, PkgFormat::Vst3]);
    }

    #[test]
    fn a_plugin_left_with_nothing_fails() {
        let envelope = test_plugin("Envelope", "envelope");
        let lists = vec![(&envelope, Vec::new())];
        let err = combine(&lists, &[PkgFormat::Standalone], Ok)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Envelope can't be built"), "{err}");
    }
}
