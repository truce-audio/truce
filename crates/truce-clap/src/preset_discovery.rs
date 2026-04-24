//! CLAP preset-discovery factory.
//!
//! Implements `clap_preset_discovery_factory_t` so hosts can scan a
//! plugin's sidecar `.presets/` directory (written at install time by
//! `truce-xtask::presets::emit_clap_presets`) and surface each preset
//! in their browser.
//!
//! Threading model: a single global provider per plugin. The factory
//! struct and every trampoline are `unsafe extern "C" fn` pointers
//! stashed in lazy statics; nothing here is instantiated per host —
//! the indexer owns the receiver lifetimes, we just write to its
//! callbacks during `get_metadata`.
//!
//! The plugin-side `export_clap!` macro calls `init_runtime_info()`
//! once at `entry_init` with the plugin's name / id / the
//! host-supplied `plugin_path`. Everything after that reads those
//! fields to build C strings on the fly.

use clap_sys::factory::preset_discovery::*;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;

use truce_presets::clap_preset::ClapPresetFile;

/// Filled in once by the `export_clap!` macro at `entry_init`.
/// Everything downstream of scan reads this. Held as raw CStrings so
/// the C-facing descriptor can point at them without re-allocating on
/// every callback.
pub struct RuntimeInfo {
    pub plugin_id: CString,
    pub plugin_name: CString,
    pub vendor: CString,
    pub provider_id: CString,
    pub provider_name: CString,
    pub location_name: CString,
    pub filetype_name: CString,
    pub filetype_description: CString,
    pub filetype_extension: CString,
    pub sidecar_dir: CString,
}

static RUNTIME: OnceLock<RuntimeInfo> = OnceLock::new();

/// Initialize the preset-discovery runtime state.
///
/// `plugin_path` is the path the host passed to `clap_plugin_entry.init`.
/// We derive the sidecar `.presets/` directory from it (same one
/// `truce-xtask::presets::clap_preset_sidecar_dir` writes). If the
/// path is empty or sidecar can't be resolved, discovery silently
/// no-ops — the plugin still loads fine.
pub fn init_runtime_info(plugin_id: &str, plugin_name: &str, vendor: &str, plugin_path: &str) {
    let _ = RUNTIME.set(build_runtime_info(plugin_id, plugin_name, vendor, plugin_path));
}

fn build_runtime_info(
    plugin_id: &str,
    plugin_name: &str,
    vendor: &str,
    plugin_path: &str,
) -> RuntimeInfo {
    let sidecar = sidecar_dir_for(plugin_path).unwrap_or_default();
    RuntimeInfo {
        plugin_id: CString::new(plugin_id).unwrap_or_default(),
        plugin_name: CString::new(plugin_name).unwrap_or_default(),
        vendor: CString::new(vendor).unwrap_or_default(),
        provider_id: CString::new(format!("{plugin_id}.presets")).unwrap_or_default(),
        provider_name: CString::new(format!("{plugin_name} presets")).unwrap_or_default(),
        location_name: CString::new("Factory").unwrap_or_default(),
        filetype_name: CString::new("CLAP preset").unwrap_or_default(),
        filetype_description: CString::new("Truce CLAP preset file").unwrap_or_default(),
        filetype_extension: CString::new("clap-preset").unwrap_or_default(),
        sidecar_dir: CString::new(sidecar.to_string_lossy().as_bytes()).unwrap_or_default(),
    }
}

/// Given the `plugin_path` the host handed us at entry-init time,
/// produce the sibling `.presets/` directory. Matches
/// `truce-xtask::presets::clap_preset_sidecar_dir`.
fn sidecar_dir_for(plugin_path: &str) -> Option<PathBuf> {
    if plugin_path.is_empty() {
        return None;
    }
    let p = std::path::Path::new(plugin_path);
    let stem = p.file_stem()?.to_str()?.to_string();
    let parent = p.parent()?;
    Some(parent.join(format!("{stem}.presets")))
}

fn runtime() -> Option<&'static RuntimeInfo> {
    RUNTIME.get()
}

// ---------------------------------------------------------------------------
// Factory — returned from the plugin entry's `get_factory`
// ---------------------------------------------------------------------------

#[no_mangle]
static PROVIDER_DESC: clap_preset_discovery_provider_descriptor =
    clap_preset_discovery_provider_descriptor {
        clap_version: CLAP_VERSION,
        id: ptr::null(),
        name: ptr::null(),
        vendor: ptr::null(),
    };

/// Runtime-filled variant. Switches `id`/`name`/`vendor` pointers to
/// the `RuntimeInfo` C-strings before returning to the host. The
/// underlying static has `null` placeholders so the struct is still
/// safely const-initializable — we write a fresh struct into a thread-
/// local leaked buffer during `count_get_descriptor` and hand back a
/// `&'static` to that.
fn descriptor_for_host() -> *const clap_preset_discovery_provider_descriptor {
    static HOLDER: OnceLock<clap_preset_discovery_provider_descriptor> = OnceLock::new();
    let rt = match runtime() {
        Some(r) => r,
        None => return ptr::null(),
    };
    let built = HOLDER.get_or_init(|| clap_preset_discovery_provider_descriptor {
        clap_version: CLAP_VERSION,
        id: rt.provider_id.as_ptr(),
        name: rt.provider_name.as_ptr(),
        vendor: rt.vendor.as_ptr(),
    });
    built as *const _
}

unsafe extern "C" fn factory_count(_factory: *const clap_preset_discovery_factory) -> u32 {
    if runtime().is_some() { 1 } else { 0 }
}

unsafe extern "C" fn factory_get_descriptor(
    _factory: *const clap_preset_discovery_factory,
    index: u32,
) -> *const clap_preset_discovery_provider_descriptor {
    if index == 0 {
        descriptor_for_host()
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn factory_create(
    _factory: *const clap_preset_discovery_factory,
    indexer: *const clap_preset_discovery_indexer,
    provider_id: *const c_char,
) -> *const clap_preset_discovery_provider {
    let rt = match runtime() {
        Some(r) => r,
        None => return ptr::null(),
    };
    if provider_id.is_null() {
        return ptr::null();
    }
    let requested = CStr::from_ptr(provider_id);
    if requested != rt.provider_id.as_c_str() {
        return ptr::null();
    }
    // One provider per plugin, leaked so it outlives the factory call.
    // Indexers cache providers for the lifetime of the scan and never
    // call destroy on a second one — the leak is bounded to one per
    // plugin load.
    let provider = Box::leak(Box::new(Provider {
        clap: clap_preset_discovery_provider {
            desc: descriptor_for_host(),
            provider_data: ptr::null_mut(),
            init: Some(provider_init),
            destroy: Some(provider_destroy),
            get_metadata: Some(provider_get_metadata),
            get_extension: Some(provider_get_extension),
        },
        indexer,
    }));
    provider.clap.provider_data = provider as *mut _ as *mut c_void;
    &provider.clap as *const _
}

/// The factory is a zero-state const struct — no per-host data.
#[no_mangle]
pub static FACTORY: clap_preset_discovery_factory = clap_preset_discovery_factory {
    count: Some(factory_count),
    get_descriptor: Some(factory_get_descriptor),
    create: Some(factory_create),
};

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[repr(C)]
struct Provider {
    /// MUST be the first field so `&provider.clap` and `provider` coerce
    /// the same way through `provider_data`.
    clap: clap_preset_discovery_provider,
    indexer: *const clap_preset_discovery_indexer,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            clap: clap_preset_discovery_provider {
                desc: ptr::null(),
                provider_data: ptr::null_mut(),
                init: None,
                destroy: None,
                get_metadata: None,
                get_extension: None,
            },
            indexer: ptr::null(),
        }
    }
}

unsafe extern "C" fn provider_init(provider: *const clap_preset_discovery_provider) -> bool {
    let rt = match runtime() {
        Some(r) => r,
        None => return false,
    };
    if rt.sidecar_dir.as_bytes().is_empty() {
        // No sidecar → no presets. Succeed quietly so the host stops
        // re-asking on every scan.
        return true;
    }
    let prov: &Provider = &*(provider as *const Provider);
    let idx = prov.indexer;
    if idx.is_null() {
        return false;
    }

    // Declare the filetype we emit.
    let filetype = clap_preset_discovery_filetype {
        name: rt.filetype_name.as_ptr(),
        description: rt.filetype_description.as_ptr(),
        file_extension: rt.filetype_extension.as_ptr(),
    };
    if let Some(declare_filetype) = (*idx).declare_filetype {
        declare_filetype(idx, &filetype);
    }

    // Declare the sidecar directory as a file-backed location. The
    // host walks it for files matching our extension.
    let location = clap_preset_discovery_location {
        flags: CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT,
        name: rt.location_name.as_ptr(),
        kind: CLAP_PRESET_DISCOVERY_LOCATION_FILE,
        location: rt.sidecar_dir.as_ptr(),
    };
    if let Some(declare_location) = (*idx).declare_location {
        declare_location(idx, &location);
    }
    true
}

unsafe extern "C" fn provider_destroy(provider: *const clap_preset_discovery_provider) {
    // The provider was leaked at create time; take the box back to
    // free it now that the indexer is done with us.
    let prov_ptr = provider as *mut Provider;
    if !prov_ptr.is_null() {
        drop(Box::from_raw(prov_ptr));
    }
}

unsafe extern "C" fn provider_get_metadata(
    _provider: *const clap_preset_discovery_provider,
    location_kind: clap_preset_discovery_location_kind,
    location: *const c_char,
    metadata_receiver: *const clap_preset_discovery_metadata_receiver,
) -> bool {
    if metadata_receiver.is_null() || location.is_null() {
        return false;
    }
    if location_kind != CLAP_PRESET_DISCOVERY_LOCATION_FILE {
        return false;
    }
    let rt = match runtime() {
        Some(r) => r,
        None => return false,
    };
    let path = match CStr::from_ptr(location).to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let file = match ClapPresetFile::from_toml(&src) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let recv = metadata_receiver;
    let name = CString::new(file.name.as_str()).unwrap_or_default();
    if let Some(begin) = (*recv).begin_preset {
        // `load_key` is null — one preset per file.
        if !begin(recv, name.as_ptr(), ptr::null()) {
            return false;
        }
    }

    // Bind the preset to our plugin via the universal-plugin-id abi.
    use clap_sys::universal_plugin_id::clap_universal_plugin_id;
    let abi = b"clap\0";
    let uid = clap_universal_plugin_id {
        abi: abi.as_ptr() as *const c_char,
        id: rt.plugin_id.as_ptr(),
    };
    if let Some(add_id) = (*recv).add_plugin_id {
        add_id(recv, &uid);
    }

    if let Some(set_flags) = (*recv).set_flags {
        set_flags(recv, CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT);
    }

    if let Some(creator) = &file.author {
        if let Ok(c) = CString::new(creator.as_str()) {
            if let Some(add_creator) = (*recv).add_creator {
                add_creator(recv, c.as_ptr());
            }
        }
    }
    if let Some(desc) = &file.comment {
        if let Ok(c) = CString::new(desc.as_str()) {
            if let Some(set_desc) = (*recv).set_description {
                set_desc(recv, c.as_ptr());
            }
        }
    }
    // Category goes through add_feature — hosts use features as
    // faceted filters in their browsers.
    if let Some(cat) = &file.category {
        if let Ok(c) = CString::new(cat.as_str()) {
            if let Some(add_feature) = (*recv).add_feature {
                add_feature(recv, c.as_ptr());
            }
        }
    }
    for tag in &file.tags {
        if let Ok(c) = CString::new(tag.as_str()) {
            if let Some(add_feature) = (*recv).add_feature {
                add_feature(recv, c.as_ptr());
            }
        }
    }

    true
}

unsafe extern "C" fn provider_get_extension(
    _provider: *const clap_preset_discovery_provider,
    _extension_id: *const c_char,
) -> *const c_void {
    ptr::null()
}
