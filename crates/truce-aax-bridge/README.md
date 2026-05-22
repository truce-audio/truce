# truce-aax-bridge

C ABI contract between the truce AAX cdylib and the C++ AAX template.

## Overview

A tiny "data carrier" crate that publishes the
`include/truce_aax_bridge.h` header (the C ABI between the Rust
cdylib and the AAX C++ template) as an embedded `&'static str`,
plus the matching Rust ABI version constant.

```rust
/// Source of truth for the C ABI the Rust cdylib and the AAX
/// C++ template agree on.
pub const TRUCE_AAX_ABI_VERSION: u32;

/// The C bridge header text, embedded at compile time.
pub const BRIDGE_HEADER: &str;
```

Split out of `truce-aax` so `cargo-truce` (the CLI that scaffolds
AAX projects) can write the header into generated projects
without pulling in `truce-aax`'s runtime dependency stack
(`truce-core`, `truce-params`, `crossbeam-queue`). Same pattern
as [`truce-shim-types`](https://crates.io/crates/truce-shim-types)
for the AU v3 bridging header.

A unit test parses `BRIDGE_HEADER` and asserts the embedded
`#define TRUCE_AAX_ABI_VERSION` matches the Rust constant, so
drift between the two sources of truth becomes a build failure.

Part of [truce](https://github.com/truce-audio/truce).
