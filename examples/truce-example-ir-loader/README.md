# truce-example-ir-loader

Demonstrates off-audio-thread file loading via `truce-sample-loader`:
an editor "Browse..." button (or a restored session) hands a file path
to a background `LoadIrRequest`, which decodes and resamples it off the
audio thread then swaps the result into `process()` through a pair of
lock-free queues. The queues are guarded by a generation counter that
rejects stale decodes from any superseded requests.

`process()` itself is a pass-through - no DSP is applied to the loaded
buffer. This example proves the load/swap plumbing only.

See `README.md` in `truce-sample-loader` for the decode/resample API,
and `examples/truce-example-fundsp-reverb-worker` for the queue-swap
pattern this is modeled on.
