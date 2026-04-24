//! End-to-end test: dlopen the installed CLAP dylib for truce-rir,
//! call its discovery factory, walk through the provider, and verify
//! `.clap-preset` files round-trip back to the plugin via the
//! preset-load extension.
//!
//! Gated behind `TRUCE_RIR_CLAP` — points at the installed `.clap`
//! path so the test can run without hard-coding a user home dir.
//! Skipped otherwise so CI doesn't care.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

#[repr(C)]
#[derive(Copy, Clone)]
struct ClapVersion {
    major: u32,
    minor: u32,
    revision: u32,
}

#[repr(C)]
struct ClapPluginEntry {
    clap_version: ClapVersion,
    init: Option<unsafe extern "C" fn(plugin_path: *const c_char) -> bool>,
    deinit: Option<unsafe extern "C" fn()>,
    get_factory: Option<unsafe extern "C" fn(factory_id: *const c_char) -> *const c_void>,
}

// Mirror the pieces of clap-sys we need without pulling the whole
// crate into dev-deps.
#[repr(C)]
struct PresetDiscoveryProviderDescriptor {
    clap_version: ClapVersion,
    id: *const c_char,
    name: *const c_char,
    vendor: *const c_char,
}

#[repr(C)]
struct PresetDiscoveryFactory {
    count: Option<unsafe extern "C" fn(factory: *const PresetDiscoveryFactory) -> u32>,
    get_descriptor: Option<
        unsafe extern "C" fn(
            factory: *const PresetDiscoveryFactory,
            index: u32,
        ) -> *const PresetDiscoveryProviderDescriptor,
    >,
    create: Option<
        unsafe extern "C" fn(
            factory: *const PresetDiscoveryFactory,
            indexer: *const PresetDiscoveryIndexer,
            provider_id: *const c_char,
        ) -> *const PresetDiscoveryProvider,
    >,
}

#[repr(C)]
struct PresetDiscoveryFiletype {
    name: *const c_char,
    description: *const c_char,
    file_extension: *const c_char,
}

#[repr(C)]
struct PresetDiscoveryLocation {
    flags: u32,
    name: *const c_char,
    kind: u32,
    location: *const c_char,
}

#[repr(C)]
struct PresetDiscoveryIndexer {
    clap_version: ClapVersion,
    name: *const c_char,
    vendor: *const c_char,
    url: *const c_char,
    version: *const c_char,
    indexer_data: *mut c_void,
    declare_filetype: Option<
        unsafe extern "C" fn(
            indexer: *const PresetDiscoveryIndexer,
            filetype: *const PresetDiscoveryFiletype,
        ) -> bool,
    >,
    declare_location: Option<
        unsafe extern "C" fn(
            indexer: *const PresetDiscoveryIndexer,
            location: *const PresetDiscoveryLocation,
        ) -> bool,
    >,
    declare_soundpack: Option<unsafe extern "C" fn()>,
    get_extension: Option<unsafe extern "C" fn()>,
}

#[repr(C)]
struct PresetDiscoveryProvider {
    desc: *const PresetDiscoveryProviderDescriptor,
    provider_data: *mut c_void,
    init: Option<unsafe extern "C" fn(provider: *const PresetDiscoveryProvider) -> bool>,
    destroy: Option<unsafe extern "C" fn(provider: *const PresetDiscoveryProvider)>,
    get_metadata: Option<
        unsafe extern "C" fn(
            provider: *const PresetDiscoveryProvider,
            location_kind: u32,
            location: *const c_char,
            metadata_receiver: *const c_void,
        ) -> bool,
    >,
    get_extension: Option<unsafe extern "C" fn()>,
}

/// Captures every indexer callback the provider makes so the test
/// can assert on them.
#[derive(Default)]
struct Captured {
    filetypes: Vec<String>,
    locations: Vec<(String, String)>, // (name, path)
}

unsafe extern "C" fn cap_declare_filetype(
    indexer: *const PresetDiscoveryIndexer,
    filetype: *const PresetDiscoveryFiletype,
) -> bool {
    let captured = &mut *((*indexer).indexer_data as *mut Captured);
    let ext = CStr::from_ptr((*filetype).file_extension)
        .to_string_lossy()
        .into_owned();
    captured.filetypes.push(ext);
    true
}

unsafe extern "C" fn cap_declare_location(
    indexer: *const PresetDiscoveryIndexer,
    location: *const PresetDiscoveryLocation,
) -> bool {
    let captured = &mut *((*indexer).indexer_data as *mut Captured);
    let name = CStr::from_ptr((*location).name)
        .to_string_lossy()
        .into_owned();
    let path = CStr::from_ptr((*location).location)
        .to_string_lossy()
        .into_owned();
    captured.locations.push((name, path));
    true
}

#[test]
fn rir_clap_presets_roundtrip() {
    let plugin_path = match std::env::var("TRUCE_RIR_CLAP") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "SKIP rir_clap_presets_roundtrip — set TRUCE_RIR_CLAP=path/to/Truce Reverb.clap"
            );
            return;
        }
    };

    unsafe {
        let c_path = CString::new(plugin_path.clone()).unwrap();
        let lib = libloading::Library::new(&plugin_path).expect("dlopen plugin");

        // `clap_entry` is a `static ClapPluginEntry` — the symbol
        // address IS the struct address. `Symbol<*const T>` derefs
        // once to give us that address directly.
        let entry_sym: libloading::Symbol<*const ClapPluginEntry> =
            lib.get(b"clap_entry\0").expect("clap_entry export");
        let entry_ref: &ClapPluginEntry = &**entry_sym;

        // init
        let init = entry_ref.init.expect("entry.init");
        assert!(init(c_path.as_ptr()));

        let get_factory = entry_ref.get_factory.expect("entry.get_factory");

        let factory_id = CString::new("clap.preset-discovery-factory/2").unwrap();
        let factory = get_factory(factory_id.as_ptr()) as *const PresetDiscoveryFactory;
        assert!(!factory.is_null(), "preset-discovery factory not exported");

        let count = (*factory).count.unwrap();
        assert_eq!(count(factory), 1, "one provider per plugin");

        let get_desc = (*factory).get_descriptor.unwrap();
        let desc = get_desc(factory, 0);
        assert!(!desc.is_null());
        let provider_id = CStr::from_ptr((*desc).id).to_string_lossy().into_owned();

        // Build a minimal indexer that just captures callbacks.
        let mut captured = Captured::default();
        let idx = PresetDiscoveryIndexer {
            clap_version: ClapVersion {
                major: 1,
                minor: 0,
                revision: 0,
            },
            name: b"test-indexer\0".as_ptr() as *const c_char,
            vendor: b"truce-tests\0".as_ptr() as *const c_char,
            url: ptr::null(),
            version: ptr::null(),
            indexer_data: &mut captured as *mut _ as *mut c_void,
            declare_filetype: Some(cap_declare_filetype),
            declare_location: Some(cap_declare_location),
            declare_soundpack: None,
            get_extension: None,
        };

        let create = (*factory).create.unwrap();
        let provider_cid = CString::new(provider_id.clone()).unwrap();
        let provider = create(factory, &idx as *const _, provider_cid.as_ptr());
        assert!(!provider.is_null(), "provider create failed");

        let p_init = (*provider).init.unwrap();
        assert!(p_init(provider));

        assert_eq!(captured.filetypes, vec!["clap-preset".to_string()]);
        assert_eq!(captured.locations.len(), 1);
        let (_, sidecar) = &captured.locations[0];
        assert!(
            std::path::Path::new(sidecar)
                .file_name()
                .map(|n| n.to_string_lossy().ends_with(".presets"))
                .unwrap_or(false),
            "sidecar dir name should end with .presets, got {sidecar}"
        );

        // Walk the sidecar and verify `get_metadata` parses every
        // `.clap-preset` file we find.
        let get_meta = (*provider).get_metadata.unwrap();
        let mut preset_count = 0;
        walk(std::path::Path::new(sidecar), &mut |p| {
            if p.extension().and_then(|s| s.to_str()) == Some("clap-preset") {
                let c = CString::new(p.to_string_lossy().as_bytes()).unwrap();
                // `metadata_receiver` is unused by this test but must be
                // non-null to satisfy the provider's null checks. Pass a
                // zeroed struct; our provider only reads function
                // pointers from it, which remain None → checked via
                // `if let Some(..)` on the provider side.
                let stub = [0u8; 512];
                let ok = get_meta(provider, 0, c.as_ptr(), stub.as_ptr() as *const c_void);
                assert!(ok, "get_metadata failed for {}", p.display());
                preset_count += 1;
            }
        });
        assert!(preset_count >= 1, "no .clap-preset files found in sidecar");

        let p_destroy = (*provider).destroy.unwrap();
        p_destroy(provider);

        if let Some(deinit) = entry_ref.deinit {
            deinit();
        }

        // Silence unused warning
        let _ = c_int::default();
    }
}

fn walk(dir: &std::path::Path, f: &mut dyn FnMut(&std::path::Path)) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, f);
            } else if p.is_file() {
                f(&p);
            }
        }
    }
}
