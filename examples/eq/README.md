# EQ

3-band parametric equalizer using biquad filters.

## What it demonstrates

- Biquad filter DSP (Direct Form II Transposed, Audio EQ Cookbook formulas)
- Per-sample coefficient updates driven by smoothed parameters
- Parameter groups (`group = "Low"`, `"Mid"`, `"High"`) for logical organization
- Logarithmic parameter ranges for frequency and Q (`range = "log(20, 1000)"`)
- Auto-bypass when band gain is near 0 dB

## Parameters

| Name      | Range          | Unit | Description                |
|-----------|----------------|------|----------------------------|
| Low Freq  | 20 -- 1000     | Hz   | Low band center frequency  |
| Low Gain  | -18 -- +18     | dB   | Low band boost/cut         |
| Low Q     | 0.1 -- 10      | --   | Low band width             |
| Mid Freq  | 200 -- 8000    | Hz   | Mid band center frequency  |
| Mid Gain  | -18 -- +18     | dB   | Mid band boost/cut         |
| Mid Q     | 0.1 -- 10      | --   | Mid band width             |
| High Freq | 1000 -- 20000  | Hz   | High band center frequency |
| High Gain | -18 -- +18     | dB   | High band boost/cut        |
| High Q    | 0.1 -- 10      | --   | High band width            |
| Output    | -18 -- +18     | dB   | Output trim                |

## Code structure

- `biquad.rs` -- standalone biquad filter: `set_peaking` computes coefficients, `process` runs DF2T
- `Eq` struct holds a `[Biquad; 3]` per channel (up to stereo)
- `process` -- reads 9 smoothed params per sample, updates coefficients, cascades 3 filters per channel
- GUI uses labeled sections (`LOW` / `MID` / `HIGH`) with 3 knobs each

## Build and test

```bash
cargo build -p eq
cargo test -p eq
```
