//! ABI canary - runtime verification that shell and dylib have
//! compatible type layouts across the hot-reload boundary. The dylib
//! exports a flat set of Rust-ABI symbols over an opaque state pointer
//! (no trait object), so this checks the sizes / layouts those symbols
//! pass by value or reference, plus the rustc build hash and sample
//! precision. Per-plugin `State` identity rides a separate
//! `truce_state_fingerprint` symbol, not this struct.

use std::mem::{align_of, size_of};
use std::ptr;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, TransportInfo as Transport};
use truce_core::process::{ProcessContext, ProcessStatus};
// Source canary types from `truce-gui-types` (the lightweight types
// crate) so the canary - which every shell needs - stays available even
// when `builtin-gui` is off and the heavy `truce-gui` renderer crate is
// out of the dep graph.
use truce_gui_types::interaction::WidgetRegion;
use truce_gui_types::layout::GridLayout;
use truce_gui_types::theme::{Color, Theme};

/// Hand-bumped ABI epoch. Sizes and alignments can't see every
/// layout change: `Event::port` landed in former padding, so
/// `event_size` stayed the same while a stale shell would read
/// uninitialized padding as the port. Bump this when any
/// boundary-crossing type changes layout invisibly to the size /
/// align fields below. When `AbiCanary` itself gains or loses a
/// field, bump the *export symbol* version instead
/// (`truce_abi_canary_vN`) - the canary crosses the boundary by
/// value, so two different canary layouts must never call each
/// other.
///
/// Epoch 2: `Event` grew `port: u8` in former padding (multi-port
/// MIDI).
/// Epoch 3: `editor` left the `PluginLogicCore` vtable (it moved to the
/// receiverless `truce_build_editor` export). The slot's removal shifts
/// every later vtable index, so a stale epoch-2 dylib would bind
/// `save_state` / `latency` / `tail` to the wrong slots and lacks the
/// new symbol; this bump rejects it cleanly at the canary instead of
/// relying on the probe to notice the misalignment.
/// Epoch 4: `reset` takes `&AudioConfig` instead of `(f64, usize)`, and
/// `ProcessContext` gained a `process_mode` field. A stale epoch-3 dylib
/// would call `reset` with mismatched arguments and read a differently
/// sized context; this bump rejects it before either can happen.
/// Epoch 5: the dylib boundary stopped being a `dyn PluginLogicCore`
/// vtable and became a flat set of Rust-ABI symbols over an opaque
/// `*mut ()` state pointer (state now lives in the shell, so it can
/// survive a code-only reload). The vtable probe is gone; a stale
/// epoch-4 dylib exports the old `truce_create` shape and lacks the new
/// symbols, so it fails at `dlsym` - this bump rejects it earlier and
/// with a clearer message.
pub const ABI_EPOCH: u32 = 5;

/// ABI fingerprint. Compared between shell and dylib before loading.
///
/// This is the ONE `#[repr(C)]` type in the system - it's the
/// bootstrap verification struct that makes everything else safe.
#[repr(C)]
pub struct AbiCanary {
    /// [`ABI_EPOCH`] the side was built with; see its rules for when
    /// to bump what.
    pub abi_epoch: u32,
    pub audio_buffer_size: usize,
    pub process_context_size: usize,
    pub process_status_size: usize,
    pub event_size: usize,
    pub event_body_size: usize,
    pub transport_size: usize,
    pub widget_region_size: usize,
    pub theme_size: usize,
    pub plugin_layout_size: usize,
    pub color_size: usize,
    pub vec_u8_size: usize,
    pub option_usize_size: usize,
    pub audio_buffer_align: usize,
    pub process_status_align: usize,
    pub result_normal_disc: u8,
    pub result_tail_disc: u8,
    pub result_keepalive_disc: u8,
    pub rustc_version_hash: u64,
    /// Bit-width of the plugin's chosen sample type - `32` for `f32`,
    /// `64` for `f64`. Without this field, a shell built against
    /// `prelude` (f32) loading a logic dylib built against `prelude64`
    /// would bind to a vtable whose `process()` slot expects
    /// `AudioBuffer<f64>` - silent UB on the first audio block. The
    /// width difference between the two `AudioBuffer<S>` instantiations
    /// (and `dyn PluginLogic<S>`) is invisible at the dyn-trait
    /// boundary, so a structural canary alone wouldn't catch it.
    pub sample_precision: u8,
}

impl AbiCanary {
    /// Build the canary for a specific sample precision `S`. The
    /// shell calls this with its own `S`; the dylib's
    /// `truce_abi_canary_v2` export does the same with its own (from the
    /// prelude alias). The two are compared at load time.
    #[must_use]
    pub fn current<S: truce_params::sample::Sample>() -> Self {
        // 8× sizeof gives us 32 for f32 / 64 for f64; the cast to u8
        // can't overflow for any plausible sample type.
        #[allow(clippy::cast_possible_truncation)]
        let sample_precision = (size_of::<S>() * 8) as u8;
        Self {
            abi_epoch: ABI_EPOCH,
            audio_buffer_size: size_of::<AudioBuffer<S>>(),
            process_context_size: size_of::<ProcessContext>(),
            process_status_size: size_of::<ProcessStatus>(),
            event_size: size_of::<Event>(),
            event_body_size: size_of::<EventBody>(),
            transport_size: size_of::<Transport>(),
            widget_region_size: size_of::<WidgetRegion>(),
            theme_size: size_of::<Theme>(),
            plugin_layout_size: size_of::<GridLayout>(),
            color_size: size_of::<Color>(),
            vec_u8_size: size_of::<Vec<u8>>(),
            option_usize_size: size_of::<Option<usize>>(),
            audio_buffer_align: align_of::<AudioBuffer<S>>(),
            process_status_align: align_of::<ProcessStatus>(),
            result_normal_disc: discriminant_byte(&ProcessStatus::Normal),
            result_tail_disc: discriminant_byte(&ProcessStatus::Tail(0)),
            result_keepalive_disc: discriminant_byte(&ProcessStatus::KeepAlive),
            rustc_version_hash: rustc_hash(),
            sample_precision,
        }
    }

    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.field_diffs(other).is_empty()
    }

    #[must_use]
    pub fn diff_report(&self, other: &Self) -> String {
        let diffs = self.field_diffs(other);
        if diffs.is_empty() {
            "no differences".into()
        } else {
            format!("ABI mismatches:\n{}", diffs.join("\n"))
        }
    }

    fn field_diffs(&self, other: &Self) -> Vec<String> {
        let mut diffs = Vec::new();
        macro_rules! check {
            ($field:ident) => {
                if self.$field != other.$field {
                    diffs.push(format!(
                        "  {}: shell={}, dylib={}",
                        stringify!($field),
                        self.$field,
                        other.$field
                    ));
                }
            };
        }
        // Single source of truth - adding a field to AbiCanary means
        // adding one line below; `matches` and `diff_report` both
        // reuse this list.
        check!(abi_epoch);
        check!(audio_buffer_size);
        check!(process_context_size);
        check!(process_status_size);
        check!(event_size);
        check!(event_body_size);
        check!(transport_size);
        check!(widget_region_size);
        check!(theme_size);
        check!(plugin_layout_size);
        check!(color_size);
        check!(vec_u8_size);
        check!(option_usize_size);
        check!(audio_buffer_align);
        check!(process_status_align);
        check!(result_normal_disc);
        check!(result_tail_disc);
        check!(result_keepalive_disc);
        check!(rustc_version_hash);
        check!(sample_precision);
        diffs
    }
}

fn discriminant_byte<T>(value: &T) -> u8 {
    // SAFETY: `value: &T` points to a valid `T`, and any `T` has at
    // least its first byte readable (alignment + size > 0). The
    // discriminant of a `#[repr(...)]`-tagged or default-repr enum
    // lives at offset 0, so the first byte is exactly the value the
    // canary wants to compare. For non-enum `T` the byte is whatever
    // the layout puts there - fine, because the canary fields that
    // call this (`result_*_disc`) only pass `ProcessStatus` variants
    // and only compare the result against the matching dylib reading
    // of the same call.
    unsafe { *ptr::from_ref::<T>(value).cast::<u8>() }
}

fn rustc_hash() -> u64 {
    env!("TRUCE_RUSTC_HASH").parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{ABI_EPOCH, AbiCanary};

    #[test]
    fn same_build_matches_itself() {
        let a = AbiCanary::current::<f32>();
        let b = AbiCanary::current::<f32>();
        assert!(a.matches(&b));
    }

    #[test]
    fn epoch_mismatch_fails_and_names_the_field() {
        // A layout change that lands in former padding leaves every
        // size field identical - the epoch is the only tripwire.
        let a = AbiCanary::current::<f32>();
        let mut b = AbiCanary::current::<f32>();
        b.abi_epoch = ABI_EPOCH - 1;
        assert!(!a.matches(&b));
        assert!(a.diff_report(&b).contains("abi_epoch"));
    }

    #[test]
    fn precision_mismatch_fails() {
        let a = AbiCanary::current::<f32>();
        let b = AbiCanary::current::<f64>();
        assert!(!a.matches(&b));
    }
}
