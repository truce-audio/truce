# Tremolo

Beat-synced amplitude LFO. Demonstrates the host-transport feature
end-to-end: the DSP reads `ProcessContext::transport` to lock the LFO
phase to the host's beat grid, and the editor reads
`EditorContext::transport` to show the current tempo / play state /
beat position live in the UI.

When the host does not report transport (standalone, hosts that don't
expose it), the DSP falls back to a free-running 2 Hz LFO so the effect
stays audible, and the readout shows `—` placeholders.

## What it demonstrates

- Reading tempo + beat position on the audio thread from `ProcessContext::transport`.
- Reading transport on the editor thread from `EditorContext::transport`.
- `#[derive(ParamEnum)]` with a selector widget.
- Graceful fallback when the host does not populate transport.

## Parameters

| Name  | Range                              | Description                     |
|-------|------------------------------------|---------------------------------|
| Depth | 0 to 1                             | LFO → amplitude modulation depth |
| Rate  | 1/1, 1/2, 1/4, 1/8, 1/16, 1/32    | LFO cycle length in note values |
| Shape | Sine / Triangle / Square           | LFO waveform                    |

## Build and test

```bash
cargo build -p truce-example-tremolo
cargo test -p truce-example-tremolo
cargo xtask install -p truce-example-tremolo
```
