# Plugin Formats

Truce compiles a single plugin crate into up to seven plugin formats.
This directory has a dedicated page per format — what it does, what
it needs, how to turn it on, and what can go wrong.

## Format matrix

| Format | Cargo feature | macOS | Windows | Linux | Scaffolded default | Extras required |
|--------|---------------|-------|---------|-------|--------------------|-----------------|
| [CLAP](clap.md)    | `clap` | ✅ | ✅ | ✅ | ✅ | — |
| [VST3](vst3.md)    | `vst3` | ✅ | ✅ | ✅ | ✅ | — |
| [VST2](vst2.md)    | `vst2` | ✅ | ✅ | ✅ | opt-in | read licensing note |
| [LV2](lv2.md)      | `lv2`  | ✅ | ✅ | ✅ | opt-in | — |
| [AU v2](au.md)     | `au`   | ✅ | — | — | opt-in | Xcode CLI tools |
| [AU v3](au.md)     | `au`   | ✅ | — | — | opt-in | full Xcode, Developer ID signing |
| [AAX](aax.md)      | `aax`  | ✅ | ✅ | — | opt-in | AAX SDK (+ PACE wraptool for retail) |

Scaffolded plugins get `clap` and `vst3` enabled in `[features].default`
in `Cargo.toml`. To opt into another format, add it to `default` or
pass it explicitly to `cargo truce install --<format>`.

## Enabling a format

Two ways to enable an opt-in format:

**Per-install** (ad-hoc):

```sh
cargo truce install --vst2
cargo truce install --lv2
cargo truce install --aax
cargo truce install --clap --vst3 --lv2    # mix and match
```

**Permanently** (edit `Cargo.toml`):

```toml
[features]
default = ["clap", "vst3", "lv2"]    # add the ones you want
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
lv2  = ["dep:truce-lv2"]
au   = ["dep:truce-au"]
aax  = ["dep:truce-aax"]
```

## Quick-reference: install destinations

| Format | macOS | Windows | Linux |
|--------|-------|---------|-------|
| CLAP   | `~/Library/Audio/Plug-Ins/CLAP/{Name}.clap` | `%COMMONPROGRAMFILES%\CLAP\{Name}.clap` | `~/.clap/{Name}.clap` |
| VST3   | `/Library/Audio/Plug-Ins/VST3/{Name}.vst3/` (sudo) | `%COMMONPROGRAMFILES%\VST3\{Name}.vst3\` | `~/.vst3/{Name}.vst3/` |
| VST2   | `~/Library/Audio/VST/{Name}.dylib` | `%PROGRAMFILES%\Steinberg\VstPlugins\{Name}.dll` | `~/.vst/{Name}.so` |
| LV2    | `~/Library/Audio/Plug-Ins/LV2/{Name}.lv2/` | `%APPDATA%\LV2\{Name}.lv2\` | `~/.lv2/{Name}.lv2/` |
| AU v2  | `/Library/Audio/Plug-Ins/Components/{Name}.component/` (sudo) | — | — |
| AU v3  | `/Applications/{Name}.app/Contents/PlugIns/AUExt.appex/` (sudo) | — | — |
| AAX    | `/Library/Application Support/Avid/Audio/Plug-Ins/{Name}.aaxplugin/` (sudo) | `%COMMONPROGRAMFILES%\Avid\Audio\Plug-Ins\{Name}.aaxplugin\` | — |

Commands documented in each format's page use `cargo truce install` so
you never touch these paths directly. They're listed here as a debug
aid if plugins aren't being picked up by your DAW.

## See also

- [First plugin](../reference/first-plugin.md) — end-to-end walkthrough
- [Reference → shipping](../reference/shipping.md) — `install` / `build` / `validate` / `package`, signing, installers, and the full `truce.toml` schema
- [Status](../status.md) — host coverage table
