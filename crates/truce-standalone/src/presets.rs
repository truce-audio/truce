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
use truce_core::presets::{PresetScope, PresetStore, mint_uuid, user_preset_root};
use truce_core::state::{apply_state, hash_plugin_id, serialize_state};
use truce_params::Params;
use truce_utils::preset::write_preset_file;
use truce_utils::safe_filename;

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
pub fn load_into<P: PluginExport>(store: &PresetStore, plugin: &Arc<Mutex<P>>, sel: &str) -> bool {
    let Ok(mut guard) = plugin.lock() else {
        eprintln!("could not lock plugin to load preset");
        return false;
    };
    apply_selected(store, &mut *guard, sel)
}

/// Snapshot the live plugin (params + `save_state`) and write it to
/// the user scope as a `.trucepreset`. `meta.name` is the only
/// required field; a same-name save keeps the preset's uuid.
/// Returns the saved preset's uri (its stable selection handle).
pub fn save_user<P: PluginExport>(
    store: &PresetStore,
    plugin: &Arc<Mutex<P>>,
    meta: truce_utils::preset::PresetMeta,
) -> Option<String> {
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
            Some(preset.uri)
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

/// One row in the preset menu: the display label, the uri to load,
/// the name / category Save reuses to target the user scope, and
/// whether it lives in the (writable) user scope - Save overwrites
/// an `editable` entry in place but routes a factory / pack entry
/// through Save As.
pub struct PresetMenuEntry {
    pub label: String,
    pub uri: String,
    pub name: String,
    pub category: Option<String>,
    pub editable: bool,
}

/// A `Clone`, plugin-type-erased handle the native menus
/// (`menu_macos` / `menu_windows`) store and act on - the preset
/// analogue of `InputController` / `OutputController`. All the
/// `P`-specific work (store construction, locking, applying) is
/// captured in closures at build time, so the menu code stays
/// generic-free. The library is re-enumerated on every call
/// ([`entries`](Self::entries)), so presets saved this session show
/// up the next time a menu opens - no relaunch. The current
/// selection is tracked by uri (not a list index) so it survives the
/// list changing under it. Main-thread only - the native menu never
/// crosses threads, so the closures need no `Send`/`Sync` bound.
#[derive(Clone)]
pub struct PresetController {
    inner: std::rc::Rc<PresetControllerInner>,
}

/// Re-enumerate the whole library, freshest first.
type EnumerateFn = Box<dyn Fn() -> Vec<PresetMenuEntry>>;
/// Write the live state under the given metadata; returns the saved
/// preset's uri.
type SaveMetaFn = Box<dyn Fn(&truce_utils::preset::PresetMeta) -> Option<String>>;
/// Prompt for a destination and save; returns the saved uri iff it
/// landed in the user library.
type SaveAsFn = Box<dyn Fn() -> Option<String>>;

struct PresetControllerInner {
    /// Re-enumerate the whole library, freshest first. Called on
    /// every menu open so saves appear without a relaunch.
    enumerate: EnumerateFn,
    /// The loaded preset's uri, or `None` before any load. Tracked
    /// by uri so re-enumeration can't desync it.
    current: std::cell::RefCell<Option<String>>,
    load: Box<dyn Fn(&str) -> bool>,
    /// Overwrite an existing user preset in place. P-erased; the
    /// policy lives in `save()`.
    save_meta: SaveMetaFn,
    /// Prompt for a destination and write a new user preset.
    save_as: SaveAsFn,
}

impl PresetController {
    /// Build a controller for `plugin`, capturing the load / save
    /// operations as P-erased closures. `presets_dir` is the
    /// `--presets-dir` factory-root override (else the store finds
    /// the installed bundle). The library itself is read fresh on
    /// every menu open, not snapshotted here.
    #[must_use]
    pub fn new<P: PluginExport>(plugin: Arc<Mutex<P>>, presets_dir: Option<PathBuf>) -> Self {
        // The factory-root override is needed by every store the
        // controller builds, so clone it into each closure.
        let enum_dir = presets_dir.clone();
        let enumerate = Box::new(move || {
            store::<P>(enum_dir.as_deref())
                .enumerate()
                .into_iter()
                .map(|p| PresetMenuEntry {
                    label: match &p.category {
                        Some(c) => format!("{c} / {}", p.name),
                        None => p.name.clone(),
                    },
                    editable: p.scope == PresetScope::User,
                    uri: p.uri,
                    name: p.name,
                    category: p.category,
                })
                .collect()
        });

        let load_plugin = Arc::clone(&plugin);
        let load_dir = presets_dir;
        let load = Box::new(move |uri: &str| {
            load_into(&store::<P>(load_dir.as_deref()), &load_plugin, uri)
        });

        let save_plugin = Arc::clone(&plugin);
        let save_meta = Box::new(move |meta: &truce_utils::preset::PresetMeta| {
            // Saving only touches the user scope; factory root is
            // irrelevant, so `None`.
            save_user::<P>(&store::<P>(None), &save_plugin, meta.clone())
        });

        let info = P::info();
        let hash = hash_plugin_id(info.clap_id);
        let user_root = user_preset_root(info.vendor, info.name, info.preset_user_dir);
        let save_as = Box::new(move || save_as_dialog::<P>(&plugin, hash, user_root.as_deref()));

        Self {
            inner: std::rc::Rc::new(PresetControllerInner {
                enumerate,
                current: std::cell::RefCell::new(None),
                load,
                save_meta,
                save_as,
            }),
        }
    }

    /// The full "Save Preset" menu-item title: overwriting the loaded
    /// user preset shows its file (`Save Preset (Glass.trucepreset)`);
    /// otherwise Save has nothing to overwrite (it's disabled - see
    /// [`save_enabled`]) so the title is the plain `Save Preset`. The
    /// menus refresh this each time the Presets menu opens.
    ///
    /// [`save_enabled`]: Self::save_enabled
    #[must_use]
    pub fn save_menu_title(&self) -> String {
        match self.current_editable() {
            Some(name) => format!(
                "Save Preset ({}.{})",
                safe_filename(&name),
                truce_utils::preset::PRESET_FILE_EXT
            ),
            None => "Save Preset".to_string(),
        }
    }

    /// Whether the Save item should be enabled: only when an editable
    /// user preset is loaded for it to overwrite. With a read-only
    /// factory / pack preset (or nothing) loaded, Save would just
    /// fall through to Save As, so the menus gray it out and nudge
    /// toward Save As instead.
    #[must_use]
    pub fn save_enabled(&self) -> bool {
        self.current_editable().is_some()
    }

    /// The loaded preset's name iff it's an editable (user-scope)
    /// preset Save can overwrite in place; `None` when nothing is
    /// loaded or the loaded preset is read-only (factory / pack).
    fn current_editable(&self) -> Option<String> {
        let uri = self.inner.current.borrow().clone()?;
        self.entries()
            .into_iter()
            .find(|e| e.uri == uri && e.editable)
            .map(|e| e.name)
    }

    /// Re-read the whole library, freshest first. Each menu open
    /// calls this, so presets saved this session appear without a
    /// relaunch.
    #[must_use]
    pub fn entries(&self) -> Vec<PresetMenuEntry> {
        (self.inner.enumerate)()
    }

    /// Load the preset whose display label matches `label` and make
    /// it current. The native menus dispatch by item title (no
    /// fragile index/tag that re-enumeration would invalidate).
    pub fn load_by_label(&self, label: &str) {
        let Some(uri) = self
            .entries()
            .into_iter()
            .find(|e| e.label == label)
            .map(|e| e.uri)
        else {
            return;
        };
        if (self.inner.load)(&uri) {
            *self.inner.current.borrow_mut() = Some(uri);
        }
    }

    /// Step the selection by `delta` through the live list, wrapping,
    /// and load it. First use (no current selection) starts at the
    /// first / last entry.
    pub fn step(&self, delta: i32) {
        let entries = self.entries();
        let n = entries.len();
        if n == 0 {
            return;
        }
        let cur = self
            .inner
            .current
            .borrow()
            .as_ref()
            .and_then(|uri| entries.iter().position(|e| &e.uri == uri));
        let next = match cur {
            #[allow(
                clippy::cast_possible_wrap,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            Some(i) => (i as i32 + delta).rem_euclid(n as i32) as usize,
            None if delta < 0 => n - 1,
            None => 0,
        };
        let uri = entries[next].uri.clone();
        if (self.inner.load)(&uri) {
            *self.inner.current.borrow_mut() = Some(uri);
        }
    }

    /// Save the live state. Overwrites the loaded preset in place
    /// when it's an editable user preset (so editing "Glass" then
    /// Save rewrites `Glass`, keeping its uuid). When nothing is
    /// loaded, or the loaded preset is read-only (factory / pack),
    /// Save can't write in place, so it routes to [`save_as`] - the
    /// destination is then explicit, never a surprise same-named
    /// override.
    ///
    /// [`save_as`]: Self::save_as
    pub fn save(&self) {
        let Some(name) = self.current_editable() else {
            self.save_as();
            return;
        };
        // Reuse the loaded entry's category so the overwrite lands on
        // the same file.
        let uri = self.inner.current.borrow().clone();
        let category = uri
            .and_then(|uri| self.entries().into_iter().find(|e| e.uri == uri))
            .and_then(|e| e.category)
            .unwrap_or_default();
        let meta = truce_utils::preset::PresetMeta {
            name,
            category,
            ..Default::default()
        };
        if let Some(saved) = (self.inner.save_meta)(&meta) {
            *self.inner.current.borrow_mut() = Some(saved);
        }
    }

    /// Save under a chosen name / location. On macOS / Windows a
    /// native save panel supplies the name and confirms any
    /// overwrite; on Linux (no panel) it writes a uniquely-named
    /// file into the user library, so it never clobbers. A preset
    /// that lands in the user library becomes the current selection.
    pub fn save_as(&self) {
        if let Some(saved) = (self.inner.save_as)() {
            *self.inner.current.borrow_mut() = Some(saved);
        }
    }
}

/// Snapshot `plugin` into a canonical state envelope.
fn snapshot<P: PluginExport>(plugin: &Arc<Mutex<P>>, hash: u64) -> Option<Vec<u8>> {
    let guard = plugin.lock().ok()?;
    let (ids, values) = guard.params().collect_values();
    let extra = guard.save_state();
    Some(serialize_state(hash, &ids, &values, &extra, &[]))
}

/// Write `blob` as a `.trucepreset` named after `path`'s stem.
/// Returns the preset name written, for resolving its library uri.
fn write_preset_at(path: &std::path::Path, blob: &[u8]) -> Option<String> {
    let name = path.file_stem().map_or_else(
        || "Preset".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let meta = truce_utils::preset::PresetMeta {
        uuid: mint_uuid(),
        name: name.clone(),
        ..Default::default()
    };
    match std::fs::write(path, write_preset_file(&meta, blob)) {
        Ok(()) => {
            vlog!("saved preset -> {}", path.display());
            Some(name)
        }
        Err(e) => {
            eprintln!("failed to write {}: {e}", path.display());
            None
        }
    }
}

/// The library uri of the just-written preset `name`, iff it landed
/// in the user scope (so the menu can select it). A Save As to a
/// path outside the library returns `None` - it's an export, not a
/// library entry.
fn saved_user_uri<P: PluginExport>(name: &str) -> Option<String> {
    store::<P>(None)
        .find(name)
        .filter(|p| p.scope == PresetScope::User)
        .map(|p| p.uri)
}

/// macOS / Windows: native save panel (which confirms overwrites),
/// defaulted to the user library. Returns the saved preset's uri
/// when it landed in the user library.
#[cfg(all(feature = "gui", any(target_os = "macos", target_os = "windows")))]
fn save_as_dialog<P: PluginExport>(
    plugin: &Arc<Mutex<P>>,
    hash: u64,
    user_root: Option<&std::path::Path>,
) -> Option<String> {
    let blob = snapshot(plugin, hash)?;
    let mut dialog = rfd::FileDialog::new()
        .set_title(format!("Save preset for {}", P::info().name))
        .add_filter("truce preset", &[truce_utils::preset::PRESET_FILE_EXT])
        .set_file_name(format!("Preset.{}", truce_utils::preset::PRESET_FILE_EXT));
    if let Some(dir) = user_root {
        let _ = std::fs::create_dir_all(dir);
        dialog = dialog.set_directory(dir);
    }
    // `save_file` is the OS save panel; it raises its own
    // "replace existing?" confirmation, so no extra dialog here.
    let path = dialog.save_file()?;
    let name = write_preset_at(&path, &blob)?;
    saved_user_uri::<P>(&name)
}

/// Linux / no-gui: no native panel (rfd's Linux backend pulls
/// `wayland-sys`), so write a uniquely-named file into the user
/// library - never overwrites, nothing to confirm. Returns the
/// saved preset's uri.
#[cfg(not(all(feature = "gui", any(target_os = "macos", target_os = "windows"))))]
fn save_as_dialog<P: PluginExport>(
    plugin: &Arc<Mutex<P>>,
    hash: u64,
    user_root: Option<&std::path::Path>,
) -> Option<String> {
    let blob = snapshot(plugin, hash)?;
    let Some(dir) = user_root else {
        eprintln!("save-as: no user preset directory available");
        return None;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("save-as: mkdir {}: {e}", dir.display());
        return None;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!(
        "Untitled {ts}.{}",
        truce_utils::preset::PRESET_FILE_EXT
    ));
    let name = write_preset_at(&path, &blob)?;
    saved_user_uri::<P>(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn controller(labels: &[&str], log: Rc<RefCell<Vec<String>>>) -> PresetController {
        let owned: Vec<String> = labels.iter().map(|l| (*l).to_string()).collect();
        let enumerate = Box::new(move || {
            owned
                .iter()
                .map(|l| PresetMenuEntry {
                    label: l.clone(),
                    uri: format!("truce-preset://v/p/{l}"),
                    name: l.clone(),
                    category: None,
                    editable: false,
                })
                .collect()
        });
        PresetController {
            inner: Rc::new(PresetControllerInner {
                enumerate,
                current: std::cell::RefCell::new(None),
                load: Box::new(move |uri: &str| {
                    log.borrow_mut().push(uri.to_string());
                    true
                }),
                save_meta: Box::new(|_| None),
                save_as: Box::new(|| None),
            }),
        }
    }

    #[test]
    fn load_by_label_loads_and_sets_current() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let c = controller(&["a", "b", "c"], Rc::clone(&log));
        c.load_by_label("b");
        c.load_by_label("nope");
        assert_eq!(*log.borrow(), vec!["truce-preset://v/p/b"]);
        // Stepping from "b" advances to "c".
        c.step(1);
        assert_eq!(log.borrow().last().unwrap(), "truce-preset://v/p/c");
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
