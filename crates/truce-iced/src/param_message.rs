//! Message types for parameter and plugin communication.

use std::fmt::Debug;

/// Low-level parameter edit messages matching the host's begin/set/end protocol.
#[derive(Debug, Clone)]
pub enum ParamMessage {
    /// Begin an edit gesture (mouse-down on a control).
    BeginEdit(u32),
    /// Set a parameter's normalized value (0.0–1.0).
    SetNormalized(u32, f64),
    /// End an edit gesture (mouse-up).
    EndEdit(u32),
    /// Multiple parameter messages in one update (e.g. XY pad).
    Batch(Vec<ParamMessage>),
}

/// Unified message type wrapping both parameter messages and plugin-specific messages.
///
/// Iced requires a single message type per program. This enum merges parameter
/// control messages with the plugin author's custom message type `M`.
#[derive(Debug, Clone)]
pub enum Message<M> {
    /// A parameter value changed (from host automation or GUI interaction).
    Param(ParamMessage),
    /// A meter value was updated (polled at ~60fps).
    Meter(u32, f32),
    /// Tick — fired ~60fps, triggers param/meter polling.
    Tick,
    /// Plugin-specific message.
    Plugin(M),
}
