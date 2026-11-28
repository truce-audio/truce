//! A small sine synth whose editor is a web page, rendered by a native
//! webview (`wry`: `WKWebView` on macOS, `WebView2` on Windows) built as a
//! child of the host's parent view - the raw-window-handle editor path,
//! with no truce GUI backend involved.
//!
//! The page's slider edits the volume through `wry`'s IPC channel
//! (begin / set / end edit), and [`Editor::idle`] pushes host-side changes
//! (automation, another editor) back into the page.
//!
//! On macOS, `wry` needs the parent `NSView` to be in an `NSWindow` when it
//! builds the webview. If it isn't, the editor says so on screen (a red
//! panel) and in the log instead of letting `wry` panic, so a host test can
//! see which formats hand `open()` a windowless view.

use std::f64::consts::TAU;
use std::sync::Arc;
use truce::prelude::*;
use truce_core::editor::{PluginContext, RawWindowHandle};

use WebviewParamsParamId as P;

const WINDOW_W: u32 = 320;
const WINDOW_H: u32 = 200;

// --- Parameters ---

#[derive(Params)]
pub struct WebviewParams {
    #[param(name = "Volume", range = "linear(-60, 0)", default = -12.0, unit = "dB", smooth = "exp(5)")]
    pub volume: FloatParam,
}

// --- Plugin ---

/// Stateless descriptor; the DSP state is one [`Voice`].
pub struct WebviewSynth;

/// A mono sine voice: the last note held plays, with a short linear
/// attack and release so notes don't click.
pub struct Voice {
    sample_rate: f64,
    phase: f64,
    freq: f64,
    /// Envelope level, `0.0..=1.0`, and where it is heading.
    level: f64,
    target: f64,
    note: Option<u8>,
}

impl Default for Voice {
    fn default() -> Self {
        Voice {
            sample_rate: 48_000.0,
            phase: 0.0,
            freq: 440.0,
            level: 0.0,
            target: 0.0,
            note: None,
        }
    }
}

const ATTACK_SECS: f64 = 0.005;
const RELEASE_SECS: f64 = 0.15;

impl PluginLogic for WebviewSynth {
    type Params = WebviewParams;
    type DspState = Voice;

    fn bus_layouts() -> Vec<BusLayout> {
        BusLayout::stereo_and_mono_output()
    }

    fn reset(voice: &mut Voice, _params: &WebviewParams, config: &AudioConfig) {
        *voice = Voice {
            sample_rate: config.sample_rate,
            ..Voice::default()
        };
    }

    fn process(
        voice: &mut Voice,
        params: &WebviewParams,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let mut next_event = 0;
        let channels = buffer.num_output_channels();
        for i in 0..buffer.num_samples() {
            while let Some(event) = events.get(next_event) {
                if event.sample_offset as usize > i {
                    break;
                }
                match &event.body {
                    EventBody::NoteOn { note, .. } => {
                        voice.note = Some(*note);
                        voice.freq = 440.0 * 2.0_f64.powf((f64::from(*note) - 69.0) / 12.0);
                        voice.target = 1.0;
                    }
                    EventBody::NoteOff { note, .. } if voice.note == Some(*note) => {
                        voice.note = None;
                        voice.target = 0.0;
                    }
                    _ => {}
                }
                next_event += 1;
            }
            let rising = voice.target > voice.level;
            let step = 1.0 / (voice.sample_rate * if rising { ATTACK_SECS } else { RELEASE_SECS });
            voice.level = if rising {
                (voice.level + step).min(voice.target)
            } else {
                (voice.level - step).max(voice.target)
            };
            let out = (voice.phase * TAU).sin()
                * voice.level
                * f64::from(db_to_linear(params.volume.read()))
                * 0.5;
            voice.phase = (voice.phase + voice.freq / voice.sample_rate).fract();
            #[allow(clippy::cast_possible_truncation)]
            let out = out as f32;
            for ch in 0..channels {
                buffer.output(ch)[i] = out;
            }
        }
        ProcessStatus::Normal
    }

    fn editor(_params: Arc<WebviewParams>) -> Box<dyn Editor> {
        Box::new(WebviewEditor::default())
    }
}

/// The page. `window.ipc.postMessage` is `wry`'s channel back to Rust;
/// `setVolume` is how [`WebviewEditor::idle`] pushes values in;
/// `truceInitial` is the value when the page is built, so the first paint
/// is right without waiting for a tick; `idleRunning` marks that the format
/// drives `idle` at all (the status line a host test reads).
#[cfg(any(target_os = "macos", target_os = "windows"))]
const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><style>
  html, body { margin: 0; height: 100%; background: #1c1d22; color: #e4e6eb;
    font: 13px/1.4 ui-monospace, "SF Mono", Menlo, Consolas, monospace; overflow: hidden; }
  header { height: 30px; display: flex; align-items: center; padding: 0 10px;
    background: #2a2c33; font-weight: 600; letter-spacing: 0.08em; }
  main { padding: 26px 20px; display: grid; gap: 14px; }
  #value { font-size: 28px; text-align: center; }
  input { width: 100%; accent-color: #4e8df5; }
  .label { text-align: center; color: #9aa0aa; }
  .status { text-align: center; color: #6c7280; font-size: 11px; }
</style></head><body>
  <header>WEBVIEW SYNTH</header>
  <main>
    <div id="value">-</div>
    <input id="volume" type="range" min="0" max="1" step="0.001" value="0">
    <div class="label">Volume</div>
    <div id="idle" class="status">idle(): not called</div>
  </main>
<script>
  const slider = document.getElementById('volume');
  const value = document.getElementById('value');
  let dragging = false;
  slider.addEventListener('pointerdown', () => { dragging = true; window.ipc.postMessage('begin'); });
  slider.addEventListener('input', () => window.ipc.postMessage('set:' + slider.value));
  const end = () => { if (dragging) { dragging = false; window.ipc.postMessage('end'); } };
  slider.addEventListener('pointerup', end);
  slider.addEventListener('pointercancel', end);
  window.setVolume = (normalized, text) => {
    if (!dragging) slider.value = normalized;
    value.textContent = text;
  };
  window.idleRunning = () => { document.getElementById('idle').textContent = 'idle(): running'; };
  // The value when the page was built (see `build_webview`): shown at once,
  // without waiting for an `Editor::idle` tick.
  if (window.truceInitial) window.setVolume(...window.truceInitial);
</script></body></html>"#;

/// Lets the webview live in an `Editor` (which must be `Send`). Editors
/// are only ever used on the host's UI thread; nothing here crosses
/// threads.
struct UiThread<T>(T);
// SAFETY: see the type's doc - the value is created, used and dropped on
// the UI thread the editor runs on.
unsafe impl<T> Send for UiThread<T> {}

#[derive(Default)]
struct WebviewEditor {
    context: Option<PluginContext>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    webview: Option<UiThread<wry::WebView>>,
    /// The red "no window" panel, when `open` couldn't build the webview.
    #[cfg(target_os = "macos")]
    notice: Option<UiThread<objc2::rc::Retained<objc2_app_kit::NSTextField>>>,
    /// The last (normalized value, text) pushed into the page.
    shown: Option<(f64, String)>,
    /// `idle` calls since `open`, to re-mark the status line now and then.
    ticks: u64,
}

impl Editor for WebviewEditor {
    fn size(&self) -> (u32, u32) {
        (WINDOW_W, WINDOW_H)
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        self.shown = None;
        self.ticks = 0;
        #[cfg(target_os = "macos")]
        if let RawWindowHandle::AppKit(view) = parent
            && !macos::in_window(view)
        {
            eprintln!(
                "truce-example-webview: Editor::open got a parent NSView that is not in an \
                 NSWindow; wry can't build a webview there"
            );
            self.notice = macos::show_notice(
                view,
                "open(): the parent view has no window",
                WINDOW_W,
                WINDOW_H,
            )
            .map(UiThread);
            self.context = Some(context);
            return;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let initial = (
                context.bridge().get_param(P::Volume.into()),
                context.format_param(P::Volume),
            );
            match build_webview(parent, context.clone(), &initial) {
                Ok(w) => {
                    self.webview = Some(UiThread(w));
                    self.shown = Some(initial);
                }
                Err(e) => eprintln!("truce-example-webview: building the webview failed: {e}"),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = parent;
            eprintln!("truce-example-webview: no webview on this platform");
        }
        self.context = Some(context);
    }

    fn close(&mut self) {
        // Dropping the webview removes it from the parent view.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            self.webview = None;
        }
        #[cfg(target_os = "macos")]
        if let Some(n) = self.notice.take() {
            n.0.removeFromSuperview();
        }
        self.context = None;
    }

    /// Push the volume into the page whenever it changes on the host side,
    /// and mark the page's status line: `idle` is running.
    fn idle(&mut self, _dt: std::time::Duration) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let (Some(ctx), Some(webview)) = (&self.context, &self.webview) else {
                return;
            };
            // Every 30 ticks (twice a second at 60 Hz), not once: the first
            // ticks can come before the page has loaded, and the call is
            // idempotent.
            if self.ticks.is_multiple_of(30) {
                let _ = webview
                    .0
                    .evaluate_script("window.idleRunning && window.idleRunning()");
            }
            self.ticks += 1;
            let normalized = ctx.bridge().get_param(P::Volume.into());
            let text = ctx.format_param(P::Volume);
            // Exact compare on purpose: any change at all is worth a push.
            if self
                .shown
                .as_ref()
                .is_some_and(|(n, t)| n.to_bits() == normalized.to_bits() && *t == text)
            {
                return;
            }
            let script = format!("window.setVolume && window.setVolume({normalized}, {text:?})");
            if webview.0.evaluate_script(&script).is_ok() {
                self.shown = Some((normalized, text));
            }
        }
    }
}

/// Build the webview as a child of `parent`, sized to the editor, showing
/// `initial` (the volume's normalized value and its text) from the start.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn build_webview(
    parent: RawWindowHandle,
    context: PluginContext,
    initial: &(f64, String),
) -> Result<wry::WebView, String> {
    use wry::dpi::{LogicalPosition, LogicalSize};
    let parent = Parent::new(parent).ok_or("no usable parent handle")?;
    wry::WebViewBuilder::new()
        // Runs before the page's own script, which then shows it.
        .with_initialization_script(format!(
            "window.truceInitial = [{}, {:?}];",
            initial.0, initial.1
        ))
        .with_html(PAGE)
        .with_bounds(wry::Rect {
            position: LogicalPosition::new(0, 0).into(),
            size: LogicalSize::new(WINDOW_W, WINDOW_H).into(),
        })
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let msg = req.body();
            match msg.as_str() {
                "begin" => context.begin_edit(P::Volume),
                "end" => context.end_edit(P::Volume),
                m => {
                    if let Some(v) = m.strip_prefix("set:").and_then(|v| v.parse::<f64>().ok()) {
                        context.set_param(P::Volume, v.clamp(0.0, 1.0));
                    }
                }
            }
        })
        .build_as_child(&parent)
        .map_err(|e| e.to_string())
}

/// truce's parent handle as the `raw-window-handle` 0.6 type `wry` takes.
#[cfg(any(target_os = "macos", target_os = "windows"))]
struct Parent(raw_window_handle::RawWindowHandle);

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Parent {
    fn new(handle: RawWindowHandle) -> Option<Self> {
        use raw_window_handle as rwh;
        match handle {
            #[cfg(target_os = "macos")]
            RawWindowHandle::AppKit(view) => Some(Self(rwh::RawWindowHandle::AppKit(
                rwh::AppKitWindowHandle::new(std::ptr::NonNull::new(view)?),
            ))),
            #[cfg(target_os = "windows")]
            RawWindowHandle::Win32(hwnd) => Some(Self(rwh::RawWindowHandle::Win32(
                rwh::Win32WindowHandle::new(std::num::NonZeroIsize::new(hwnd as isize)?),
            ))),
            _ => None,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl raw_window_handle::HasWindowHandle for Parent {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        // SAFETY: the host's parent view outlives the editor (it's only
        // used while `open` builds the child webview).
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.0) })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSColor, NSTextField, NSView};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    /// Whether the `NSView*` the host passed is in a window.
    pub fn in_window(view: *mut std::ffi::c_void) -> bool {
        // SAFETY: the host passes a live NSView for the duration of `open`.
        let view = unsafe { &*view.cast::<NSView>() };
        view.window().is_some()
    }

    /// A red panel filling the editor, with `message` on it.
    pub fn show_notice(
        view: *mut std::ffi::c_void,
        message: &str,
        w: u32,
        h: u32,
    ) -> Option<Retained<NSTextField>> {
        let mtm = MainThreadMarker::new()?;
        // SAFETY: as in `in_window`; `open` runs on the main thread.
        let view = unsafe { &*view.cast::<NSView>() };
        let label = NSTextField::labelWithString(&NSString::from_str(message), mtm);
        label.setDrawsBackground(true);
        label.setBackgroundColor(Some(&NSColor::systemRedColor()));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(f64::from(w), f64::from(h)),
        ));
        view.addSubview(&label);
        Some(label)
    }
}

truce::plugin! {
    logic: WebviewSynth,
    params: WebviewParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }
}
