# truce-slint

Slint GUI backend for truce audio plugins.

## Overview

Provides `SlintEditor`, an implementation of `truce_core::Editor` that
renders [Slint](https://slint.dev/) UIs via Slint's software renderer
on top of baseview + wgpu compositing. Developers write their UI in
`.slint` markup (compiled at build time) and bind parameters through
a `PluginContext`.

Use this backend when you want Slint's declarative `.slint` markup,
its IDE tooling (preview, autocompletion), or a non-Rust UI design
workflow.

## Key types

- **`SlintEditor`** -- the `Editor` implementation
- **`PluginContext`** -- parameter bridge for reading/writing truce
  params from Slint properties (re-exported from `truce-core`)
- **`bind!`** -- macro for connecting Slint properties to truce
  parameter IDs

## Usage

```rust
use truce_slint::{SlintEditor, PluginContext};

SlintEditor::new(params, (400, 300), |context: PluginContext<P>| {
    let ui = MyPluginUi::new().unwrap();
    truce_slint::bind! { context, ui,
        P::Gain   => gain,
        P::Pan    => pan,
        P::Bypass => bypass: bool,
    }
})
```

Part of [truce](https://github.com/truce-audio/truce).
