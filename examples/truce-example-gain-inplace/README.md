# Gain (In-Place)

Stereo gain and pan, identical DSP to `truce-example-gain`, but opting
into truce's zero-copy in-place I/O (`supports_in_place() = true`). When
the host aliases a channel's input and output, the plugin processes the
shared buffer directly through `AudioBuffer::in_out_mut(ch)` instead of
truce copying the input into scratch.

**In-place I/O is experimental and not recommended.** The default
(`supports_in_place() = false`) copies aliased inputs so `input(ch)` and
`output(ch)` are always disjoint, at the cost of one negligible memcpy per
aliased channel per block. Opting in trades that safety for a
micro-optimization and forces every channel read to branch on
`is_in_place(ch)`. Reach for it only if profiling proves the copy is a
bottleneck. This example documents the path; it doesn't endorse it.
