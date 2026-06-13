//! Preset library access for the standalone host.
//!
//! Standalone is the one "host" truce owns, so it browses and loads
//! presets itself rather than deferring to a DAW. This module wires
//! `truce_core::presets::PresetStore` to the plugin's identity and
//! resolves the **factory** root from two first-class sources (the
//! installed `.app`/bundle, or a `--presets-dir` handoff during
//! `cargo truce run`); user and pack scopes always resolve from
//! `P::info()` alone.
//!
//! Lean by construction: this reads only `.trucepreset` containers
//! through `truce-core`, never authored TOML, so the shipped binary
//! gains no `toml` / `truce-build` weight.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use truce_core::export::PluginExport;
use truce_core::presets::{PresetScope, PresetStore};
use truce_core::state::{apply_state, hash_plugin_id};
use truce_params::Params;

use crate::vlog;

/// Build a `PresetStore` for `P`, resolving the factory root from
/// `presets_dir` (the `--presets-dir` / `TRUCE_PRESETS_DIR` handoff)
/// when given, else from the host's own installed bundle.
#[must_use]
pub fn store<P: PluginExport>(presets_dir: Option<&std::path::Path>) -> PresetStore {
    let info = P::info();
    let hash = hash_plugin_id(info.clap_id);
    let mut store = PresetStore::new(info.vendor, info.name, hash, info.preset_user_dir);
    if let Some(root) = presets_dir
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(installed_factory_root)
    {
        store = store.with_factory_root(root);
    }
    store
}

/// Factory presets shipped inside the host's own installed bundle.
/// Standalone packages as `<Plugin>.app/Contents/MacOS/<bin>` on
/// macOS, so `Contents/Resources/Presets/` is two levels up from the
/// executable's directory; other platforms place the binary directly
/// in the install tree, where a sibling `<bin>.presets/` directory
/// carries them. Returns `None` when running from `target/` (no
/// bundle) - the dev loop uses `--presets-dir` instead.
fn installed_factory_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    #[cfg(target_os = "macos")]
    {
        // .../Contents/MacOS/<bin> -> .../Contents/Resources/Presets
        let resources = dir.parent()?.join("Resources/Presets");
        if resources.is_dir() {
            return Some(resources);
        }
    }

    // Sibling `<bin>.presets/` next to the executable (Linux/Windows
    // install trees, and a macOS fallback if the layout above misses).
    let stem = exe.file_stem()?.to_string_lossy().into_owned();
    let sibling = dir.join(format!("{stem}.presets"));
    sibling.is_dir().then_some(sibling)
}

/// Resolve a preset by uri / uuid / name and apply it to `plugin`.
/// The `&mut P` core both the build-time path (which holds the
/// plugin directly before it goes behind the `Arc<Mutex>`) and the
/// runtime path ([`load_into`]) call. Logs one line either way.
pub fn apply_selected<P: PluginExport>(store: &PresetStore, plugin: &mut P, sel: &str) -> bool {
    let Some(preset) = store.find(sel) else {
        eprintln!("no preset matching {sel:?}");
        return false;
    };
    match store.load(&preset.uri) {
        Ok(state) => {
            apply_state(plugin, &state);
            vlog!(
                "loaded preset \"{}\" ({})",
                preset.name,
                scope_label(preset.scope)
            );
            true
        }
        Err(e) => {
            eprintln!("failed to load preset {:?}: {e}", preset.name);
            false
        }
    }
}

/// Build-time entry: resolve the store from `presets_dir` and apply
/// `sel` to a plugin still held by value. Used where `--state` is
/// applied, before `snap_smoothers`.
pub fn apply_on_launch<P: PluginExport>(
    presets_dir: Option<&std::path::Path>,
    plugin: &mut P,
    sel: &str,
) -> bool {
    apply_selected(&store::<P>(presets_dir), plugin, sel)
}

/// Runtime entry: lock the plugin briefly on the calling (UI/main)
/// thread and apply. The audio callback `try_lock`s and skips at
/// most one block while held. For the (deferred) preset menu /
/// key bindings.
#[allow(dead_code)]
pub fn load_into<P: PluginExport>(store: &PresetStore, plugin: &Arc<Mutex<P>>, sel: &str) -> bool {
    let Ok(mut guard) = plugin.lock() else {
        eprintln!("could not lock plugin to load preset");
        return false;
    };
    apply_selected(store, &mut *guard, sel)
}

/// Snapshot the live plugin (params + `save_state`) and write it to
/// the user scope as a `.trucepreset`. The Cmd+S quicksave path:
/// `meta.name` is the only required field; a same-name save keeps
/// the preset's uuid. Returns the saved file path.
pub fn save_user<P: PluginExport>(
    store: &PresetStore,
    plugin: &Arc<Mutex<P>>,
    meta: truce_utils::preset::PresetMeta,
) -> Option<PathBuf> {
    let (ids, values, extra) = {
        let guard = plugin.lock().ok()?;
        let (ids, values) = guard.params().collect_values();
        (ids, values, guard.save_state())
    };
    let params: Vec<(u32, f64)> = ids.into_iter().zip(values).collect();
    match store.save(meta, &params, &extra) {
        Ok(preset) => {
            vlog!(
                "saved preset \"{}\" -> {}",
                preset.name,
                preset.path.display()
            );
            Some(preset.path)
        }
        Err(e) => {
            eprintln!("failed to save preset: {e}");
            None
        }
    }
}

/// Print every preset across all scopes - the `--list-presets`
/// implementation, standalone's analogue of `cargo truce preset
/// list`.
pub fn print_list<P: PluginExport>(presets_dir: Option<&std::path::Path>) {
    let store = store::<P>(presets_dir);
    let presets = store.enumerate();
    if presets.is_empty() {
        eprintln!("no presets for {}", P::info().name);
        if let Some(root) = store.user_root() {
            eprintln!("(user presets would live in {})", root.display());
        }
        return;
    }
    for p in presets {
        let category = p.category.as_deref().unwrap_or("-");
        println!("{:<8} {:<14} {}", scope_label(p.scope), category, p.name);
    }
}

fn scope_label(scope: PresetScope) -> &'static str {
    match scope {
        PresetScope::Factory => "factory",
        PresetScope::User => "user",
        PresetScope::Pack => "pack",
    }
}

// ---------------------------------------------------------------------------
// PresetController - the native menu's type-erased handle
// ---------------------------------------------------------------------------

/// One row in the preset menu: a display label and the uri to load.
pub struct PresetMenuEntry {
    pub label: String,
    pub uri: String,
}

/// A `Clone`, plugin-type-erased handle the native menus
/// (`menu_macos` / `menu_windows`) store and act on - the preset
/// analogue of `InputController` / `OutputController`. All the
/// `P`-specific work (store construction, locking, applying) is
/// captured in closures at build time, so the menu code stays
/// generic-free. The entry list is a snapshot taken at construction;
/// presets saved during the session don't appear until relaunch
/// (dynamic re-enumeration would need menu repopulation on open,
/// deferred). Main-thread only - the native menu never crosses
/// threads, so the closures need no `Send`/`Sync` bound.
#[derive(Clone)]
pub struct PresetController {
    inner: std::rc::Rc<PresetControllerInner>,
}

struct PresetControllerInner {
    entries: Vec<PresetMenuEntry>,
    current: std::cell::Cell<Option<usize>>,
    load: Box<dyn Fn(&str) -> bool>,
    save: Box<dyn Fn()>,
}

impl PresetController {
    /// Build a controller for `plugin`, snapshotting the library and
    /// capturing the load / save operations. `presets_dir` is the
    /// `--presets-dir` factory-root override (else the store finds
    /// the installed bundle).
    #[must_use]
    pub fn new<P: PluginExport>(plugin: Arc<Mutex<P>>, presets_dir: Option<PathBuf>) -> Self {
        let entries = store::<P>(presets_dir.as_deref())
            .enumerate()
            .into_iter()
            .map(|p| PresetMenuEntry {
                label: match &p.category {
                    Some(c) => format!("{c} / {}", p.name),
                    None => p.name.clone(),
                },
                uri: p.uri,
            })
            .collect();

        let load_plugin = Arc::clone(&plugin);
        // Move `presets_dir` into the load closure (its last use) so
        // the controller owns the factory-root override for its life.
        let load = Box::new(move |uri: &str| {
            load_into(&store::<P>(presets_dir.as_deref()), &load_plugin, uri)
        });

        let save = Box::new(move || {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let meta = truce_utils::preset::PresetMeta {
                name: format!("Untitled {ts}"),
                ..Default::default()
            };
            // Saving only touches the user scope; factory root is
            // irrelevant, so `None`.
            save_user::<P>(&store::<P>(None), &plugin, meta);
        });

        Self {
            inner: std::rc::Rc::new(PresetControllerInner {
                entries,
                current: std::cell::Cell::new(None),
                load,
                save,
            }),
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[PresetMenuEntry] {
        &self.inner.entries
    }

    /// Load the preset at `index`; updates the current-selection
    /// cursor on success. Out-of-range is a no-op.
    #[must_use]
    pub fn load_index(&self, index: usize) -> bool {
        let Some(entry) = self.inner.entries.get(index) else {
            return false;
        };
        if (self.inner.load)(&entry.uri) {
            self.inner.current.set(Some(index));
            true
        } else {
            false
        }
    }

    /// Step the selection by `delta`, wrapping, and load it. First
    /// use (no current selection) starts at the first / last entry.
    pub fn step(&self, delta: i32) {
        let n = self.inner.entries.len();
        if n == 0 {
            return;
        }
        let next = match self.inner.current.get() {
            Some(cur) => {
                #[allow(
                    clippy::cast_possible_wrap,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let idx = (cur as i32 + delta).rem_euclid(n as i32) as usize;
                idx
            }
            None if delta < 0 => n - 1,
            None => 0,
        };
        let _ = self.load_index(next);
    }

    pub fn save(&self) {
        (self.inner.save)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn controller(labels: &[&str], log: Rc<RefCell<Vec<String>>>) -> PresetController {
        let entries = labels
            .iter()
            .map(|l| PresetMenuEntry {
                label: (*l).to_string(),
                uri: format!("truce-preset://v/p/{l}"),
            })
            .collect();
        PresetController {
            inner: Rc::new(PresetControllerInner {
                entries,
                current: std::cell::Cell::new(None),
                load: Box::new(move |uri: &str| {
                    log.borrow_mut().push(uri.to_string());
                    true
                }),
                save: Box::new(|| {}),
            }),
        }
    }

    #[test]
    fn load_index_bounds_and_cursor() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let c = controller(&["a", "b", "c"], Rc::clone(&log));
        assert!(c.load_index(1));
        assert!(!c.load_index(9));
        assert_eq!(*log.borrow(), vec!["truce-preset://v/p/b"]);
    }

    #[test]
    fn step_wraps_both_directions() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let c = controller(&["a", "b", "c"], Rc::clone(&log));
        c.step(1);
        c.step(1);
        c.step(-1);
        c.step(-1);
        assert_eq!(
            *log.borrow(),
            vec![
                "truce-preset://v/p/a",
                "truce-preset://v/p/b",
                "truce-preset://v/p/a",
                "truce-preset://v/p/c",
            ]
        );
    }

    #[test]
    fn step_on_empty_is_noop() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let c = controller(&[], Rc::clone(&log));
        c.step(1);
        c.step(-1);
        assert!(log.borrow().is_empty());
    }
}
