//! Shared meter storage - the mediation channel between the audio
//! thread (which publishes meter values from inside `process()`) and
//! the GUI thread (which reads them at frame rate).
//!
//! The store lives outside the plugin instance, behind an `Arc`, for
//! the same reason `params_arc` does: the audio thread holds
//! `&mut P` for the full duration of a block, so a GUI closure that
//! dereferenced the instance to call `get_meter` would violate
//! `&mut` exclusivity. Reading a shared atomic slot has no such
//! problem, and is what the LV2 wrapper always did - this type makes
//! that pattern the only one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use truce_params::METER_ID_BASE;

/// Number of meter slots. Meters count upward from
/// [`METER_ID_BASE`]; 256 per plugin is far above any real surface.
const NUM_SLOTS: usize = 256;

/// Fixed array of f32-bit atomic meter slots, indexed by meter id.
///
/// Writers (the shells' `meter_fn`, called from `process()`) and
/// readers (editor `get_meter` closures) address slots by the meter's
/// param-space id (`METER_ID_BASE + index`); ids outside the slot
/// range read as `0.0` and write as a no-op, so a stale or
/// out-of-range id can't panic on either thread.
pub struct MeterStore {
    slots: [AtomicU32; NUM_SLOTS],
}

impl MeterStore {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: std::array::from_fn(|_| AtomicU32::new(0)),
        })
    }

    /// Read the meter value for `meter_id`. `0.0` for ids outside
    /// the slot range.
    #[must_use]
    pub fn read(&self, meter_id: u32) -> f32 {
        // `wrapping_sub` keeps ids below `METER_ID_BASE` from
        // panicking - they wrap to a huge index and fall through to
        // the `None` arm.
        let idx = meter_id.wrapping_sub(METER_ID_BASE) as usize;
        self.slots
            .get(idx)
            .map_or(0.0, |slot| f32::from_bits(slot.load(Ordering::Relaxed)))
    }

    /// Publish the meter value for `meter_id`. No-op for ids outside
    /// the slot range.
    pub fn write(&self, meter_id: u32, value: f32) {
        let idx = meter_id.wrapping_sub(METER_ID_BASE) as usize;
        if let Some(slot) = self.slots.get(idx) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_by_meter_id() {
        let store = MeterStore::new();
        store.write(METER_ID_BASE, 0.5);
        store.write(METER_ID_BASE + 255, -1.0);
        assert!((store.read(METER_ID_BASE) - 0.5).abs() < f32::EPSILON);
        assert!((store.read(METER_ID_BASE + 255) + 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn out_of_range_ids_are_inert() {
        let store = MeterStore::new();
        store.write(0, 1.0);
        store.write(METER_ID_BASE + 256, 1.0);
        assert!(store.read(0).abs() < f32::EPSILON);
        assert!(store.read(METER_ID_BASE + 256).abs() < f32::EPSILON);
    }
}
