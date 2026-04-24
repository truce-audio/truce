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
//! Then use it in your plugin's `save_state`/`load_state`:
//!
//! ```ignore
//! fn save_state(&self) -> Vec<u8> {
//!     self.persistent.serialize()
//! }
//! fn load_state(&mut self, data: &[u8]) {
//!     if let Some(s) = MyState::deserialize(data) {
//!         self.persistent = s;
//!     }
//! }
//! ```

/// Cursor for reading binary state data.
pub struct StateCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> StateCursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

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
    pub fn skip_field(&mut self) -> bool {
        // Fields are prefixed with a u32 byte length by the derive macro.
        if let Some(bytes) = self.read_bytes(4) {
            let len = u32::from_le_bytes(bytes.try_into().unwrap()) as usize;
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
/// so missing fields can be filled with defaults when loading old state.
pub trait State: Sized + Default {
    fn serialize(&self) -> Vec<u8>;
    fn deserialize(data: &[u8]) -> Option<Self>;
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
        buf.push(if *self { 1 } else { 0 });
    }
    fn read_field(cursor: &mut StateCursor) -> Option<Self> {
        let b = cursor.read_bytes(1)?;
        Some(b[0] != 0)
    }
}

impl StateField for String {
    fn write_field(&self, buf: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        (bytes.len() as u32).write_field(buf);
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
        (self.len() as u32).write_field(buf);
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
// StateBinding — typed wrapper for editor state access
// ---------------------------------------------------------------------------

use crate::editor::EditorContext;

/// Typed state binding for editors.
///
/// Wraps the `get_state`/`set_state` closures from `EditorContext` with
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
    get_state: std::sync::Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    set_state: std::sync::Arc<dyn Fn(Vec<u8>) + Send + Sync>,
}

impl<T: State> StateBinding<T> {
    /// Create a new binding from an editor context.
    pub fn new(context: &EditorContext) -> Self {
        let mut binding = Self {
            cached: T::default(),
            get_state: context.get_state.clone(),
            set_state: context.set_state.clone(),
        };
        binding.sync();
        binding
    }

    /// Re-read state from the plugin. Call this from `state_changed()`.
    pub fn sync(&mut self) {
        let data = (self.get_state)();
        if !data.is_empty() {
            if let Some(s) = T::deserialize(&data) {
                self.cached = s;
            }
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
    fn default() -> Self {
        Self {
            cached: T::default(),
            get_state: std::sync::Arc::new(|| Vec::new()),
            set_state: std::sync::Arc::new(|_| {}),
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
        3.14f64.write_field(&mut buf);
        true.write_field(&mut buf);

        let mut cursor = StateCursor::new(&buf);
        assert_eq!(u32::read_field(&mut cursor), Some(42));
        assert_eq!(f64::read_field(&mut cursor), Some(3.14));
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
}
