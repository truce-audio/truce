//! `NativeLoader` - loads and hot-reloads a plugin dylib.
//!
//! Uses native Rust ABI (no C translation layer). Verifies
//! compatibility via `AbiCanary` + symbol presence before use. The
//! dylib exports a flat set of functions over an opaque `*mut ()` state
//! pointer (see `export_plugin!`); the loader resolves them into
//! [`LogicSymbols`]. The DSP state itself is owned by the shell
//! ([`crate::shell::HotShell`]), not the loader, so it can survive a
//! reload - the loader only swaps the code.

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
/// `Relaxed` ordering is sufficient - the only consumer is the
/// owning `NativeLoader`, which reads the value back from its own
/// `instance_id` field, never via a re-load of `LOADER_ID`.
static LOADER_ID: AtomicU64 = AtomicU64::new(0);

use libloading::{Library, Symbol};

use crate::canary::AbiCanary;
use truce_core::buffer::AudioBuffer;
use truce_core::config::AudioConfig;
use truce_core::events::EventList;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::state::StateLoadError;
use truce_params::sample::Sample;

/// The `truce_process` export's signature (state, params, buffer,
/// events, ctx) -> status. Aliased to keep [`LogicSymbols`] readable.
type ProcessFn<S> =
    fn(*mut (), *const (), &mut AudioBuffer<S>, &EventList, &mut ProcessContext) -> ProcessStatus;

/// The `truce_drop_state` export's signature. The shell keeps one of
/// these alongside its state so the allocation is freed by the dylib
/// that made it, even after a reload.
pub type StateDropFn = fn(*mut ());

/// The flat function-pointer table resolved from a loaded dylib. Every
/// entry operates on an opaque `*mut ()` / `*const ()` state pointer
/// (an erased `Box<State>`) plus a `*const ()` params pointer (the
/// shell's `Arc<Params>`). The pointers stay valid because the loader
/// never `dlclose`s a library (handles leak by design), so a table
/// resolved from an older dylib keeps working for state that dylib made.
struct LogicSymbols<S: Sample> {
    init_state: fn(*const ()) -> *mut (),
    drop_state: StateDropFn,
    reset: fn(*mut (), *const (), &AudioConfig),
    process: ProcessFn<S>,
    latency: fn(*const ()) -> u32,
    tail: fn(*const ()) -> u32,
    save_state: fn(*const ()) -> Vec<u8>,
    snapshot_into: fn(*const (), &mut Vec<u8>) -> bool,
    snapshot_version: fn(*const ()) -> Option<u64>,
    load_state: fn(*mut (), &[u8]) -> Result<(), StateLoadError>,
    state_changed: fn(*mut (), *const ()),
    /// Structural fingerprint of the plugin's `State`. The shell keeps
    /// live state across a reload only when this matches the fingerprint
    /// the held state was created with.
    fingerprint: u64,
}

impl<S: Sample> LogicSymbols<S> {
    /// Resolve every exported symbol from `lib`. Returns `None` (and
    /// logs) if any is missing - a stale dylib built before the flat ABI
    /// won't have them, and is refused rather than half-bound.
    ///
    /// # Safety
    /// `lib` must be a truce logic dylib whose `AbiCanary` already
    /// matched the shell (checked before this call), so each symbol has
    /// the signature named here.
    unsafe fn resolve(lib: &Library) -> Option<Self> {
        // Each `*sym` copies the bare `fn` pointer out of the borrowed
        // `Symbol`; it stays valid as long as `lib`'s code is mapped,
        // which it always is (libraries are leaked, never closed).
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let s: Symbol<$ty> = match unsafe { lib.get($name) } {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!(
                            "missing export {} (stale pre-flat-ABI dylib?): {e}",
                            std::str::from_utf8($name).unwrap_or("?")
                        );
                        return None;
                    }
                };
                *s
            }};
        }
        let fingerprint_fn: fn() -> u64 = sym!(b"truce_state_fingerprint", fn() -> u64);
        Some(Self {
            init_state: sym!(b"truce_init_state", fn(*const ()) -> *mut ()),
            drop_state: sym!(b"truce_drop_state", fn(*mut ())),
            reset: sym!(b"truce_reset", fn(*mut (), *const (), &AudioConfig)),
            process: sym!(b"truce_process", ProcessFn<S>),
            latency: sym!(b"truce_latency", fn(*const ()) -> u32),
            tail: sym!(b"truce_tail", fn(*const ()) -> u32),
            save_state: sym!(b"truce_save_state", fn(*const ()) -> Vec<u8>),
            snapshot_into: sym!(b"truce_snapshot_into", fn(*const (), &mut Vec<u8>) -> bool),
            snapshot_version: sym!(b"truce_snapshot_version", fn(*const ()) -> Option<u64>),
            load_state: sym!(
                b"truce_load_state",
                fn(*mut (), &[u8]) -> Result<(), StateLoadError>
            ),
            state_changed: sym!(b"truce_state_changed", fn(*mut (), *const ())),
            fingerprint: fingerprint_fn(),
        })
    }
}

/// Verified candidate dylib + resolved symbol table, ready to swap in.
struct Candidate<S: Sample> {
    library: Library,
    symbols: LogicSymbols<S>,
    hash: u32,
    mtime: SystemTime,
    /// Path of the versioned copy in the system temp dir. Tracked so
    /// the loader can unlink it on Drop after the matching `Library`
    /// handle has been released.
    temp_path: PathBuf,
}

/// Manages a hot-reloadable plugin dylib.
///
/// Generic over `S` (the plugin's sample type - `f32` by default, the
/// host-wire format). A `prelude64` plugin built into a logic dylib
/// must be loaded by an `S = f64` shell; the precision is also baked
/// into [`AbiCanary::sample_precision`] so a mismatch fails the canary
/// check rather than silently binding to a wrong-shape vtable.
pub struct NativeLoader<S: Sample = f32> {
    dylib_path: PathBuf,
    library: Option<Library>,
    /// Resolved flat-ABI function table for the currently loaded dylib.
    /// `None` before the first successful load. State is not held here -
    /// the shell owns it so it can survive a reload.
    symbols: Option<LogicSymbols<S>>,
    /// Raw pointer to the shell's `Arc<Params>` (type-erased), passed to
    /// every state-op symbol so the plugin shares the shell's params.
    params_ptr: *const (),
    last_modified: SystemTime,
    last_hash: u32,
    /// Set to true to stop the file watcher thread.
    watcher_stop: Arc<AtomicBool>,
    /// Old library handles - leaked to avoid TLS destructor segfaults.
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
unsafe impl<S: Sample> Send for NativeLoader<S> {}

impl<S: Sample> NativeLoader<S> {
    /// Construct the loader and run the initial load.
    ///
    /// Does not spawn the file watcher - call
    /// [`NativeLoader::spawn_watcher`] after wrapping the loader in an
    /// `Arc<Mutex<...>>` so the watcher thread can drive reloads
    /// itself, off the audio thread.
    pub fn new(dylib_path: PathBuf, params_ptr: *const ()) -> Self {
        let mut loader = Self {
            dylib_path,
            library: None,
            symbols: None,
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
    /// off the audio thread - the audio thread only observes
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
            .spawn(move || watch_loop::<S>(&path, &weak, &stop))
            .ok();
    }

    /// Build, verify, and instantiate a fresh dylib at `dylib_path`.
    /// Does not touch `self.library` / `self.plugin`. Caller decides
    /// whether to swap the old state out for the result.
    ///
    /// `new_hash` comes from the caller to avoid re-reading the dylib;
    /// `load` and `reload` already hashed it to detect "unchanged"
    /// before deciding to call us. Re-hashing inside here would double
    /// the per-reload I/O on a 5-20 MB dylib.
    fn build_candidate(&mut self, new_hash: u32) -> Option<Candidate<S>> {
        // Copy to versioned temp path to defeat macOS dyld cache.
        let temp = match self.copy_versioned() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("failed to copy dylib: {e}");
                return None;
            }
        };

        // macOS: ad-hoc codesign (required by SIP). If the temp path
        // is non-UTF-8 (rare - `std::env::temp_dir()` usually lives
        // under a UTF-8 prefix, but the user can override via env)
        // `to_str` fails and codesign would silently no-op against an
        // empty path. The `Library::new` call below then fails on
        // an unsigned dylib with an opaque SIP error, two error
        // sites from the root cause. Log up front so the cause is
        // visible.
        #[cfg(target_os = "macos")]
        if let Some(temp_str) = temp.to_str() {
            let _ = std::process::Command::new("codesign")
                .args(["--sign", "-", "--force", temp_str])
                .output();
        } else {
            log::warn!(
                "codesign skipped: temp dylib path is not valid UTF-8 ({}); \
                 dlopen will likely fail under SIP",
                temp.display()
            );
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

        // The versioned symbol makes canary-layout evolution safe: the
        // struct returns by value, so a shell must never call a canary
        // of a different shape. A dylib exporting only an older
        // `truce_abi_canary*` fails the lookup and is refused here.
        let canary_fn: Symbol<fn() -> AbiCanary> = match unsafe { lib.get(b"truce_abi_canary_v2") }
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("missing truce_abi_canary_v2 export (stale pre-2.0 logic dylib?): {e}");
                cleanup_temp(lib, &temp);
                return None;
            }
        };
        let dylib_canary = canary_fn();
        let shell_canary = AbiCanary::current::<S>();
        if !shell_canary.matches(&dylib_canary) {
            log::error!(
                "ABI mismatch - rebuild both shell and logic:\n{}",
                shell_canary.diff_report(&dylib_canary)
            );
            cleanup_temp(lib, &temp);
            return None;
        }

        // Resolve the flat-ABI symbol table. Symbol presence (plus the
        // canary above) replaces the old vtable probe: a mismatched or
        // stale dylib is missing these exports and is refused here.
        // SAFETY: the canary matched, so the exports have the signatures
        // `LogicSymbols::resolve` names.
        let Some(symbols) = (unsafe { LogicSymbols::<S>::resolve(&lib) }) else {
            cleanup_temp(lib, &temp);
            return None;
        };

        Some(Candidate {
            library: lib,
            symbols,
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
                self.symbols = Some(cand.symbols);
                self.last_hash = cand.hash;
                self.last_modified = cand.mtime;
                self.current_temp = Some(cand.temp_path);
                log::info!("loaded plugin dylib: {}", self.dylib_path.display());
                true
            }
            None => false,
        }
    }

    /// Reload the dylib. Verifies the *new* dylib first; only swaps the
    /// symbol table after the candidate is fully constructed, so a failed
    /// canary or missing symbol leaves the host on the previous code
    /// instead of silence.
    ///
    /// State is not touched here: the shell owns it and, on seeing the
    /// [`load_counter`](Self::load_counter) advance, decides via the
    /// [`state_fingerprint`](Self::state_fingerprint) whether to keep the
    /// live state (code-only edit) or re-init it (layout changed).
    pub fn reload(&mut self) -> bool {
        let Some(new_hash) = crc32_file(&self.dylib_path) else {
            log::warn!(
                "failed to hash dylib at {} (missing / unreadable / mid-write); keeping previous code loaded",
                self.dylib_path.display()
            );
            return false;
        };
        if new_hash == self.last_hash && self.library.is_some() {
            log::debug!("dylib unchanged (CRC32 match), skipping reload");
            return true;
        }

        // Build + verify the candidate while the old code is still live.
        let Some(candidate) = self.build_candidate(new_hash) else {
            log::warn!("hot-reload failed; keeping previous code loaded");
            return false;
        };

        // Leak the old library: its code must stay mapped because the
        // shell may still hold state whose `drop_state` lives in it, and
        // `dlclose` would segfault on TLS destructors (macOS). The temp
        // file is tracked so `Drop` removes it once the process exits.
        if let Some(old) = self.library.take() {
            self.leaked_handles.push(old);
            if let Some(p) = self.current_temp.take() {
                self.temp_paths.push(p);
            }
        }

        self.library = Some(candidate.library);
        self.symbols = Some(candidate.symbols);
        self.last_hash = candidate.hash;
        self.last_modified = candidate.mtime;
        self.current_temp = Some(candidate.temp_path);

        log::info!(
            "hot-reload complete (load #{}, {} leaked handles)",
            self.load_counter,
            self.leaked_handles.len()
        );
        true
    }

    /// Fingerprint of the currently loaded dylib's `State` layout, or
    /// `None` if nothing is loaded. The shell compares this to the
    /// fingerprint its live state was born with to decide preservation.
    #[must_use]
    pub fn state_fingerprint(&self) -> Option<u64> {
        self.symbols.as_ref().map(|s| s.fingerprint)
    }

    /// Allocate fresh DSP state from the current dylib. Returns the
    /// opaque state pointer, its layout fingerprint, and the matching
    /// `drop` function (which the shell must keep and call to free that
    /// exact allocation, since it lives in this dylib's code).
    #[must_use]
    pub fn init_state(&self) -> Option<(*mut (), u64, StateDropFn)> {
        let s = self.symbols.as_ref()?;
        Some(((s.init_state)(self.params_ptr), s.fingerprint, s.drop_state))
    }

    /// Run the current dylib's `process` on `state` (opaque, layout must
    /// match the current fingerprint). Returns `Normal` if nothing is
    /// loaded (silent block).
    pub fn process(
        &self,
        state: *mut (),
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        match self.symbols.as_ref() {
            Some(s) => (s.process)(state, self.params_ptr, buffer, events, ctx),
            None => ProcessStatus::Normal,
        }
    }

    /// Run the current dylib's `reset` on `state`.
    pub fn reset(&self, state: *mut (), config: &AudioConfig) {
        if let Some(s) = self.symbols.as_ref() {
            (s.reset)(state, self.params_ptr, config);
        }
    }

    #[must_use]
    pub fn latency(&self, state: *const ()) -> u32 {
        self.symbols.as_ref().map_or(0, |s| (s.latency)(state))
    }

    #[must_use]
    pub fn tail(&self, state: *const ()) -> u32 {
        self.symbols.as_ref().map_or(0, |s| (s.tail)(state))
    }

    #[must_use]
    pub fn save_state(&self, state: *const ()) -> Vec<u8> {
        self.symbols
            .as_ref()
            .map_or_else(Vec::new, |s| (s.save_state)(state))
    }

    pub fn snapshot_into(&self, state: *const (), buf: &mut Vec<u8>) -> bool {
        self.symbols
            .as_ref()
            .is_some_and(|s| (s.snapshot_into)(state, buf))
    }

    /// Snapshot generation token for the loaded logic, or `None` when no
    /// dylib is loaded or the plugin doesn't version its snapshot (in
    /// which case the shell re-serializes every block, as before).
    #[must_use]
    pub fn snapshot_version(&self, state: *const ()) -> Option<u64> {
        self.symbols
            .as_ref()
            .and_then(|s| (s.snapshot_version)(state))
    }

    /// Restore `state` from `data`, then fire `state_changed` in the same
    /// window (matching the format-wrapper bridges' policy).
    ///
    /// # Errors
    /// Forwards the dylib's `load_state` failure (malformed / stale blob).
    pub fn load_state(&self, state: *mut (), data: &[u8]) -> Result<(), StateLoadError> {
        match self.symbols.as_ref() {
            Some(s) => {
                let r = (s.load_state)(state, data);
                (s.state_changed)(state, self.params_ptr);
                r
            }
            None => Ok(()),
        }
    }

    /// Build the loaded plugin's editor via the dylib's
    /// `truce_build_editor` symbol, from `params_ptr` (the shell's
    /// shared params). Receiverless by design: it does not borrow the
    /// loaded logic instance (whose `&mut` the audio thread holds during
    /// a block), so the reloaded editor code is picked up without racing
    /// `process`. `None` when no library is loaded or the symbol is
    /// missing (a stale pre-epoch-3 dylib, already refused by the canary
    /// before it reaches here).
    #[must_use]
    pub fn build_editor(
        &self,
        params_ptr: *const (),
    ) -> Option<Box<dyn truce_core::editor::Editor>> {
        type BuildEditorFn = fn(*const ()) -> Box<dyn truce_core::editor::Editor>;
        let library = self.library.as_ref()?;
        // SAFETY: `export_plugin!` fixes this symbol's signature, and the
        // ABI canary already verified this dylib matches the shell
        // before the library was bound.
        let build: Symbol<BuildEditorFn> = unsafe { library.get(b"truce_build_editor").ok()? };
        Some(build(params_ptr))
    }

    /// Whether a dylib is currently loaded (symbols resolved).
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.symbols.is_some()
    }

    /// Monotonic counter of successful (or attempted) reloads: bumps
    /// once per `copy_versioned()` invocation, which precedes every
    /// candidate build. Consumers that share the same `NativeLoader`
    /// use this to detect when "the other side already reloaded"
    /// without having to drive reload themselves.
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

impl<S: Sample> Drop for NativeLoader<S> {
    fn drop(&mut self) {
        self.watcher_stop.store(true, Ordering::Relaxed);
        // The loader owns only the resolved symbol table (bare fn
        // pointers, nothing to drop); the DSP state is owned and freed
        // by the shell. Drop the symbols before the library, matching
        // library-outlives-its-code ordering.
        self.symbols = None;
        // Leaked handles are intentionally not closed (TLS destructors
        // in the dylib could segfault on unload). But we *can* clean
        // up the temp files for the active handle - its plugin is gone
        // now and there's no possibility of a future `dlsym`.
        if let (Some(lib), Some(path)) = (self.library.take(), self.current_temp.take()) {
            drop(lib);
            let _ = std::fs::remove_file(&path);
        }
        // Files behind `leaked_handles` stay on disk until the process
        // exits - matches the leak-the-handle policy. macOS / Linux
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
fn watch_loop<S: Sample>(
    path: &std::path::Path,
    loader: &Weak<Mutex<NativeLoader<S>>>,
    stop: &AtomicBool,
) {
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
        // Wait for the compiler to finish writing - broken into
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
/// permissions, mid-write interruption - the compiler's mid-write
/// window is the common case). An empty file successfully hashes
/// to `Some(0)` - distinct from the I/O failure case so a caller
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
            // Partial-read interruption / I/O failure - return None and
            // let the caller retry on the next poll. The compiler's
            // mid-write window is the common case here.
            Err(_) => return None,
        }
    }
    Some(hasher.finalize())
}
