# Fundsp Reverb (Worker variant)

Stereo plate reverb wired through a [`fundsp`](https://github.com/SamiPerttu/fundsp) audio graph. The point of this example is the integration shape — how to hold a fundsp graph inside a truce plugin and keep it alloc-free on the audio thread.

This is the **production-pattern** variant: graph rebuilds happen on a dedicated worker thread and the audio thread picks them up via a lock-free swap. For the simpler inline-rebuild version (rt-unsafe but easier to read top-to-bottom), see [`truce-example-fundsp-reverb-simple`](../truce-example-fundsp-reverb-simple/). Both crates share the same topology, params, and signal flow.

```text
in (L,R) ──► high-pass (low cut)  ──► low-pass (high cut)  ──► reverb_stereo ──┐
                                                                                │
in (L,R) ─────────────────────────────────────────────────────────────────► dry ┤──► out
                                                                                │
                                                              mix ──────────────┘
```

| Param    | Range                  |
|----------|------------------------|
| Low Cut  | 20 Hz → 2 kHz (log)    |
| High Cut | 500 Hz → 18 kHz (log)  |
| Time     | 0.1 s → 20 s (log)     |
| Mix      | 0 → 1                  |

## Integration patterns

- **Graph built off the audio thread.** `reset()` builds the initial graph synchronously (host calls it off the audio path); subsequent rebuilds run on a dedicated worker thread. `process()` never allocates, never calls `Box::new`, never calls `graph.allocate()`, and never drops a `Box<dyn AudioUnit>`.
- **Lock-free worker handoff.** Three `crossbeam_queue::ArrayQueue`s shuttle work between the audio thread and the rebuild worker: `requests` (audio → worker, latest target wins via `force_push`), `ready` (worker → audio, the freshly-built graph), `discard` (audio → worker, so the old graph is dropped off-thread). The worker `park`s when idle and the audio thread `unpark`s it on a new request.
- **Worker rebuilds carry their SR.** Each ready graph is tagged with the sample rate it was built for. If `reset()` swaps in a new SR while a worker rebuild is in flight, the audio thread sees the SR mismatch and reroutes the stale graph to the discard queue rather than swapping it in.
- **Params reach the graph through `fundsp::Shared` atomics.** `var(&shared)` reads them per sample; the closure inside `for_each_frame` writes the smoothed truce-side value into the cell on the same tick (sample-accurate automation).
- **`Box<dyn AudioUnit>`** for the field type. The concrete `An<…>` is hundreds of chars of nested generics; the vtable cost is one indirection per block.
- **`AudioBuffer::for_each_frame::<2, _>`** transposes truce's per-channel layout into stack-allocated frames so fundsp's `tick(in, out)` callback can be called directly. No scratch field.
- **Reverb time triggers a worker rebuild** when the param drifts ≥ 5% — `reverb_stereo`'s `time` argument is baked at construction. The audio thread reads the raw `param.value()` (not the smoothed `.read()`) so a knob ramp doesn't trip the threshold every block while the smoother crawls across it.

## Gotchas

- **Filter input order is positional and unchecked.** `highpass()` / `lowpass()` take `(signal, cutoff, Q)`. Every connection is `f32`, so `(cutoff | Q | signal) >> highpass()` compiles fine and silently feeds the filter cutoff in as audio — the resulting filter blows up the reverb FDN to peak ~7000 within a second. Test against constant input + `assert_peak_below`.
- **Type-level channels.** `dry * mix` fails to compile when `dry` is stereo and `mix` is a 1-channel `Shared` read; broadcast the mix to stereo manually with `var(&mix) | var(&mix)`. fundsp's payoff (graph composition with `>>`/`|`/`&`) costs this kind of explicit plumbing.

## Build

```sh
cargo build -p truce-example-fundsp-reverb-worker
cargo test  -p truce-example-fundsp-reverb-worker --release
cargo truce install -p truce-example-fundsp-reverb-worker
cargo truce run     -p truce-example-fundsp-reverb-worker
```
