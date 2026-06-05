//! Portable SIMD primitives + canonical block-rate audio ops.
//!
//! Two backends, feature-gated:
//! - `wide-backend` (default): stable Rust, uses the `wide` crate.
//!   Maps `f32x4`/`f32x8`/`f64x2`/`f64x4` onto SSE2 / AVX2 on x86,
//!   NEON on `AArch64`, scalar fallback elsewhere.
//! - (planned) `portable-simd-backend`: nightly-only, swaps in
//!   `core::simd` when it stabilizes. Same public type aliases.
//!
//! Consumers never name the backend - they import
//! `truce_simd::ops` (f32) / `truce_simd::ops64` (f64) and the
//! correct intrinsics get wired in automatically.
//!
//! All ops here are pure math. No atomics, no parameter reads, no
//! audio-thread allocation. They're meant to be the inner-loop
//! complement to a `truce_params::FloatParam::read_into(&mut buf)` /
//! `truce_core::AudioBuffer::chunks_mut::<N>()` driver in
//! `process()`.

#![allow(clippy::module_name_repetitions)]

pub mod math;
pub mod ops;
pub mod ops64;

/// Lane-width aliases. Stable across backends so consumers don't
/// have to rewrite types when we flip the default to
/// `portable-simd-backend`.
#[cfg(feature = "wide-backend")]
pub mod simd {
    pub use wide::{f32x4, f32x8, f64x2, f64x4};
}
