//! A small generational arena.
//!
//! The host addresses model objects by the opaque `ARA*Ref` values the
//! plug-in hands back. Rather than leak a raw `Box` pointer per object
//! (a stale ref would then be an immediate use-after-free), truce stores
//! objects in an arena and encodes an `(index, generation)` [`Handle`]
//! into the ref. A ref that outlives its object fails the generation
//! check and reads back as `None` instead of touching freed memory.

use core::ffi::c_void;

/// A generation-checked slot handle. Encoded into an `ARA*Ref` for the
/// host and decoded back on each call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Handle {
    index: u32,
    generation: u32,
}

impl Handle {
    /// Encode as a non-null pointer-sized ref: `(generation << 32) |
    /// (index + 1)`. The `+ 1` keeps index 0 / generation 0 from
    /// colliding with a null ref.
    pub(crate) fn to_ref(self) -> *mut c_void {
        let bits = (u64::from(self.generation) << 32) | (u64::from(self.index) + 1);
        // All truce targets are 64-bit, so `usize` holds the 64-bit
        // encoding without truncation.
        #[allow(clippy::cast_possible_truncation)]
        {
            bits as usize as *mut c_void
        }
    }

    /// Decode a ref produced by [`Self::to_ref`]. Returns `None` for a
    /// null / zero ref.
    pub(crate) fn from_ref(r: *mut c_void) -> Option<Self> {
        let bits = r as usize as u64;
        if bits == 0 {
            return None;
        }
        let low = (bits & 0xFFFF_FFFF) as u32;
        let index = low.checked_sub(1)?;
        let generation = (bits >> 32) as u32;
        Some(Self { index, generation })
    }
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// A generation-checked slab. Reuses freed slots, bumping their
/// generation so a stale [`Handle`] can't alias the reused object.
pub(crate) struct Arena<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

impl<T> Arena<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    pub(crate) fn insert(&mut self, value: T) -> Handle {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.value = Some(value);
            Handle {
                index,
                generation: slot.generation,
            }
        } else {
            // Arenas never approach `u32::MAX` live objects in a session.
            #[allow(clippy::cast_possible_truncation)]
            let index = self.slots.len() as u32;
            self.slots.push(Slot {
                generation: 0,
                value: Some(value),
            });
            Handle {
                index,
                generation: 0,
            }
        }
    }

    pub(crate) fn get(&self, handle: Handle) -> Option<&T> {
        let slot = self.slots.get(handle.index as usize)?;
        (slot.generation == handle.generation)
            .then_some(slot.value.as_ref())
            .flatten()
    }

    pub(crate) fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation == handle.generation {
            slot.value.as_mut()
        } else {
            None
        }
    }

    pub(crate) fn remove(&mut self, handle: Handle) -> Option<T> {
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        let value = slot.value.take();
        if value.is_some() {
            slot.generation = slot.generation.wrapping_add(1);
            self.free.push(handle.index);
        }
        value
    }

    /// Number of live objects. (Used by the model-graph tests and by
    /// later phases' introspection.)
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.value.is_some()).count()
    }

    /// Iterate the live objects (order unspecified). Used by archive
    /// store/restore, which walk every object.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|s| s.value.as_ref())
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|s| s.value.as_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_ref_roundtrips_and_avoids_null() {
        let h = Handle {
            index: 0,
            generation: 0,
        };
        assert!(!h.to_ref().is_null(), "index 0 / gen 0 must not be null");
        assert_eq!(Handle::from_ref(h.to_ref()), Some(h));

        let h2 = Handle {
            index: 7,
            generation: 3,
        };
        assert_eq!(Handle::from_ref(h2.to_ref()), Some(h2));
        assert_eq!(Handle::from_ref(core::ptr::null_mut()), None);
    }

    #[test]
    fn stale_handle_fails_generation_check() {
        let mut arena = Arena::new();
        let a = arena.insert("a");
        assert_eq!(arena.get(a), Some(&"a"));
        assert_eq!(arena.remove(a), Some("a"));
        // The slot is reused with a bumped generation.
        let b = arena.insert("b");
        assert_eq!(arena.get(a), None, "stale handle must not resolve");
        assert_eq!(arena.get(b), Some(&"b"));
        assert_eq!(arena.len(), 1);
    }
}
