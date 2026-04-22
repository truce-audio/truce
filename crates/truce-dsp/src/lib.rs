//! Realtime-safe DSP utilities shared across truce plugins.
//!
//! The primary inhabitant today is [`audio_tap`], a lock-free SPSC ring
//! for handing audio-derived data from the DSP thread to the editor /
//! UI thread (oscilloscopes, spectrum, waveform history, visualizers).
//!
//! Future additions (smoothers, envelope followers, etc.) can live
//! alongside it; nothing here depends on the wider truce framework.

pub mod audio_tap;

pub use audio_tap::{audio_tap, AudioTapConsumer, AudioTapProducer};
