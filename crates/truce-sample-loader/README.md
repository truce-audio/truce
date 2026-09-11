# truce-sample-loader

`decode_file` and `resample` decode and resample audio files for
truce plugins. Both functions run off the audio thread.

`decode_file` reads a WAV file into an in-memory `DecodedAudio`
structure: per-channel `f32` samples, deinterleaved, plus the file's
native sample rate. `hound` provides WAV support by default. Enable
the `symphonia` feature for more formats: AIFF, FLAC, MP3, and AAC in
MP4.

`resample` is a separate step from `decode_file`. This design lets
the caller control when resampling happens, and whether it happens
at all. It also lets the crate's own tests run without a host sample
rate.

CAUTION: Do not call `decode_file` or `resample` from `process()`.
Both functions allocate memory and perform I/O, so neither function
is real-time safe.

Call `decode_file` and `resample` from a background thread, for
example a truce `BackgroundTask::run`. See
`examples/truce-example-ir-loader` for the full pattern: decode and
resample off the audio thread, then swap in the result with a
lock-free operation.

```rust,ignore
let decoded = truce_sample_loader::decode_file(path)?;
let resampled = truce_sample_loader::resample(decoded, host_sample_rate);
```
