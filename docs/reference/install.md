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

## Install the CLI

```sh
cargo install cargo-truce
```

Pulls `cargo-truce` from crates.io. Cargo picks it up as the
`cargo truce` subcommand. Re-run with `--force` to upgrade; the
scaffold templates ship with the CLI binary, so upgrading the CLI
is how you get new scaffold features.

To pin a specific version:

```sh
cargo install cargo-truce --version X.Y.Z              # crates.io, exact
cargo install --git https://github.com/truce-audio/truce \
              --tag vX.Y.Z cargo-truce                  # git, exact pin
```

## Platform deps

Pick one — macOS, Windows, or Linux. All three build CLAP, VST3,
VST2, and LV2; macOS adds AU v2/v3; macOS and Windows add AAX.

### What each format needs

The table below summarizes the dependencies per format. The
"always" row covers the toolchain you need regardless. Other rows
add to that. **CLAP, VST3, VST2, and LV2 don't need anything beyond
the always-required toolchain** — start with those if you want a
minimum-friction setup.

| Need                 | macOS                | Windows              | Linux              |
|----------------------|----------------------|----------------------|--------------------|
| **always**           | Xcode CLI tools      | VS 2019+ Desktop C++ | `build-essential` + `pkg-config` |
| any plugin with a GUI | (in OS)             | (in OS)              | + X11 / GL / Vulkan / font dev libs |
| **standalone host**  | (in OS)              | (in OS)              | + ALSA + JACK or PipeWire-JACK shim |
| **AU v3** *(plugins)* | + full Xcode (`xcodebuild`) | n/a            | n/a                |
| **AAX**              | + Avid AAX SDK (`AAX_SDK_PATH`) + PACE/iLok for retail | + Avid AAX SDK + PACE/iLok | n/a |
| **`cargo truce package` installer signing** | (in OS) | + Inno Setup       | n/a (no signed installer yet) |

CLAP / VST3 / VST2 / LV2 / AU v2 ship without per-format extras.
Pull in the GUI / standalone / AU v3 / AAX rows only if you'll
build something that needs them.

### macOS

```sh
xcode-select --install     # Xcode CLI tools (provides clang)
```

That's it for CLAP / VST3 / VST2 / LV2 / AU v2. The system
frameworks (Cocoa, AudioToolbox, etc.) come with macOS — no
package-manager work needed for GUI or standalone-audio support.

**Full Xcode** (not just CLI tools) is required for **AU v3** —
`xcodebuild` builds the `.appex` bundle. Defer this until you're
ready to package AU v3. Switch the active developer directory with:

```sh
sudo xcode-select -s /Applications/Xcode.app
```

**AAX** needs the Avid AAX SDK (point `AAX_SDK_PATH` at it — set
it in `.cargo/config.toml` under `[env]`, or export it in your
shell) plus PACE/wraptool for retail Pro Tools releases. Skip both
if you're not shipping AAX.

### Windows

Visual Studio 2019+ with the **"Desktop development with C++"**
workload. Pick ARM64 / C++ / Windows SDK components too if you want
dual-arch installers.

That covers CLAP / VST3 / VST2 / LV2. The system libraries (Win32,
Direct3D, etc.) come with Windows — GUI and standalone audio work
out of the box.

**AAX** needs the Avid AAX SDK + PACE/iLok signing for retail Pro
Tools (same shape as macOS).

**`cargo truce package`** (signed `.exe` installer) needs
[Inno Setup](https://jrsoftware.org/isinfo.php). Skip if you're not
producing installers.

`cargo truce install` defaults to **user-scope**
(`%LOCALAPPDATA%\Programs\Common\CLAP\`,
`%LOCALAPPDATA%\Programs\Common\VST3\`) — no Administrator prompt
needed for the dev loop. Pass `--system` to install into
`%COMMONPROGRAMFILES%\...` instead (run from an Administrator
shell). AAX and Windows VST2 are always system-only; `cargo truce
install --aax` / `--vst2` will need an Administrator shell.

WSL users: install Rust on Windows itself, not inside WSL. Plugin
hosts are Windows apps and can't load ELF binaries from WSL paths.

### Linux

A C/C++ toolchain plus optional GUI / audio dev headers. Linux
breaks into the most rows because none of the GUI / audio plumbing
is in the OS — every dep is opt-in.

**Always required** (any format, any plugin shape):

```sh
sudo apt install build-essential pkg-config        # Ubuntu / Debian
sudo dnf install @development-tools pkgconf-pkg-config  # Fedora
```

That's enough for a *headless* CLAP / VST3 / VST2 / LV2 plugin (no
GUI, no standalone host).

**Add for any plugin with a GUI** — every truce GUI backend
(built-in widgets, egui, iced, slint) goes through `baseview` and
needs the X11 + GL + Vulkan + font stack:

```sh
# Ubuntu / Debian
sudo apt install \
  libx11-dev libx11-xcb-dev libxcb1-dev libxcb-dri2-0-dev \
  libxcb-icccm4-dev libxcursor-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libxrandr-dev \
  libgl1-mesa-dev libvulkan-dev mesa-vulkan-drivers \
  libfontconfig1-dev libfreetype-dev

# Fedora
sudo dnf install \
  libX11-devel libxcb-devel libXcursor-devel \
  libxkbcommon-devel libxkbcommon-x11-devel libXrandr-devel \
  mesa-libGL-devel vulkan-loader-devel mesa-vulkan-drivers \
  fontconfig-devel freetype-devel
```

**Add for the standalone host** — ALSA backend + JACK headers (or
the PipeWire JACK shim):

```sh
# Ubuntu / Debian
sudo apt install libasound2-dev libjack-jackd2-dev \
  pipewire-jack libspa-0.2-jack       # PipeWire shim, optional

# Fedora
sudo dnf install alsa-lib-devel jack-audio-connection-kit-devel
```

On modern distros with PipeWire (Ubuntu 24.04+, Fedora 40+), install
the PipeWire JACK shim (`pipewire-jack` / `pipewire-jack-audio-
connection-kit`) and **don't** also run `jackd2` — they fight for
the same socket. The shim replaces `libjack.so.0` so JACK clients
route through PipeWire.

**No AU / AAX on Linux** — those formats are macOS / macOS+Windows
only. There's no Linux-specific extra dep beyond the rows above.

Plugin installs are user-scope on Linux (`~/.clap`, `~/.vst3`,
`~/.vst`, `~/.lv2`) — no `sudo` needed.

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

If [sccache](https://github.com/mozilla/sccache) is on `PATH`,
`cargo truce` auto-uses it as `RUSTC_WRAPPER`. `doctor` reports
when it's active. Set `TRUCE_DISABLE_SCCACHE=1` to skip for one
invocation.

## What's next

You're ready for [chapter 2 → first-plugin.md](first-plugin.md) —
scaffold, build, and load a plugin in a DAW.
