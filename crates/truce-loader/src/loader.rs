//! NativeLoader — loads and hot-reloads a PluginLogic dylib.
//!
//! Uses native Rust ABI (no C translation layer). Verifies
//! compatibility via AbiCanary + vtable probe before use.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use libloading::{Library, Symbol};

use crate::canary::{AbiCanary, verify_probe};
use crate::traits::PluginLogic;

/// Manages a hot-reloadable plugin dylib.
pub struct NativeLoader {
    dylib_path: PathBuf,
    library: Option<Library>,
    plugin: Option<Box<dyn PluginLogic>>,
    /// Raw pointer to the shell's `Arc<Params>` (type-erased).
    /// Passed to `truce_create()` so the plugin shares the same params.
    params_ptr: *const (),
    last_modified: SystemTime,
    last_hash: u32,
    reload_pending: Arc<AtomicBool>,
    /// Old library handles — leaked to avoid TLS destructor segfaults.
    leaked_handles: Vec<Library>,
    load_counter: u64,
}

// Safety: NativeLoader is only accessed from one thread at a time.
// The audio thread calls process/reload, the main thread calls render.
// The shell wraps access in a parking_lot::Mutex.
unsafe impl Send for NativeLoader {}

impl NativeLoader {
    pub fn new(dylib_path: PathBuf, params_ptr: *const ()) -> Self {
        let reload_pending = Arc::new(AtomicBool::new(false));

        // Spawn file watcher thread.
        let flag = reload_pending.clone();
        let path = dylib_path.clone();
        std::thread::Builder::new()
            .name("truce-hot-watcher".into())
            .spawn(move || watch_loop(&path, &flag))
            .ok();

        let mut loader = Self {
            dylib_path,
            library: None,
            plugin: None,
            params_ptr,
            last_modified: SystemTime::UNIX_EPOCH,
            last_hash: 0,
            reload_pending,
            leaked_handles: Vec::new(),
            load_counter: 0,
        };
        loader.load();
        loader
    }

    /// Load (or reload) the dylib.
    fn load(&mut self) -> bool {
        // CRC32 check: skip if content hasn't changed.
        let new_hash = crc32_file(&self.dylib_path);
        if new_hash == self.last_hash && self.library.is_some() {
            log::debug!("dylib unchanged (CRC32 match), skipping reload");
            return true;
        }

        // Copy to versioned temp path to defeat macOS dyld cache.
        let temp = match self.copy_versioned() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("failed to copy dylib: {e}");
                return false;
            }
        };

        // macOS: ad-hoc codesign (required by SIP).
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("codesign")
                .args(["--sign", "-", "--force", temp.to_str().unwrap_or("")])
                .output();
        }

        // dlopen.
        let lib = match unsafe { Library::new(&temp) } {
            Ok(l) => l,
            Err(e) => {
                log::warn!("dlopen failed: {e}");
                let _ = std::fs::remove_file(&temp);
                return false;
            }
        };

        // Canary check.
        let canary_fn: Symbol<fn() -> AbiCanary> = match unsafe { lib.get(b"truce_abi_canary") } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("missing truce_abi_canary export: {e}");
                return false;
            }
        };
        let dylib_canary = canary_fn();
        let shell_canary = AbiCanary::current();
        if !shell_canary.matches(&dylib_canary) {
            log::error!("ABI mismatch — rebuild both shell and logic:\n{}",
                shell_canary.diff_report(&dylib_canary));
            return false;
        }

        // Vtable probe.
        let probe_fn: Symbol<fn() -> Box<dyn PluginLogic>> =
            match unsafe { lib.get(b"truce_vtable_probe") } {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("missing truce_vtable_probe export: {e}");
                    return false;
                }
            };
        let probe = probe_fn();
        if let Err(msg) = verify_probe(probe.as_ref()) {
            log::error!("vtable probe failed: {msg}");
            return false;
        }
        drop(probe);

        // Load the real plugin.
        let create_fn: Symbol<fn(*const ()) -> Box<dyn PluginLogic>> =
            match unsafe { lib.get(b"truce_create") } {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("missing truce_create export: {e}");
                    return false;
                }
            };
        let plugin = create_fn(self.params_ptr);

        self.library = Some(lib);
        self.plugin = Some(plugin);
        self.last_modified = file_mtime(&self.dylib_path);
        self.last_hash = new_hash;

        log::info!("loaded plugin dylib: {}", self.dylib_path.display());
        true
    }

    /// Reload the dylib. Saves state from the old instance, loads the
    /// new dylib, creates a new instance, and restores state.
    pub fn reload(&mut self) -> bool {
        self.reload_pending.store(false, Ordering::Relaxed);

        // Save state from old instance.
        let state = self.plugin.as_ref().map(|p| p.save_state());

        // Drop old plugin BEFORE leaking old library (plugin's drop
        // impl lives in the old library).
        self.plugin = None;

        // Leak old library handle (never dlclose Rust dylibs).
        if let Some(old) = self.library.take() {
            self.leaked_handles.push(old);
        }

        // Load new.
        if !self.load() {
            log::warn!("hot-reload failed, plugin unavailable");
            return false;
        }

        // Restore state.
        if let (Some(state), Some(plugin)) = (state, self.plugin.as_mut()) {
            if !state.is_empty() {
                plugin.load_state(&state);
            }
        }

        log::info!("hot-reload complete (load #{}, {} leaked handles)",
            self.load_counter, self.leaked_handles.len());
        true
    }

    pub fn plugin(&self) -> Option<&dyn PluginLogic> {
        self.plugin.as_ref().map(|p| p.as_ref())
    }

    pub fn plugin_mut(&mut self) -> Option<&mut dyn PluginLogic> {
        self.plugin.as_mut().map(|p| p.as_mut())
    }

    pub fn is_reload_pending(&self) -> bool {
        self.reload_pending.load(Ordering::Relaxed)
    }

    fn copy_versioned(&mut self) -> Result<PathBuf, std::io::Error> {
        self.load_counter += 1;
        let ext = self.dylib_path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("dylib");
        let stem = self.dylib_path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        let temp = std::env::temp_dir()
            .join(format!("truce-hot-{stem}-{}.{ext}", self.load_counter));
        std::fs::copy(&self.dylib_path, &temp)?;
        Ok(temp)
    }
}

impl Drop for NativeLoader {
    fn drop(&mut self) {
        // Drop plugin before library (plugin's drop is in the library).
        self.plugin = None;
        // Leaked handles are intentionally not closed.
    }
}

/// File watcher loop. Polls mtime every 500ms.
fn watch_loop(path: &std::path::Path, flag: &AtomicBool) {
    let mut last_mtime = file_mtime(path);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mtime = file_mtime(path);
        if mtime > last_mtime {
            // Wait for compiler to finish writing.
            std::thread::sleep(std::time::Duration::from_millis(200));
            last_mtime = file_mtime(path);
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn file_mtime(path: &std::path::Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Simple CRC32 of a file's contents (no external dependency).
fn crc32_file(path: &std::path::Path) -> u32 {
    let Ok(data) = std::fs::read(path) else { return 0 };
    crc32(&data)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
