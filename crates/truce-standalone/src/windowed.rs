//! Windowed standalone host.
//!
//! Opens an outer parentless baseview window and hosts the plugin's
//! own editor (obtained via `plugin.editor()`) as a child of it —
//! same contract CLAP / VST3 / AU follow. The plugin library is
//! unchanged; standalone is a "host" like any other.
//!
//! The outer window captures keyboard input so QWERTY keystrokes
//! can be translated into MIDI note events and `SPACE` / `S` /
//! `Z` / `X` hotkeys drive transport / state / octave-shift.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};
use keyboard_types::{Code, KeyState, Modifiers};
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle as RwhHandle};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_core::events::EventBody;
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_params::Params;

use crate::audio::{self, InputController, MidiEvent, OutputController};
use crate::cli::Options;
use crate::keyboard;
use crate::midi::MidiInputThread;
use crate::transport::Transport;

/// Run the plugin with a window. Blocks until the window closes.
pub fn run<P: PluginExport>(opts: &Options)
where
    P::Params: 'static,
{
    println!("=== truce standalone ===");
    println!("Plugin: {}", P::info().name);
    println!(
        "Category: {}",
        match P::info().category {
            PluginCategory::Effect => "effect",
            PluginCategory::Instrument => "instrument",
            PluginCategory::NoteEffect => "midi effect",
            PluginCategory::Analyzer => "analyzer",
            PluginCategory::Tool => "tool",
        }
    );

    let audio_handles = match audio::start_audio::<P>(opts) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // --state <path>: restore plugin state before opening the editor
    // so the editor reflects the loaded values on first paint.
    if let Some(path) = opts.state_path.as_ref() {
        match std::fs::read(path) {
            Ok(bytes) => {
                if let Ok(mut p) = audio_handles.plugin.lock() {
                    p.load_state(&bytes);
                    eprintln!("[truce-standalone] loaded state from {}", path.display());
                }
            }
            Err(e) => eprintln!(
                "[truce-standalone] failed to read state {}: {e}",
                path.display()
            ),
        }
    }

    let midi_thread = MidiInputThread::start(opts, Arc::clone(&audio_handles.pending));

    let editor: Option<Box<dyn Editor>> = {
        let mut plugin = audio_handles.plugin.lock().unwrap();
        plugin.editor()
    };
    let mut editor = match editor {
        Some(e) => e,
        None => {
            eprintln!("Plugin returned no editor — falling back to headless mode.");
            drop(audio_handles);
            crate::headless::run::<P>(opts);
            return;
        }
    };
    let (lw, lh) = editor.size();

    let window_opts = WindowOpenOptions {
        title: format!("{} — standalone", P::info().name),
        size: baseview::Size::new(lw as f64, lh as f64),
        scale: WindowScalePolicy::SystemScaleFactor,
    };

    let plugin = Arc::clone(&audio_handles.plugin);
    let pending = Arc::clone(&audio_handles.pending);
    let transport = audio_handles.transport.clone();

    // Both controllers are `Send + Sync` — the cpal streams they
    // wrap live on dedicated worker threads, not on `audio_handles`.
    let input_ctrl = audio_handles.input.clone();
    let output_ctrl = audio_handles.output.clone();
    let is_effect = audio_handles.is_effect;

    Window::open_blocking(window_opts, move |window| {
        let truce_parent = match window.raw_window_handle() {
            RwhHandle::AppKit(h) => RawWindowHandle::AppKit(h.ns_view),
            RwhHandle::Win32(h) => RawWindowHandle::Win32(h.hwnd),
            // `h.window` is `c_ulong` — u64 on 64-bit Linux, u32 on
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
        // The menu installs for every plugin category — the
        // Audio Output toggle and Output Device picker are
        // universally useful. The install path itself omits the
        // Mic Input toggle and Input Device picker for non-effects
        // (input is silent for instruments / analyzers without
        // input routing).
        #[cfg(target_os = "macos")]
        crate::menu_macos::install(
            P::info().name,
            is_effect,
            input_ctrl.clone(),
            output_ctrl.clone(),
        );

        // Windows: same idea, but the menu bar lives inside the
        // window's non-client area, so the install path also grows
        // the parent so the editor child keeps its requested size.
        // Must run BEFORE `editor.open()` below — the resize has to
        // settle before the editor's child window sizes itself.
        #[cfg(target_os = "windows")]
        if let RwhHandle::Win32(h) = window.raw_window_handle() {
            crate::menu_windows::install(
                h.hwnd,
                P::info().name,
                is_effect,
                input_ctrl.clone(),
                output_ctrl.clone(),
            );
        }

        let ctx = synthesize_editor_context::<P>(&plugin, &transport);
        editor.open(truce_parent, ctx);

        StandaloneHandler {
            _editor: editor,
            plugin,
            pending,
            transport,
            input_ctrl,
            output_ctrl,
            is_effect,
            octave_offset: 0,
            _midi_thread: midi_thread,
        }
    });

    drop(audio_handles);
    println!("Goodbye!");
}

struct StandaloneHandler<P: PluginExport + 'static>
where
    P::Params: 'static,
{
    _editor: Box<dyn Editor>,
    plugin: Arc<Mutex<P>>,
    pending: Arc<Mutex<Vec<MidiEvent>>>,
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
    _midi_thread: Option<MidiInputThread>,
}

impl<P: PluginExport + 'static> WindowHandler for StandaloneHandler<P>
where
    P::Params: 'static,
{
    fn on_frame(&mut self, _window: &mut Window) {
        // Editor drives its own frame loop inside its child window.
    }

    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        match event {
            Event::Keyboard(kb) => self.handle_keyboard(kb),
            _ => EventStatus::Ignored,
        }
    }
}

impl<P: PluginExport + 'static> StandaloneHandler<P>
where
    P::Params: 'static,
{
    fn handle_keyboard(&mut self, kb: keyboard_types::KeyboardEvent) -> EventStatus {
        // Ctrl-S / Cmd-S → save state
        if kb.state == KeyState::Down && kb.code == Code::KeyS && is_mod_pressed(&kb.modifiers) {
            self.save_state_to_default_path();
            return EventStatus::Captured;
        }

        // SPACE → transport play/stop (on keydown only; ignore repeats).
        if kb.state == KeyState::Down && kb.code == Code::Space {
            self.transport.toggle_playing();
            eprintln!(
                "[truce-standalone] transport: {}",
                if self.transport.is_playing() {
                    "playing"
                } else {
                    "stopped"
                }
            );
            return EventStatus::Captured;
        }

        // I → toggle mic input (only meaningful for effects).
        // First press on macOS triggers the system permission
        // dialog; subsequent presses toggle without prompting.
        // On Windows, Ctrl+I is also accepted to mirror the
        // native menu's accelerator hint (Win32 menu accelerators
        // need an HACCEL table + TranslateAccelerator in the
        // message loop, which baseview doesn't expose — so we
        // dispatch the modifier shortcut from here instead).
        if kb.state == KeyState::Down && kb.code == Code::KeyI && self.is_effect {
            let bare = !is_mod_pressed(&kb.modifiers);
            #[cfg(target_os = "windows")]
            let with_ctrl = kb.modifiers.contains(Modifiers::CONTROL);
            #[cfg(not(target_os = "windows"))]
            let with_ctrl = false;
            if bare || with_ctrl {
                let want = !self.input_ctrl.is_enabled();
                self.input_ctrl.set_enabled(want);
                eprintln!(
                    "[truce-standalone] mic: {} (request)",
                    if want { "ON" } else { "OFF" }
                );
                return EventStatus::Captured;
            }
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
        if kb.code == Code::KeyO && is_mod_pressed(&kb.modifiers) {
            if kb.state == KeyState::Down {
                let want = !self.output_ctrl.is_enabled();
                self.output_ctrl.set_enabled(want);
                eprintln!(
                    "[truce-standalone] output: {} (request)",
                    if want { "ON" } else { "OFF" }
                );
            }
            return EventStatus::Captured;
        }

        if kb.state == KeyState::Down {
            if let Some(shift) = keyboard::code_to_octave_shift(kb.code) {
                self.octave_offset = (self.octave_offset + shift).clamp(-3, 3);
                return EventStatus::Captured;
            }
        }

        if let Some(note) = keyboard::code_to_midi_note(kb.code, self.octave_offset) {
            let body = match kb.state {
                KeyState::Down => EventBody::NoteOn {
                    channel: 0,
                    note,
                    velocity: 0.8,
                },
                KeyState::Up => EventBody::NoteOff {
                    channel: 0,
                    note,
                    velocity: 0.0,
                },
            };
            if let Ok(mut events) = self.pending.lock() {
                events.push(MidiEvent { body });
            }
            return EventStatus::Captured;
        }
        EventStatus::Ignored
    }

    fn save_state_to_default_path(&self) {
        let Ok(plugin) = self.plugin.lock() else {
            return;
        };
        let Some(bytes) = plugin.save_state() else {
            eprintln!("[truce-standalone] plugin has no state to save");
            return;
        };
        let Some(dir) = dirs::data_local_dir() else {
            eprintln!("[truce-standalone] could not resolve local data dir");
            return;
        };
        let plugin_slug = P::info()
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>();
        let dir = dir.join("truce").join(&plugin_slug);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("[truce-standalone] mkdir {}: {e}", dir.display());
            return;
        }
        let ts = Instant::now().elapsed().as_secs();
        let path = dir.join(format!("quicksave-{ts}.state"));
        match std::fs::write(&path, &bytes) {
            Ok(()) => eprintln!("[truce-standalone] state saved: {}", path.display()),
            Err(e) => eprintln!("[truce-standalone] write {}: {e}", path.display()),
        }
    }
}

/// macOS uses Cmd (`meta`); Linux/Windows use Ctrl.
fn is_mod_pressed(mods: &Modifiers) -> bool {
    #[cfg(target_os = "macos")]
    return mods.contains(Modifiers::META);
    #[cfg(not(target_os = "macos"))]
    return mods.contains(Modifiers::CONTROL);
}

/// Build a minimal `EditorContext` that routes parameter reads /
/// writes / meter reads through the live plugin instance. Transport
/// closure reads from the shared `Transport` the audio thread writes.
fn synthesize_editor_context<P: PluginExport>(
    plugin: &Arc<Mutex<P>>,
    transport: &Transport,
) -> EditorContext
where
    P::Params: 'static,
{
    let params: Arc<P::Params> = plugin.lock().unwrap().params_arc();
    let transport_read = transport.clone();

    let params_read = Arc::clone(&params);
    let params_write = Arc::clone(&params);
    let params_plain = Arc::clone(&params);
    let params_format = Arc::clone(&params);
    let plugin_meter = Arc::clone(plugin);
    let plugin_save = Arc::clone(plugin);
    let plugin_load = Arc::clone(plugin);

    EditorContext {
        begin_edit: Arc::new(|_id| {}),
        set_param: Arc::new(move |id, norm| {
            params_write.set_normalized(id, norm);
        }),
        end_edit: Arc::new(|_id| {}),
        request_resize: Arc::new(|_w, _h| false),
        get_param: Arc::new(move |id| params_read.get_normalized(id).unwrap_or(0.0)),
        get_param_plain: Arc::new(move |id| params_plain.get_plain(id).unwrap_or(0.0)),
        format_param: Arc::new(move |id| {
            let value = params_format.get_plain(id).unwrap_or(0.0);
            params_format.format_value(id, value).unwrap_or_default()
        }),
        get_meter: Arc::new(move |id| {
            plugin_meter
                .try_lock()
                .map(|p| p.get_meter(id))
                .unwrap_or(0.0)
        }),
        get_state: Arc::new(move || {
            plugin_save
                .try_lock()
                .ok()
                .and_then(|p| p.save_state())
                .unwrap_or_default()
        }),
        set_state: Arc::new(move |bytes| {
            if let Ok(mut p) = plugin_load.try_lock() {
                p.load_state(&bytes);
            }
        }),
        transport: Arc::new(move || Some(transport_read.snapshot())),
    }
}
