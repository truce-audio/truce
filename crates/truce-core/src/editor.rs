use std::ops::Deref;
use std::sync::Arc;

use truce_params::Params;
use truce_params::sample::Float;

use crate::events::TransportInfo;
use crate::tasks::{AnyTaskSpawner, TaskSpawner};

/// A lock-free editor factory bound to a plugin's param store.
///
/// `PluginExport::editor_builder` returns one of these at instance
/// creation; format wrappers cache it outside the plugin lock and call
/// it when the host opens the GUI. For a static build the closure builds
/// the editor from the concrete logic type; for a `--shell` build it
/// rebuilds from the currently loaded dylib, so GUI edits hot-reload
/// (picked up on the next editor close+open). `Send + Sync` so wrappers
/// can stash it in their instance struct and call it from the GUI thread.
pub type EditorBuilder<P> = Box<dyn Fn(Arc<P>) -> Option<Box<dyn Editor>> + Send + Sync>;

/// A raw pointer wrapper that is `Send + Sync`.
///
/// Used to capture `*const Params` / host-handle pointers in
/// `PluginContext` closures without the `ptr as usize` hack. The
/// `Send`/`Sync` impls are unconditional in `T` - they have to be,
/// because the wrapped types are typically `#[repr(C)]` host structs
/// that are themselves `!Send + !Sync` by default. Construction is
/// therefore `unsafe`: each call site must justify why cross-thread
/// access to the pointed-to data is sound.
///
/// Justifications used in-tree:
/// - **`P: Params`** - fields are atomic; concurrent reads from the
///   GUI thread while the audio thread writes are safe by design.
/// - **Format-host handles** (`clap_host`, `AEffect`, etc.) - used
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
    /// 2. Cross-thread access to `*ptr` is sound - either because `T`
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
    AppKit(*mut std::ffi::c_void), // macOS NSView*
    UiKit(*mut std::ffi::c_void),  // iOS / iPadOS UIView*
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

    /// Whether the editor permits the standalone window to be
    /// maximized (the WM maximize button / double-click-titlebar
    /// maximize / macOS zoom-and-fullscreen / Windows maximize box).
    ///
    /// Standalone-only: in CLAP / VST3 / AU the host owns the window
    /// frame, so this is ignored there (same as `size_increment`'s
    /// WM-snap note). Subordinate to [`Self::can_resize`] - a
    /// non-resizable editor can never be maximized regardless of this
    /// value, since the standalone pins min == max, which already
    /// blocks it.
    ///
    /// Defaults to `false`: the standalone host removes the maximize
    /// affordance from resizable editors, so the window stays within
    /// the edge-drag bounds the WM already enforces and can't jump past
    /// the editor's [`Self::max_size`] into an unpainted margin around
    /// the clamped surface. Override to `true` for editors that render
    /// correctly at arbitrary size (typically an unbounded `max_size`)
    /// and want the maximize affordance.
    fn can_maximize(&self) -> bool {
        false
    }

    /// Minimum size the editor can render at, in logical points.
    /// Defaults to `(1, 1)`. Wrappers consult this for CLAP's
    /// `gui_get_resize_hints` and VST3's `checkSizeConstraint`.
    /// Ignored when `can_resize()` returns `false`.
    fn min_size(&self) -> (u32, u32) {
        (1, 1)
    }

    /// Maximum size the editor can render at, in logical points.
    /// Defaults to `(u32::MAX, u32::MAX)`. Same wrapper consumers
    /// as `min_size`.
    fn max_size(&self) -> (u32, u32) {
        (u32::MAX, u32::MAX)
    }

    /// Logical-point granularity for interactive resize, or `None`
    /// for free (pixel-precise) resizing. The standalone X11 host
    /// maps this onto WM resize increments (`PResizeInc`) so the
    /// window manager snaps edge-drags to whole cells - the same
    /// mechanism terminal emulators use to snap to character cells.
    /// The snap counts from [`Self::min_size`], which is already
    /// cell-aligned, so every allowed size lands on a boundary.
    /// Ignored when `can_resize()` returns `false`.
    fn size_increment(&self) -> Option<(u32, u32)> {
        None
    }

    /// Aspect-ratio constraint as `(numerator, denominator)`, or
    /// `None` for free resizing. CLAP, VST3, AU v3, standalone, and
    /// LV2 honour this; VST2 / AAX silently ignore. Integer pair
    /// (not `f64`) avoids the Cubase-9 aspect-rounding quirk JUCE
    /// special-cases.
    fn aspect_ratio(&self) -> Option<(u32, u32)> {
        None
    }

    /// Hint that the renderer prefers power-of-two surface sizes
    /// (some GPU-backed editors). Maps onto CLAP's
    /// `clap_gui_resize_hints.preserve_aspect_ratio` /
    /// `aspect_ratio_width` siblings; ignored on formats without
    /// an equivalent.
    fn prefers_pow2(&self) -> bool {
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

    /// Opt the editor into honoring the desktop (system) scale.
    ///
    /// The standalone app calls this with `true` before [`open`] because
    /// it owns a real top-level window that should match the desktop
    /// (`Xft.dpi` on Linux). Plugin formats leave the default: an
    /// embedded editor drives its Linux scale from the host's
    /// content-scale callback (default 1.0) instead of the desktop,
    /// since a non-DPI-aware host (e.g. Bitwig on X11) runs at 1x
    /// regardless of desktop scaling and would otherwise get a
    /// double-sized window. No-op on macOS/Windows, where the OS
    /// reports a reliable per-window scale.
    ///
    /// [`open`]: Editor::open
    fn set_uses_system_scale(&mut self, _yes: bool) {}

    /// Plugin state was restored (preset recall, undo, session load).
    ///
    /// Called after `load_state()` while the editor is open. Re-read any
    /// cached state from the plugin. Parameter values are already updated
    /// and will be picked up on the next render - this is only needed for
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
    /// Returns `(rgba_pixels, physical_width, physical_height)` - RGBA8
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

/// Fluent terminal for `editor()` impls: box any concrete editor into
/// the `Box<dyn Editor>` the trait returns, dropping the `Box::new(…)`
/// wrapper.
///
/// ```ignore
/// fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
///     EguiEditor::new(params, (W, H), ui)
///         .with_visuals(theme)
///         .into_editor()
/// }
/// ```
///
/// Implemented for every [`Editor`] via a blanket impl and re-exported
/// from every `truce::prelude*`, so it's in scope without an extra
/// import - egui / iced / slint / hand-rolled editors all use it.
/// Layout-only plugins use `truce_gui::IntoLayoutEditor` instead (its
/// `into_editor` takes `&Arc<Params>` and picks the built-in renderer).
pub trait IntoEditor {
    /// Box this editor into a `Box<dyn Editor>`.
    fn into_editor(self) -> Box<dyn Editor>;
}

impl<E: Editor + 'static> IntoEditor for E {
    fn into_editor(self) -> Box<dyn Editor> {
        Box::new(self)
    }
}

/// Bridge between the editor and the host / plugin. Format wrappers
/// (CLAP / VST3 / VST2 / AU / AAX / LV2) implement this trait - or
/// build a [`ClosureBridge`] from per-method closures - and pass an
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
/// construct one of these and wrap it in an `Arc<dyn EditorBridge>`.
/// Wrappers that already have a typed host-pointer struct should
/// `impl EditorBridge` for that struct directly and skip this
/// adapter; one less layer of indirection per call.
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
/// - An `Arc<dyn EditorBridge>` - the host-plugin protocol surface
///   (begin/set/end edit, `request_resize`, `get_state`, transport, …).
/// - An `Arc<P>` typed parameter store - plugin authors `Deref` to
///   `&P` and read fields directly: `state.gain.read()`.
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
    /// Background-task spawner bundle (one lane per declared task type),
    /// stamped by the format wrapper from `PluginExport::task_spawner`
    /// when the plugin wired `tasks:`. Lets the editor schedule work via
    /// [`Self::tasks`]. `None` for a plugin with no background tasks.
    tasks: Option<AnyTaskSpawner>,
    params: Arc<P>,
}

impl<P: ?Sized> Clone for PluginContext<P> {
    fn clone(&self) -> Self {
        Self {
            bridge: Arc::clone(&self.bridge),
            tasks: self.tasks.clone(),
            params: Arc::clone(&self.params),
        }
    }
}

impl<P: ?Sized> PluginContext<P> {
    /// Build a typed context from any [`EditorBridge`] implementor and
    /// the plugin's typed param store. Add background-task scheduling
    /// with [`Self::with_tasks`].
    pub fn new(bridge: Arc<dyn EditorBridge>, params: Arc<P>) -> Self {
        Self {
            bridge,
            tasks: None,
            params,
        }
    }

    /// Attach the background-task spawner (from
    /// `PluginExport::task_spawner`). Format wrappers call this when
    /// building the editor context.
    #[must_use]
    pub fn with_tasks(mut self, tasks: Option<AnyTaskSpawner>) -> Self {
        self.tasks = tasks;
        self
    }

    /// The background-task spawner for task type `T`, or `None` if the
    /// plugin declared no `tasks:` lane of that type. Scheduling with it is
    /// wait-free, so it is safe from the GUI thread.
    #[must_use]
    pub fn tasks<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.tasks.as_ref().and_then(AnyTaskSpawner::downcast::<T>)
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
            tasks: self.tasks.clone(),
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
    /// Ask the host to resize the editor to `(w, h)` logical points.
    /// Returns `true` if the request was accepted.
    ///
    /// **Re-entrancy:** call this from your editor's own event / render loop,
    /// not synchronously from inside an [`Editor`] method the host drives
    /// (`open`, `set_size`, `size`, `state_changed`, `close`). Several
    /// wrappers hold an internal editor lock across those calls, and on some
    /// formats `request_resize` reaches back into it - directly (AU) or via a
    /// host that answers `resizeView` synchronously (VST3). Such a re-entrant
    /// call is deferred (applied on a later frame) rather than applied inline,
    /// so a "correct my aspect ratio from within `set_size`" pattern won't
    /// take effect immediately; do that shaping by returning the adjusted size
    /// or requesting from the next frame instead.
    #[must_use]
    pub fn request_resize(&self, w: u32, h: u32) -> bool {
        self.bridge.request_resize(w, h)
    }
    pub fn format_param(&self, id: impl Into<u32>) -> String {
        self.bridge.format_param(id.into())
    }
    /// Format into a caller-owned buffer. See
    /// [`EditorBridge::format_param_into`] for the allocation
    /// trade-off - the caller's buffer is reused, but bridges that
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
            tasks: None,
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
            tasks: self.tasks,
            params: self.params as Arc<dyn Params>,
        }
    }
}

/// Plugin authors read parameter fields directly via `Deref`:
/// `state.gain.read()`, `state.bypass.value()`. The `state`
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

// ---------------------------------------------------------------------------
// Precision-routed parameter reads
//
// The editor-bridge surface is sample-agnostic (`f64` on the wire, the
// lossless lowest-common-denominator that round-trips any host
// automation precision). These two extension traits route the call
// site to the user's chosen precision - same pattern as
// `FloatParamReadF32` / `FloatParamReadF64` for the audio-thread
// param reads. Brought into scope via `pub use ... as _;` in each
// prelude:
//   - `prelude` / `prelude32`        → `PluginContextReadF32`
//   - `prelude64` / `prelude64m`     → `PluginContextReadF64`
//
// Single-prelude code dispatches unambiguously. Importing both
// preludes in the same file collides on `get_param` - the right
// error if the file hasn't committed to a precision.
// ---------------------------------------------------------------------------

/// `f32`-precision parameter reads on `PluginContext`. Brought into
/// scope by `truce::prelude` / `truce::prelude32` / `truce::prelude64m`
/// (the `f32`-buffer preludes). GUI binding crates (slint, egui,
/// iced) take `f32` natively, so this is the common case.
pub trait PluginContextReadF32 {
    /// Normalized `[0, 1]` value of the parameter, narrowed to `f32`.
    fn get_param(&self, id: impl Into<u32>) -> f32;
    /// Plain (denormalized) value of the parameter, narrowed to `f32`.
    fn get_param_plain(&self, id: impl Into<u32>) -> f32;
}

/// `f64`-precision parameter reads on `PluginContext`. Brought into
/// scope by `truce::prelude64`. Same surface as
/// [`PluginContextReadF32`] but returns the bridge's `f64` value
/// directly without narrowing.
pub trait PluginContextReadF64 {
    /// Normalized `[0, 1]` value of the parameter.
    fn get_param(&self, id: impl Into<u32>) -> f64;
    /// Plain (denormalized) value of the parameter.
    fn get_param_plain(&self, id: impl Into<u32>) -> f64;
}

impl<P: ?Sized> PluginContextReadF32 for PluginContext<P> {
    fn get_param(&self, id: impl Into<u32>) -> f32 {
        self.bridge.get_param(id.into()).to_f32()
    }
    fn get_param_plain(&self, id: impl Into<u32>) -> f32 {
        self.bridge.get_param_plain(id.into()).to_f32()
    }
}

impl<P: ?Sized> PluginContextReadF64 for PluginContext<P> {
    fn get_param(&self, id: impl Into<u32>) -> f64 {
        self.bridge.get_param(id.into())
    }
    fn get_param_plain(&self, id: impl Into<u32>) -> f64 {
        self.bridge.get_param_plain(id.into())
    }
}

/// Constrain a host-requested logical size to an editor's
/// [`Editor::min_size`] / [`Editor::max_size`] / [`Editor::aspect_ratio`].
/// Shared by every format wrapper so they enforce identical constraints.
///
/// With an aspect ratio set, fit the *largest on-ratio rectangle that fits
/// inside* the requested box: derive height from width and keep it if it
/// fits, otherwise the width is the limiting axis and height drives it. The
/// result is `<=` the request on both axes, so the editor surface never
/// exceeds the host window and can never clip - whatever odd size a host
/// hands us (some skip an aspect pre-flight and pass raw drag dimensions),
/// the worst case is an on-ratio letterbox inside the window. The rule is a
/// pure function of `(w, h)` - no "which edge moved" guess - so a drag can't
/// make the chosen axis flip and judder. `u64` arithmetic for the
/// multiplication so a hypothetical `(u32::MAX, 1)` aspect doesn't overflow
/// before the clamp lands.
#[must_use]
pub fn fit_logical_size(w: u32, h: u32, editor: &dyn Editor) -> (u32, u32) {
    fit_size(
        w,
        h,
        editor.min_size(),
        editor.max_size(),
        editor.aspect_ratio(),
    )
}

/// Same fit as [`fit_logical_size`] but over raw constraints rather than
/// an `&dyn Editor`. Lets call sites that have already captured the
/// bounds (e.g. an Objective-C resize callback that can't carry a trait
/// object) reuse the identical rule.
#[must_use]
pub fn fit_size(
    w: u32,
    h: u32,
    min: (u32, u32),
    max: (u32, u32),
    aspect: Option<(u32, u32)>,
) -> (u32, u32) {
    let (min_w, min_h) = min;
    let (max_w, max_h) = max;
    let mut w = w.clamp(min_w.max(1), max_w);
    let mut h = h.clamp(min_h.max(1), max_h);
    if let Some((num64, denom64)) = ratio64(aspect) {
        // The on-ratio height for this width. Unclamped so the comparison
        // sees the true ratio, not a bound-pinned value.
        let h_from_w = u64::from(w) * denom64 / num64;
        if h_from_w <= u64::from(h) {
            // Width is the limiting axis: shrink height onto the ratio.
            h = derive_height(w, min_h, max_h, num64, denom64).0;
        } else {
            // Height is the limiting axis: shrink width onto the ratio.
            w = derive_width(h, min_w, max_w, num64, denom64).0;
        }
    }
    (w, h)
}

/// Clamp a host-committed size to the editor's `[min, max]` bounds only,
/// leaving the aspect ratio untouched so the editor fills the host window
/// exactly. The commit-time counterpart to [`fit_logical_size`]: on-ratio
/// shaping already happened during the host's drag negotiation (a preflight
/// such as VST3 `checkSizeConstraint`), so re-fitting onto the ratio here
/// would only floor the editor a pixel under the window and leave an
/// unpainted letterbox line. The `max` clamp keeps the surface inside the
/// window; the `min` clamp upholds the editor's "can't render smaller than
/// this" floor even when a host hands over a too-small box.
#[must_use]
pub fn clamp_logical_size(w: u32, h: u32, editor: &dyn Editor) -> (u32, u32) {
    let (min_w, min_h) = editor.min_size();
    let (max_w, max_h) = editor.max_size();
    (w.clamp(min_w.max(1), max_w), h.clamp(min_h.max(1), max_h))
}

/// Enforces size constraints on host resizes that bypassed the format's
/// negotiation hooks. Some hosts resize the plugin's embedded window
/// directly at the windowing-system level (Bitwig on Linux/X11 resizes
/// the embed window itself), so no `checkSizeConstraint`-style preflight
/// ever runs - the editor's own `Resized` handler is the last place that
/// can enforce `min_size` / `max_size` / `aspect_ratio`.
///
/// [`Self::fit`] returns the size the editor should render at, plus an
/// optional corrective size to push back to the host
/// (`PluginContext::request_resize`). Each offending host size triggers at
/// most one correction, and the corrective size itself satisfies the
/// constraints, so a host that refuses (or echoes) the request can't be
/// spun into a resize feedback loop.
#[derive(Default)]
pub struct ResizeCorrector {
    /// Whether we've already pushed a corrective resize back to the host
    /// for the current out-of-bounds excursion. Latches on the first
    /// push-back and clears only when the host hands us an in-bounds size
    /// again. A host that bypasses negotiation and then ignores the
    /// push-back would otherwise be re-asked every frame and spun into a
    /// runaway resize loop: Bitwig on Linux returns success to
    /// `request_resize` but instead *grows* the embed window a few px per
    /// call, and jitters the size on the un-clamped axis so the fitted
    /// target changes every frame. Latching on "have we asked since the
    /// last in-bounds size" - rather than on the requested size - sends
    /// exactly one request per excursion even when that target wobbles.
    pushed_back: bool,
}

impl ResizeCorrector {
    /// Fit a host-driven logical size against the constraints. Returns
    /// the fitted size to render at and, when the host size was out of
    /// bounds and we haven't already pushed back this excursion, the size
    /// to request back.
    pub fn fit(
        &mut self,
        w: u32,
        h: u32,
        min: (u32, u32),
        max: (u32, u32),
        aspect: Option<(u32, u32)>,
    ) -> ((u32, u32), Option<(u32, u32)>) {
        let fitted = fit_size(w, h, min, max, aspect);
        if fitted == (w, h) {
            // In-bounds: the host is cooperating (or the drag returned
            // within bounds); re-arm the next excursion's one-shot push.
            self.pushed_back = false;
            return (fitted, None);
        }
        // Out of bounds: push back exactly once per excursion.
        let request = (!self.pushed_back).then_some(fitted);
        self.pushed_back = true;
        (fitted, request)
    }
}

/// A usable `(num, denom)` ratio as `u64`, or `None` when no aspect is set
/// or either term is zero. `u64` so the on-ratio multiplications below can't
/// overflow before the clamp lands (a hypothetical `(u32::MAX, 1)` aspect).
fn ratio64(aspect: Option<(u32, u32)>) -> Option<(u64, u64)> {
    match aspect {
        Some((num, denom)) if num > 0 && denom > 0 => Some((u64::from(num), u64::from(denom))),
        _ => None,
    }
}

/// On-ratio height for `w`, clamped into `[min_h, max_h]`. The flag reports
/// whether the clamp moved the value (i.e. a bound was hit), so the caller
/// knows whether the source axis needs re-deriving to stay on-ratio.
#[allow(clippy::cast_possible_truncation)]
fn derive_height(w: u32, min_h: u32, max_h: u32, num64: u64, denom64: u64) -> (u32, bool) {
    let on_ratio = (u64::from(w) * denom64 / num64).clamp(1, u64::from(u32::MAX)) as u32;
    let clamped = on_ratio.clamp(min_h.max(1), max_h);
    (clamped, clamped != on_ratio)
}

/// On-ratio width for `h`, clamped into `[min_w, max_w]`. Flag as in
/// [`derive_height`].
#[allow(clippy::cast_possible_truncation)]
fn derive_width(h: u32, min_w: u32, max_w: u32, num64: u64, denom64: u64) -> (u32, bool) {
    let on_ratio = (u64::from(h) * num64 / denom64).clamp(1, u64::from(u32::MAX)) as u32;
    let clamped = on_ratio.clamp(min_w.max(1), max_w);
    (clamped, clamped != on_ratio)
}

#[cfg(test)]
mod corrector_tests {
    use super::ResizeCorrector;

    const MIN: (u32, u32) = (300, 200);
    const MAX: (u32, u32) = (900, 600);

    #[test]
    fn in_bounds_size_passes_through_without_correction() {
        let mut c = ResizeCorrector::default();
        assert_eq!(c.fit(400, 300, MIN, MAX, None), ((400, 300), None));
    }

    #[test]
    fn out_of_bounds_pushes_once_per_excursion() {
        let mut c = ResizeCorrector::default();
        // First out-of-bounds sight: fit + one push-back.
        let (fitted, req) = c.fit(1200, 800, MIN, MAX, None);
        assert_eq!(fitted, (900, 600));
        assert_eq!(req, Some((900, 600)));
        // Host refused / echoed the same size: no repeat request.
        assert_eq!(c.fit(1200, 800, MIN, MAX, None), ((900, 600), None));
        // Host ignores it and keeps feeding out-of-bounds sizes - crucially,
        // even ones whose *fitted target wobbles* (a different clamp on the
        // un-pinned axis each frame). We stay quiet, so a host that grows in
        // response to each request (Bitwig) can't be spun into a runaway.
        assert_eq!(c.fit(1300, 590, MIN, MAX, None), ((900, 590), None));
        assert_eq!(c.fit(1300, 595, MIN, MAX, None), ((900, 595), None));
        // ...even a swing to the opposite bound stays quiet mid-excursion.
        assert_eq!(c.fit(100, 100, MIN, MAX, None), ((300, 200), None));
        // Host finally hands us an in-bounds size: excursion over, re-arm.
        assert_eq!(c.fit(800, 500, MIN, MAX, None), ((800, 500), None));
        // Next excursion gets a fresh single push-back.
        let (_, req) = c.fit(1200, 800, MIN, MAX, None);
        assert_eq!(req, Some((900, 600)));
    }

    #[test]
    fn honoured_correction_resets_the_guard() {
        let mut c = ResizeCorrector::default();
        let _ = c.fit(1200, 800, MIN, MAX, None);
        // Host applied the corrective size: in bounds, guard resets...
        assert_eq!(c.fit(900, 600, MIN, MAX, None), ((900, 600), None));
        // ...so the same offending size requests again next time.
        let (_, req) = c.fit(1200, 800, MIN, MAX, None);
        assert_eq!(req, Some((900, 600)));
    }

    #[test]
    fn aspect_violation_corrects_onto_ratio() {
        let mut c = ResizeCorrector::default();
        let ((w, h), req) = c.fit(800, 600, MIN, MAX, Some((4, 3)));
        assert_eq!((w, h), (800, 600), "already on-ratio passes through");
        assert_eq!(req, None);
        let ((w, h), req) = c.fit(800, 400, MIN, MAX, Some((4, 3)));
        assert_eq!((w, h), (533, 400), "height-limited fit onto 4:3");
        assert_eq!(req, Some((533, 400)));
    }
}

#[cfg(test)]
mod fit_tests {
    use super::{Editor, PluginContext, RawWindowHandle, fit_logical_size};

    /// Minimal editor stub: only the bounds/aspect hooks
    /// `fit_logical_size` reads carry meaning; the rest are unused.
    struct StubEditor {
        min: (u32, u32),
        max: (u32, u32),
        aspect: Option<(u32, u32)>,
    }

    impl Editor for StubEditor {
        fn size(&self) -> (u32, u32) {
            self.min
        }
        fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {}
        fn close(&mut self) {}
        fn min_size(&self) -> (u32, u32) {
            self.min
        }
        fn max_size(&self) -> (u32, u32) {
            self.max
        }
        fn aspect_ratio(&self) -> Option<(u32, u32)> {
            self.aspect
        }
    }

    fn stub(aspect: Option<(u32, u32)>) -> StubEditor {
        StubEditor {
            min: (320, 240),
            max: (u32::MAX, u32::MAX),
            aspect,
        }
    }

    #[test]
    fn no_aspect_clamps_each_axis_to_bounds() {
        let e = stub(None);
        assert_eq!(fit_logical_size(800, 600, &e), (800, 600));
        assert_eq!(fit_logical_size(100, 100, &e), (320, 240));
    }

    #[test]
    fn tall_box_is_width_bound() {
        // A box taller than 4:3 fits the full width; height shrinks onto
        // the ratio so the result never overflows the box.
        let e = stub(Some((4, 3)));
        assert_eq!(fit_logical_size(640, 800, &e), (640, 480));
    }

    #[test]
    fn wide_box_is_height_bound() {
        // A box wider than 4:3 fits the full height; width shrinks instead.
        let e = stub(Some((4, 3)));
        assert_eq!(fit_logical_size(800, 480, &e), (640, 480));
    }

    #[test]
    fn on_ratio_box_is_unchanged() {
        let e = stub(Some((4, 3)));
        assert_eq!(fit_logical_size(800, 600, &e), (800, 600));
    }

    #[test]
    fn fit_never_exceeds_the_requested_box() {
        // The no-clip invariant: for any box at or above `min_size`, the
        // aspect fit stays inside it on both axes, so the editor surface
        // can never overflow the host window. `min` is on the 16:9 ratio so
        // the fit also stays exactly on-ratio right down to the corner.
        let e = StubEditor {
            min: (320, 180),
            max: (u32::MAX, u32::MAX),
            aspect: Some((16, 9)),
        };
        for &(w, h) in &[
            (640, 800),
            (800, 480),
            (1000, 1000),
            (321, 900),
            (1920, 300),
        ] {
            let (rw, rh) = fit_logical_size(w, h, &e);
            assert!(rw <= w && rh <= h, "{rw}x{rh} exceeds box {w}x{h}");
            assert!((i64::from(rw) * 9 - i64::from(rh) * 16).abs() <= 16);
            assert!(rw >= 320 && rh >= 180);
        }
    }
}
