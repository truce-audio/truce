# Gain

Stereo gain and pan utility with level metering.

## What it demonstrates

- Exponentially smoothed parameters (`smooth = "exp(5)"`)
- Equal-power pan law using `cos`/`sin` on a quarter-circle angle
- Peak metering via `ProcessContext::set_meter`
- Multiple widget types in a single GUI layout: knob, slider, toggle, XY pad, stereo meter
- Bypass parameter with the `bypass` flag

## Parameters

| Name   | Range       | Unit | Description                     |
|--------|-------------|------|---------------------------------|
| Gain   | -60 to +6   | dB   | Output level                    |
| Pan    | -1 to +1    | pan  | Stereo balance (equal-power)    |
| Bypass | on/off      | --   | Skips processing when enabled   |

## Code structure

- `GainParams` -- derived with `#[derive(Params)]`, three parameters with attribute-driven ranges
- `Gain::process` -- per-sample loop: reads smoothed gain/pan, applies equal-power pan law, writes output
- `Gain::layout` -- two-row GUI: knobs/slider/toggle on top, XY pad + stereo meter on bottom
- Tests use `truce_test` harness: render, state round-trip, editor lifecycle, param validation

## Build and test

```bash
cargo build -p gain
cargo test -p gain
cargo xtask install -p gain   # install CLAP + VST3
```
