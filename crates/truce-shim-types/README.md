# truce-shim-types

Shared C header for the truce AU shim.

## Overview

A tiny "data carrier" crate that publishes the
`include/au_shim_types.h` header (defining `AuTransportSnapshot` and
the other structs that bridge the AU shim's C/Objective-C/Swift code
to truce-au's Rust FFI) as both an embedded `&'static str` constant
and an on-disk path that `cc-rs` can use as an include directory.

```rust
// the bytes, for embedding into a generated build tree
pub const AU_SHIM_TYPES_H: &str;

// the directory, for `cc::Build::include()` / clang `-I<dir>`
pub fn include_dir() -> std::path::PathBuf;
```

## Why a separate crate

Three consumers need the **exact same bytes** of the header:

1. **`truce-au/build.rs`** - passes `include_dir()` to `cc-rs` so the
   shim sources (`au_shim_common.c`, `au_v2_shim.c`) can
   `#include "au_shim_types.h"` during compile.
2. **`truce-au/src/ffi.rs`** - defines `AuTransportSnapshot` whose
   Rust layout has to match the C struct in the header.
3. **`cargo-truce/src/templates.rs`** - embeds `AU_SHIM_TYPES_H` into
   the AU v3 Xcode template that `cargo truce install --au3` writes
   into the user's build tree, where the Swift `BridgingHeader.h`
   then `#import`s it.

Merging into either consumer breaks the other:

- **Into `truce-au`**: `cargo-truce` would have to depend on
  `truce-au`, dragging the AU NSView Objective-C compile cone into
  `cargo install cargo-truce`. Wrong shape - cargo-truce is meant to
  be lean.
- **Into `cargo-truce/templates/`**: works for the embedding case
  but `truce-au/build.rs` loses the published path it can pin to
  once truce-au gets published. Workspace-relative paths break
  post-publish, which is exactly when stable cross-crate sharing
  matters.

So this crate exists to be the single, published, version-pinnable
source of the header bytes. Same shape of rationale as `truce-font`
(a data-only crate with multiple consumers needing identical bytes).

Part of [truce](https://github.com/truce-audio/truce).
