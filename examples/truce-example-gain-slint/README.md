# Gain (slint)

The gain plugin reimplemented with a [Slint](https://github.com/slint-ui/slint)
GUI. Same DSP as the built-in gain example, different frontend.

## What it demonstrates

- `SlintEditor` with setup closure and per-frame sync
- Custom `.slint` markup with `Knob`, `Meter`, `XYPad` from `@truce`
- Manual callback wiring (`on_gain_changed`, `on_pan_changed`)
- Formatted value text synced from Rust (`state.format()`)
- Header bar implemented in `.slint` markup
- Consistent window sizing between editor and snapshot test
- Screenshot testing with `truce_slint::snapshot` (software renderer)

## Layout

Header bar with plugin name. Left column with two knobs (Gain, Pan)
and an XY pad. Thin stereo meter spanning the full height on the right.
All defined in `ui/main.slint`.

## Files

- `src/lib.rs` — plugin logic, editor wiring, snapshot test
- `ui/main.slint` — declarative UI markup
- `build.rs` — compiles `.slint` to Rust

## See also

- [slint integration guide](../../docs/gui/slint.md)
- [gain](../gain/) — same plugin with built-in GUI
- [gain-egui](../gain-egui/) — same plugin with egui
- [gain-iced](../gain-iced/) — same plugin with iced

## Build and test

```bash
cargo build -p truce-example-gain-slint
cargo test -p truce-example-gain-slint
```
