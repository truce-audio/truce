use std::sync::Arc;

use crate::events::TransportInfo;

/// A raw pointer wrapper that is `Send + Sync`.
///
/// Used to capture `*const Params` in EditorContext closures without
/// the `ptr as usize` hack. Safe because `Params` fields use atomic
/// storage — concurrent reads from the GUI thread while the audio
/// thread writes are safe by design.
///
/// The pointed-to data must outlive the `SendPtr`. In the plugin
/// context, the plugin instance (which owns the params) always
/// outlives the editor.
pub struct SendPtr<T>(*const T);

impl<T> SendPtr<T> {
    /// Wrap a raw pointer.
    pub fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    /// Dereference the pointer.
    ///
    /// # Safety
    /// The pointed-to data must still be alive.
    pub unsafe fn get(&self) -> &T {
        &*self.0
    }

    /// Get the raw pointer.
    pub fn as_ptr(&self) -> *const T {
        self.0
    }
}

impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SendPtr<T> {}

// SAFETY: The pointed-to data is accessed only from the GUI thread
// (EditorContext closures run on the main/GUI thread). The data is
// pinned (Box::into_raw'd plugin instance) and outlives the editor.
// For Params types, concurrent reads are safe because they use
// AtomicF64. For Plugin types, access is single-threaded (GUI only).
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// Raw platform window handle for GUI parenting.
#[derive(Clone, Copy, Debug)]
pub enum RawWindowHandle {
    AppKit(*mut std::ffi::c_void), // NSView*
    Win32(*mut std::ffi::c_void),  // HWND
    X11(u64),                      // X11 Window ID
}

/// Plugin GUI editor.
pub trait Editor: Send {
    /// Initial window size in logical points.
    ///
    /// On a 2x Retina display, `(400, 300)` produces an 800x600 pixel window.
    /// On a 1x display, it produces a 400x300 pixel window.
    fn size(&self) -> (u32, u32);

    /// Create the GUI as a child of the host-provided parent window.
    fn open(&mut self, parent: RawWindowHandle, context: EditorContext);

    /// Destroy the GUI.
    fn close(&mut self);

    /// Called ~60fps on the host's UI thread for repaint/animation.
    fn idle(&mut self) {}

    /// Host requests a resize. Return true to accept.
    fn set_size(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Whether the plugin supports resizing.
    fn can_resize(&self) -> bool {
        false
    }

    /// Host notifies the editor of a new content scale factor.
    ///
    /// DPI/scale is a host→plugin concept: on VST3 Windows the host
    /// delivers it via `IPlugViewContentScaleSupport`; on CLAP via
    /// `clap_plugin_gui::set_scale`; on macOS/Cocoa AppKit handles
    /// Retina backing automatically and hosts typically never call
    /// this at all. Editors that need to size off-screen buffers in
    /// physical pixels should react here, not by exposing a pull-style
    /// `scale_factor()` method that format wrappers were tempted to
    /// multiply `size()` by (which caused double-scaling on macOS
    /// VST3 — see `docs/internal/vst3-macos-scale-factor.md`).
    fn set_scale_factor(&mut self, _factor: f64) {}

    /// Plugin state was restored (preset recall, undo, session load).
    ///
    /// Called after `load_state()` while the editor is open. Re-read any
    /// cached state from the plugin. Parameter values are already updated
    /// and will be picked up on the next render — this is only needed for
    /// custom state stored outside the parameter system.
    fn state_changed(&mut self) {}

    /// Render a headless screenshot of the editor at its natural size.
    ///
    /// `params` is a type-erased default-state instance the caller
    /// constructs from the plugin's `Params` type. Backends use it to
    /// build a synthetic `ParamState` / render context so the
    /// screenshot reflects parameter defaults without needing a live
    /// host.
    ///
    /// Returns `(rgba_pixels, physical_width, physical_height)` — RGBA8
    /// row-major, ready to feed into `truce_test::assert_screenshot_pixels`.
    /// Default impl returns `None`; backends that support headless
    /// capture (built-in widgets, egui, iced, slint) override.
    ///
    /// Used by `truce_test::assert_screenshot::<Plugin>(...)` for one-line
    /// snapshot regression tests. Editors backed by frameworks that
    /// don't expose a headless render path (e.g. raw-window-handle
    /// users wiring their own Metal/OpenGL) keep the default `None`.
    fn screenshot(&mut self, params: Arc<dyn truce_params::Params>) -> Option<(Vec<u8>, u32, u32)> {
        let _ = params;
        None
    }
}

/// Context passed to Editor::open(). Provides communication
/// with the host and parameter store.
///
/// All fields are `Arc`-wrapped, so cloning is cheap (reference count bump).
#[derive(Clone)]
pub struct EditorContext {
    pub begin_edit: Arc<dyn Fn(u32) + Send + Sync>,
    pub set_param: Arc<dyn Fn(u32, f64) + Send + Sync>,
    pub end_edit: Arc<dyn Fn(u32) + Send + Sync>,
    pub request_resize: Arc<dyn Fn(u32, u32) -> bool + Send + Sync>,
    /// Read a parameter's normalized value from the plugin (for host→GUI sync).
    pub get_param: Arc<dyn Fn(u32) -> f64 + Send + Sync>,
    /// Read a parameter's plain value from the plugin.
    pub get_param_plain: Arc<dyn Fn(u32) -> f64 + Send + Sync>,
    /// Format a parameter's current value as a display string.
    pub format_param: Arc<dyn Fn(u32) -> String + Send + Sync>,
    /// Read a meter value (0.0–1.0) by meter ID. Used for level meters.
    /// Returns 0.0 if the meter ID doesn't exist.
    pub get_meter: Arc<dyn Fn(u32) -> f32 + Send + Sync>,
    /// Read the plugin's custom state (from `save_state()`).
    /// Returns empty vec if the plugin has no custom state.
    pub get_state: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    /// Write custom state back to the plugin (calls `load_state()`).
    pub set_state: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    /// Most-recently-reported host transport state, or `None` if the
    /// host does not expose transport to plugin editors or the plugin
    /// has not yet received a process block.
    ///
    /// Format wrappers populate a shared [`TransportSlot`](crate::TransportSlot)
    /// from their process callback; this closure reads from it.
    pub transport: Arc<dyn Fn() -> Option<TransportInfo> + Send + Sync>,
}
