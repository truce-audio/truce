# truce -- Getting Started

Build audio plugins in Rust. One codebase, every format.

---

## Install

You need Rust 1.75+ and:
- **macOS**: Xcode (`xcode-select --install`). Full Xcode required for AU v3 appex.
- **Windows**: MSVC build tools (Visual Studio 2019+ with "Desktop development with C++" workload). The Rust `x86_64-pc-windows-msvc` toolchain is required. WSL users should install Rust natively on Windows, not inside WSL.
- **Linux**: X11 + Vulkan + JACK headers. On Ubuntu/Debian:
  ```
  sudo apt install build-essential pkg-config \
    libx11-dev libx11-xcb-dev libxcb1-dev libxcb-dri2-0-dev libxcb-icccm4-dev libxcursor-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libxrandr-dev \
    libgl1-mesa-dev libvulkan-dev mesa-vulkan-drivers \
    libasound2-dev libjack-jackd2-dev \
    libfontconfig1-dev libfreetype-dev \
    pipewire-jack libspa-0.2-jack
  ```
  On modern distros with PipeWire, do not run a separate `jackd` — the PipeWire JACK shim provides `libjack.so.0` and routes JACK clients through PipeWire automatically.

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
- **Linux**: CLAP, VST3, and VST2 build and install (all four GUI backends render in Reaper). AAX and AU are not supported on Linux (by design). Plugin installs are user-scope by default (`~/.clap`, `~/.vst3`, `~/.vst`) — no sudo needed.

---

---

[Next →](02-first-plugin.md) | [Index](README.md)
