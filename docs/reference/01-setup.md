# truce -- Getting Started

Build audio plugins in Rust. One codebase, every format.

---

## Install

You need Rust 1.75+ and:
- **macOS**: Xcode (`xcode-select --install`). Full Xcode required for AU v3 appex.
- **Windows**: MSVC build tools
- **Linux**: `libasound2-dev` and `libjack-jackd2-dev` (for standalone mode)

The workspace includes a `rust-toolchain.toml` that pins the Rust
version. This is required for hot-reload (shell and logic dylib
must use the same compiler). Run `rustup show` to confirm the
pinned version is installed.

The build tool is `cargo xtask` — it handles building, bundling,
signing, installing, and cache clearing for all formats.

---

---

[Next →](02-first-plugin.md) | [Index](README.md)
