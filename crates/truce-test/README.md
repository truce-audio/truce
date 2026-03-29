# truce-test

Test utilities for truce plugins.

Provides helpers to render audio, inject MIDI, verify output, and round-trip
state — all in-process, no host simulation needed.

## Usage

```rust
let result = truce_test::render_effect::<MyEffect>(1024, 44100.0);
assert!(result.output.iter().all(|s| s.abs() <= 1.0));
```

Part of [truce](https://github.com/truce-audio/truce).
