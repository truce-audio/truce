//! AU v2 Cocoa UI view factory — the `AUCocoaUIBase` class the host
//! reads via `kAudioUnitProperty_CocoaUI` to find our editor entry
//! point.
//!
//! The class is allocated and registered with the `ObjC` runtime at
//! plugin-load time with a unique per-plugin name (derived from the
//! AU manufacturer + subtype fourcc). Each truce plugin must publish a
//! distinct `ObjC` class name because hosts like Logic load multiple
//! `.component` dylibs into one process and the `ObjC` class registry
//! is process-flat — two plugins sharing a class name would have the
//! second plugin's methods silently shadowed by the first, with method
//! dispatch reaching the wrong dylib's `g_callbacks`.
//!
//! Registering at runtime in Rust keeps the C/`ObjC` shim
//! plugin-agnostic, so `truce-au` is compiled once per workspace and
//! reused across every plugin.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::sel;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::ffi::{AuCallbacks, AuPluginDescriptor};

// ---------------------------------------------------------------------------
// Per-dylib state.
//
// One plugin per cdylib means one class name and one callback table per
// dylib — both effectively static-lifetime. `CALLBACKS_PTR` is read
// from the host main thread when the user opens the editor; the only
// writer is `register` (called once from the dylib constructor before
// the host can request a UI). `Acquire`/`Release` is enough.
// ---------------------------------------------------------------------------

static CLASS_NAME: OnceLock<CString> = OnceLock::new();
static CALLBACKS_PTR: AtomicPtr<AuCallbacks> = AtomicPtr::new(std::ptr::null_mut());

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Allocate + register the cocoa UI view factory class with a unique
/// name derived from `descriptor`. Idempotent: a second call (e.g.
/// from a shell-mode reload that re-runs registration) is a no-op.
///
/// `descriptor` and `callbacks` must point to memory that lives for
/// the rest of the process — both come from a `Box::leak` site in
/// `register_au_inner`, satisfying that.
pub(crate) fn register(descriptor: *const AuPluginDescriptor, callbacks: *const AuCallbacks) {
    if CLASS_NAME.get().is_some() {
        return;
    }

    // SAFETY: descriptor points to leaked-Box static memory, valid
    // for the rest of the process.
    let suffix = unsafe { sanitize_suffix(&*descriptor) };
    let class_name = CString::new(format!("TruceAUView_{suffix}"))
        .expect("class name is ASCII alphanumerics + underscores");

    CALLBACKS_PTR.store(callbacks.cast_mut(), Ordering::Release);

    let superclass = AnyClass::get(c"NSObject").expect("NSObject class missing");
    let Some(mut builder) = ClassBuilder::new(class_name.as_c_str(), superclass) else {
        // Class already registered — happens if two builds of the
        // same plugin are loaded (shell-mode dev), or if
        // `register` somehow ran twice. Either way, silently keep
        // the previously-registered class active.
        let _ = CLASS_NAME.set(class_name);
        return;
    };

    // SAFETY: each function's signature matches the `ObjC` method
    // shape (`(receiver, _cmd, ...args) -> ret`). The receiver is
    // `&AnyObject` per `MethodImplementation`'s signature; we never
    // dereference it for these stateless methods.
    unsafe {
        builder.add_method(
            sel!(interfaceVersion),
            interface_version as unsafe extern "C" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(uiViewForAudioUnit:withSize:),
            ui_view_for_audio_unit_with_size as unsafe extern "C" fn(_, _, _, _) -> _,
        );
    }

    let _ = builder.register();
    let _ = CLASS_NAME.set(class_name);
}

/// Return the registered class name as a NUL-terminated C string,
/// which `au_v2_shim.c` hands to `CFStringCreateWithCString` when
/// answering `kAudioUnitProperty_CocoaUI`. Returns NULL only if
/// `register` has not yet run, which in practice cannot happen — the
/// AU host doesn't query `CocoaUI` until after plugin instantiation,
/// which happens after the dylib constructor.
#[unsafe(no_mangle)]
pub extern "C" fn truce_au_view_factory_class_name() -> *const c_char {
    CLASS_NAME.get().map_or(std::ptr::null(), |s| s.as_ptr())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sanitize_suffix(d: &AuPluginDescriptor) -> String {
    let mut s = String::with_capacity(8);
    for &b in d
        .component_manufacturer
        .iter()
        .chain(d.component_subtype.iter())
    {
        let c = b as char;
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Method implementations
// ---------------------------------------------------------------------------

// SAFETY for both: invoked by the `ObjC` runtime through `objc_msgSend`.
// `_self` points at a valid instance allocated by `+[NSObject alloc]`.
// `_cmd` is the matching selector. Subsequent args follow the C ABI
// for the method's `ObjC` type encoding.

unsafe extern "C" fn interface_version(_self: &AnyObject, _cmd: Sel) -> u32 {
    0
}

unsafe extern "C" fn ui_view_for_audio_unit_with_size(
    _self: &AnyObject,
    _cmd: Sel,
    au: *mut c_void,
    _size: NSSize,
) -> *mut AnyObject {
    // Pull the Rust ctx out of the AudioUnit via our private property.
    // `au_v2_shim.c` exposes the Rust ctx as
    // `kTrucePrivateProperty_RustContext` so the cocoa view bridge can
    // talk back to the same plugin instance the host is rendering.
    let mut ctx: *mut c_void = std::ptr::null_mut();
    let mut sz: u32 = u32::try_from(std::mem::size_of::<*mut c_void>()).unwrap_or(8);

    // SAFETY: AudioUnitGetProperty FFI; arg layout is C-stable and
    // `au` came from the host as a non-null AudioUnit reference.
    let err = unsafe {
        AudioUnitGetProperty(
            au,
            K_TRUCE_PRIVATE_PROPERTY_RUST_CONTEXT,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            (&raw mut ctx).cast::<c_void>(),
            &raw mut sz,
        )
    };
    if err != 0 || ctx.is_null() {
        return std::ptr::null_mut();
    }

    let cb_ptr = CALLBACKS_PTR.load(Ordering::Acquire);
    if cb_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: cb_ptr came from `Box::leak` in `register_au_inner`,
    // valid for the process lifetime.
    let cb = unsafe { &*cb_ptr };

    // Plugin reports it has no editor — return nil so the host shows
    // its generic parameter UI instead.
    let has_editor = unsafe { (cb.gui_has_editor)(ctx) };
    if has_editor == 0 {
        return std::ptr::null_mut();
    }

    let mut w: u32 = 0;
    let mut h: u32 = 0;
    unsafe { (cb.gui_get_size)(ctx, &raw mut w, &raw mut h) };
    if w == 0 || h == 0 {
        return std::ptr::null_mut();
    }

    // Container NSView the editor's child view will be installed into.
    // AppKit hands the host process a Cocoa NSWindow; `gui_open`
    // calls into baseview which adds the editor's NSView as a subview
    // of `container`.
    let frame = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: f64::from(w),
            height: f64::from(h),
        },
    };

    let nsview_cls = AnyClass::get(c"NSView").expect("NSView class missing — AppKit not loaded?");
    // SAFETY: `+[NSView alloc]` / `-[NSView initWithFrame:]` are the
    // standard pair for NSView construction; both are documented to
    // return a retained instance.
    let container: *mut AnyObject = unsafe {
        let alloc: *mut AnyObject = msg_send![nsview_cls, alloc];
        msg_send![alloc, initWithFrame: frame]
    };
    if container.is_null() {
        return std::ptr::null_mut();
    }

    // SAFETY: `gui_open` is documented to take ownership of the
    // parent NSView pointer for the lifetime of the editor; the
    // host retains the returned NSView via the cocoa view system.
    unsafe { (cb.gui_open)(ctx, container.cast::<c_void>()) };
    container
}

// ---------------------------------------------------------------------------
// AudioToolbox FFI
// ---------------------------------------------------------------------------

const K_TRUCE_PRIVATE_PROPERTY_RUST_CONTEXT: u32 = 64000;
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;

unsafe extern "C" {
    fn AudioUnitGetProperty(
        unit: *mut c_void,
        prop_id: u32,
        scope: u32,
        elem: u32,
        out_data: *mut c_void,
        io_size: *mut u32,
    ) -> i32;
}
