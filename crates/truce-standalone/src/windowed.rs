//! Windowed standalone host.
//!
//! Opens an outer parentless baseview window and hosts the plugin's
//! own editor (obtained via `plugin.editor()`) as a child of it -
//! same contract CLAP / VST3 / AU follow. The plugin library is
//! unchanged; standalone is a "host" like any other.
//!
//! The outer window captures keyboard input so QWERTY keystrokes
//! can be translated into MIDI note events and `SPACE` / `S` /
//! `Z` / `X` hotkeys drive transport / state / octave-shift.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};
use keyboard_types::{Code, KeyState, Modifiers};
#[cfg(target_os = "linux")]
use raw_window_handle::HasRawDisplayHandle;
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle as RwhHandle};

use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle};
use truce_core::events::EventBody;
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_params::Params;

use crate::audio::{self, InputController, MidiEvent, OutputController};
use crate::cli::Options;
use crate::keyboard;
use crate::midi::{MidiController, MidiInputThread};
use crate::transport::Transport;
use crate::vlog;

/// Run the plugin with a window. Blocks until the window closes.
///
/// # Panics
///
/// Panics if the host platform reports a raw-window-handle variant
/// the editor backends don't support, or if the audio start /
/// transport / MIDI threads return a fatal error that has no
/// recovery path. Plugin-side panics propagate through the poisoned
/// mutex unchanged.
pub fn run<P: PluginExport>(opts: &Options)
where
    P::Params: 'static,
{
    vlog!("Plugin: {}", P::info().name);
    vlog!(
        "Category: {}",
        match P::info().category {
            PluginCategory::Effect => "effect",
            PluginCategory::Instrument => "instrument",
            PluginCategory::NoteEffect => "midi effect",
            PluginCategory::Analyzer => "analyzer",
            PluginCategory::Tool => "tool",
        }
    );

    #[cfg_attr(not(feature = "playback"), allow(unused_mut))]
    let mut audio_handles = match audio::start_audio::<P>(opts) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // `--state <path>` was already applied inside `audio::start_audio`
    // - it loads BEFORE `snap_smoothers` so the editor + first audio
    // block see the restored values, not defaults ramping toward them.

    let (midi_thread, midi_ctrl) = MidiInputThread::start(opts, Arc::clone(&audio_handles.pending));

    let editor: Option<Box<dyn Editor>> = {
        // Recover from a poisoned plugin mutex (audio thread panicked
        // while holding the lock) instead of cascading the panic
        // through the UI thread. The plugin instance itself may be
        // in a degraded state but the editor handle is just a
        // factory - recovering is enough to keep the standalone
        // alive long enough for the user to save state and exit.
        let mut plugin = audio_handles
            .plugin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        plugin.editor()
    };
    let Some(mut editor) = editor else {
        eprintln!("Plugin returned no editor - falling back to headless mode.");
        drop(audio_handles);
        crate::headless::run::<P>(opts);
        return;
    };
    let (lw, lh) = editor.size();
    // Consulted inside the per-OS cfg blocks below: Linux/Windows
    // skip the lock-window pin, macOS ORs the
    // `NSWindowStyleMaskResizable` bit into baseview's style mask.
    let editor_can_resize = editor.can_resize();

    // Logical-points size handoff between the editor (via the
    // `request_resize` closure) and the outer baseview window handler.
    // Packed as `(width << 32) | height`; sentinel `0` means "no
    // resize pending". Polled at the top of `on_frame` so a request
    // posted from inside the editor's own event loop lands on the
    // outer window within one frame.
    let pending_resize = Arc::new(AtomicU64::new(0));

    let window_opts = WindowOpenOptions {
        title: P::info().name.to_string(),
        size: baseview::Size::new(f64::from(lw), f64::from(lh)),
        scale: WindowScalePolicy::SystemScaleFactor,
    };

    let plugin = Arc::clone(&audio_handles.plugin);
    let pending = Arc::clone(&audio_handles.pending);
    let transport = audio_handles.transport.clone();

    // Both controllers are `Send + Sync` - the cpal streams they
    // wrap live on dedicated worker threads, not on `audio_handles`.
    let input_ctrl = audio_handles.input.clone();
    let output_ctrl = audio_handles.output.clone();
    let is_effect = audio_handles.is_effect;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let channels = audio_handles.channels;
    // Move the `--output-file` capture sink into the handler so the
    // Linux WillClose path (which bypasses Drop via `_exit(0)`) can
    // explicitly finalize it. On non-Linux the handler is dropped
    // normally, which runs `CaptureSink::Drop` and joins the writer
    // thread.
    #[cfg(feature = "playback")]
    let capture = audio_handles.capture.take();

    Window::open_blocking(window_opts, move |window| {
        let truce_parent = match window.raw_window_handle() {
            RwhHandle::AppKit(h) => RawWindowHandle::AppKit(h.ns_view),
            RwhHandle::Win32(h) => RawWindowHandle::Win32(h.hwnd),
            // `h.window` is `c_ulong` - u64 on 64-bit Linux, u32 on
            // Windows. The match arm has to type-check on every
            // platform even though X11 only actually fires on Linux,
            // so widen explicitly. Identity on Linux/macOS, real
            // u32→u64 widening on Windows.
            #[allow(clippy::useless_conversion)]
            RwhHandle::Xlib(h) => RawWindowHandle::X11(h.window.into()),
            _ => panic!("unsupported raw-window-handle variant"),
        };

        // Install the macOS native menu bar (App + Plugin →
        // toggles + device pickers). Must run on the main thread
        // after baseview has initialized NSApp, which it does as
        // part of opening the window. The closure builder runs on
        // the main thread before the event loop starts, so this is
        // the right hook.
        //
        // The menu installs for every plugin category - the
        // Audio Output toggle and Output Device picker are
        // universally useful. The install path itself omits the
        // Mic Input toggle and Input Device picker for non-effects
        // (input is silent for instruments / analyzers without
        // input routing).
        #[cfg(target_os = "macos")]
        crate::menu_macos::install(
            P::info().name,
            is_effect,
            channels,
            &input_ctrl,
            &output_ctrl,
            &midi_ctrl,
        );

        // Windows: same idea, but the menu bar lives inside the
        // window's non-client area, so the install path also grows
        // the parent so the editor child keeps its requested size.
        // Must run BEFORE `editor.open()` below - the resize has to
        // settle before the editor's child window sizes itself.
        #[cfg(target_os = "windows")]
        if let RwhHandle::Win32(h) = window.raw_window_handle() {
            crate::menu_windows::install(
                h.hwnd,
                P::info().name,
                is_effect,
                channels,
                input_ctrl.clone(),
                output_ctrl.clone(),
                midi_ctrl.clone(),
            );
            // Fixed-size editors get the window locked to a
            // close-only frame so maximizing / dragging doesn't
            // stretch the child surface. Resizable editors keep
            // the full Windows non-client frame; the `Resized`
            // event flows back to `editor.set_size` via `on_event`.
            // Linux equivalent: `windowed_x11::pin_size` below.
            if !editor_can_resize {
                crate::windowed_windows::lock_window(h.hwnd);
            }
            // Title-bar / taskbar icon from the icon embedded in the
            // packaged .exe (no-op in un-packaged dev builds).
            crate::windowed_windows::set_window_icon(h.hwnd);
        }

        // Linux: when the editor doesn't opt into resize, X11 window
        // managers happily let the user drag the outer baseview
        // frame even though the child editor surface can't follow.
        // Pin min == max size hints so resize grips disappear in
        // that case; resizable editors leave the WM in charge and
        // the `Resized` event flows back to `editor.set_size` via
        // `on_event` below.
        #[cfg(target_os = "linux")]
        if !editor_can_resize && let RwhHandle::Xlib(h) = window.raw_window_handle() {
            crate::windowed_x11::pin_size(window.raw_display_handle(), &h);
        }

        // macOS: baseview-truce creates its NSWindow with `Titled |
        // Closable | Miniaturizable` only - no resize affordance.
        // When the editor opts into resize, OR in
        // `NSWindowStyleMaskResizable` so the edge-drag behaviour
        // becomes available. Use the rwh `ns_window` field
        // directly: baseview calls `setContentView:` after the
        // build closure runs, so `[ns_view window]` returns nil at
        // this point - going via the populated `ns_window` slot
        // avoids that timing trap.
        #[cfg(target_os = "macos")]
        if editor_can_resize && let RwhHandle::AppKit(h) = window.raw_window_handle() {
            // SAFETY: ns_window is a live NSWindow * baseview owns
            // and has just finished initialising
            // (`makeKeyAndOrderFront` ran before this closure).
            // We're on the main thread - `open_blocking` only runs
            // its builder on the thread that owns the event loop.
            unsafe { crate::windowed_macos::make_resizable(h.ns_window) };
        }

        let ctx = synthesize_editor_context::<P>(&plugin, &transport, Arc::clone(&pending_resize));
        editor.open(truce_parent, ctx);

        StandaloneHandler {
            editor,
            pending_resize,
            current_size: (lw, lh),
            plugin,
            pending,
            transport,
            input_ctrl,
            output_ctrl,
            is_effect,
            octave_offset: 0,
            _midi_thread: midi_thread,
            _midi_ctrl: midi_ctrl,
            #[cfg(feature = "playback")]
            _capture: capture,
        }
    });

    drop(audio_handles);
    vlog!("Goodbye!");
}

struct StandaloneHandler<P: PluginExport + 'static>
where
    P::Params: 'static,
{
    /// Owned for drop-order: the editor's child window must close
    /// before the outer one. Also actively driven by `on_frame` /
    /// `on_event` for the resize round-trip.
    editor: Box<dyn Editor>,
    /// Shared with the editor's `request_resize` closure. Polled
    /// every `on_frame` tick; non-zero values are popped and
    /// applied via `Window::resize`.
    pending_resize: Arc<AtomicU64>,
    /// Last logical size we know the outer baseview window holds.
    /// Used to suppress duplicate `Window::resize` calls on
    /// already-applied targets, and as the baseline for the
    /// `Resized` event -> `editor.set_size` propagation so the
    /// editor only sees real OS-driven changes.
    current_size: (u32, u32),
    plugin: Arc<Mutex<P>>,
    pending: Arc<ArrayQueue<MidiEvent>>,
    transport: Transport,
    /// Toggle handle for mic input (sends to the worker thread
    /// owning the cpal input stream).
    input_ctrl: InputController,
    /// Toggle / device-switch handle for the output. Cmd+O / Ctrl+O
    /// dispatches mute through this; the menu owns device switching.
    output_ctrl: OutputController,
    /// True only for effect plugins; gates the `I` keyboard
    /// shortcut.
    is_effect: bool,
    octave_offset: i8,
    /// Keeps the MIDI hot-plug thread alive for the lifetime of the
    /// window; dropped when the window closes.
    _midi_thread: MidiInputThread,
    /// MIDI device / channel control handle. The menu holds its own
    /// clone; kept here too so it lives for the window's lifetime
    /// (and stays a live binding on platforms with no native menu).
    _midi_ctrl: MidiController,
    /// `--output-file` capture sink, owned by the handler so the
    /// Linux `WillClose` path can finalize it before `_exit(0)`. On
    /// non-Linux the handler's Drop runs naturally and
    /// `CaptureSink::Drop` joins the writer thread to flush the WAV
    /// header.
    #[cfg(feature = "playback")]
    _capture: Option<crate::playback::CaptureSink>,
}

impl<P: PluginExport + 'static> WindowHandler for StandaloneHandler<P>
where
    P::Params: 'static,
{
    fn on_frame(&mut self, window: &mut Window) {
        // Editor drives its own frame loop inside its child window;
        // the standalone outer handler does two things here:
        //  (1) honour pending resize requests posted by the editor
        //      through the `request_resize` closure;
        //  (2) poll the OS window size and forward user-driven
        //      edge drags to `editor.set_size`. baseview-truce
        //      0.1.1-truce.6 only emits `WindowEvent::Resized` for
        //      DPI changes, never for user drags, so detection
        //      lives here.
        let packed = self.pending_resize.swap(0, Ordering::Acquire);
        if packed != 0 {
            let (w, h) = unpack_size(packed);
            if (w, h) != self.current_size && w > 0 && h > 0 {
                window.resize(baseview::Size::new(f64::from(w), f64::from(h)));
                self.current_size = (w, h);
                self.editor.set_size(w, h);
            }
        }
        // OS-driven user resize poll (macOS only - other platforms
        // emit `WindowEvent::Resized` for user drags via `on_event`).
        #[cfg(target_os = "macos")]
        if let RwhHandle::AppKit(h) = window.raw_window_handle()
            && let Some(os_size) =
                unsafe { crate::windowed_macos::content_logical_size(h.ns_window) }
            && os_size != self.current_size
            && os_size.0 > 0
            && os_size.1 > 0
        {
            self.current_size = os_size;
            self.editor.set_size(os_size.0, os_size.1);
        }
    }

    // The Linux `_exit` extern lives inline with its single caller
    // (see comment block below) so the rationale doesn't get orphaned
    // from the API name; hence the function-level allow.
    #[allow(clippy::items_after_statements)]
    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        // OS-driven resize (user dragged the window edge): forward to
        // the editor so the child surface follows the outer frame.
        // `current_size` suppresses the round-trip when the resize
        // originated from our own `request_resize` path - in that
        // case `on_frame` already called `editor.set_size`.
        if let Event::Window(baseview::WindowEvent::Resized(info)) = &event {
            let phys = info.physical_size();
            let scale = info.scale();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (lw, lh) = if scale > 0.0 {
                (
                    (f64::from(phys.width) / scale).round() as u32,
                    (f64::from(phys.height) / scale).round() as u32,
                )
            } else {
                (phys.width, phys.height)
            };
            if (lw, lh) != self.current_size && lw > 0 && lh > 0 {
                self.current_size = (lw, lh);
                self.editor.set_size(lw, lh);
            }
        }

        // On Linux X11 + NVIDIA, letting baseview unwind the parent
        // window normally crashes inside `XCloseDisplay` - the
        // driver's Xlib extension cleanup callback segfaults during
        // teardown of the wgpu-bearing child window thread, even
        // when the wgpu surface/device/instance themselves drop
        // cleanly. The standalone has no clean-shutdown invariants
        // we care about (audio is a passthrough; persistent state
        // is saved on Ctrl-S, not at exit), so when the user closes
        // the window we bypass Drop / atexit entirely via `_exit`.
        // The OS reclaims the audio FDs, X handles, and the wgpu
        // child thread - no driver teardown ever runs.
        //
        // The one piece of state we *do* finalize explicitly is the
        // `--output-file` capture sink: skipping its writer-thread
        // join would leave the WAV header un-rewritten and the file
        // truncated. We take it here and call `finalize` (signals
        // shutdown + joins the writer) before `_exit`.
        #[cfg(target_os = "linux")]
        if matches!(event, Event::Window(baseview::WindowEvent::WillClose)) {
            vlog!("Goodbye!");
            // The `_capture` prefix marks the field as "owned for Drop"
            // on macOS / Windows, but on the Linux `_exit` path we *do*
            // read it (to finalize the WAV header before bypassing
            // Drop). Allow the leading-underscore access at this site
            // rather than renaming the field - the Drop-only semantics
            // on the other platforms are still the dominant case.
            #[cfg(feature = "playback")]
            #[allow(clippy::used_underscore_binding)]
            if let Some(capture) = self._capture.take() {
                capture.finalize();
            }
            // The `_exit` extern is declared next to its single caller
            // so the comment block above stays adjacent to the API
            // it's documenting; hoisting it would orphan the rationale
            // from the call. Hence the function-scoped allow on the
            // outer `on_event`.
            unsafe extern "C" {
                fn _exit(status: i32) -> !;
            }
            unsafe { _exit(0) };
        }

        match event {
            Event::Keyboard(kb) => self.handle_keyboard(&kb),
            _ => EventStatus::Ignored,
        }
    }
}

impl<P: PluginExport + 'static> StandaloneHandler<P>
where
    P::Params: 'static,
{
    fn handle_keyboard(&mut self, kb: &keyboard_types::KeyboardEvent) -> EventStatus {
        // Ctrl-S / Cmd-S → save state
        if kb.state == KeyState::Down && kb.code == Code::KeyS && is_mod_pressed(kb.modifiers) {
            self.save_state_via_picker();
            return EventStatus::Captured;
        }

        // SPACE → transport play/stop (on keydown only; ignore repeats).
        if kb.state == KeyState::Down && kb.code == Code::Space {
            self.transport.toggle_playing();
            vlog!(
                "transport: {}",
                if self.transport.is_playing() {
                    "playing"
                } else {
                    "stopped"
                }
            );
            return EventStatus::Captured;
        }

        // Cmd+I (macOS) / Ctrl+I (Linux / Windows) → toggle mic
        // input (effects only). First press on macOS triggers the
        // system permission dialog; subsequent toggles don't
        // re-prompt. On macOS the NSMenuItem accelerator usually
        // dispatches this before baseview sees the event - the
        // handler below is the only path on Windows / Linux (Win32
        // menu accelerators need an HACCEL table baseview doesn't
        // expose) and a guard on macOS. Capture both Down and Up
        // so the note-handler below never sees a stray modifier+I
        // Up that would emit a NoteOff for a note we never played.
        if kb.code == Code::KeyI && self.is_effect && is_mod_pressed(kb.modifiers) {
            if kb.state == KeyState::Down {
                let want = !self.input_ctrl.is_enabled();
                self.input_ctrl.set_enabled(want);
                vlog!("mic: {} (request)", if want { "ON" } else { "OFF" });
            }
            return EventStatus::Captured;
        }

        // Cmd+O (macOS) / Ctrl+O (Linux / Windows) → toggle audio
        // output (mute / unmute). Bare `O` is reserved for the
        // QWERTY note keyboard (C#4 by default), so a modifier is
        // required.
        //
        // On macOS the NSMenuItem accelerator dispatches this
        // before baseview sees the event, so the handler below is
        // mainly a guard. On Windows / Linux it's the only path:
        // Win32 menu accelerators need an HACCEL table that
        // baseview doesn't expose. Capture both Down and Up so the
        // note-handler below never sees a stray modifier+O Up that
        // would emit a NoteOff for a note we never played.
        if kb.code == Code::KeyO && is_mod_pressed(kb.modifiers) {
            if kb.state == KeyState::Down {
                let want = !self.output_ctrl.is_enabled();
                self.output_ctrl.set_enabled(want);
                vlog!("output: {} (request)", if want { "ON" } else { "OFF" });
            }
            return EventStatus::Captured;
        }

        if kb.state == KeyState::Down
            && let Some(shift) = keyboard::code_to_octave_shift(kb.code)
        {
            self.octave_offset = (self.octave_offset + shift).clamp(-3, 3);
            return EventStatus::Captured;
        }

        if let Some(note) = keyboard::code_to_midi_note(kb.code, self.octave_offset) {
            let body = match kb.state {
                // Software keyboard fires NoteOn at 80% velocity
                // (102/127); a held key has no real velocity.
                KeyState::Down => EventBody::NoteOn {
                    group: 0,
                    channel: 0,
                    note,
                    velocity: 102,
                },
                KeyState::Up => EventBody::NoteOff {
                    group: 0,
                    channel: 0,
                    note,
                    velocity: 0,
                },
            };
            // `force_push` drops the oldest event on overflow - see
            // audio.rs for the rationale (audio thread is the only
            // consumer; dropping ancient events beats mutex contention).
            let _ = self.pending.force_push(MidiEvent { body });
            return EventStatus::Captured;
        }
        EventStatus::Ignored
    }

    /// Snapshot the plugin (params + custom state) and write it to a
    /// user-picked path. Same envelope every other host format
    /// produces, so the resulting `.pluginstate` file round-trips
    /// into CLAP / VST3 / AU / a future
    /// `--state foo.pluginstate` standalone launch.
    ///
    /// On macOS / Windows the path comes from a native save
    /// dialog; on Linux we fall back to a default
    /// `<data_local_dir>/truce/<slug>/quicksave-<ts>.pluginstate` path
    /// because `rfd`'s Linux backend (xdg-portal) drags
    /// `wayland-sys` into the dep tree, which would force every
    /// truce-using project on a typical Linux dev machine to
    /// install Wayland system headers just to `cargo check`.
    /// Linux gets a real picker once we find a wayland-free
    /// backend (or once that dep tree thins out).
    fn save_state_via_picker(&self) {
        let Ok(plugin) = self.plugin.lock() else {
            eprintln!("could not lock plugin to save state");
            return;
        };
        let blob = truce_core::state::snapshot_plugin(&*plugin);
        let param_count = plugin.params().param_infos().len();
        // Drop the plugin lock before we open the dialog - the
        // audio thread acquires this mutex on every block, and a
        // few-second dialog wait is enough to glitch playback.
        drop(plugin);

        let plugin_slug = truce_core::slugify(P::info().name);
        let Some(path) = pick_save_path::<P>(&plugin_slug) else {
            return; // user cancelled, or no fallback dir on Linux
        };
        match std::fs::write(&path, &blob) {
            Ok(()) => vlog!(
                "state saved: {} ({param_count} params, {} bytes)",
                path.display(),
                blob.len(),
            ),
            Err(e) => eprintln!("write {}: {e}", path.display()),
        }
    }
}

/// Plugin-name → filesystem-friendly slug. Lowercase, ASCII
/// Resolve a destination path for Cmd-S. Native dialog on macOS /
/// Windows; default-path fallback on Linux. Returns `None` if the
/// user cancels (native picker) or if `data_local_dir` /
/// `mkdir -p` fails (Linux fallback).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn pick_save_path<P: PluginExport>(plugin_slug: &str) -> Option<std::path::PathBuf> {
    // Default to <data_local_dir>/truce/<slug>/ if it exists,
    // otherwise the home dir. User can navigate elsewhere from the
    // dialog. `<plugin>.pluginstate` is the suggested filename.
    let initial_dir = dirs::data_local_dir()
        .map(|d| d.join("truce").join(plugin_slug))
        .filter(|p| p.exists())
        .or_else(dirs::home_dir);
    let mut dialog = rfd::FileDialog::new()
        .set_title(format!("Save state for {}", P::info().name))
        .add_filter(".pluginstate file", &["pluginstate"])
        .set_file_name(format!("{plugin_slug}.pluginstate"));
    if let Some(dir) = initial_dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
// `P` unused on this branch (no native picker), but kept for signature
// parity with the macOS/Windows variant so the call site's turbofish works.
#[allow(clippy::extra_unused_type_parameters)]
fn pick_save_path<P: PluginExport>(plugin_slug: &str) -> Option<std::path::PathBuf> {
    let dir = dirs::data_local_dir()?.join("truce").join(plugin_slug);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("mkdir {}: {e}", dir.display());
        return None;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let path = dir.join(format!("quicksave-{ts}.pluginstate"));
    eprintln!(
        "native save dialog not yet wired on Linux - \
         saving to {}",
        path.display()
    );
    Some(path)
}

/// Pack `(width, height)` into a single `u64` for the
/// `pending_resize` `AtomicU64` handoff between the editor closure
/// and the outer `StandaloneHandler`. Sentinel value `0` (both
/// halves zero) means "no resize pending."
#[inline]
fn pack_size(size: (u32, u32)) -> u64 {
    (u64::from(size.0) << 32) | u64::from(size.1)
}

/// Inverse of `pack_size`.
#[inline]
fn unpack_size(packed: u64) -> (u32, u32) {
    #[allow(clippy::cast_possible_truncation)]
    {
        ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
    }
}

/// macOS uses Cmd (`meta`); Linux/Windows use Ctrl.
fn is_mod_pressed(mods: Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        mods.contains(Modifiers::META)
    } else {
        mods.contains(Modifiers::CONTROL)
    }
}

/// Build a minimal `PluginContext` that routes parameter reads /
/// writes / meter reads through the live plugin instance. Transport
/// closure reads from the shared `Transport` the audio thread writes.
fn synthesize_editor_context<P: PluginExport>(
    plugin: &Arc<Mutex<P>>,
    transport: &Transport,
    pending_resize: Arc<AtomicU64>,
) -> PluginContext
where
    P::Params: 'static,
{
    // Poison-tolerant: if the audio thread panicked, the editor
    // context still needs the params Arc to function. Recovering
    // the inner guard is safe because params_arc only clones an
    // Arc<P::Params> - it doesn't read mutable state.
    let params: Arc<P::Params> = plugin
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .params_arc();
    let transport_read = transport.clone();

    let params_read = Arc::clone(&params);
    let params_write = Arc::clone(&params);
    let params_plain = Arc::clone(&params);
    let params_format = Arc::clone(&params);
    let params_for_ctx = Arc::clone(&params);
    let plugin_meter = Arc::clone(plugin);
    let plugin_save = Arc::clone(plugin);
    let plugin_load = Arc::clone(plugin);

    PluginContext::from_closures(
        ClosureBridge {
            begin_edit: Box::new(|_id| {}),
            set_param: Box::new(move |id, norm| {
                params_write.set_normalized(id, norm);
            }),
            end_edit: Box::new(|_id| {}),
            request_resize: Box::new(move |w, h| {
                if w == 0 || h == 0 {
                    return false;
                }
                pending_resize.store(pack_size((w, h)), Ordering::Release);
                true
            }),
            get_param: Box::new(move |id| params_read.get_normalized(id).unwrap_or(0.0)),
            get_param_plain: Box::new(move |id| params_plain.get_plain(id).unwrap_or(0.0)),
            format_param: Box::new(move |id| {
                let value = params_format.get_plain(id).unwrap_or(0.0);
                params_format.format_value(id, value).unwrap_or_default()
            }),
            get_meter: Box::new(move |id| plugin_meter.try_lock().map_or(0.0, |p| p.get_meter(id))),
            get_state: Box::new(move || {
                plugin_save
                    .try_lock()
                    .ok()
                    .map(|p| p.save_state())
                    .unwrap_or_default()
            }),
            set_state: Box::new(move |bytes| {
                if let Ok(mut p) = plugin_load.try_lock()
                    && let Err(e) = p.load_state(&bytes)
                {
                    eprintln!("truce-standalone: load_state failed: {e}");
                }
            }),
            transport: Box::new(move || Some(transport_read.snapshot())),
        },
        params_for_ctx,
    )
}
