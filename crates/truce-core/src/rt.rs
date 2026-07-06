//! Paranoid real-time allocation checking (the `rt-paranoid` feature).
//!
//! A wrapping global allocator (`RtCheckAlloc`) plus a thread-local
//! "audio section" guard ([`RtSection`]). While the audio thread is
//! inside a section, any allocation it makes is a real-time contract
//! violation and gets recorded and reported.
//!
//! The section is entered around the single `plugin.process()` call in
//! `chunked_process`, which every format wrapper and the test driver
//! route through, so one guard covers all of them. The allocator is
//! installed by the artifact (a plugin cdylib via `truce::plugin!`, or a
//! test binary) with `truce::enable_rt_paranoid!`; a library cannot set
//! a downstream binary's global allocator.
//!
//! Everything here is inert unless the `rt-paranoid` feature is on:
//! [`RtSection::enter`] is a zero-sized no-op and [`allow_alloc`] just
//! calls the closure, so release builds are unaffected.

#[cfg(feature = "rt-paranoid")]
mod imp {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU8, Ordering};

    const MAX_FRAMES: usize = 32;

    #[derive(Clone, Copy)]
    struct FrameBuf {
        ips: [usize; MAX_FRAMES],
        len: usize,
    }

    impl FrameBuf {
        const EMPTY: Self = Self {
            ips: [0; MAX_FRAMES],
            len: 0,
        };
    }

    // Const-initialized so access never lazily allocates - critical,
    // since these are read from inside the allocator hook.
    thread_local! {
        static DEPTH: Cell<u32> = const { Cell::new(0) };
        static RECORDING: Cell<bool> = const { Cell::new(false) };
        static VIOLATIONS: Cell<u32> = const { Cell::new(0) };
        static FIRST: Cell<FrameBuf> = const { Cell::new(FrameBuf::EMPTY) };
        // `Some(n)` while inside `audit`: section violations accumulate
        // here and the normal report/panic is suppressed, so a test can
        // assert on the count instead.
        static AUDIT: Cell<Option<u32>> = const { Cell::new(None) };
    }

    /// What the checker does when the audio thread allocates inside a
    /// `process` section.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Mode {
        /// Log the count and a backtrace after the block; keep running.
        Count,
        /// Panic - fails the block, gating a whole test suite.
        Panic,
        /// Abort at the offending allocation (catch the live stack in a
        /// debugger).
        Trap,
    }

    impl Mode {
        const fn to_u8(self) -> u8 {
            match self {
                Mode::Count => 0,
                Mode::Panic => 1,
                Mode::Trap => 2,
            }
        }
        fn from_u8(v: u8) -> Self {
            match v {
                1 => Mode::Panic,
                2 => Mode::Trap,
                _ => Mode::Count,
            }
        }
    }

    // `MODE_UNINIT` until first read or `set_mode`, so the env var is
    // consulted at most once and only when nothing set the mode
    // programmatically. Backing it with an atomic (not a `OnceLock<Mode>`
    // seeded from `env::var`) means the read on the audio thread is a
    // plain load with no `String` allocation once a `set_mode` has run.
    const MODE_UNINIT: u8 = u8::MAX;
    static MODE: AtomicU8 = AtomicU8::new(MODE_UNINIT);

    /// Set the reaction the checker takes on a violation, overriding
    /// `TRUCE_RT_PARANOID`. Call before the first audio block (a test
    /// harness, `main`, or a `#[ctor]`); the last call wins. Setting the
    /// mode this way skips the one-time env read the default path does on
    /// the audio thread.
    pub fn set_mode(mode: Mode) {
        MODE.store(mode.to_u8(), Ordering::Relaxed);
    }

    fn mode() -> Mode {
        let v = MODE.load(Ordering::Relaxed);
        if v != MODE_UNINIT {
            return Mode::from_u8(v);
        }
        // No explicit `set_mode`: seed from `TRUCE_RT_PARANOID` once.
        // `env::var` allocates, but this runs at most once (guarded by
        // the compare-exchange) and never after a `set_mode`.
        let seeded = mode_from_env().to_u8();
        match MODE.compare_exchange(MODE_UNINIT, seeded, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => Mode::from_u8(seeded),
            Err(actual) => Mode::from_u8(actual),
        }
    }

    fn mode_from_env() -> Mode {
        match std::env::var("TRUCE_RT_PARANOID").as_deref() {
            Ok("panic") => Mode::Panic,
            Ok("trap" | "abort") => Mode::Trap,
            _ => Mode::Count,
        }
    }

    #[cfg(test)]
    pub(crate) fn current_mode() -> Mode {
        mode()
    }

    /// Guard around a real-time section (one `plugin.process()` call).
    /// Nesting composes via a depth counter; the report fires only when
    /// the outermost guard drops.
    pub struct RtSection {
        _private: (),
    }

    impl RtSection {
        #[inline]
        #[must_use]
        pub fn enter() -> Self {
            DEPTH.with(|d| d.set(d.get().wrapping_add(1)));
            Self { _private: () }
        }
    }

    impl Drop for RtSection {
        fn drop(&mut self) {
            let depth = DEPTH.with(|d| {
                let n = d.get().wrapping_sub(1);
                d.set(n);
                n
            });
            // Report only on full exit, where DEPTH is 0 so the report's
            // own allocations aren't re-flagged.
            if depth == 0 {
                let count = VIOLATIONS.with(|v| v.replace(0));
                if count > 0 {
                    // Inside `audit`, accumulate and stay quiet so the
                    // test decides what the count means. Otherwise report
                    // per the global mode.
                    if AUDIT.with(Cell::get).is_some() {
                        AUDIT.with(|a| a.set(a.get().map(|n| n.saturating_add(count))));
                    } else {
                        report(count);
                    }
                }
            }
        }
    }

    /// Run `f` and return `(result, allocations)` where `allocations` is
    /// the number of audio-thread allocations made inside `process`
    /// sections during `f`, with the normal per-section report/panic
    /// suppressed. Same-thread only (the test driver runs `process` on
    /// the calling thread). Underpins the `truce-test` audio-alloc
    /// assertions.
    pub fn audit<R>(f: impl FnOnce() -> R) -> (R, u32) {
        let prev = AUDIT.with(|a| a.replace(Some(0)));
        let r = f();
        let count = AUDIT.with(|a| a.replace(prev)).unwrap_or(0);
        (r, count)
    }

    /// Whether the checker is compiled in (the `rt-paranoid` feature).
    /// A test asserting that code *does* allocate skips its assertion
    /// when this is false, so it doesn't fail an ordinary build.
    #[must_use]
    pub fn is_active() -> bool {
        true
    }

    /// Enter a section, run `f`, and return how many allocations it made,
    /// skipping the reporting path so tests can assert on the count
    /// directly. Only compiled for the crate's own tests.
    #[cfg(test)]
    pub(crate) fn count_allocs<R>(f: impl FnOnce() -> R) -> u32 {
        DEPTH.with(|d| d.set(d.get().wrapping_add(1)));
        VIOLATIONS.with(|v| v.set(0));
        let _ = f();
        let n = VIOLATIONS.with(|v| v.replace(0));
        DEPTH.with(|d| d.set(d.get().wrapping_sub(1)));
        n
    }

    /// Suspend checking for `f`, for a region inside `process` that must
    /// legitimately allocate (a debug-only measurement, a first-block
    /// lazy init). Restores on return or panic.
    pub fn allow_alloc<R>(f: impl FnOnce() -> R) -> R {
        struct Restore(u32);
        impl Drop for Restore {
            fn drop(&mut self) {
                DEPTH.with(|d| d.set(self.0));
            }
        }
        let _restore = Restore(DEPTH.with(|d| d.replace(0)));
        f()
    }

    /// Called from the allocator hook. Records a violation when the
    /// current thread is inside a section. Must not allocate: the
    /// `RECORDING` re-entrancy flag makes any allocation triggered by
    /// the recording path itself a no-op instead of infinite recursion.
    #[inline]
    fn note_alloc() {
        if DEPTH.with(Cell::get) == 0 || RECORDING.with(Cell::get) {
            return;
        }
        RECORDING.with(|r| r.set(true));
        let n = VIOLATIONS.with(|v| {
            let n = v.get().wrapping_add(1);
            v.set(n);
            n
        });
        if n == 1 {
            capture_first();
        }
        if mode() == Mode::Trap {
            // SIGABRT stops a debugger on the offending allocation with
            // the live audio-thread stack.
            std::process::abort();
        }
        RECORDING.with(|r| r.set(false));
    }

    /// Walk the stack into a fixed thread-local buffer. The raw address
    /// walk does not allocate; symbol resolution is deferred to
    /// `report`, which runs after the section with allocation allowed.
    fn capture_first() {
        let mut buf = FrameBuf::EMPTY;
        backtrace::trace(|frame| {
            if buf.len < MAX_FRAMES {
                buf.ips[buf.len] = frame.ip() as usize;
                buf.len += 1;
                true
            } else {
                false
            }
        });
        FIRST.with(|f| f.set(buf));
    }

    fn report(count: u32) {
        use std::fmt::Write as _;

        let buf = FIRST.with(|f| f.replace(FrameBuf::EMPTY));
        // Resolve into a separate buffer so the "first allocation" header
        // is only emitted when at least one frame resolves - macOS test
        // builds without a dSYM resolve to nothing, and a dangling header
        // reads as broken.
        let mut frames = String::new();
        for &ip in &buf.ips[..buf.len] {
            backtrace::resolve(ip as *mut _, |s| {
                let name = s.name().map(|n| n.to_string()).unwrap_or_default();
                if name.starts_with("truce_core::rt") || name.starts_with("backtrace") {
                    return; // skip our own hook / capture frames
                }
                match (s.filename(), s.lineno()) {
                    (Some(file), Some(line)) => {
                        let _ = write!(frames, "\n    {name} ({}:{line})", file.display());
                    }
                    _ if !name.is_empty() => {
                        let _ = write!(frames, "\n    {name}");
                    }
                    _ => {}
                }
            });
        }
        let mut msg =
            format!("truce rt-paranoid: {count} allocation(s) on the audio thread in process()");
        if !frames.is_empty() {
            msg.push_str("\n  first allocation:");
            msg.push_str(&frames);
        }
        // Panicking in `RtSection::drop` while the thread is already
        // unwinding (process itself panicked) would abort; downgrade to
        // a log in that case.
        match mode() {
            Mode::Panic if !std::thread::panicking() => panic!("{msg}"),
            _ => eprintln!("{msg}"),
        }
    }

    /// Global allocator that flags allocations made on the audio thread
    /// inside an [`RtSection`]. Delegates to [`System`] for the actual
    /// allocation so the program keeps running (in `count` mode).
    ///
    /// Install it in the artifact with `truce::enable_rt_paranoid!`.
    pub struct RtCheckAlloc;

    impl RtCheckAlloc {
        #[must_use]
        pub const fn new() -> Self {
            Self
        }
    }

    impl Default for RtCheckAlloc {
        fn default() -> Self {
            Self::new()
        }
    }

    // SAFETY: every method forwards to the global `System` allocator
    // with the same arguments; `note_alloc` only reads/writes thread-
    // local `Cell`s and never itself allocates (guarded by `RECORDING`),
    // so it cannot violate the `GlobalAlloc` contract.
    unsafe impl GlobalAlloc for RtCheckAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note_alloc();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note_alloc();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note_alloc();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // Freeing on the audio thread is also non-RT, but flagging
            // every drop is noisy (a value moved in from a prior block),
            // so `dealloc` forwards silently - a future opt-in sub-mode.
            unsafe { System.dealloc(ptr, layout) }
        }
    }
}

#[cfg(not(feature = "rt-paranoid"))]
mod imp {
    /// No-op real-time section guard. With `rt-paranoid` off this is a
    /// zero-sized type whose `enter`/drop compile away.
    pub struct RtSection {
        _private: (),
    }

    impl RtSection {
        #[inline]
        #[must_use]
        pub fn enter() -> Self {
            Self { _private: () }
        }
    }

    /// No-op with `rt-paranoid` off: just calls `f`.
    #[inline]
    pub fn allow_alloc<R>(f: impl FnOnce() -> R) -> R {
        f()
    }

    /// No-op with `rt-paranoid` off: runs `f`, reports zero allocations.
    #[inline]
    pub fn audit<R>(f: impl FnOnce() -> R) -> (R, u32) {
        (f(), 0)
    }

    /// The checker is not compiled in.
    #[must_use]
    #[inline]
    pub fn is_active() -> bool {
        false
    }

    /// What the checker does on a violation. Present with the feature off
    /// so `set_mode` call sites compile unconditionally; the checker is
    /// inert, so it has no effect.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Mode {
        Count,
        Panic,
        Trap,
    }

    /// No-op with `rt-paranoid` off.
    #[inline]
    pub fn set_mode(_mode: Mode) {}
}

pub use imp::{Mode, RtSection, allow_alloc, audit, is_active, set_mode};

#[cfg(feature = "rt-paranoid")]
pub use imp::RtCheckAlloc;

// Install the checking allocator for this crate's own test binary so the
// mechanism can be exercised. A `#[global_allocator]` in a lib applies to
// that lib's test/bench binaries only, never to downstream crates.
#[cfg(all(test, feature = "rt-paranoid"))]
#[global_allocator]
static TEST_ALLOC: RtCheckAlloc = RtCheckAlloc::new();

#[cfg(all(test, feature = "rt-paranoid"))]
mod tests {
    use super::allow_alloc;
    use super::imp::count_allocs;
    use std::hint::black_box;

    #[test]
    fn alloc_in_section_is_flagged() {
        let n = count_allocs(|| {
            let v: Vec<u8> = Vec::with_capacity(4096);
            black_box(v.as_ptr());
        });
        assert!(n >= 1, "expected the in-section allocation to be flagged");
    }

    #[test]
    fn no_alloc_in_section_is_clean() {
        let n = count_allocs(|| {
            let x = black_box(2) + black_box(3);
            black_box(x);
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn allow_alloc_suppresses_flagging() {
        let n = count_allocs(|| {
            allow_alloc(|| {
                let v: Vec<u8> = Vec::with_capacity(4096);
                black_box(v.as_ptr());
            });
        });
        assert_eq!(n, 0, "allow_alloc should suspend checking for its scope");
    }

    #[test]
    fn shared_plugin_warms_the_mediation_lock() {
        use super::imp::count_allocs;
        use crate::wrapper::{lock_plugin, shared_plugin};

        // `shared_plugin` locks once at construction so the first lock on
        // the audio thread doesn't allocate (macOS std `Mutex` boxes its
        // `pthread_mutex_t` lazily). Taking the lock here must be clean.
        let shared = shared_plugin(vec![0u8; 16]);
        let n = count_allocs(|| {
            drop(lock_plugin(&shared));
        });
        assert_eq!(n, 0, "the mediation lock's first lock must be warmed");
    }

    #[test]
    fn set_mode_overrides_the_default() {
        use super::imp::current_mode;
        use super::{Mode, set_mode};

        // `Panic` is safe to leave briefly: `count_allocs` (the other
        // tests) never routes through the report path. Restore `Count`
        // so nothing else in the binary is affected. Never set `Trap` -
        // it aborts the process.
        set_mode(Mode::Panic);
        assert_eq!(current_mode(), Mode::Panic);
        set_mode(Mode::Count);
        assert_eq!(current_mode(), Mode::Count);
    }
}
