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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
use truce_core::tasks::AnyTaskSpawner;
use truce_params::Params;

use crate::audio::{self, InputController, MidiEvent, OutputController};
use crate::cli::Options;
use crate::keyboard;
use crate::midi::{MidiController, MidiInputThread};
use crate::transport::Transport;
use crate::vlog;

fn category_label(category: PluginCategory) -> &'static str {
    match category {
        PluginCategory::Effect => "effect",
        PluginCategory::Instrument => "instrument",
        PluginCategory::NoteEffect => "midi effect",
        PluginCategory::Analyzer => "analyzer",
        PluginCategory::Tool => "tool",
    }
}

/// Run the plugin with a window. Blocks until the window closes.
///
/// # Panics
///
/// Panics if the host platform reports a raw-window-handle variant
/// the editor backends don't support, or if the audio start /
/// transport / MIDI threads return a fatal error that has no
/// recovery path. Plugin-side panics propagate through the poisoned
/// mutex unchanged.
// Linear top-level orchestration: start audio/MIDI/transport, open the
// window, pump the event loop, tear down. Splitting it further would
// just thread the same locals through helpers without aiding clarity.
#[allow(clippy::too_many_lines)]
pub fn run<P: PluginExport>(opts: &Options)
where
    P::Params: 'static,
{
    vlog!("Plugin: {}", P::info().name);
    vlog!("Category: {}", category_label(P::info().category));

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

    let (midi_thread, midi_ctrl) = MidiInputThread::start(
        opts,
        usize::from(P::info().midi_input_ports),
        Arc::clone(&audio_handles.pending),
    );

    let editor: Option<Box<dyn Editor>> = {
        // The editor is built through the lock-free builder, not the
        // plugin instance, so lock only long enough to grab the builder
        // and param `Arc` then release before constructing. Recover from
        // a poisoned plugin mutex (audio thread panicked while holding
        // the lock) instead of cascading the panic through the UI thread.
        let (build, params) = {
            let plugin = audio_handles
                .plugin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (plugin.editor_builder(), plugin.params_arc())
        };
        build(params)
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
    // A resizable editor that opts out of maximize gets the WM
    // maximize affordance stripped (per-OS blocks below). Read once
    // here for the same borrow reason as `editor_can_resize` - the
    // cfg blocks run while `editor` is otherwise tied up. Only
    // meaningful when `editor_can_resize`; a non-resizable editor is
    // already pinned to a fixed size.
    let editor_can_maximize = editor.can_maximize();
    // Constraint snapshot for the outer-frame limits
    // (`windowed_windows::install_size_limits` /
    // `windowed_macos::install_content_limits`); read here for the
    // same borrow reason as the flags above.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let editor_limits = (editor.min_size(), editor.max_size(), editor.aspect_ratio());

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
    // QWERTY-keyboard-to-MIDI is opt-in (off by default). One shared flag
    // backs the key handler's gate, the Cmd/Ctrl+K toggle, and the
    // Settings-menu item, so all three read and write the same state.
    let qwerty_enabled = Arc::new(AtomicBool::new(opts.qwerty_midi));
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

    // Owned copy for the `move` editor closure - `opts` is a borrow
    // that can't escape into it.
    let presets_dir = opts.presets_dir.clone();
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
        let preset_ctrl =
            crate::presets::PresetController::new::<P>(Arc::clone(&plugin), presets_dir.clone());

        // The plugin's bus layouts as (in, out) channel counts, for the
        // Bus Layout menu (built only when there's more than one). Channel
        // counts are tiny; clamp the u32->u16 cast rather than truncate.
        // Only macOS and Windows carry the native menu bar that consumes it;
        // X11 has no menu, so gate the binding to avoid an unused warning.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let bus_layouts: Vec<(u16, u16)> = {
            let c16 = |c: u32| u16::try_from(c).unwrap_or(u16::MAX);
            P::bus_layouts()
                .iter()
                .map(|l| {
                    (
                        c16(l.total_input_channels()),
                        c16(l.total_output_channels()),
                    )
                })
                .collect()
        };

        #[cfg(target_os = "macos")]
        {
            crate::menu_macos::install(
                P::info().name,
                is_effect,
                channels,
                &input_ctrl,
                &output_ctrl,
                &midi_ctrl,
                &preset_ctrl,
                &qwerty_enabled,
                &bus_layouts,
            );
        }

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
                preset_ctrl.clone(),
                qwerty_enabled.clone(),
                bus_layouts.clone(),
            );
            // Fixed-size editors get the window locked to a
            // close-only frame so maximizing / dragging doesn't
            // stretch the child surface. Resizable editors keep
            // the full Windows non-client frame; the `Resized`
            // event flows back to `editor.set_size` via `on_event`.
            // Linux equivalent: `windowed_x11::pin_size` below.
            if editor_can_resize {
                if !editor_can_maximize {
                    // Resizable but maximize opted out: keep the resize
                    // border + minimize, drop only the maximize box so the
                    // window can't jump past the editor's max_size.
                    crate::windowed_windows::disable_maximize(h.hwnd);
                }
                // Enforce the editor's min / max / aspect on the outer
                // frame itself (WM_GETMINMAXINFO / WM_SIZING): backends
                // whose `set_size` accepts any size verbatim (egui /
                // iced / Slint letterbox instead of clamping) otherwise
                // let the window shrink below min or grow past max.
                let (emin, emax, easpect) = editor_limits;
                crate::windowed_windows::install_size_limits(h.hwnd, emin, emax, easpect);
            } else {
                crate::windowed_windows::lock_window(h.hwnd);
                // The cleared sizing border only stops interactive
                // resizes; programmatic `SetWindowPos` (scripting,
                // automation tools) bypasses window styles. Pin via
                // the min-max path too, at the editor's natural size.
                crate::windowed_windows::install_size_limits(h.hwnd, (lw, lh), (lw, lh), None);
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

        // Linux: a resizable editor that opts out of maximize gets the
        // Motif maximize function stripped so the WM drops the maximize
        // button / double-click-titlebar maximize. Edge-drag resize
        // (clamped by `set_resize_hints` on the first `Resized`) stays.
        #[cfg(target_os = "linux")]
        if editor_can_resize
            && !editor_can_maximize
            && let RwhHandle::Xlib(h) = window.raw_window_handle()
        {
            crate::windowed_x11::disable_maximize(window.raw_display_handle(), &h);
        }

        // Linux: paint the outer window black so any area the editor
        // child doesn't cover reads as opaque black instead of
        // glitched server memory. Matters when a resizable editor is
        // maximized past its max bounds (the WM ignores max size hints
        // in the maximized state), leaving a margin around the clamped
        // child; harmless for pinned fixed-size editors, which never
        // expose a margin. Set unconditionally - it's a one-time
        // persistent attribute the server fills from on every resize.
        #[cfg(target_os = "linux")]
        if let RwhHandle::Xlib(h) = window.raw_window_handle() {
            crate::windowed_x11::set_background_black(window.raw_display_handle(), &h);
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
            // Strip zoom + native fullscreen when the editor opts out
            // of maximize, after the resizable bit is set (the zoom
            // button only becomes active on a resizable window).
            if !editor_can_maximize {
                // SAFETY: same live `ns_window`, same main thread.
                unsafe { crate::windowed_macos::disable_zoom(h.ns_window) };
            }
            // Enforce the editor's min / max / aspect on the frame
            // itself, mirroring Windows `install_size_limits`: backends
            // whose `set_size` accepts any size verbatim (egui / iced /
            // Slint letterbox instead of clamping) otherwise let drags
            // or zoom take the window out of bounds.
            let (emin, emax, easpect) = editor_limits;
            // SAFETY: same live `ns_window`, same main thread.
            unsafe {
                crate::windowed_macos::install_content_limits(h.ns_window, emin, emax, easpect);
            }
        }

        // macOS: a fixed-size editor's window must stay at the editor's
        // geometry. baseview leaves out `Resizable` (no edge-drag), but
        // zoom / double-click-titlebar can still grow the window past the
        // child, exposing an unpainted margin. Pin content min == max so
        // every resize path clamps to the editor. Linux: `pin_size`;
        // Windows: `lock_window`.
        #[cfg(target_os = "macos")]
        if !editor_can_resize && let RwhHandle::AppKit(h) = window.raw_window_handle() {
            // SAFETY: live `ns_window`, main thread, baseview init done.
            unsafe { crate::windowed_macos::pin_content_size(h.ns_window, lw, lh) };
        }

        let ctx = synthesize_editor_context::<P>(&plugin, &transport, Arc::clone(&pending_resize));
        // The standalone owns a real top-level window and should honor
        // the desktop scale (Xft.dpi on Linux); plugins leave the default
        // and drive scale from the host instead. See
        // `truce_gui::platform::editor_window_scale`.
        editor.set_uses_system_scale(true);
        editor.open(truce_parent, ctx);

        // After `editor.open()` reparents baseview's child under the
        // standalone's NSView, give the child flexible margins so
        // AppKit keeps it centred (not stretched) when the NSWindow
        // grows past the editor; `on_frame`'s `layout_child_centered`
        // then drives its actual size. We pass `h.ns_view` (baseview's
        // standalone view) rather than `h.ns_window`:
        // `Window::open_blocking` doesn't run `setContentView:` on the
        // NSWindow until *after* this build closure returns, so
        // `[ns_window contentView]` here is the NSWindow's default
        // vanilla view (with no subviews). baseview's view, however,
        // is already the parent of the editor's child by the time
        // `editor.open()` returns.
        #[cfg(target_os = "macos")]
        if editor_can_resize && let RwhHandle::AppKit(h) = window.raw_window_handle() {
            // SAFETY: `editor.open()` just finished embedding the
            // child view; we're on the main thread.
            unsafe { crate::windowed_macos::install_subview_centering(h.ns_view) };
        }

        StandaloneHandler {
            editor,
            pending_resize,
            current_size: (lw, lh),
            #[cfg(target_os = "macos")]
            editor_reports_size: true,
            #[cfg(target_os = "macos")]
            editor_min_max: (editor_limits.0, editor_limits.1),
            #[cfg(target_os = "linux")]
            size_hints_scale: 0.0,
            _plugin: plugin,
            pending,
            transport,
            input_ctrl,
            output_ctrl,
            is_effect,
            qwerty_enabled: qwerty_enabled.clone(),
            octave_offset: 0,
            presets: preset_ctrl,
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
    /// Whether the editor reports its rendered size through
    /// `Editor::size` after a `set_size` (true for builtin / egui / iced
    /// / slint). Vizia's `set_size` is a no-op returning false; its child
    /// fills the window through baseview instead. macOS reads this each
    /// frame to pick the child's reconcile size (editor size vs window
    /// size). Updated wherever `set_size` is called.
    #[cfg(target_os = "macos")]
    editor_reports_size: bool,
    /// Editor min / max snapshot for the per-frame out-of-bounds check.
    /// `AppKit`'s `contentMinSize` / `contentMaxSize` only bound drags
    /// and zoom; a programmatic `setFrame:` bypasses them, so the
    /// `on_frame` poll snaps such a frame back (the counterpart of the
    /// Windows `WM_GETMINMAXINFO` subclass, which covers programmatic
    /// resizes natively).
    #[cfg(target_os = "macos")]
    editor_min_max: ((u32, u32), (u32, u32)),
    /// Scale factor the X11 WM min/max size hints were last computed
    /// at, or `0.0` if they haven't been set yet. The editor's
    /// min/max bounds are logical, so the physical-pixel hints the WM
    /// enforces have to be recomputed whenever the backing scale moves
    /// (e.g. dragging across monitors). Linux-only; unused elsewhere.
    #[cfg(target_os = "linux")]
    size_hints_scale: f64,
    /// Held for the window's lifetime so the plugin outlives the
    /// audio stream and the preset controller's clones; not read
    /// directly (preset actions go through `presets`).
    _plugin: Arc<Mutex<P>>,
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
    /// QWERTY-keyboard-to-MIDI, off by default. Shared with the
    /// Settings menu item and flipped by Cmd/Ctrl+K; the note handler
    /// only plays keys when this is set.
    qwerty_enabled: Arc<AtomicBool>,
    octave_offset: i8,
    /// Preset library handle - Save / Save As keyboard shortcuts.
    presets: crate::presets::PresetController,
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

/// Resize the outer standalone window so its client area becomes
/// `(w, h)` logical points. On Windows the height is padded by the
/// menu-bar band first: baseview's `Window::resize` is menu-blind and
/// would otherwise clip the editor child by the menu height on every
/// resize (see `crate::windowed_windows::menu_reserve_logical`).
/// Every platform then goes through baseview's deferred resize, which
/// is the only re-entrancy-safe way to resize from inside an event
/// handler.
fn resize_outer_window(window: &mut Window, w: u32, h: u32) {
    #[cfg(target_os = "windows")]
    let h = h + if let RwhHandle::Win32(handle) = window.raw_window_handle() {
        crate::windowed_windows::menu_reserve_logical(handle.hwnd)
    } else {
        0
    };
    window.resize(baseview::Size::new(f64::from(w), f64::from(h)));
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
                resize_outer_window(window, w, h);
                self.current_size = (w, h);
                let accepted = self.editor.set_size(w, h);
                #[cfg(target_os = "macos")]
                {
                    self.editor_reports_size = accepted;
                }
                #[cfg(not(target_os = "macos"))]
                let _ = accepted;
            }
        }
        // macOS: forward OS-driven resizes the editor missed and re-pin
        // the child view's frame every frame.
        #[cfg(target_os = "macos")]
        if let RwhHandle::AppKit(h) = window.raw_window_handle()
            && let Some(os_size) =
                unsafe { crate::windowed_macos::content_logical_size(h.ns_window) }
            && os_size.0 > 0
            && os_size.1 > 0
        {
            // A programmatic `setFrame:` bypasses AppKit's
            // `contentMinSize` / `contentMaxSize`; snap an out-of-bounds
            // frame back to the clamped size (deferred via baseview's
            // resize, so it lands next frame). In-bounds sizes are a
            // no-op, so the correction can't loop. The editor is
            // forwarded the clamped size below either way, so it never
            // renders out of bounds.
            let ((min_w, min_h), (max_w, max_h)) = self.editor_min_max;
            let os_size = {
                // Guard `clamp`'s min <= max precondition: an editor
                // whose reported max is below its min (inconsistent
                // bounds) would otherwise panic the host. Floor the max
                // at the min so a bad editor pins to its min size
                // instead of crashing.
                let (min_w, min_h) = (min_w.max(1), min_h.max(1));
                let clamped = (
                    os_size.0.clamp(min_w, max_w.max(min_w)),
                    os_size.1.clamp(min_h, max_h.max(min_h)),
                );
                if clamped != os_size {
                    resize_outer_window(window, clamped.0, clamped.1);
                }
                clamped
            };
            // Some resizes arrive as a `Resized` event (handled in
            // `on_event`); others only surface by polling the content
            // size here. Forward the ones `on_event` didn't see.
            if os_size != self.current_size {
                self.current_size = os_size;
                self.editor_reports_size = self.editor.set_size(os_size.0, os_size.1);
            }
            // Re-pin the child frame every frame. The autoresize centering
            // mask moves the (old-size) child toward the centre as the
            // window grows, and the editor's own child-window resize then
            // grows it in place - keeping that shifted origin; a
            // `Resized`-event resize never corrects it either. Left as-is
            // the child overhangs one edge and exposes an unpainted margin
            // on the other. Re-centre at the editor's rendered size
            // (window size for vizia, whose child fills via baseview).
            // `layout_child_centered` no-ops once the frame matches.
            if self.editor.can_resize() {
                let (cw, ch) = if self.editor_reports_size {
                    self.editor.size()
                } else {
                    os_size
                };
                if cw > 0 && ch > 0 {
                    // SAFETY: `h.ns_view` is the live baseview view; we're
                    // on the main thread and the child is settled.
                    unsafe {
                        crate::windowed_macos::layout_child_centered(
                            h.ns_view, cw, ch, os_size.0, os_size.1,
                        );
                    }
                }
            }
        }

        // Re-centre the editor child within the outer window when the
        // outer is larger than the (clamped) editor - the maximize /
        // past-max-size case. Linux only: Windows snaps the outer back
        // to the editor size on every drag (the `not(linux)` branch in
        // `on_event`), so its window is never larger than the editor;
        // macOS centres via the autoresize layout installed at open.
        // Gated on `can_resize` because fixed-size editors are pinned
        // to their exact geometry (`pin_size`) and never leave a
        // margin. `center_child` no-ops when nothing moved, so calling
        // it each frame is cheap.
        #[cfg(target_os = "linux")]
        if self.editor.can_resize()
            && let RwhHandle::Xlib(h) = window.raw_window_handle()
        {
            crate::windowed_x11::center_child(window.raw_display_handle(), &h);
        }
    }

    // The Linux `_exit` extern lives inline with its single caller
    // (see comment block below) so the rationale doesn't get orphaned
    // from the API name; hence the function-level allow.
    #[allow(clippy::items_after_statements)]
    fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus {
        // OS-driven resize (user dragged the window edge): forward to
        // the editor so the child surface follows the outer frame.
        // `current_size` suppresses the round-trip when the resize
        // originated from our own `request_resize` path - in that
        // case `on_frame` already called `editor.set_size`.
        if let Event::Window(baseview::WindowEvent::Resized(info)) = &event {
            let phys = info.physical_size();
            let scale = info.scale();
            // Hand the editor's logical min/max bounds and cell-step
            // increment to the X11 WM as physical-pixel size hints so
            // it clamps interactive edge-drags to the editor's bounds
            // and snaps them to whole cells itself. This replaces the
            // old client-side snap-back (removed below for Linux) that
            // ran away by fighting the WM's resize grab. The bounds /
            // increment are logical and the hints physical, so
            // recompute whenever the backing scale moves;
            // `size_hints_scale` gates the redundant calls.
            // `scale` is written verbatim from `info.scale()`, never
            // through accumulating arithmetic, so bit-equality is the
            // right gate - an epsilon check would miss a legitimate
            // host scale change. Same rationale as
            // `EditorScale::take_change`.
            #[cfg(target_os = "linux")]
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::float_cmp
            )]
            if self.editor.can_resize()
                && scale > 0.0
                && scale != self.size_hints_scale
                && let RwhHandle::Xlib(h) = window.raw_window_handle()
            {
                let to_phys = |v: u32| -> u32 {
                    // `u32::MAX` is the trait's "unbounded" sentinel;
                    // map it to 0 so `set_resize_hints` omits the cap
                    // on that axis instead of overflowing the multiply.
                    if v == u32::MAX {
                        0
                    } else {
                        (f64::from(v) * scale).round() as u32
                    }
                };
                let (min_w, min_h) = self.editor.min_size();
                let (max_w, max_h) = self.editor.max_size();
                let (inc_w, inc_h) = self.editor.size_increment().unwrap_or((0, 0));
                crate::windowed_x11::set_resize_hints(
                    window.raw_display_handle(),
                    &h,
                    to_phys(min_w),
                    to_phys(min_h),
                    to_phys(max_w),
                    to_phys(max_h),
                    to_phys(inc_w),
                    to_phys(inc_h),
                    // Ratio is scale-independent - hand the raw editor
                    // pair to the WM so it constrains the drag itself
                    // instead of letting egui / iced / Slint letterbox.
                    self.editor.aspect_ratio(),
                );
                self.size_hints_scale = scale;
            }
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
                let accepted = self.editor.set_size(lw, lh);
                // macOS reconciles the child frame each frame off this.
                #[cfg(target_os = "macos")]
                {
                    self.editor_reports_size = accepted;
                }
                // Snap the outer standalone window to the editor's
                // post-clamp / post-snap size. truce-gui's
                // `BuiltinEditor` clamps to `min_cols` / `max_cols`,
                // so when the user drags past the editor's bounds
                // the editor stops growing but without this snap
                // the outer NSWindow would keep going - leaving
                // empty space around a clamped editor. egui / iced
                // / slint take the new size verbatim, so the snap
                // is a no-op for them. Vizia's `set_size` returns
                // `false`; we leave the outer alone in that case
                // so the autoresize cascade still works.
                //
                // NOT on Linux/X11: issuing `configure_window` from
                // inside the handling of the window's own
                // `ConfigureNotify` fights the WM's interactive
                // resize grab. The grid snaps to whole columns on
                // nearly every drag tick, so `(ew, eh) != (lw, lh)`
                // is true almost always and the echo fires
                // continuously; grab-rebaselining WMs (mutter, kwin)
                // fold each injected size into the grab and the
                // window runs away ("huge while dragging"). macOS
                // already lives with `set_size`-only here (the child
                // follows via `NSViewWidthSizable` autoresize); on
                // Linux the editor's own `on_frame` resizes the child
                // surface to the snapped size, so it still follows -
                // we just let the WM own the outer frame during the
                // drag and accept a thin margin at the clamp bounds.
                #[cfg(not(target_os = "linux"))]
                if accepted {
                    let (ew, eh) = self.editor.size();
                    if (ew, eh) != (lw, lh) && ew > 0 && eh > 0 {
                        self.current_size = (ew, eh);
                        resize_outer_window(window, ew, eh);
                    }
                }
                #[cfg(target_os = "linux")]
                let _ = accepted;
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
        // Ctrl/Cmd-S → quicksave; add Shift for Save As (name +
        // location, with overwrite confirmation where the OS panel
        // provides one).
        if kb.state == KeyState::Down && kb.code == Code::KeyS && is_mod_pressed(kb.modifiers) {
            if kb.modifiers.contains(Modifiers::SHIFT) {
                self.presets.save_as();
            } else {
                self.presets.save();
            }
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

        // Cmd+K (macOS) / Ctrl+K (Linux / Windows) → toggle the QWERTY
        // note keyboard. Bare `K` plays a note, so a modifier is required.
        // On macOS the NSMenuItem accelerator usually dispatches first;
        // on Windows / Linux this is the only path. Both Down and Up are
        // captured so a stray modifier+K Up never reaches the note handler.
        if kb.code == Code::KeyK && is_mod_pressed(kb.modifiers) {
            if kb.state == KeyState::Down {
                let want = !self.qwerty_enabled.load(Ordering::Relaxed);
                self.qwerty_enabled.store(want, Ordering::Relaxed);
                vlog!(
                    "computer keyboard: {} (request)",
                    if want { "ON" } else { "OFF" }
                );
            }
            return EventStatus::Captured;
        }

        // The QWERTY note keyboard (octave shifts + note keys) is opt-in;
        // when off, typing falls through untouched so it never surprises
        // the user with notes.
        if !self.qwerty_enabled.load(Ordering::Relaxed) {
            return EventStatus::Ignored;
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
            // The computer keyboard always plays the plugin's first
            // MIDI port; device input targets its mapped port.
            let _ = self.pending.force_push(MidiEvent { body, port: 0 });
            return EventStatus::Captured;
        }
        EventStatus::Ignored
    }
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
    let task_spawner: Option<AnyTaskSpawner> = plugin
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .task_spawner();
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
                // Blocking is bounded by the audio callback's lock
                // hold (a block's worth); a try_lock's empty fallback
                // silently kept stale editor state on a lost race.
                // Poisoned (audio thread panicked) degrades to empty.
                plugin_save
                    .lock()
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
    .with_tasks(task_spawner)
}
