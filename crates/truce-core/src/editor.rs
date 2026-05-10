use std::ops::Deref;
use std::sync::Arc;

use truce_params::Params;

use crate::events::TransportInfo;

/// A raw pointer wrapper that is `Send + Sync`.
///
/// Used to capture `*const Params` / host-handle pointers in
/// `PluginContext` closures without the `ptr as usize` hack. The
/// `Send`/`Sync` impls are unconditional in `T` — they have to be,
/// because the wrapped types are typically `#[repr(C)]` host structs
/// that are themselves `!Send + !Sync` by default. Construction is
/// therefore `unsafe`: each call site must justify why cross-thread
/// access to the pointed-to data is sound.
///
/// Justifications used in-tree:
/// - **`P: Params`** — fields are atomic; concurrent reads from the
///   GUI thread while the audio thread writes are safe by design.
/// - **Format-host handles** (`clap_host`, `AEffect`, etc.) — used
///   only from a single thread (UI), and the wrapping is purely for
///   capturing in `Send + Sync` closures stored in `PluginContext`.
///
/// The pointed-to data must outlive the `SendPtr`. In the plugin
/// context, the plugin instance (which owns the params) always
/// outlives the editor.
pub struct SendPtr<T>(*const T);

impl<T> SendPtr<T> {
    /// Wrap a raw pointer.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// 1. The pointed-to data outlives every clone of this `SendPtr`.
    /// 2. Cross-thread access to `*ptr` is sound — either because `T`
    ///    is `Sync`, because access is synchronized externally
    ///    (atomic fields, Mutex, single-thread-only access pattern),
    ///    or because the wrapper is only ever read on a thread where
    ///    `T: Sync` would hold.
    pub unsafe fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    /// Dereference the pointer.
    ///
    /// # Safety
    /// The pointed-to data must still be alive.
    #[must_use]
    pub unsafe fn get(&self) -> &T {
        unsafe { &*self.0 }
    }

    /// Get the raw pointer.
    #[must_use]
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

// SAFETY: justified at each `unsafe SendPtr::new(...)` call site.
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
    fn open(&mut self, parent: RawWindowHandle, context: PluginContext);

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
    /// `clap_plugin_gui::set_scale`; on macOS/Cocoa `AppKit` handles
    /// Retina backing automatically and hosts typically never call
    /// this at all. Editors that need to size off-screen buffers in
    /// physical pixels should react here, not by exposing a pull-style
    /// `scale_factor()` method that format wrappers were tempted to
    /// multiply `size()` by (which caused double-scaling on macOS VST3).
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
    /// build a synthetic `PluginContext` / render context so the
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

/// Bridge between the editor and the host / plugin. Format wrappers
/// (CLAP / VST3 / VST2 / AU / AAX / LV2) implement this trait — or
/// build a [`ClosureBridge`] from per-method closures — and pass an
/// `Arc<dyn EditorBridge>` to the editor through [`PluginContext`].
///
/// Editors call into the bridge for everything they can't do
/// directly: starting / ending an automation gesture, reading or
/// writing parameters in normalized or plain form, requesting a
/// window resize, exchanging custom state, sampling the host's
/// transport. Implementations carry whatever per-format pointers
/// the work needs (`clap_host*`, `AEffect*`, an `Arc<P>` for the
/// param store, etc.).
///
/// `Send + Sync` is required so editors can clone the
/// `Arc<dyn EditorBridge>` and hand it to UI worker threads or
/// background animation timers without forcing every implementor to
/// rederive thread-safety bounds.
pub trait EditorBridge: Send + Sync {
    /// Start an automation gesture for `id`. Hosts that show "touched"
    /// state in the automation lane use this to render the
    /// in-progress edit.
    fn begin_edit(&self, id: u32);
    /// Set parameter `id` to `normalized` (clamped to `0.0..=1.0`).
    /// Format wrappers usually plumb this through both the plugin's
    /// own param store and the host's automation channel.
    fn set_param(&self, id: u32, normalized: f64);
    /// End the automation gesture started by [`Self::begin_edit`].
    fn end_edit(&self, id: u32);
    /// Ask the host to resize the editor window to `(w, h)` logical
    /// points. Returns `true` if the host accepted the request.
    fn request_resize(&self, w: u32, h: u32) -> bool;
    /// Read the parameter's current normalized value from the plugin
    /// (host→GUI sync path).
    fn get_param(&self, id: u32) -> f64;
    /// Read the parameter's current plain (denormalized) value.
    fn get_param_plain(&self, id: u32) -> f64;
    /// Format the parameter's current value as a display string,
    /// applying the plugin's `format_value` impl + unit suffix.
    fn format_param(&self, id: u32) -> String;
    /// Format into a caller-provided buffer instead of returning a
    /// fresh `String`. The default impl calls
    /// [`Self::format_param`] and copies, so the *bridge-internal*
    /// allocation still happens; the win for the caller is that the
    /// `out` buffer's capacity is reused across calls (e.g.
    /// `ParamCache::sync` polls one label per changed param per
    /// frame and would otherwise drop+reallocate the cached
    /// `String` slot every time). Bridges that produce the formatted
    /// string from raw value bytes can override to drop the
    /// internal allocation too.
    fn format_param_into(&self, id: u32, out: &mut String) {
        out.clear();
        out.push_str(&self.format_param(id));
    }
    /// Read a meter value (0.0–1.0) by meter ID. Returns 0.0 if the
    /// meter ID isn't registered.
    fn get_meter(&self, id: u32) -> f32;
    /// Read the plugin's custom state (everything outside the
    /// parameter system). Returns an empty `Vec` when the plugin has
    /// no custom state.
    fn get_state(&self) -> Vec<u8>;
    /// Write custom state back to the plugin (calls `load_state()`).
    fn set_state(&self, data: Vec<u8>);
    /// Most-recently-reported host transport state, or `None` if the
    /// host does not expose transport to plugin editors or the plugin
    /// has not yet received a process block.
    ///
    /// Format wrappers populate a shared [`TransportSlot`](crate::TransportSlot)
    /// from their process callback; this method reads from it.
    fn transport(&self) -> Option<TransportInfo>;
}

/// Adapter that implements [`EditorBridge`] over per-method closures.
///
/// Format wrappers that prefer to compose state inline via closures
/// (the historical shape, before the trait existed) construct one of
/// these and wrap it in an `Arc<dyn EditorBridge>`. Wrappers that
/// already have a typed host-pointer struct should `impl EditorBridge`
/// for that struct directly and skip this adapter — one less layer of
/// indirection per call.
pub struct ClosureBridge {
    pub begin_edit: Box<dyn Fn(u32) + Send + Sync>,
    pub set_param: Box<dyn Fn(u32, f64) + Send + Sync>,
    pub end_edit: Box<dyn Fn(u32) + Send + Sync>,
    pub request_resize: Box<dyn Fn(u32, u32) -> bool + Send + Sync>,
    pub get_param: Box<dyn Fn(u32) -> f64 + Send + Sync>,
    pub get_param_plain: Box<dyn Fn(u32) -> f64 + Send + Sync>,
    pub format_param: Box<dyn Fn(u32) -> String + Send + Sync>,
    pub get_meter: Box<dyn Fn(u32) -> f32 + Send + Sync>,
    pub get_state: Box<dyn Fn() -> Vec<u8> + Send + Sync>,
    pub set_state: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    pub transport: Box<dyn Fn() -> Option<TransportInfo> + Send + Sync>,
}

impl EditorBridge for ClosureBridge {
    fn begin_edit(&self, id: u32) {
        (self.begin_edit)(id);
    }
    fn set_param(&self, id: u32, normalized: f64) {
        (self.set_param)(id, normalized);
    }
    fn end_edit(&self, id: u32) {
        (self.end_edit)(id);
    }
    fn request_resize(&self, w: u32, h: u32) -> bool {
        (self.request_resize)(w, h)
    }
    fn get_param(&self, id: u32) -> f64 {
        (self.get_param)(id)
    }
    fn get_param_plain(&self, id: u32) -> f64 {
        (self.get_param_plain)(id)
    }
    fn format_param(&self, id: u32) -> String {
        (self.format_param)(id)
    }
    fn get_meter(&self, id: u32) -> f32 {
        (self.get_meter)(id)
    }
    fn get_state(&self) -> Vec<u8> {
        (self.get_state)()
    }
    fn set_state(&self, data: Vec<u8>) {
        (self.set_state)(data);
    }
    fn transport(&self) -> Option<TransportInfo> {
        (self.transport)()
    }
}

/// Context passed to [`Editor::open`]. Carries:
///
/// - An `Arc<dyn EditorBridge>` — the host-plugin protocol surface
///   (begin/set/end edit, `request_resize`, `get_state`, transport, …).
/// - An `Arc<P>` typed parameter store — plugin authors `Deref` to
///   `&P` and read fields directly: `state.gain.smoothed_next()`.
///
/// The default `P = dyn Params` keeps the trait-object boundary
/// (`Editor::open(ctx: PluginContext)`) one-typed; editor crates
/// that want typed access (truce-egui, truce-slint, truce-iced) carry
/// their own `<P>` and reconstitute `PluginContext<P>` internally
/// via [`PluginContext::with_params`] using the `Arc<P>` they stored
/// at editor construction.
///
/// `Clone` is two refcount bumps (bridge + params). Editors that need
/// to hand the context to UI worker threads or animation timers clone
/// freely.
pub struct PluginContext<P: ?Sized = dyn Params> {
    bridge: Arc<dyn EditorBridge>,
    params: Arc<P>,
}

impl<P: ?Sized> Clone for PluginContext<P> {
    fn clone(&self) -> Self {
        Self {
            bridge: Arc::clone(&self.bridge),
            params: Arc::clone(&self.params),
        }
    }
}

impl<P: ?Sized> PluginContext<P> {
    /// Build a typed context from any [`EditorBridge`] implementor and
    /// the plugin's typed param store.
    pub fn new(bridge: Arc<dyn EditorBridge>, params: Arc<P>) -> Self {
        Self { bridge, params }
    }

    /// Access the underlying bridge handle. Editors that want to clone
    /// the bridge into a worker thread without cloning the surrounding
    /// `PluginContext` use this.
    #[must_use]
    pub fn bridge(&self) -> &Arc<dyn EditorBridge> {
        &self.bridge
    }

    /// Access the typed param store as an `Arc`. Use this when you
    /// need to capture the params in a `'static` closure (e.g. an iced
    /// `Subscription` or a worker thread).
    #[must_use]
    pub fn params(&self) -> &Arc<P> {
        &self.params
    }

    /// Replace the param-store generic parameter while reusing the
    /// same bridge. Used by editor crates that receive the dyn-erased
    /// `PluginContext` from [`Editor::open`] and want the typed
    /// `PluginContext<P>` for their UI closure.
    pub fn with_params<Q: ?Sized>(&self, params: Arc<Q>) -> PluginContext<Q> {
        PluginContext {
            bridge: Arc::clone(&self.bridge),
            params,
        }
    }

    pub fn begin_edit(&self, id: impl Into<u32>) {
        self.bridge.begin_edit(id.into());
    }
    pub fn set_param(&self, id: impl Into<u32>, normalized: f64) {
        self.bridge.set_param(id.into(), normalized);
    }
    pub fn end_edit(&self, id: impl Into<u32>) {
        self.bridge.end_edit(id.into());
    }
    /// Begin + set + end in one call. Use for click-to-toggle widgets
    /// and similar single-shot edits where the gesture and the value
    /// arrive together.
    pub fn automate(&self, id: impl Into<u32>, normalized: f64) {
        let id = id.into();
        self.bridge.begin_edit(id);
        self.bridge.set_param(id, normalized);
        self.bridge.end_edit(id);
    }
    #[must_use]
    pub fn request_resize(&self, w: u32, h: u32) -> bool {
        self.bridge.request_resize(w, h)
    }
    pub fn get_param(&self, id: impl Into<u32>) -> f64 {
        self.bridge.get_param(id.into())
    }
    pub fn get_param_plain(&self, id: impl Into<u32>) -> f64 {
        self.bridge.get_param_plain(id.into())
    }
    pub fn format_param(&self, id: impl Into<u32>) -> String {
        self.bridge.format_param(id.into())
    }
    /// Format into a caller-owned buffer. See
    /// [`EditorBridge::format_param_into`] for the allocation
    /// trade-off — the caller's buffer is reused, but bridges that
    /// don't override the default impl still allocate internally.
    pub fn format_param_into(&self, id: impl Into<u32>, out: &mut String) {
        self.bridge.format_param_into(id.into(), out);
    }
    pub fn get_meter(&self, id: impl Into<u32>) -> f32 {
        self.bridge.get_meter(id.into())
    }
    #[must_use]
    pub fn get_state(&self) -> Vec<u8> {
        self.bridge.get_state()
    }
    pub fn set_state(&self, data: Vec<u8>) {
        self.bridge.set_state(data);
    }
    #[must_use]
    pub fn transport(&self) -> Option<TransportInfo> {
        self.bridge.transport()
    }
}

impl PluginContext<dyn Params> {
    /// Build a dyn-erased context from a [`ClosureBridge`]. Convenience
    /// for format wrappers that compose state inline via closures.
    pub fn from_closures(bridge: ClosureBridge, params: Arc<dyn Params>) -> Self {
        Self {
            bridge: Arc::new(bridge),
            params,
        }
    }
}

impl<P: Params + 'static> PluginContext<P> {
    /// Drop the typed `<P>` and return the dyn-erased context that
    /// crosses the `Editor::open` trait-object boundary.
    #[must_use]
    pub fn dyn_erase(self) -> PluginContext<dyn Params> {
        PluginContext {
            bridge: self.bridge,
            params: self.params as Arc<dyn Params>,
        }
    }
}

/// Plugin authors read parameter fields directly via `Deref`:
/// `state.gain.smoothed_next()`, `state.bypass.value()`. The `state`
/// here is `&PluginContext<MyParams>` and `Deref::Target = MyParams`.
impl<P: ?Sized> Deref for PluginContext<P> {
    type Target = P;
    fn deref(&self) -> &P {
        &self.params
    }
}

/// Build a [`PluginContext`] backed only by `params`. All write
/// closures are no-ops; reads delegate to the params `Arc`; the
/// transport reports the deterministic
/// [`crate::events::TransportInfo::for_screenshot`] state so
/// screenshot tests stay reproducible across CI runs.
///
/// Used by editor backends inside their `Editor::screenshot()` impl,
/// and re-exported from `truce-test` for plugin authors that want to
/// drive snapshot tests directly.
pub fn for_test_params(params: Arc<dyn Params>) -> PluginContext<dyn Params> {
    let p_get = Arc::clone(&params);
    let p_plain = Arc::clone(&params);
    let p_fmt = Arc::clone(&params);
    let transport = TransportInfo::for_screenshot();
    PluginContext::from_closures(
        ClosureBridge {
            begin_edit: Box::new(|_| {}),
            set_param: Box::new(|_, _| {}),
            end_edit: Box::new(|_| {}),
            request_resize: Box::new(|_, _| false),
            get_param: Box::new(move |id| p_get.get_normalized(id).unwrap_or(0.5)),
            get_param_plain: Box::new(move |id| p_plain.get_plain(id).unwrap_or(0.0)),
            format_param: Box::new(move |id| {
                let plain = p_fmt.get_plain(id).unwrap_or(0.0);
                p_fmt
                    .format_value(id, plain)
                    .unwrap_or_else(|| format!("{plain:.2}"))
            }),
            get_meter: Box::new(|_| 0.0),
            get_state: Box::new(Vec::new),
            set_state: Box::new(|_| {}),
            transport: Box::new(move || Some(transport)),
        },
        params,
    )
}
