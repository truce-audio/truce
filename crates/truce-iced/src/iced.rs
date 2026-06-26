//! Internal facade mirroring the `iced` umbrella crate's surface, sourced
//! from iced's sub-crates directly.
//!
//! The umbrella unconditionally depends on `iced_winit`, whose 0.14
//! keyboard conversion calls a winit API (`KeyEventExtModifierSupplement`)
//! that winit exposes only on desktop - so the umbrella can't compile for
//! iOS. truce-iced drives `UserInterface` itself (baseview on desktop, a
//! `CAMetalLayer` host on iOS) and never needs winit's event loop, so we
//! bypass the umbrella entirely and the same code compiles on every
//! target.
//!
//! With the `wgpu` feature and no `tiny-skia`, `iced_renderer::Renderer`
//! resolves to `iced_wgpu::Renderer`, so the editor's concrete
//! `iced_wgpu::Renderer` and these defaulted `Element` / widget types are
//! one and the same - exactly as the umbrella arranged.

pub use iced_core::InputMethod;
pub use iced_core::{
    Alignment, Border, Color, Element as CoreElement, Event, Font, Length, Padding, Pixels, Point,
    Radians, Rectangle, Size, Theme, Vector,
};
pub use iced_core::{alignment, border, color, mouse, padding, theme, touch};
pub use iced_futures::{Subscription, futures};
pub use iced_renderer::Renderer;
pub use iced_runtime::Task;

pub mod event {
    pub use iced_core::event::{Event, Status};
    pub use iced_futures::event::{listen, listen_raw, listen_with};
}

pub mod font {
    pub use iced_core::font::*;
    pub use iced_runtime::font::*;
}

pub mod keyboard {
    pub use iced_core::keyboard::{Event, Key, Location, Modifiers, key};
    pub use iced_futures::keyboard::listen;
}

pub mod window {
    pub use iced_core::window::*;
}

pub mod widget {
    pub use iced_runtime::widget::*;
    pub use iced_widget::*;
}

/// `crate::iced::Element` with the umbrella's defaults (the shared `Theme` and
/// the wgpu renderer). Spelled as a generic alias so two-parameter uses
/// (`Element<'a, Message>`) keep working unchanged.
pub type Element<'a, Message, Theme = iced_core::Theme, Renderer = iced_renderer::Renderer> =
    iced_core::Element<'a, Message, Theme, Renderer>;
