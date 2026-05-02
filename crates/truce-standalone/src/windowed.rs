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
use crate::vlog;

/// Run the plugin with a window. Blocks until the window closes.
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

    let audio_handles = match audio::start_audio::<P>(opts) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // `--state <path>` was already applied inside `audio::start_audio`
    // — it loads BEFORE `snap_smoothers` so the editor + first audio
    // block see the restored values, not defaults ramping toward them.

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
    vlog!("Goodbye!");
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
        // On Linux X11 + NVIDIA, letting baseview unwind the parent
        // window normally crashes inside `XCloseDisplay` — the
        // driver's Xlib extension cleanup callback segfaults during
        // teardown of the wgpu-bearing child window thread, even
        // when the wgpu surface/device/instance themselves drop
        // cleanly. The standalone has no clean-shutdown invariants
        // we care about (audio is a passthrough; persistent state
        // is saved on Ctrl-S, not at exit), so when the user closes
        // the window we bypass Drop / atexit entirely via `_exit`.
        // The OS reclaims the audio FDs, X handles, and the wgpu
        // child thread — no driver teardown ever runs.
        #[cfg(target_os = "linux")]
        if matches!(event, Event::Window(baseview::WindowEvent::WillClose)) {
            vlog!("Goodbye!");
            unsafe extern "C" {
                fn _exit(status: i32) -> !;
            }
            unsafe { _exit(0) };
        }

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
        // dispatches this before baseview sees the event — the
        // handler below is the only path on Windows / Linux (Win32
        // menu accelerators need an HACCEL table baseview doesn't
        // expose) and a guard on macOS. Capture both Down and Up
        // so the note-handler below never sees a stray modifier+I
        // Up that would emit a NoteOff for a note we never played.
        if kb.code == Code::KeyI && self.is_effect && is_mod_pressed(&kb.modifiers) {
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
        if kb.code == Code::KeyO && is_mod_pressed(&kb.modifiers) {
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
        // Drop the plugin lock before we open the dialog — the
        // audio thread acquires this mutex on every block, and a
        // few-second dialog wait is enough to glitch playback.
        drop(plugin);

        let plugin_slug = slugify(P::info().name);
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
/// alphanumerics passed through, everything else collapsed to `-`.
fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

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
        "native save dialog not yet wired on Linux — \
         saving to {}",
        path.display()
    );
    Some(path)
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
