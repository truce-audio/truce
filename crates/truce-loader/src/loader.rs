//! `NativeLoader` — loads and hot-reloads a plugin dylib.
//!
//! Uses native Rust ABI (no C translation layer). Verifies
//! compatibility via `AbiCanary` + vtable probe before use.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

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

use crate::LoaderPlugin;
use crate::canary::{AbiCanary, verify_probe};

type ProbeFn = fn() -> Box<dyn LoaderPlugin>;
type CreateFn = fn(*const ()) -> Box<dyn LoaderPlugin>;

/// Verified candidate dylib + instance, ready to swap in.
struct Candidate {
    library: Library,
    plugin: Box<dyn LoaderPlugin>,
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
    plugin: Option<Box<dyn LoaderPlugin>>,
    /// Raw pointer to the shell's `Arc<Params>` (type-erased).
    /// Passed to `truce_create()` so the plugin shares the same params.
    params_ptr: *const (),
    last_modified: SystemTime,
    last_hash: u32,
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

// SAFETY: NativeLoader is only accessed from one thread at a time.
// The audio thread calls process/reload, the main thread calls render.
// The shell wraps access in a parking_lot::Mutex.
unsafe impl Send for NativeLoader {}

impl NativeLoader {
    /// Construct the loader and run the initial load.
    ///
    /// Does not spawn the file watcher — call
    /// [`NativeLoader::spawn_watcher`] after wrapping the loader in an
    /// `Arc<Mutex<...>>` so the watcher thread can drive reloads
    /// itself, off the audio thread.
    pub fn new(dylib_path: PathBuf, params_ptr: *const ()) -> Self {
        let mut loader = Self {
            dylib_path,
            library: None,
            plugin: None,
            params_ptr,
            last_modified: SystemTime::UNIX_EPOCH,
            last_hash: 0,
            watcher_stop: Arc::new(AtomicBool::new(false)),
            leaked_handles: Vec::new(),
            temp_paths: Vec::new(),
            current_temp: None,
            load_counter: 0,
            instance_id: LOADER_ID.fetch_add(1, Ordering::Relaxed),
        };
        loader.load();
        loader
    }

    /// Spawn the file-mtime watcher thread.
    ///
    /// The watcher polls the dylib path; when mtime advances and
    /// settles, it acquires `loader` and runs [`NativeLoader::reload`]
    /// directly. This keeps the codesign / dlopen / canary-probe work
    /// off the audio thread — the audio thread only observes
    /// reloads via [`NativeLoader::load_counter`] advances and runs
    /// `plugin.reset()` to match the new sample rate / block size.
    ///
    /// Held as a `Weak` so dropping the last `Arc<Mutex<NativeLoader>>`
    /// breaks the watcher's reference and lets the thread exit on its
    /// next stop-flag check.
    pub fn spawn_watcher(loader: &Arc<Mutex<Self>>) {
        let weak = Arc::downgrade(loader);
        let (path, stop) = {
            let guard = loader.lock();
            (guard.dylib_path.clone(), guard.watcher_stop.clone())
        };
        std::thread::Builder::new()
            .name("truce-hot-watcher".into())
            .spawn(move || watch_loop(&path, &weak, &stop))
            .ok();
    }

    /// Build, verify, and instantiate a fresh dylib at `dylib_path`.
    /// Does not touch `self.library` / `self.plugin`. Caller decides
    /// whether to swap the old state out for the result.
    ///
    /// `new_hash` comes from the caller to avoid re-reading the dylib
    /// — `load` and `reload` already hashed it to detect "unchanged"
    /// before deciding to call us. Re-hashing inside here would double
    /// the per-reload I/O on a 5-20 MB dylib.
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
    pub fn plugin(&self) -> Option<&dyn LoaderPlugin> {
        self.plugin.as_ref().map(std::convert::AsRef::as_ref)
    }

    pub fn plugin_mut(&mut self) -> Option<&mut dyn LoaderPlugin> {
        self.plugin.as_mut().map(std::convert::AsMut::as_mut)
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

/// File watcher loop. Polls mtime ~every 500ms, but checks the stop
/// flag every 50ms so dropping the loader doesn't block waiting for
/// the next poll cycle.
///
/// On a stable mtime advance, takes the loader lock with
/// `try_lock_for` and runs `reload()` directly. Earlier shapes only
/// set a `reload_pending` flag and let the audio thread call
/// `reload()` itself, which spawned `codesign` and dlopen on the
/// audio thread. Driving reload here keeps that work off the audio
/// path entirely.
fn watch_loop(path: &std::path::Path, loader: &Weak<Mutex<NativeLoader>>, stop: &AtomicBool) {
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const STOP_CHECK: Duration = Duration::from_millis(50);
    const SETTLE: Duration = Duration::from_millis(200);
    /// How long to wait for the audio thread to release the loader
    /// mutex before giving up and retrying on the next poll. Short
    /// enough that a stuck audio thread doesn't pin the watcher; long
    /// enough to cover a single `process()` call (typically ≪ 50 ms).
    const LOCK_WAIT: Duration = Duration::from_millis(50);
    // Both constants are sub-second; the u128 → u32 cast is bounded.
    #[allow(clippy::cast_possible_truncation)]
    let chunks = (POLL_INTERVAL.as_millis() / STOP_CHECK.as_millis()) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let settle_chunks = (SETTLE.as_millis() / STOP_CHECK.as_millis()) as u32;

    let mut last_mtime = file_mtime(path);
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..chunks {
            std::thread::sleep(STOP_CHECK);
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        let mtime = file_mtime(path);
        if mtime <= last_mtime {
            continue;
        }
        // Wait for the compiler to finish writing — broken into
        // STOP_CHECK chunks so dropping the loader during the settle
        // window doesn't block for the full SETTLE duration.
        for _ in 0..settle_chunks {
            std::thread::sleep(STOP_CHECK);
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        last_mtime = file_mtime(path);

        let Some(loader) = loader.upgrade() else {
            return;
        };
        let Some(mut guard) = loader.try_lock_for(LOCK_WAIT) else {
            // Audio thread holds the lock; try again on the next poll.
            continue;
        };
        guard.reload();
    }
}

fn file_mtime(path: &std::path::Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Streaming CRC32 fingerprint of `path`'s contents.
///
/// Reads through an 8 KiB buffer so a 5–20 MB dylib polled every
/// 500 ms doesn't allocate its full contents per poll cycle.
///
/// Returns `None` on `open` / `read` failure (file missing,
/// permissions, mid-write interruption — the compiler's mid-write
/// window is the common case). An empty file successfully hashes
/// to `Some(0)` — distinct from the I/O failure case so a caller
/// can't conflate "unreadable" with "unchanged" against an initial
/// `last_hash = 0`.
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
