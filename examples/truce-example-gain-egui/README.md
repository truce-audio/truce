# Gain (egui)

The gain plugin reimplemented with an [egui](https://github.com/emilk/egui)
GUI. Same DSP as the built-in gain example, different frontend.

## What it demonstrates

- `EguiEditor` with a closure-based UI function
- `param_knob`, `param_xy_pad`, `level_meter` helper widgets
- Custom header using `egui::TopBottomPanel`
- Theme color constants from `truce_egui::theme`
- Consistent window sizing with `WINDOW_W`/`WINDOW_H` constants
- Screenshot testing with `truce_egui::screenshot`

## Layout

Header bar with plugin name, followed by two knobs (Gain, Pan),
a thin stereo meter spanning the full height, and an XY pad below
the knobs.

## See also

- [egui integration guide](../../docs/reference/gui/egui.md)
- [gain](../gain/) — same plugin with built-in GUI
- [gain-iced](../gain-iced/) — same plugin with iced
- [gain-slint](../gain-slint/) — same plugin with slint

## Build and test

```bash
cargo build -p truce-example-gain-egui
cargo test -p truce-example-gain-egui
```
