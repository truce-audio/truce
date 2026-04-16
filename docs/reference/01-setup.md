# truce -- Getting Started

Build audio plugins in Rust. One codebase, every format.

---

## Install

You need Rust 1.75+ and:
- **macOS**: Xcode (`xcode-select --install`). Full Xcode required for AU v3 appex.
- **Windows**: MSVC build tools (Visual Studio 2019+ with "Desktop development with C++" workload). The Rust `x86_64-pc-windows-msvc` toolchain is required. WSL users should install Rust natively on Windows, not inside WSL.
- **Linux**: `libasound2-dev` and `libjack-jackd2-dev` (for standalone mode). Note: Linux format wrappers are planned but not yet implemented.

The workspace includes a `rust-toolchain.toml` that pins the Rust
version. This is required for hot-reload (shell and logic dylib
must use the same compiler). Run `rustup show` to confirm the
pinned version is installed.

The build tool is `cargo truce` — it handles building, bundling,
signing, installing, and cache clearing for all formats.

### Platform notes

- **macOS**: All six plugin formats supported (CLAP, VST3, VST2, AU v2, AU v3, AAX).
- **Windows**: CLAP, VST3, and VST2 fully working. AAX template build on Windows is not yet implemented. AU is macOS-only by design.
- **Windows installs require admin** because plugin directories (`C:\Program Files\Common Files\...`, `C:\Program Files\Steinberg\VstPlugins`) are system-wide. Run your command prompt as Administrator before `cargo truce install`.

---

---

[Next →](02-first-plugin.md) | [Index](README.md)
