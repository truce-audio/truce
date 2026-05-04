//! `NativeLoader` — loads and hot-reloads a `PluginLogic` dylib.
//!
//! Uses native Rust ABI (no C translation layer). Verifies
//! compatibility via `AbiCanary` + vtable probe before use.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

/// Process-wide counter assigning a unique `instance_id` to each
/// `NativeLoader` constructed in this process. Used as a tiebreaker
/// in temp-file names so two plugins hot-reloading the same dylib
/// path (multi-instance / dual-bus session) can't collide on a
/// `<stem>-truce<id>.so` filename.
///
/// A truly per-instance counter wouldn't help: each `NativeLoader`
/// needs an ID *unique among other `NativeLoaders` in the same
/// process*, and only a process-scoped atomic can guarantee that.
/// `Relaxed` ordering is sufficient — the only consumer is the
/// owning `NativeLoader`, which reads the value back from its own
/// `instance_id` field, never via a re-load of `LOADER_ID`.
static LOADER_ID: AtomicU64 = AtomicU64::new(0);

use libloading::{Library, Symbol};

use crate::canary::{AbiCanary, verify_probe};
use crate::traits::PluginLogic;

type ProbeFn = fn() -> Box<dyn PluginLogic>;
type CreateFn = fn(*const ()) -> Box<dyn PluginLogic>;

/// Verified candidate dylib + instance, ready to swap in.
struct Candidate {
    library: Library,
    plugin: Box<dyn PluginLogic>,
    hash: u32,
    mtime: SystemTime,
    /// Path of the versioned copy in the system temp dir. Tracked so
    /// the loader can unlink it on Drop after the matching `Library`
    /// handle has been released.
    temp_path: PathBuf,
}

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
    /// Set to true to stop the file watcher thread.
    watcher_stop: Arc<AtomicBool>,
    /// Old library handles — leaked to avoid TLS destructor segfaults.
    leaked_handles: Vec<Library>,
    /// Temp-file paths corresponding 1:1 to `leaked_handles` plus the
    /// currently active library. The dylib at each path is mmap-backed
    /// so we can't unlink it while the matching `Library` handle is
    /// alive. `Drop` walks both vectors in lockstep so the file is
    /// removed only after its owning handle has been released.
    temp_paths: Vec<PathBuf>,
    /// Path of the temp copy currently bound to `self.library`. Moved
    /// into `temp_paths` when the library rotates out into
    /// `leaked_handles` (or alongside it on shutdown).
    current_temp: Option<PathBuf>,
    load_counter: u64,
    /// Unique ID for this loader instance (used in temp file names).
    instance_id: u64,
}

// Safety: NativeLoader is only accessed from one thread at a time.
// The audio thread calls process/reload, the main thread calls render.
// The shell wraps access in a parking_lot::Mutex.
unsafe impl Send for NativeLoader {}

impl NativeLoader {
    pub fn new(dylib_path: PathBuf, params_ptr: *const ()) -> Self {
        let reload_pending = Arc::new(AtomicBool::new(false));
        let watcher_stop = Arc::new(AtomicBool::new(false));

        // Spawn file watcher thread.
        let flag = reload_pending.clone();
        let stop = watcher_stop.clone();
        let path = dylib_path.clone();
        std::thread::Builder::new()
            .name("truce-hot-watcher".into())
            .spawn(move || watch_loop(&path, &flag, &stop))
            .ok();

        let mut loader = Self {
            dylib_path,
            library: None,
            plugin: None,
            params_ptr,
            last_modified: SystemTime::UNIX_EPOCH,
            last_hash: 0,
            reload_pending,
            watcher_stop,
            leaked_handles: Vec::new(),
            temp_paths: Vec::new(),
            current_temp: None,
            load_counter: 0,
            instance_id: LOADER_ID.fetch_add(1, Ordering::Relaxed),
        };
        loader.load();
        loader
    }

    /// Build, verify, and instantiate a fresh dylib at `dylib_path`.
    /// Does not touch `self.library` / `self.plugin`. Caller decides
    /// whether to swap the old state out for the result.
    ///
    /// `new_hash` comes from the caller to avoid re-reading the dylib
    /// — `load` and `reload` already hashed it to detect "unchanged"
    /// before deciding to call us. The previous shape re-hashed inside
    /// here, doubling I/O on every successful reload (a 5-20 MB read
    /// twice per poll for a 5-20 MB dylib).
    fn build_candidate(&mut self, new_hash: u32) -> Option<Candidate> {
        // Copy to versioned temp path to defeat macOS dyld cache.
        let temp = match self.copy_versioned() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("failed to copy dylib: {e}");
                return None;
            }
        };

        // macOS: ad-hoc codesign (required by SIP).
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("codesign")
                .args(["--sign", "-", "--force", temp.to_str().unwrap_or("")])
                .output();
        }

        let lib = match unsafe { Library::new(&temp) } {
            Ok(l) => l,
            Err(e) => {
                log::warn!("dlopen failed: {e}");
                let _ = std::fs::remove_file(&temp);
                return None;
            }
        };

        // After this point, every early-return drops `lib` (which may
        // close the dylib handle) and we then unlink the temp file so
        // it doesn't accumulate in /tmp across dozens of failed reloads
        // during iterative plugin development.
        let cleanup_temp = |lib: Library, temp: &std::path::Path| {
            drop(lib);
            let _ = std::fs::remove_file(temp);
        };

        let canary_fn: Symbol<fn() -> AbiCanary> = match unsafe { lib.get(b"truce_abi_canary") } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("missing truce_abi_canary export: {e}");
                cleanup_temp(lib, &temp);
                return None;
            }
        };
        let dylib_canary = canary_fn();
        let shell_canary = AbiCanary::current();
        if !shell_canary.matches(&dylib_canary) {
            log::error!(
                "ABI mismatch — rebuild both shell and logic:\n{}",
                shell_canary.diff_report(&dylib_canary)
            );
            cleanup_temp(lib, &temp);
            return None;
        }

        let probe_fn: Symbol<ProbeFn> = match unsafe { lib.get(b"truce_vtable_probe") } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("missing truce_vtable_probe export: {e}");
                cleanup_temp(lib, &temp);
                return None;
            }
        };
        let mut probe = probe_fn();
        let probe_result = verify_probe(probe.as_mut());
        drop(probe);
        if let Err(msg) = probe_result {
            log::error!("vtable probe failed: {msg}");
            cleanup_temp(lib, &temp);
            return None;
        }

        let create_fn: Symbol<CreateFn> = match unsafe { lib.get(b"truce_create") } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("missing truce_create export: {e}");
                cleanup_temp(lib, &temp);
                return None;
            }
        };
        let plugin = create_fn(self.params_ptr);

        Some(Candidate {
            library: lib,
            plugin,
            hash: new_hash,
            mtime: file_mtime(&self.dylib_path),
            temp_path: temp,
        })
    }

    /// Initial load. Called from `new()`.
    fn load(&mut self) -> bool {
        let Some(new_hash) = crc32_file(&self.dylib_path) else {
            log::warn!(
                "failed to hash dylib at {} (missing / unreadable / mid-write); skipping load",
                self.dylib_path.display()
            );
            return false;
        };
        if new_hash == self.last_hash && self.library.is_some() {
            log::debug!("dylib unchanged (CRC32 match), skipping reload");
            return true;
        }
        match self.build_candidate(new_hash) {
            Some(cand) => {
                self.library = Some(cand.library);
                self.plugin = Some(cand.plugin);
                self.last_hash = cand.hash;
                self.last_modified = cand.mtime;
                self.current_temp = Some(cand.temp_path);
                log::info!("loaded plugin dylib: {}", self.dylib_path.display());
                true
            }
            None => false,
        }
    }

    /// Reload the dylib. Verifies the *new* dylib first; only drops the
    /// old plugin/library after the candidate is fully constructed, so a
    /// failed canary or vtable probe leaves the host with the previous
    /// plugin still loaded instead of silence.
    pub fn reload(&mut self) -> bool {
        self.reload_pending.store(false, Ordering::Relaxed);

        let Some(new_hash) = crc32_file(&self.dylib_path) else {
            log::warn!(
                "failed to hash dylib at {} (missing / unreadable / mid-write); keeping previous plugin loaded",
                self.dylib_path.display()
            );
            return false;
        };
        if new_hash == self.last_hash && self.library.is_some() {
            log::debug!("dylib unchanged (CRC32 match), skipping reload");
            return true;
        }

        // Build + verify the candidate while the old plugin is still alive.
        let Some(candidate) = self.build_candidate(new_hash) else {
            log::warn!("hot-reload failed; keeping previous plugin loaded");
            return false;
        };

        // Save state from the old instance, then swap.
        let state = self.plugin.as_ref().map(|p| p.save_state());
        // Drop old plugin before leaking the library (plugin's `Drop`
        // lives in that library).
        self.plugin = None;
        if let Some(old) = self.library.take() {
            self.leaked_handles.push(old);
            if let Some(p) = self.current_temp.take() {
                // Track the temp path alongside the leaked handle so
                // `Drop` can remove the file once the handle is gone.
                self.temp_paths.push(p);
            }
        }

        self.library = Some(candidate.library);
        self.plugin = Some(candidate.plugin);
        self.last_hash = candidate.hash;
        self.last_modified = candidate.mtime;
        self.current_temp = Some(candidate.temp_path);

        if let (Some(state), Some(plugin)) = (state, self.plugin.as_mut())
            && !state.is_empty()
        {
            plugin.load_state(&state);
        }

        log::info!(
            "hot-reload complete (load #{}, {} leaked handles)",
            self.load_counter,
            self.leaked_handles.len()
        );
        true
    }

    #[must_use] 
    pub fn plugin(&self) -> Option<&dyn PluginLogic> {
        self.plugin.as_ref().map(std::convert::AsRef::as_ref)
    }

    pub fn plugin_mut(&mut self) -> Option<&mut dyn PluginLogic> {
        self.plugin.as_mut().map(std::convert::AsMut::as_mut)
    }

    #[must_use] 
    pub fn is_reload_pending(&self) -> bool {
        self.reload_pending.load(Ordering::Relaxed)
    }

    /// Monotonic counter of successful (or attempted) reloads — bumps
    /// once per `copy_versioned()` invocation, which precedes every
    /// candidate build. Two consumers (audio path + GUI watcher) that
    /// share the same `NativeLoader` use this to detect "the other
    /// side already reloaded" without having to drive reload
    /// themselves.
    #[must_use] 
    pub fn load_counter(&self) -> u64 {
        self.load_counter
    }

    fn copy_versioned(&mut self) -> Result<PathBuf, std::io::Error> {
        self.load_counter += 1;
        let ext = self
            .dylib_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("dylib");
        let stem = self
            .dylib_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        let temp = std::env::temp_dir().join(format!(
            "truce-hot-{stem}-{}-{}.{ext}",
            self.instance_id, self.load_counter
        ));
        std::fs::copy(&self.dylib_path, &temp)?;
        Ok(temp)
    }
}

impl Drop for NativeLoader {
    fn drop(&mut self) {
        self.watcher_stop.store(true, Ordering::Relaxed);
        // Drop plugin before library (plugin's drop is in the library).
        self.plugin = None;
        // Leaked handles are intentionally not closed (TLS destructors
        // in the dylib could segfault on unload). But we *can* clean
        // up the temp files for the active handle — its plugin is gone
        // now and there's no possibility of a future `dlsym`.
        if let (Some(lib), Some(path)) = (self.library.take(), self.current_temp.take()) {
            drop(lib);
            let _ = std::fs::remove_file(&path);
        }
        // Files behind `leaked_handles` stay on disk until the process
        // exits — matches the leak-the-handle policy. macOS / Linux
        // mmap survives the unlink, but on Windows the file is locked
        // while loaded so we cannot delete it; either way, leaving
        // them is no worse than the leaked dlopen handle itself.
    }
}

/// File watcher loop. Polls mtime ~every 500ms, but checks the stop flag
/// every 50ms so dropping the loader doesn't block waiting for the next
/// poll cycle.
fn watch_loop(path: &std::path::Path, flag: &AtomicBool, stop: &AtomicBool) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    const STOP_CHECK: std::time::Duration = std::time::Duration::from_millis(50);
    let chunks = (POLL_INTERVAL.as_millis() / STOP_CHECK.as_millis()) as u32;

    let mut last_mtime = file_mtime(path);
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..chunks {
            std::thread::sleep(STOP_CHECK);
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        let mtime = file_mtime(path);
        if mtime > last_mtime {
            // Wait for compiler to finish writing — also broken into
            // STOP_CHECK chunks so a Drop during the settle window
            // doesn't have to wait the full 200ms.
            for _ in 0..(200 / STOP_CHECK.as_millis() as u32) {
                std::thread::sleep(STOP_CHECK);
                if stop.load(Ordering::Relaxed) {
                    return;
                }
            }
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

/// Streaming CRC32 fingerprint of `path`'s contents.
///
/// Reads through an 8 KiB buffer instead of slurping the whole dylib
/// into memory. A 5–20 MB dylib, polled every 500 ms, used to allocate
/// and free its full contents on every poll cycle; this keeps the working
/// set bounded and lets the kernel page-cache do the actual I/O work.
///
/// Uses `crc32fast`, which is already in the workspace dep graph
/// transitively (via `png`/`image` etc.) and is roughly 30× faster
/// than the table-free byte-at-a-time implementation we previously
/// inlined to avoid adding an explicit dep.
///
/// Returns `None` on `open` / `read` failure (file missing,
/// permissions, mid-write interruption). An empty file successfully
/// hashes to `Some(0)` (CRC of empty input) — distinct from the I/O
/// failure case so callers can log the two differently. The previous
/// signature collapsed both into `0`, which then aliased the initial
/// `last_hash = 0` and silently treated unreadable files as "unchanged".
fn crc32_file(path: &std::path::Path) -> Option<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            // Partial-read interruption / I/O failure — return None and
            // let the caller retry on the next poll. The compiler's
            // mid-write window is the common case here.
            Err(_) => return None,
        }
    }
    Some(hasher.finalize())
}
