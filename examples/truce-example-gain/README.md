# Gain

Stereo gain and pan utility with level metering. Uses the built-in GUI.

## What it demonstrates

- Exponentially smoothed parameters (`smooth = "exp(5)"`)
- Equal-power pan law using `cos`/`sin` on a quarter-circle angle
- Peak metering via `ProcessContext::set_meter`
- Grid layout with knobs, XY pad, and stereo meter

## Parameters

| Name | Range | Unit | Description |
|------|-------|------|-------------|
| Gain | -60 to +6 | dB | Output level |
| Pan | -1 to +1 | pan | Stereo balance (equal-power) |

## GUI variants

The same gain plugin is implemented with four different GUI backends:

- **gain** (this one) — built-in grid layout
- **[gain-egui](../gain-egui/)** — egui immediate-mode widgets
- **[gain-iced](../gain-iced/)** — iced retained-mode widgets
- **[gain-slint](../gain-slint/)** — declarative `.slint` markup

Compare them to see how each framework handles the same layout.

## Build and test

```bash
cargo build -p truce-example-gain
cargo test -p truce-example-gain
cargo truce install -p truce-example-gain
```
