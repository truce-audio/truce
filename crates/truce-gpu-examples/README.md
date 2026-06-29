# truce-gpu-examples

Example plugins exercising the `truce-gui/gpu` (wgpu) rendering path.

## Overview

Not a library - a small Cargo sub-workspace holding the GPU-rendered example
plugins. It is split out from the main workspace on purpose: cargo's feature
unification would otherwise let one GPU example's `truce-gui/gpu` request leak
into the parent workspace's CPU-only examples, silently switching them from
`BuiltinEditor` to `GpuEditor` and breaking their pixel-exact screenshot
baselines. Isolating the GPU examples in their own workspace keeps the two
rendering paths from cross-contaminating.

End-user plugins choose CPU *or* GPU per their own `truce-gui` features; only
this repo, which exercises both, needs the split. The sub-workspace carries
its own `truce.toml` so `cargo truce` invoked from inside it finds these
plugin entries instead of the parent's.

## Members

- **`truce-example-gui-zoo-gpu`** -- the GPU-backed variant of the gui-zoo
  widget showcase

Run one with the GPU backend from inside this directory:

```sh
cargo truce run -p truce-example-gui-zoo-gpu
```

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
