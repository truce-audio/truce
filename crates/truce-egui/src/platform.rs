//! Platform window bridging for baseview - re-exports from
//! `truce_gui::platform`.
//!
//! iOS uses the stub editor in `editor_ios.rs` and doesn't go
//! through the baseview / wgpu path, so the heavier re-exports
//! (`ParentWindow`, `create_wgpu_surface`) gate to non-iOS.

#[cfg(not(target_os = "ios"))]
pub use truce_gui::platform::{ParentWindow, create_wgpu_surface};

pub use truce_gui::platform::query_backing_scale;
