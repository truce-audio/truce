# truce-test

Test harness for truce audio plugins.

## Overview

Provides helpers for offline rendering, assertion, and validation of truce
plugins -- all in-process with no DAW or host simulation needed. Use this crate
in your plugin's integration tests to verify audio output, parameter behavior,
state persistence, and editor lifecycle.

## Key functions

- **`render_effect`** -- render an effect plugin with silence or provided input
- **`render_instrument`** -- render an instrument plugin with MIDI note events
- **`assert_audio_contains_signal`** -- verify output is not silent
- **`assert_state_roundtrip`** -- save state, reload, and verify parameters match
- **`assert_params_valid`** -- check parameter ranges, names, and formatting
- **`assert_editor_lifecycle`** -- open, idle, and close editor without panics
- **GUI snapshot utilities** -- pixel-level regression testing for editors

## Usage

```rust
#[test]
fn test_effect_output() {
    let result = truce_test::render_effect::<MyEffect>(1024, 44100.0);
    assert!(result.output.iter().all(|s| s.abs() <= 1.0));
    truce_test::assert_audio_contains_signal(&result.output);
}

#[test]
fn test_state_persistence() {
    truce_test::assert_state_roundtrip::<MyEffect>();
}
```

Part of [truce](https://github.com/truce-audio/truce).
