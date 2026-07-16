//! Custom state serialization for plugin-specific persistent data.
//!
//! Use `#[derive(State)]` on a struct to auto-generate binary serialization:
//!
//! ```ignore
//! #[derive(State, Default)]
//! pub struct MyState {
//!     pub instance_name: String,
//!     pub view_mode: u8,
//!     pub selected_ids: Vec<u32>,
//! }
//! ```
//!
//! Then persist it through your plugin's `snapshot_into`/`load_state` -
//! the real-time-safe custom-state path. `snapshot_into` publishes into a
//! lock-free slot the host reads without stalling audio, and CLAP / VST3
//! / AU persist from it:
//!
//! ```ignore
//! fn snapshot_into(state: &Self::DspState, buf: &mut Vec<u8>) -> bool {
//!     buf.extend_from_slice(&state.persistent.serialize());
//!     true
//! }
//! fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
//!     match MyState::deserialize(data) {
//!         Some(s) => { state.persistent = s; Ok(()) }
//!         None => Err(StateLoadError::Malformed("MyState")),
//!     }
//! }
//! ```
//!
//! For MB-scale state (a sampler's audio, big wavetables), publish it off
//! the audio thread via `InitContext::snapshot_publisher()` instead. The
//! older `save_state` still works, but on CLAP / VST3 / AU it runs on the
//! audio thread, so prefer `snapshot_into`.
//!
//! ## Schema evolution
//!
//! The codec is **keyed by field name** (like `#[persist]`), so evolving
//! the struct is safe: adding, removing, *and* reordering fields all keep
//! every surviving field's value. A field absent from an older blob loads
//! as its `Default`; a field present in the blob but no longer declared is
//! ignored. Two fields whose names collide under the key hash are rejected
//! at compile time. Pre-keyed blobs (saved by an older truce) still load
//! through a legacy positional path, so existing sessions are unaffected.

/// Re-exported so plugin authors name the lock-free `#[persist]` cell
/// as `AtomicCell` through the prelude, without depending on crossbeam.
pub use crossbeam_utils::atomic::AtomicCell;

/// Cursor for reading binary state data.
pub struct StateCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> StateCursor<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn read_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return None;
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Some(slice)
    }

    /// Skip the next field (reads its encoded size and advances past it).
    /// Returns false if the data is malformed.
    ///
    /// # Panics
    ///
    /// Does not panic - the `expect` inside is unreachable because
    /// `read_bytes(4)` only returns `Some` when the slice is exactly
    /// 4 bytes long.
    pub fn skip_field(&mut self) -> bool {
        // Fields are prefixed with a u32 byte length by the derive macro.
        if let Some(bytes) = self.read_bytes(4) {
            // `read_bytes(4)` returns `Some(slice of length 4)` or
            // `None`, so the `try_into::<[u8; 4]>()` here cannot fail.
            // The `expect` documents that invariant for readers.
            let len = u32::from_le_bytes(
                bytes
                    .try_into()
                    .expect("read_bytes(4) returned a slice of unexpected length"),
            ) as usize;
            if self.pos + len <= self.data.len() {
                self.pos += len;
                return true;
            }
        }
        false
    }
}

/// Trait for types that can be serialized as a single state field.
///
/// Implemented for primitives, `String`, `Vec<T>`, and `Option<T>`.
pub trait StateField: Sized {
    fn write_field(&self, buf: &mut Vec<u8>);
    fn read_field(cursor: &mut StateCursor) -> Option<Self>;
}

/// Trait for custom plugin state structs.
///
/// Derive with `#[derive(State)]`. The struct must also implement `Default`
/// so a field absent from an older (or reordered / trimmed) blob loads as
/// its default - the codec matches fields by name, so schema evolution is
/// safe in every direction (see the module docs).
pub trait State: Sized + Default {
    /// Serialize into `buf`. Clears `buf` first, then reuses its
    /// capacity - calling this repeatedly with the same buffer is
    /// allocation-free once warmed, so it is the form to use on the
    /// audio thread (e.g. `PluginLogic::snapshot_into`).
    fn serialize_into(&self, buf: &mut Vec<u8>);

    /// Serialize to a fresh `Vec`. Convenience wrapper over
    /// [`Self::serialize_into`]; allocates, so prefer `serialize_into`
    /// on the real-time path.
    #[must_use]
    fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.serialize_into(&mut buf);
        buf
    }

    fn deserialize(data: &[u8]) -> Option<Self>;
}

/// A `#[persist]` field on a `#[derive(Params)]` struct: an
/// interior-mutable, `Sync` wrapper around a [`StateField`] value that
/// the host saves alongside the parameter values (session / preset) and
/// restores on load. Implemented for `RwLock<T>` / `Mutex<T>` where
/// `T: StateField` (a primitive, `String`, `Vec`, `Option`, or a
/// `#[derive(State)]` struct - the derive also emits a `StateField`
/// impl). Interior mutability is required because a load reaches the
/// field through `&Params` (the store is shared via `Arc<Params>`).
pub trait PersistField {
    /// Append the current value's bytes.
    fn persist_write(&self, buf: &mut Vec<u8>);
    /// Read a value from `cursor` and store it in place. A short or
    /// malformed read leaves the current value untouched.
    fn persist_read(&self, cursor: &mut StateCursor);
}

impl<T: StateField> PersistField for std::sync::RwLock<T> {
    fn persist_write(&self, buf: &mut Vec<u8>) {
        if let Ok(guard) = self.read() {
            guard.write_field(buf);
        }
    }
    fn persist_read(&self, cursor: &mut StateCursor) {
        if let Some(value) = T::read_field(cursor)
            && let Ok(mut guard) = self.write()
        {
            *guard = value;
        }
    }
}

impl<T: StateField> PersistField for std::sync::Mutex<T> {
    fn persist_write(&self, buf: &mut Vec<u8>) {
        if let Ok(guard) = self.lock() {
            guard.write_field(buf);
        }
    }
    fn persist_read(&self, cursor: &mut StateCursor) {
        if let Some(value) = T::read_field(cursor)
            && let Ok(mut guard) = self.lock()
        {
            *guard = value;
        }
    }
}

/// Lock-free-friendly cell for `Copy` config: a persisted `f32`, small
/// enum, or index reads as `AtomicCell<T>` instead of sitting behind a
/// `Mutex`. `AtomicCell` is genuinely atomic for word-sized types and
/// falls back to an internal lock for larger ones.
impl<T: StateField + Copy> PersistField for AtomicCell<T> {
    fn persist_write(&self, buf: &mut Vec<u8>) {
        self.load().write_field(buf);
    }
    fn persist_read(&self, cursor: &mut StateCursor) {
        if let Some(value) = T::read_field(cursor) {
            self.store(value);
        }
    }
}

// ---------------------------------------------------------------------------
// StateField implementations for primitives
// ---------------------------------------------------------------------------

macro_rules! impl_state_field_int {
    ($($ty:ty),*) => {
        $(
            impl StateField for $ty {
                fn write_field(&self, buf: &mut Vec<u8>) {
                    buf.extend_from_slice(&self.to_le_bytes());
                }
                fn read_field(cursor: &mut StateCursor) -> Option<Self> {
                    let bytes = cursor.read_bytes(std::mem::size_of::<Self>())?;
                    Some(Self::from_le_bytes(bytes.try_into().ok()?))
                }
            }
        )*
    };
}

impl_state_field_int!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

impl StateField for bool {
    fn write_field(&self, buf: &mut Vec<u8>) {
        buf.push(u8::from(*self));
    }
    fn read_field(cursor: &mut StateCursor) -> Option<Self> {
        let b = cursor.read_bytes(1)?;
        Some(b[0] != 0)
    }
}

impl StateField for String {
    fn write_field(&self, buf: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        crate::cast::len_u32(bytes.len()).write_field(buf);
        buf.extend_from_slice(bytes);
    }
    fn read_field(cursor: &mut StateCursor) -> Option<Self> {
        let len = u32::read_field(cursor)? as usize;
        let bytes = cursor.read_bytes(len)?;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

impl<T: StateField> StateField for Vec<T> {
    fn write_field(&self, buf: &mut Vec<u8>) {
        crate::cast::len_u32(self.len()).write_field(buf);
        for item in self {
            item.write_field(buf);
        }
    }
    fn read_field(cursor: &mut StateCursor) -> Option<Self> {
        let len = u32::read_field(cursor)? as usize;
        let mut vec = Vec::with_capacity(len.min(1024));
        for _ in 0..len {
            vec.push(T::read_field(cursor)?);
        }
        Some(vec)
    }
}

impl<T: StateField> StateField for Option<T> {
    fn write_field(&self, buf: &mut Vec<u8>) {
        match self {
            Some(val) => {
                1u8.write_field(buf);
                val.write_field(buf);
            }
            None => {
                0u8.write_field(buf);
            }
        }
    }
    fn read_field(cursor: &mut StateCursor) -> Option<Self> {
        let tag = u8::read_field(cursor)?;
        if tag == 0 {
            Some(None)
        } else {
            Some(Some(T::read_field(cursor)?))
        }
    }
}

// ---------------------------------------------------------------------------
// StateBinding - typed wrapper for editor state access
// ---------------------------------------------------------------------------

use crate::editor::PluginContext;
use std::sync::Arc;

/// Typed state binding for editors.
///
/// Wraps the `get_state`/`set_state` closures from `PluginContext` with
/// typed serialization. Caches the deserialized state to avoid repeated
/// deserialization each frame.
///
/// ```ignore
/// struct MyEditor {
///     state: StateBinding<PersistentState>,
/// }
///
/// // In open():
/// self.state = StateBinding::new(&context);
///
/// // In state_changed():
/// self.state.sync();
///
/// // Reading:
/// let name = &self.state.get().instance_name;
///
/// // Writing:
/// self.state.update(|s| s.instance_name = new_name);
/// ```
pub struct StateBinding<T: State> {
    cached: T,
    get_state: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    set_state: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
}

impl<T: State> StateBinding<T> {
    /// Create a new binding from a [`PluginContext`]. Generic over the
    /// context's `<P>` since `StateBinding` cares only about the
    /// `get_state` / `set_state` channel on the underlying
    /// `EditorBridge`, never about parameter typing.
    #[must_use]
    pub fn new<P: ?Sized>(context: &PluginContext<P>) -> Self {
        let bridge_for_get = Arc::clone(context.bridge());
        let bridge_for_set = Arc::clone(context.bridge());
        let mut binding = Self {
            cached: T::default(),
            get_state: Arc::new(move || bridge_for_get.get_state()),
            set_state: Arc::new(move |data| bridge_for_set.set_state(data)),
        };
        binding.sync();
        binding
    }

    /// Re-read state from the plugin. Call this from `state_changed()`.
    pub fn sync(&mut self) {
        let data = (self.get_state)();
        if !data.is_empty()
            && let Some(s) = T::deserialize(&data)
        {
            self.cached = s;
        }
    }

    /// Get the current cached state.
    pub fn get(&self) -> &T {
        &self.cached
    }

    /// Modify state and write it back to the plugin.
    pub fn update(&mut self, f: impl FnOnce(&mut T)) {
        f(&mut self.cached);
        let data = self.cached.serialize();
        (self.set_state)(data);
    }
}

impl<T: State> Default for StateBinding<T> {
    /// Construct an **unwired** binding: `get()` returns `T::default()`
    /// and `update()` *silently discards* the new state. Only useful
    /// as a placeholder before the editor is opened; replace with
    /// [`StateBinding::new(&context)`](StateBinding::new) inside
    /// `Editor::open` once a [`PluginContext`] is available. If you
    /// see writes vanishing, check that the binding has been wired up
    /// before you call `update`.
    fn default() -> Self {
        Self {
            cached: T::default(),
            get_state: Arc::new(Vec::new),
            set_state: Arc::new(|_| {}),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        let mut buf = Vec::new();
        42u32.write_field(&mut buf);
        2.5f64.write_field(&mut buf);
        true.write_field(&mut buf);

        let mut cursor = StateCursor::new(&buf);
        assert_eq!(u32::read_field(&mut cursor), Some(42));
        assert_eq!(f64::read_field(&mut cursor), Some(2.5));
        assert_eq!(bool::read_field(&mut cursor), Some(true));
    }

    #[test]
    fn string_round_trip() {
        let mut buf = Vec::new();
        "hello world".to_string().write_field(&mut buf);

        let mut cursor = StateCursor::new(&buf);
        assert_eq!(
            String::read_field(&mut cursor),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn vec_round_trip() {
        let mut buf = Vec::new();
        vec![1u32, 2, 3].write_field(&mut buf);

        let mut cursor = StateCursor::new(&buf);
        assert_eq!(Vec::<u32>::read_field(&mut cursor), Some(vec![1, 2, 3]));
    }

    #[test]
    fn option_round_trip() {
        let mut buf = Vec::new();
        Some(42u32).write_field(&mut buf);
        None::<u32>.write_field(&mut buf);

        let mut cursor = StateCursor::new(&buf);
        assert_eq!(Option::<u32>::read_field(&mut cursor), Some(Some(42)));
        assert_eq!(Option::<u32>::read_field(&mut cursor), Some(None));
    }

    #[test]
    fn nested_vec_string() {
        let mut buf = Vec::new();
        let v = vec!["foo".to_string(), "bar".to_string()];
        v.write_field(&mut buf);

        let mut cursor = StateCursor::new(&buf);
        assert_eq!(Vec::<String>::read_field(&mut cursor), Some(v));
    }

    #[test]
    fn persist_field_lock_round_trip() {
        let src = std::sync::RwLock::new("guitar bus".to_string());
        let mut buf = Vec::new();
        src.persist_write(&mut buf);

        let dst = std::sync::RwLock::new(String::new());
        dst.persist_read(&mut StateCursor::new(&buf));
        assert_eq!(*dst.read().unwrap(), "guitar bus");

        let m_src = std::sync::Mutex::new(vec![1u32, 2, 3]);
        let mut m_buf = Vec::new();
        m_src.persist_write(&mut m_buf);
        let m_dst = std::sync::Mutex::new(Vec::<u32>::new());
        m_dst.persist_read(&mut StateCursor::new(&m_buf));
        assert_eq!(*m_dst.lock().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn persist_field_atomic_cell_round_trip() {
        let src = AtomicCell::new(0.75f32);
        let mut buf = Vec::new();
        src.persist_write(&mut buf);

        let dst = AtomicCell::new(0.0f32);
        dst.persist_read(&mut StateCursor::new(&buf));
        // Bit-exact: the value round-trips through `to_le_bytes`.
        assert_eq!(dst.load().to_bits(), 0.75f32.to_bits());
    }

    #[test]
    fn persist_field_leaves_value_on_short_read() {
        // A truncated blob must not clobber the current value.
        let dst = AtomicCell::new(42u32);
        dst.persist_read(&mut StateCursor::new(&[0xFF, 0xFF]));
        assert_eq!(dst.load(), 42);
    }
}
