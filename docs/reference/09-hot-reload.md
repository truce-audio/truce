## Hot Reload

Edit DSP or layout code, rebuild, hear the change in ~2 seconds.
No DAW restart. Same source file, same `truce::plugin!` macro —
just a feature flag.

### Setup

Add `dev = ["truce/dev"]` to your `[features]` in Cargo.toml:

```toml
[features]
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
dev = ["truce/dev"]    # ← add this
```

### Workflow

```bash
# One-time: build and install the hot-reload shell (release)
cargo xtask install --dev

# Iterate: rebuild the logic dylib (debug, fast)
cargo watch -x "build -p my-plugin"
```

Same source file. The `dev` feature makes `truce::plugin!` produce
a hot-reload shell instead of a static binary. The shell watches
`target/debug/` for the logic dylib and reloads on change.

When you're done iterating, build the release version:

```bash
cargo xtask install    # no --dev = static, zero overhead
```

Zero code changes between dev and release.

### What reloads

| What you edit | Recompile? | How fast? |
|---------------|-----------|-----------|
| DSP algorithm | Yes | ~2s |
| Widget layout | Yes | ~2s |
| Meter logic | Yes | ~2s |
| MIDI processing | Yes | ~2s |

### What does NOT reload

| What | Why |
|------|-----|
| Parameter definitions | Host caches param count/names at load |
| Plugin name / ID | Host caches metadata at scan |
| Bus layout | Host configures at init |

Changing parameters requires rebuilding the shell and restarting
the DAW. This is rare — most iteration is on DSP and layout.

---

### How it works

The `truce::plugin!` macro expands differently with the `dev`
feature:

- **Without `dev`**: `StaticShell` embeds the `PluginLogic` directly.
  Zero overhead.
- **With `dev`**: `HotShell` loads the `PluginLogic` from a separate
  dylib via native Rust ABI. A file watcher thread monitors the dylib.

#### Reload sequence

1. File watcher detects the dylib changed (polls mtime every 500ms)
2. CRC32 hash confirms the content actually changed
3. Audio thread serializes the current plugin state
4. Old dylib handle is leaked (never dlclose — TLS destructor
   segfault on macOS)
5. New dylib copied to versioned temp path (defeats macOS dyld cache)
6. The copy is ad-hoc codesigned (required by macOS SIP)
7. dlopen loads the new dylib
8. ABI canary verifies type layouts match
9. Vtable probe verifies trait method dispatch order
10. `truce_create()` returns a new `Box<dyn PluginLogic>`
11. New instance reset with current sample rate, state restored

#### Thread safety

The plugin instance is behind a `parking_lot::Mutex`. Audio thread
locks for `process()`, main thread locks for `render()`. Mouse
event handlers drop the lock before calling host callbacks to avoid
deadlocks.

---

### Troubleshooting

**Plugin doesn't detect the new dylib:**
- Check `target/debug/lib{crate_name}.dylib` exists after build
- Override: `TRUCE_LOGIC_PATH=/path/to/lib.dylib`
- Shell skips reload if CRC32 hash hasn't changed

**macOS "code signature invalid":**
- Shell codesigns the dylib copy. Install Xcode CLI tools:
  `xcode-select --install`

**Audio glitch on reload:**
- Expected (~5-50ms dropout). Development tool, not production.

**"ABI mismatch" error:**
- Shell and logic compiled with different Rust versions. Both must
  use the same toolchain. `rust-toolchain.toml` pins the version.

**State lost on reload:**
- `save_state()`/`load_state()` format changed between reloads.
  Plugin resets to defaults.

---

[← Previous](08-gui.md) | [Next →](10-state.md) | [Index](README.md)
