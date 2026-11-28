//! Top-level command implementations. Each `cmd_*` function corresponds
//! to one entry in the dispatch match in `crate::run`.

pub(crate) mod build;
pub(crate) mod build_dylibs;
pub(crate) mod doctor;
pub(crate) mod install;
pub(crate) mod log_stream_au;
pub(crate) mod package;
pub(crate) mod preset;
pub(crate) mod reset_aax;
pub(crate) mod reset_au;
pub(crate) mod run;
pub(crate) mod screenshot;
pub(crate) mod status;
pub(crate) mod uninstall;
pub(crate) mod validate;

use crate::{Config, PluginDef};

/// Filter `config.plugin` by an optional `-p <crate>` value. Returns
/// the full plugin list when `filter` is `None`. On miss, the error
/// lists available crate names so the user can fix a typo without
/// re-reading their truce.toml.
///
/// Three subcommands (`install`, `build`, `validate`) share this
/// pattern verbatim; centralizing the lookup keeps the not-found
/// message phrased identically across them.
/// The plugins named by `filters` (crate names, each as `-p` takes it), in
/// `truce.toml` order; every plugin when `filters` is empty.
pub(crate) fn pick_plugins_many<'a>(
    config: &'a Config,
    filters: &[String],
) -> Result<Vec<&'a PluginDef>, crate::CargoTruceError> {
    if filters.is_empty() {
        return pick_plugins(config, None);
    }
    for f in filters {
        pick_plugins(config, Some(f))?;
    }
    Ok(config
        .plugin
        .iter()
        .filter(|p| filters.iter().any(|f| *f == p.crate_name))
        .collect())
}

pub(crate) fn pick_plugins<'a>(
    config: &'a Config,
    filter: Option<&str>,
) -> Result<Vec<&'a PluginDef>, crate::CargoTruceError> {
    match filter {
        Some(f) => {
            let matched: Vec<_> = config.plugin.iter().filter(|p| p.crate_name == f).collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with crate name '{f}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| p.crate_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            Ok(matched)
        }
        None => Ok(config.plugin.iter().collect()),
    }
}
