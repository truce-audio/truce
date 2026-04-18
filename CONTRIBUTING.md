# Contributing to truce

## Project Status

truce is a working audio plugin framework with 6 format wrappers (CLAP,
VST3, VST2, AU v2, AU v3, AAX), 8 example plugins, a built-in GUI
system, and hot-reload support. All formats are tested in real DAWs on
macOS. CLAP/VST3/VST2 are tested on Windows. CLAP/VST3 are tested on
Linux (Reaper).

For detailed status, see [docs/status.md](docs/status.md).

## Development Environment

### Requirements

- **Rust 1.75+** — `rustup update`
- **macOS**: Xcode CLI tools — `xcode-select --install`
- **Full Xcode** for AU v3 builds (appex signing)
- **AAX SDK** (optional) — obtain from [developer.avid.com](https://developer.avid.com)

### Getting Started

```sh
git clone https://github.com/truce-audio/truce
cd truce

# Build and install all example plugins
cargo truce install

# Run tests
cargo truce test

# Run validators (auval, pluginval, clap-validator)
cargo truce validate

# Check your environment
cargo truce doctor
```

### Repo Structure

```
crates/           # all library crates (truce-core, truce-params, etc.)
examples/         # example plugins (gain, eq, synth, transpose, arpeggio)
docs/             # documentation (quickstart, reference, gui guides)
screenshots/        # GUI snapshot reference PNGs
```

See [docs/status.md](docs/status.md) for a full crate-by-crate breakdown.

### Useful Commands

```sh
cargo truce install --clap           # build + install CLAP only (fastest)
cargo truce install --dev            # hot-reload shells
cargo truce install -p gain          # single plugin
cargo truce install -p gain          # single plugin
cargo truce clean                    # clear AU and DAW plugin caches
cargo truce validate --auval         # AU validator only
cargo truce validate --pluginval     # pluginval only
cargo test --workspace               # run all Rust tests
cargo doc --workspace --open         # API docs
```

### Hot Reload for Development

When working on DSP or GUI layout code, hot-reload lets you hear and see
changes without restarting the DAW:

```sh
cargo truce install --dev            # one-time: install reload shells
cargo watch -x "build -p gain"       # iterate: rebuild on save
```

## Development Process

### Branching

Work on a feature branch and open a PR against `main`. Keep PRs focused
on a single change when possible.

### Testing

All PRs should pass:

```sh
cargo test --workspace               # unit + integration tests
cargo truce test                     # in-process plugin tests
cargo truce validate                 # auval + pluginval + clap-validator
```

If you add a new widget or change rendering, update or add GUI snapshot
tests. Snapshots live in `screenshots/` and are compared pixel-by-pixel.
Delete a PNG to regenerate it.

### Code Style

- `cargo fmt` before committing
- `cargo clippy --workspace` should be clean
- `#![forbid(unsafe_code)]` in safe crates (truce, truce-params, truce-build)
- Unsafe code in format wrappers and platform layers should be minimal
  and well-documented
- Prefer `Arc<P>` for param sharing over raw pointers
- Follow the gesture protocol (begin/set/end) in all GUI backends

### Commit Messages

Use concise commit messages that describe the "why":

```
Add stereo meter widget to GridLayout

Support multi-channel level meters in the grid layout system.
Meters accept a slice of meter IDs and render one bar per channel.
```

## What Needs Help

### High Priority

**Linux finishing work** — Core and all four GUI backends render in
Reaper, but gaps remain:
- Parameter automation round-trip and preset save/restore are untested.
- Bitwig and Ardour validation pending.
- `cargo truce package` has no Linux target (`.deb` / `.rpm` / AppImage
  would be the standard output).
- CI on ubuntu-24.04.
- LV2 format wrapper for first-class Ardour support.

**CLAP GUI-to-host sync** — Parameter changes from the GUI don't
reliably update the host's slider position in some CLAP hosts (notably
Reaper). The automation data records correctly, but the visual feedback
is wrong.

### Medium Priority

**More example plugins** — Delay, compressor, reverb, and other common
effects would demonstrate more framework capabilities and serve as
integration tests.

**HiDPI on Linux** — baseview's scale is captured and cached so
live rendering is correct, but the built-in `truce-gui` backend
renders its tiny-skia pixmap at logical resolution and blits up,
which produces softer output than egui at HiDPI. Render at physical
resolution instead.

**Documentation** — The reference tutorials cover the basics but could
use more depth on advanced topics: custom parameter formatting, complex
bus layouts, state migration between versions.

### Good First Issues

- Add missing `rust-version.workspace = true` to crates that don't have it
- Improve error messages in `truce-build` when `truce.toml` is malformed
- Add more assertion helpers to `truce-test`
- Write snapshot tests for example plugins that don't have them

## License

By contributing, you agree that your contributions will be licensed under
MIT OR Apache-2.0, matching the project license.
