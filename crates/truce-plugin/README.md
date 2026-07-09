# truce-plugin

User-facing plugin traits for the truce audio plugin framework.

## Overview

`truce-plugin` is the home of `PluginLogic` - the one trait plugin
authors implement, covering both audio-thread DSP and main-thread
GUI. The crate is intentionally light: it depends on `truce-core`,
`truce-gui-types` (data + render-trait surface only), and
`truce-params`. No tiny-skia, no baseview, no fonts. Plugin
authors using a custom editor (egui, iced, slint, raw window
handle) end up with this crate in their dep tree but never pull in
the built-in editor's heavy stack.

## Three traits, one source of truth

| Trait | Sample type | Where it lives |
|---|---|---|
| `PluginLogic` | `f32` | What `f32` plugins implement |
| `PluginLogic64` | `f64` | What `f64` plugins implement |
| `PluginLogicCore<S>` | generic | What format wrappers consume |

Both leaf traits are stamped from one `plugin_logic_leaf_trait!`
`macro_rules!` definition so the method shapes stay in lock-step.
Each leaf gets a blanket impl that forwards every method to
`PluginLogicCore<S>` with the matching `S`. Wrappers
(`StaticShell`, `HotShell`, the format crates) bind on
`PluginLogicCore<S>`; the leaf-vs-leaf distinction is purely
user-facing.

One layer of sugar sits on top: `PurePluginLogic` /
`PurePluginLogic64` for plugins with no DSP state. They drop the
`type DspState` / `init` / `state` plumbing from the surface and
blanket-implement the matching leaf with `DspState = ()`, so
everything downstream consumes them unchanged.

## What this buys

The `truce::prelude64` re-export aliases `PluginLogic64` as
`PluginLogic` in the user's scope, so the same impl header reads
the same regardless of which precision the prelude chose:

```rust
use truce::prelude::*;          // or prelude64::*
impl PluginLogic for MyPlugin {
    fn process(state: &mut Self::DspState, params: &Self::Params, buffer: &mut AudioBuffer, ...) -> ProcessStatus { ... }
}
```

Zero `<Sample>` mentions in user code.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
