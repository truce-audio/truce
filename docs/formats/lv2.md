# LV2

LV2 is the open plugin standard that dominates Linux-native DAWs
(Ardour, Qtractor, Carla, Zrythm, Jalv) and is also supported by
Reaper and Bitwig on other platforms. Truce ships hand-rolled C
bindings (`truce-lv2/src/lib.rs`, no `lv2-sys` dependency), so the
crate itself is MIT/Apache-2.0 licensed and has no runtime
dependencies beyond libc.

## Status

Opt-in. Working on macOS, Windows, and Linux. Tested in Reaper for
Linux; other LV2 hosts (Ardour, Carla, Jalv, Zrythm) not yet
validated but expected to work.

## Enable

Opt-in. Either add `"lv2"` to `[features].default` in
`Cargo.toml`, or pass `--lv2`.

```toml
[features]
default = ["clap", "vst3", "lv2"]
lv2 = ["dep:truce-lv2"]
```

```sh
cargo truce install --lv2
```

## Requirements

- Nothing beyond the usual platform toolchain. The shim is pure
  Rust; no SDK, no env vars, no external signing.

## Install paths

| Platform | Path |
|----------|------|
| Linux | `~/.lv2/{slug}.lv2/` |
| macOS | `~/Library/Audio/Plug-Ins/LV2/{slug}.lv2/` |
| Windows | `%APPDATA%\LV2\{slug}.lv2\` |

All three are **user-scope** — no admin / sudo needed on any
platform.

`{slug}` is the plugin name lowercased with hyphens (e.g. "Truce
Gain" → `truce-gain`). The slug lives in the bundle directory name,
the `.so` / `.dylib` / `.dll` filename, and the Turtle IRI
references in `manifest.ttl`. The slugging avoids the need for
percent-encoding in TTL IRIs and keeps strict LV2 hosts happy.

Bundle contents:

```
{slug}.lv2/
├─ manifest.ttl          (plugin URI, binary path, extensions)
├─ plugin.ttl            (port layout, parameter descriptors, UI type)
└─ {slug}.{so,dylib,dll} (the compiled plugin)
```

The TTL files are generated at install time by calling the
plugin's `__truce_lv2_emit_bundle` FFI entry point. They're
regenerated on every install and don't need to be committed.

## Plugin URI

The plugin URI is derived from `vendor.url` + `/lv2/` +
`clap_id`:

```
https://github.com/your-org/your-repo/lv2/com.vendor.my-plugin
```

Many LV2 hosts (lilv's reference loader, Reaper) require an HTTP
URI. Don't change `vendor.url` or `clap_id` after release — the URI
is the LV2 host's identity for the plugin.

## Supported extensions

- **`lv2:AudioPort`** — audio in/out.
- **`atom:AtomPort` with `midi:MidiEvent`** — MIDI input/output as
  LV2 Atom messages.
- **`state:interface`** — preset save/restore via truce's
  `save_state` / `load_state`.
- **UI types**:
  - Linux: `ui:X11UI` (GUI hosted directly on the parent X11 window
    ID).
  - macOS: `ui:CocoaUI` (GUI hosted on an `NSView`).
  - Windows: `ui:WindowsUI` (GUI hosted on an `HWND`).

## Signing

None. LV2 has no signing requirements on any platform. `cargo truce
package` still emits the bundle as part of the platform installer.

## Build / install / package

```sh
cargo truce install --lv2
cargo truce build --lv2
cargo truce package --formats lv2
```

## Validate

No first-party LV2 validator is wired into `cargo truce validate`
yet. For manual checking, Carla or Jalv will print any TTL parse
errors when they try to load the bundle:

```sh
carla   # Load from file… → select {slug}.lv2/
jalv 'https://your-uri/here'
```

## Hosts

| Host | Platform | Status |
|------|----------|--------|
| Reaper | Linux / macOS / Windows | primary testbed |
| Ardour | Linux | should work; not yet validated |
| Qtractor | Linux | should work; not yet validated |
| Carla (plugin host) | Linux | should work; not yet validated |
| Jalv (CLI host) | Linux | should work; not yet validated |
| Zrythm | Linux | should work; not yet validated |
| Bitwig Studio | any | LV2 support is nominal; prefer CLAP there |

## Gotchas

- **Plugin URI is immutable**. Once a plugin ships, the URI is the
  handle by which hosts refer to it in saved sessions. Changing
  `vendor.url` or `clap_id` will break every saved session using
  your plugin. Choose them carefully.
- **X11UI `widget` = parent window ID**. truce's X11UI reports the
  plugin's parent window as its own widget (rather than creating a
  distinct child). Works in Reaper, expected to work in Ardour /
  Jalv; stricter hosts may object and require a follow-up. Flag
  bugs against `truce-lv2`.
- **No MIDI 2.0**. LV2 Atom carries MIDI 1.0 byte streams. Plugins
  exposing MIDI 2.0 `EventBody` variants only see the MIDI 1.0
  mapping when loaded as LV2.
- **Bundle slugs are case-insensitive**. `Truce Gain`, `TRUCE-gain`,
  and `truce gain` all produce the same slug `truce-gain`. If two
  plugins in the same project slug to the same name they will
  overwrite each other — differentiate by editing `bundle_id` /
  `name` in `truce.toml`.
