# EQ

3-band parametric equalizer using biquad filters.

## What it demonstrates

- Biquad filter DSP (Direct Form II Transposed)
- Per-sample coefficient updates from smoothed parameters
- Logarithmic parameter ranges for frequency and Q
- Section-based grid layout (`LOW`, `MID`, `HIGH`)
- Auto-bypass when band gain is near 0 dB

## Parameters

| Name | Range | Unit | Description |
|------|-------|------|-------------|
| Low Freq | 20 to 1000 | Hz | Low band center frequency |
| Low Gain | -18 to +18 | dB | Low band boost/cut |
| Low Q | 0.1 to 10 | -- | Low band width |
| Mid Freq | 200 to 8000 | Hz | Mid band center frequency |
| Mid Gain | -18 to +18 | dB | Mid band boost/cut |
| Mid Q | 0.1 to 10 | -- | Mid band width |
| High Freq | 1000 to 20000 | Hz | High band center frequency |
| High Gain | -18 to +18 | dB | High band boost/cut |
| High Q | 0.1 to 10 | -- | High band width |
| Output | -18 to +18 | dB | Output trim |

## Files

- `src/lib.rs` — plugin logic, GUI layout with sections
- `src/biquad.rs` — standalone biquad filter implementation

## Build and test

```bash
cargo build -p truce-example-eq
cargo test -p truce-example-eq
```
