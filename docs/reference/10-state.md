## State persistence

Parameter values are saved and restored automatically. But if your
plugin has extra state (loaded audio files, drawn curves, custom
wavetables), use `save_state` / `load_state`:

```rust
impl PluginLogic for MyConvolver {
    // ...

    fn save_state(&self) -> Vec<u8> {
        let state = ExtraState {
            ir_file_path: self.ir_path.clone(),
            custom_curve: self.curve_points.clone(),
        };
        bincode::serialize(&state).unwrap()
    }

    fn load_state(&mut self, data: &[u8]) {
        if let Ok(state) = bincode::deserialize::<ExtraState>(data) {
            self.load_ir(&state.ir_file_path);
            self.curve_points = state.custom_curve;
        }
    }
}
```

The framework handles the outer serialization envelope -- your
`save_state` bytes are embedded inside a versioned container
alongside parameter values. You don't need to save params yourself.

---


---

[← Previous](09-hot-reload.md) | [Next →](11-building.md) | [Index](README.md)
