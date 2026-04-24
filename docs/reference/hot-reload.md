# 7. Hot reload

Edit DSP or layout code, rebuild, hear the change in ~2 seconds.
No DAW restart. No plugin window close. Same source file, same
`truce::plugin!` macro — just a Cargo feature.

## Setup

The default `cargo truce new` scaffold produces a single-crate
plugin with the `dev` feature pre-wired:

```toml
[features]
default  = ["clap", "vst3"]
clap     = ["dep:truce-clap", "dep:clap-sys"]
vst3     = ["dep:truce-vst3"]
# ... other format features ...
dev      = ["truce/dev"]     # ← the hot-reload feature
```

```sh
# One-time: build and install the hot-reload shell.
cargo truce install --dev

# Iterate: rebuild the logic dylib on every save (debug, fast).
cargo watch -x "build -p my-plugin"
```

`--dev` flips the `dev` feature on, which makes `truce::plugin!`
expand into a shell that loads your `PluginLogic` out of a
separate dylib. The shell watches the dylib for content changes
and swaps in the new one while the plugin is live.

When you're done iterating, ship the release build:

```sh
cargo truce install          # no --dev = static, zero overhead
```

Zero code changes between dev and release.

## What reloads

| What you edit | How fast? |
|---------------|-----------|
| DSP algorithm | ~2 s |
| MIDI handling | ~2 s |
| Widget layout (built-in GUI) | ~2 s |
| Meter logic | ~2 s |

**Built-in GUI reloads too.** Editing `layout()` and rebuilding
swaps the new layout into the running editor without closing the
window. The `HotEditor` wrapper delegates to `GpuEditor` for
rendering and spawns a background thread watching the dylib. On
change, the new `BuiltinEditor` is installed via a shared mutex —
no flicker.

**Custom editors (egui, iced, Slint) do not hot-reload the UI
itself** — they reload the DSP, but you still need to close and
reopen the plugin window to see layout changes in the custom UI.

## What does **not** reload

| What | Why |
|------|-----|
| Parameter definitions (adding / removing a `#[param]`) | The host caches parameter count + IDs + names at scan time. |
| Plugin name / IDs | Host caches metadata at scan. |
| Bus layout | Host configures at init. |

Changing any of these requires rebuilding the shell
(`cargo truce install --dev`) and having the host rescan. That's
rare — most iteration is on DSP and GUI layout.

## How it works

The `truce::plugin!` macro expands differently when the `dev`
feature is on:

- **Without `dev`**: `StaticShell` embeds the `PluginLogic`
  directly. Zero overhead, ships in production.
- **With `dev`**: `HotShell` loads the `PluginLogic` from a
  separate dylib via native Rust ABI. A file watcher thread
  monitors that dylib.

### Reload sequence

1. File watcher detects the dylib changed (mtime poll every 500 ms).
2. CRC32 hash confirms content actually changed.
3. Audio thread serializes the current plugin state.
4. Old dylib handle is leaked — never `dlclose` (a TLS destructor
   segfault on macOS, plus stale pointers in TLS on every OS).
5. New dylib copied to a versioned temp path to defeat macOS's
   dyld cache.
6. On macOS the copy is ad-hoc codesigned (required by SIP).
7. `dlopen` loads the new dylib.
8. An ABI canary verifies type layouts match.
9. A vtable probe verifies trait-method dispatch order.
10. `truce_create()` returns a new `Box<dyn PluginLogic>`.
11. The new instance is reset with the current sample rate, then
    state is restored.

The plugin instance is behind a `parking_lot::Mutex`. Audio thread
locks for `process()`, main thread locks for `render()`. Mouse
event handlers release the lock before calling host callbacks, to
avoid deadlocks.

## Troubleshooting

**Plugin doesn't notice the new dylib.**
Check `target/debug/lib{crate_name}.{dylib,so,dll}` exists after
build. Override with `TRUCE_LOGIC_PATH=/absolute/path/to/lib...`.
The shell skips reload if CRC32 hasn't changed.

**macOS "code signature invalid."**
The shell codesigns the dylib copy. Ensure Xcode CLI tools are
installed: `xcode-select --install`.

**Audio glitch on reload.**
Expected — a 5–50 ms dropout while the swap happens. Hot reload
is a development tool, not a live-performance feature.

**"ABI mismatch" error.**
The shell and logic were built with different Rust toolchains.
Both must use the same compiler. `rust-toolchain.toml` in the
workspace pins it.

**State lost on reload.**
`save_state()` / `load_state()` format changed between builds.
The plugin falls back to defaults.

## What's next

- **[Chapter 8 → shipping.md](shipping.md)** — when you're done
  iterating, package a signed installer.
- **[docs/internal/hot-reload-architecture.md](../../../truce-docs/docs/internal/hot-reload-architecture.md)**
  (in the docs repo) — deeper internals: ABI safety, canary,
  vtable probe, memory model.
