## State persistence

Parameter values are saved and restored automatically. But if your
plugin has extra state (instance names, view modes, loaded files,
custom curves), you have two options.

### Option 1: `#[derive(State)]` (recommended)

Define a state struct and derive binary serialization:

```rust
#[derive(State, Default)]
pub struct MyState {
    pub ir_file_path: String,
    pub view_mode: u8,
    pub selected_ids: Vec<u32>,
}
```

Then implement `save_state` / `load_state` on your plugin:

```rust
pub struct MyPlugin {
    params: Arc<MyPluginParams>,
    state: MyState,
}

impl PluginLogic for MyPlugin {
    fn save_state(&self) -> Vec<u8> {
        self.state.serialize()
    }

    fn load_state(&mut self, data: &[u8]) {
        if let Some(s) = MyState::deserialize(data) {
            self.state = s;
        }
    }

    // ...
}
```

Supported field types: `u8`..`u64`, `i8`..`i64`, `f32`, `f64`, `bool`,
`String`, `Vec<T>`, `Option<T>`, and nested `State` structs.

The format is forward-compatible: if you add fields to the struct later,
old saved state will deserialize successfully with defaults for new fields.

### Option 2: Manual serialization

If you need a custom format or have complex data:

```rust
fn save_state(&self) -> Vec<u8> {
    bincode::serialize(&self.extra).unwrap()
}

fn load_state(&mut self, data: &[u8]) {
    if let Ok(state) = bincode::deserialize::<ExtraState>(data) {
        self.extra = state;
    }
}
```

### How it works

The framework handles the outer serialization envelope -- your
`save_state` bytes are embedded inside a versioned container
alongside parameter values. You don't need to save params yourself.

### Editor state sync

If your editor caches custom state, it needs to know when state
changes (preset recall, undo, session load). Use `StateBinding<T>`:

```rust
struct MyEditor {
    state: StateBinding<MyState>,
}

impl Editor for MyEditor {
    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        self.state = StateBinding::new(&context);
        // ...
    }

    fn state_changed(&mut self) {
        self.state.sync();
    }
}

// Reading state:
let path = &self.state.get().ir_file_path;

// Writing state (e.g., user renames the instance):
self.state.update(|s| s.ir_file_path = new_path);
```

`StateBinding<T>` handles serialization and communication with the
plugin automatically. `sync()` re-reads from the plugin; `update()`
writes back.

If your plugin only uses `#[param]` fields and no custom state, you
don't need any of this. The built-in GUI reads parameters every frame.

---

[← Previous](09-hot-reload.md) | [Next →](11-building.md) | [Index](README.md)
