//! iOS placeholder editor - no live iced render yet.
//!
//! The desktop editor (`editor.rs`) is wired through baseview +
//! iced_wgpu. Neither is viable in an iOS App Extension sandbox
//! without the UIKit `CAMetalLayer` plumbing that lives in
//! `truce-gui` only. The placeholder here:
//!
//! - Implements `Editor` so plugins consuming `truce_iced::IcedEditor`
//!   compile cleanly for iOS targets.
//! - Attaches a `UIView` with a label so an installed plugin's editor
//!   paints something instead of black.
//! - Preserves the `IcedPlugin` trait shape so plugin code stays
//!   portable across platforms.

#![cfg(target_os = "ios")]

use std::marker::PhantomData;
use std::sync::Arc;

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_gui::layout::GridLayout;
use truce_params::Params;

pub trait IcedPlugin<P: Params>: Sized + 'static {
    type Message: 'static + Send + Clone + std::fmt::Debug;
    fn new(params: Arc<P>) -> Self;
}

/// Built-in `IcedPlugin` placeholder used by `from_layout`.
pub struct AutoPlugin {
    #[allow(dead_code)]
    layout: GridLayout,
}

impl<P: Params> IcedPlugin<P> for AutoPlugin {
    type Message = ();
    fn new(_: Arc<P>) -> Self {
        panic!("AutoPlugin must be created via IcedEditor::from_layout")
    }
}

pub struct IcedEditor<P, M>
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    #[allow(dead_code)]
    params: Arc<P>,
    size: (u32, u32),
    child_view: *mut AnyObject,
    _marker: PhantomData<fn(M)>,
}

// SAFETY: see truce-slint/editor_ios.rs for the symmetric rationale.
unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedEditor<P, M> {}

impl<P: Params + 'static, M: IcedPlugin<P>> IcedEditor<P, M> {
    pub fn new(params: Arc<P>, size: (u32, u32)) -> Self {
        Self {
            params,
            size,
            child_view: std::ptr::null_mut(),
            _marker: PhantomData,
        }
    }
}

impl<P: Params + 'static> IcedEditor<P, AutoPlugin> {
    /// Create an editor that would auto-generate the UI from a
    /// `GridLayout` on the desktop. On iOS, the stub does no
    /// rendering - the layout is held for future use.
    pub fn from_layout(params: Arc<P>, layout: GridLayout) -> Self {
        let size = (layout.width, layout.height);
        let _ = layout;
        Self {
            params,
            size,
            child_view: std::ptr::null_mut(),
            _marker: PhantomData,
        }
    }
}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> Editor for IcedEditor<P, M> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, _context: PluginContext) {
        let RawWindowHandle::UiKit(parent_ptr) = parent else {
            log::warn!("IcedEditor (iOS stub) got non-UiKit parent handle");
            return;
        };
        if parent_ptr.is_null() {
            return;
        }
        unsafe {
            self.child_view = build_placeholder_view(parent_ptr.cast(), self.size);
        }
    }

    fn close(&mut self) {
        if !self.child_view.is_null() {
            unsafe {
                let _: () = msg_send![self.child_view, removeFromSuperview];
            }
            self.child_view = std::ptr::null_mut();
        }
    }
}

unsafe fn build_placeholder_view(parent: *mut AnyObject, size: (u32, u32)) -> *mut AnyObject {
    unsafe {
        let uiview = AnyClass::get(c"UIView").expect("UIView missing");
        let frame = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: f64::from(size.0),
                height: f64::from(size.1),
            },
        };
        let alloc: *mut AnyObject = msg_send![uiview, alloc];
        let view: *mut AnyObject = msg_send![alloc, initWithFrame: frame];
        if view.is_null() {
            return std::ptr::null_mut();
        }
        let color_cls = AnyClass::get(c"UIColor").expect("UIColor missing");
        let bg: *mut AnyObject = msg_send![color_cls, darkGrayColor];
        let _: () = msg_send![view, setBackgroundColor: bg];
        let label_cls = AnyClass::get(c"UILabel").expect("UILabel missing");
        let label_alloc: *mut AnyObject = msg_send![label_cls, alloc];
        let label_frame = NSRect {
            origin: NSPoint { x: 8.0, y: 8.0 },
            size: NSSize {
                width: f64::from(size.0).max(0.0) - 16.0,
                height: f64::from(size.1).max(0.0) - 16.0,
            },
        };
        let label: *mut AnyObject = msg_send![label_alloc, initWithFrame: label_frame];
        let txt = NSString::from_str(
            "iced editor placeholder on iOS.\n\
             Build + parameter wiring work end-to-end;\n\
             the iced render pump is not yet hooked up.",
        );
        let _: () = msg_send![label, setText: &*txt];
        let _: () = msg_send![label, setNumberOfLines: 0_isize];
        let white: *mut AnyObject = msg_send![color_cls, whiteColor];
        let _: () = msg_send![label, setTextColor: white];
        let _: () = msg_send![view, addSubview: label];
        let _: () = msg_send![parent, addSubview: view];
        view
    }
}
