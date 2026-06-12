//! CLAP preset discovery: the `clap_preset_discovery_factory_t`
//! provider hosts use to index this plugin's presets.
//!
//! The provider declares two locations - the factory presets shipped
//! inside the installed plugin (written by `cargo truce install`) and
//! the per-OS truce user root - and parses `.trucepreset` containers
//! on demand when the host crawls them. Loading goes through the
//! `CLAP_EXT_PRESET_LOAD` extension in `lib.rs`, which feeds the
//! embedded state envelope down the same handoff path session restore
//! uses.
//!
//! Everything here runs on the host's scan thread with no plugin
//! instance alive; plugin identity comes from `P::info()`, which is
//! statically reachable.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use clap_sys::factory::preset_discovery::{
    CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT, CLAP_PRESET_DISCOVERY_IS_USER_CONTENT,
    CLAP_PRESET_DISCOVERY_LOCATION_FILE, clap_preset_discovery_factory,
    clap_preset_discovery_filetype, clap_preset_discovery_indexer, clap_preset_discovery_location,
    clap_preset_discovery_location_kind, clap_preset_discovery_metadata_receiver,
    clap_preset_discovery_provider, clap_preset_discovery_provider_descriptor,
};
use clap_sys::universal_plugin_id::clap_universal_plugin_id;
use clap_sys::version::CLAP_VERSION;

use truce_core::export::PluginExport;
use truce_core::presets::{PresetRef, PresetScope, read_preset_ref, user_preset_root};

/// The dylib's own path, captured from `clap_plugin_entry.init`
/// (which the CLAP spec guarantees runs before any `get_factory`
/// call). The factory-preset location is derived from it. One static
/// per shared library matches CLAP's one-entry-per-library model.
static PLUGIN_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Record the plugin path the host passed to `clap_plugin_entry.init`.
///
/// # Safety
///
/// `plugin_path` must be a valid NUL-terminated string or null
/// (tolerated: factory-preset discovery is skipped).
pub unsafe fn set_plugin_path(plugin_path: *const c_char) {
    if plugin_path.is_null() {
        return;
    }
    let path = unsafe { CStr::from_ptr(plugin_path) };
    if let Ok(s) = path.to_str()
        && !s.is_empty()
    {
        let _ = PLUGIN_PATH.set(PathBuf::from(s));
    }
}

/// Where `cargo truce install` put the factory presets, derived from
/// the plugin's own location: inside the bundle on macOS
/// (`<X>.clap/Contents/Resources/Presets/`), a `<stem>.presets/`
/// sibling directory where the `.clap` is a single file.
fn factory_preset_root() -> Option<PathBuf> {
    let plugin_path = PLUGIN_PATH.get()?;
    let root = if plugin_path.is_dir() {
        plugin_path.join("Contents/Resources/Presets")
    } else {
        let stem = plugin_path.file_stem()?;
        plugin_path
            .parent()?
            .join(format!("{}.presets", stem.to_string_lossy()))
    };
    root.is_dir().then_some(root)
}

/// Owns the C strings the provider descriptor points into. The
/// strings are heap allocations (stable across moves of the holder),
/// so handing out `desc` pointers from the `OnceLock`-pinned value is
/// sound.
struct ProviderDescHolder {
    _id: CString,
    _name: CString,
    _vendor: CString,
    desc: clap_preset_discovery_provider_descriptor,
}

// SAFETY: the raw pointers inside `desc` reference the owned
// `CString`s above, which are immutable after construction; the
// holder is only ever read once initialized.
unsafe impl Send for ProviderDescHolder {}
unsafe impl Sync for ProviderDescHolder {}

/// One per shared library, like every other CLAP static here - a
/// `.clap` ships exactly one plugin type, so the single `OnceLock`
/// never sees a second monomorphization.
static PROVIDER_DESC: OnceLock<ProviderDescHolder> = OnceLock::new();

fn provider_descriptor<P: PluginExport>() -> &'static ProviderDescHolder {
    PROVIDER_DESC.get_or_init(|| {
        let info = P::info();
        let id = CString::new(format!("{}.presets", info.clap_id)).unwrap_or_default();
        let name = CString::new(format!("{} presets", info.name)).unwrap_or_default();
        let vendor = CString::new(info.vendor).unwrap_or_default();
        let desc = clap_preset_discovery_provider_descriptor {
            clap_version: CLAP_VERSION,
            id: id.as_ptr(),
            name: name.as_ptr(),
            vendor: vendor.as_ptr(),
        };
        ProviderDescHolder {
            _id: id,
            _name: name,
            _vendor: vendor,
            desc,
        }
    })
}

/// Build the preset-discovery factory for the export macro's
/// `entry_get_factory`.
#[must_use]
pub fn discovery_factory<P: PluginExport>() -> *const clap_preset_discovery_factory {
    struct Holder<P>(std::marker::PhantomData<P>);
    impl<P: PluginExport> Holder<P> {
        const FACTORY: clap_preset_discovery_factory = clap_preset_discovery_factory {
            count: Some(factory_count),
            get_descriptor: Some(factory_get_descriptor::<P>),
            create: Some(factory_create::<P>),
        };
    }
    &<Holder<P>>::FACTORY
}

unsafe extern "C" fn factory_count(_factory: *const clap_preset_discovery_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_get_descriptor<P: PluginExport>(
    _factory: *const clap_preset_discovery_factory,
    index: u32,
) -> *const clap_preset_discovery_provider_descriptor {
    if index == 0 {
        &raw const provider_descriptor::<P>().desc
    } else {
        ptr::null()
    }
}

/// Provider instance handed to the host. `provider` must stay the
/// first field: the host's pointer to it is what `destroy` reboxes.
#[repr(C)]
struct ProviderHandle {
    provider: clap_preset_discovery_provider,
    indexer: *const clap_preset_discovery_indexer,
}

unsafe extern "C" fn factory_create<P: PluginExport>(
    _factory: *const clap_preset_discovery_factory,
    indexer: *const clap_preset_discovery_indexer,
    provider_id: *const c_char,
) -> *const clap_preset_discovery_provider {
    if indexer.is_null() || provider_id.is_null() {
        return ptr::null();
    }
    let requested = unsafe { CStr::from_ptr(provider_id) };
    let ours = provider_descriptor::<P>();
    // SAFETY: `desc.id` points into the holder's owned CString.
    if requested != unsafe { CStr::from_ptr(ours.desc.id) } {
        return ptr::null();
    }

    let handle = Box::new(ProviderHandle {
        provider: clap_preset_discovery_provider {
            desc: &raw const ours.desc,
            provider_data: ptr::null_mut(),
            init: Some(provider_init::<P>),
            destroy: Some(provider_destroy),
            get_metadata: Some(provider_get_metadata::<P>),
            get_extension: Some(provider_get_extension),
        },
        indexer,
    });
    let raw = Box::into_raw(handle);
    // SAFETY: `raw` is the freshly leaked allocation above.
    unsafe {
        (*raw).provider.provider_data = raw.cast::<c_void>();
    }
    raw.cast::<clap_preset_discovery_provider>()
}

unsafe extern "C" fn provider_init<P: PluginExport>(
    provider: *const clap_preset_discovery_provider,
) -> bool {
    let Some(handle) = (unsafe { provider.cast::<ProviderHandle>().as_ref() }) else {
        return false;
    };
    let indexer = unsafe { handle.indexer.as_ref() };
    let Some(indexer) = indexer else {
        return false;
    };

    if let Some(declare_filetype) = indexer.declare_filetype {
        let name = c"truce preset";
        let description = c"truce plugin preset (.trucepreset)";
        let extension = CString::new(truce_core::presets::PRESET_FILE_EXT).unwrap_or_default();
        let filetype = clap_preset_discovery_filetype {
            name: name.as_ptr(),
            description: description.as_ptr(),
            file_extension: extension.as_ptr(),
        };
        if !unsafe { declare_filetype(handle.indexer, &raw const filetype) } {
            return false;
        }
    }

    let Some(declare_location) = indexer.declare_location else {
        return false;
    };
    let mut declared_any = false;
    if let Some(root) = factory_preset_root() {
        declared_any |= declare_dir(
            handle.indexer,
            declare_location,
            &root,
            c"Factory",
            CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT,
        );
    }
    let info = P::info();
    if let Some(root) = user_preset_root(info.vendor, info.name) {
        declared_any |= declare_dir(
            handle.indexer,
            declare_location,
            &root,
            c"User",
            CLAP_PRESET_DISCOVERY_IS_USER_CONTENT,
        );
    }
    // A provider with zero locations is useless to the host; telling
    // it init failed lets it skip the provider entirely.
    declared_any
}

fn declare_dir(
    indexer: *const clap_preset_discovery_indexer,
    declare_location: unsafe extern "C" fn(
        *const clap_preset_discovery_indexer,
        *const clap_preset_discovery_location,
    ) -> bool,
    dir: &Path,
    name: &CStr,
    flags: u32,
) -> bool {
    let Some(path) = dir.to_str() else {
        return false;
    };
    let Ok(location) = CString::new(path) else {
        return false;
    };
    let loc = clap_preset_discovery_location {
        flags,
        name: name.as_ptr(),
        kind: CLAP_PRESET_DISCOVERY_LOCATION_FILE,
        location: location.as_ptr(),
    };
    unsafe { declare_location(indexer, &raw const loc) }
}

unsafe extern "C" fn provider_destroy(provider: *const clap_preset_discovery_provider) {
    if !provider.is_null() {
        // SAFETY: `provider` is the `Box<ProviderHandle>` leaked in
        // `factory_create` (provider is the first, repr(C) field).
        drop(unsafe { Box::from_raw(provider.cast_mut().cast::<ProviderHandle>()) });
    }
}

unsafe extern "C" fn provider_get_extension(
    _provider: *const clap_preset_discovery_provider,
    _extension_id: *const c_char,
) -> *const c_void {
    ptr::null()
}

unsafe extern "C" fn provider_get_metadata<P: PluginExport>(
    _provider: *const clap_preset_discovery_provider,
    location_kind: clap_preset_discovery_location_kind,
    location: *const c_char,
    metadata_receiver: *const clap_preset_discovery_metadata_receiver,
) -> bool {
    if location_kind != CLAP_PRESET_DISCOVERY_LOCATION_FILE
        || location.is_null()
        || metadata_receiver.is_null()
    {
        return false;
    }
    let Ok(location) = (unsafe { CStr::from_ptr(location) }).to_str() else {
        return false;
    };
    let path = Path::new(location);
    let receiver = unsafe { &*metadata_receiver };
    let info = P::info();

    // Hosts crawl declared directories and call per-file, but the
    // spec also allows handing back the directory itself - walk it
    // ourselves in that case.
    if path.is_dir() {
        let scope = if factory_preset_root().is_some_and(|f| path.starts_with(f)) {
            PresetScope::Factory
        } else {
            PresetScope::User
        };
        for preset in truce_core::presets::enumerate_scope(path, scope, info.vendor, info.name) {
            if !report_preset::<P>(receiver, metadata_receiver, &preset) {
                return false;
            }
        }
        return true;
    }

    let Some(preset) = read_preset_ref(
        path.parent(),
        path,
        PresetScope::User,
        info.vendor,
        info.name,
    ) else {
        return false;
    };
    report_preset::<P>(receiver, metadata_receiver, &preset)
}

fn report_preset<P: PluginExport>(
    receiver: &clap_preset_discovery_metadata_receiver,
    receiver_ptr: *const clap_preset_discovery_metadata_receiver,
    preset: &PresetRef,
) -> bool {
    let Some(begin_preset) = receiver.begin_preset else {
        return false;
    };
    let Ok(name) = CString::new(preset.name.as_str()) else {
        return true; // skip this preset, keep scanning
    };
    let load_key = CString::new(preset.uuid.as_str()).unwrap_or_default();
    if !unsafe { begin_preset(receiver_ptr, name.as_ptr(), load_key.as_ptr()) } {
        // Host not interested in this preset; keep going.
        return true;
    }

    if let Some(add_plugin_id) = receiver.add_plugin_id {
        let info = P::info();
        if let Ok(id) = CString::new(info.clap_id) {
            let universal = clap_universal_plugin_id {
                abi: c"clap".as_ptr(),
                id: id.as_ptr(),
            };
            unsafe { add_plugin_id(receiver_ptr, &raw const universal) };
        }
    }
    if let (Some(add_creator), Some(author)) = (receiver.add_creator, preset.author.as_deref())
        && let Ok(author) = CString::new(author)
    {
        unsafe { add_creator(receiver_ptr, author.as_ptr()) };
    }
    if let (Some(set_description), Some(comment)) =
        (receiver.set_description, preset.comment.as_deref())
        && let Ok(comment) = CString::new(comment)
    {
        unsafe { set_description(receiver_ptr, comment.as_ptr()) };
    }
    true
}
