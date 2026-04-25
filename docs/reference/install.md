# 1. Install

Set up your machine to build truce plugins. One Rust toolchain, a
platform-specific C/C++ compiler, and the `cargo truce` CLI.

## Rust

Rust **1.88+** via [rustup.rs]. `rustup update` if you already have
it.

[rustup.rs]: https://rustup.rs

The truce workspace pins a toolchain and the desktop targets in
`rust-toolchain.toml`. Running `rustup show` inside a truce plugin
project installs the pinned version.

## Platform deps

Pick one — macOS, Windows, or Linux. All three build CLAP, VST3,
VST2, and LV2; macOS adds AU v2/v3; macOS and Windows add AAX.

### macOS

```sh
xcode-select --install     # Xcode CLI tools (provides clang)
```

**Full Xcode** (not just CLI tools) is required if you want to ship
AU v3 — `xcodebuild` needs to be on `PATH`. You can defer this
until you're ready to package AU v3. Switch the active developer
directory with:

```sh
sudo xcode-select -s /Applications/Xcode.app
```

### Windows

Visual Studio 2019+ with the **"Desktop development with C++"**
workload. Pick ARM64 / C++ / Windows SDK components too if you want
dual-arch installers.

Plugin directories (`C:\Program Files\Common Files\...`, Steinberg
VST2 folder, Avid AAX folder) are system-wide, so
`cargo truce install` has to run from an **Administrator** command
prompt. Use the "Developer PowerShell for VS" shell and right-click
→ Run as Administrator.

WSL users: install Rust on Windows itself, not inside WSL. Plugin
hosts are Windows apps and can't load ELF binaries from WSL paths.

### Linux

A C/C++ toolchain plus the X11, Vulkan, ALSA/JACK, and font headers
the GUI backends need.

**Ubuntu / Debian:**

```sh
sudo apt install build-essential pkg-config \
  libx11-dev libx11-xcb-dev libxcb1-dev libxcb-dri2-0-dev \
  libxcb-icccm4-dev libxcursor-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libxrandr-dev \
  libgl1-mesa-dev libvulkan-dev mesa-vulkan-drivers \
  libasound2-dev libjack-jackd2-dev \
  libfontconfig1-dev libfreetype-dev \
  pipewire-jack libspa-0.2-jack
```

**Fedora:**

```sh
sudo dnf install @development-tools pkgconf-pkg-config \
  libX11-devel libxcb-devel libXcursor-devel \
  libxkbcommon-devel libxkbcommon-x11-devel libXrandr-devel \
  mesa-libGL-devel vulkan-loader-devel mesa-vulkan-drivers \
  alsa-lib-devel jack-audio-connection-kit-devel \
  fontconfig-devel freetype-devel
```

On modern distros with PipeWire (Ubuntu 24.04+, Fedora 40+), install
the PipeWire JACK shim (`pipewire-jack` / `pipewire-jack-audio-
connection-kit`) and **don't** also run `jackd2` — they fight for
the same socket. The shim replaces `libjack.so.0` so JACK clients
route through PipeWire.

Plugin installs are user-scope on Linux (`~/.clap`, `~/.vst3`,
`~/.vst`, `~/.lv2`) — no `sudo` needed.

## Install the CLI

```sh
cargo install --git https://github.com/truce-audio/truce cargo-truce
```

This installs `cargo-truce`, which Cargo picks up as the
`cargo truce` subcommand. Re-run it to upgrade; the scaffold
templates ship with the CLI binary, so upgrading the CLI is how you
get new scaffold features.

## Sanity check

```sh
cargo truce doctor
```

Reports what's present and what's missing: Rust version, per-OS
compilers, Xcode state, Inno Setup (Windows), Vulkan drivers
(Linux), `AAX_SDK_PATH` if you have one set. Green rows are ready;
yellow ones are optional; red ones block something.

Run `doctor` any time a build behaves oddly — it's usually faster
than debugging the error message.

## What's next

You're ready for [chapter 2 → first-plugin.md](first-plugin.md) —
scaffold, build, and load a plugin in a DAW.
