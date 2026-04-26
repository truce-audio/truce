# Audio Unit (AU v2 and AU v3)

Audio Unit is Apple's native plugin format on macOS. It's the only
format Logic Pro and GarageBand accept, so shipping into the Apple
pro-audio market means shipping AU. Truce supports both AU v2 (the
legacy `.component` bundle loaded in-process) and AU v3 (the
sandboxed `.appex` App Extension loaded out-of-process).

## Status

Opt-in. macOS-only (iOS is planned — see `../../docs-internal/ios.md`
in the docs repo). Tested in Logic Pro, GarageBand, Ableton Live,
and Reaper.

Both AU v2 and AU v3 ship from the **same `au` feature flag** and
the **same Rust staticlib**. What differs is only the bundle layout
and the host integration path.

## Enable

```toml
[features]
au = ["dep:truce-au"]
# or, to enable by default:
default = ["clap", "vst3", "au"]
```

```sh
cargo truce install --au2    # AU v2 only — .component bundle
cargo truce install --au3    # AU v3 only — .appex inside a container .app
cargo truce install --au2 --au3
```

## Requirements

| Format | Toolchain | Signing |
|--------|-----------|---------|
| AU v2  | Xcode CLI tools (`xcode-select --install`) | `$TRUCE_SIGNING_IDENTITY` (ad-hoc `-` works locally) |
| AU v3  | **full Xcode** (`xcodebuild` required) | **Developer ID Application** — ad-hoc signing is rejected |

AU v3 requires full Xcode because `cargo truce install --au3`
generates an Xcode project and drives `xcodebuild` to build the
Swift `AudioUnitFactory` appex into a container app. The CLI tools
alone don't include `xcodebuild`.

### AU v3 signing

The AU v3 appex must be signed with a Developer ID (or development)
identity that Apple's `pluginkit` will accept. Ad-hoc signing (`-`)
is rejected at registration. Set:

```toml
# .cargo/config.toml  (gitignored)
[env]
TRUCE_SIGNING_IDENTITY = "Developer ID Application: Your Name (TEAMID)"
```

Or export it:

```sh
export TRUCE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
```

If `TRUCE_SIGNING_IDENTITY` is `-` or unset when you try to install
AU v3, the install bails with a clear error. AU v2 falls back to
ad-hoc and works with `-`.

## Install paths

| Format | Path (system-wide, **sudo required**) |
|--------|----------------------------------------|
| AU v2 | `/Library/Audio/Plug-Ins/Components/{Name}.component/` |
| AU v3 | `/Applications/{Name}.app/Contents/PlugIns/AUExt.appex/` |

The AU v3 appex is embedded inside a container `.app` at
`/Applications/`. Apple requires this — App Extensions cannot ship
on their own. The container app is a minimal stub that exists only
to host the appex; you don't interact with it.

After an AU v3 install, `pkd` (the PluginKit daemon) picks up the
new extension. If a host doesn't see it, run `cargo truce reset-au`
to flush `pkd` caches and restart the daemon for a clean re-scan.

## Identifiers

From `truce.toml`:

```toml
[vendor]
au_manufacturer = "MyCo"     # 4-char manufacturer code

[[plugin]]
fourcc       = "MyFx"         # AU v2 subtype (and fallback for v3)
au3_subtype  = "MyF3"         # optional — distinct subtype for v3
au_tag       = "Effects"      # "Effects" | "Synthesizers" | "Music Effects"
```

AU v3 can use a **different subtype** from AU v2 (`au3_subtype`).
This matters if you want both versions installed at once without
hosts colliding on the component ID — Logic is particularly strict
about this.

## Build / install / package

```sh
cargo truce install --au2                # installs .component only
cargo truce install --au3                # installs .app into /Applications
cargo truce install                      # all enabled formats (AU v2 + v3 if both in default)

cargo truce build --au2                  # bundle into target/bundles/ without installing
cargo truce build --au3                  # AU v3 .app into target/bundles/ (signed, ready to copy)

cargo truce package --formats clap,vst3,au2,au3     # installer with AU included
```

Both `--au2` and `--au3` produce complete, signed bundles in
`target/bundles/`. `install` is a pure copy + `pluginkit` register
step — the xcodebuild / framework-assembly / inside-out codesign
work all happens at build time, no sudo or `/Applications/` write
needed to produce a bundle. See
[build-install-split.md](../../../truce-docs/docs/internal/build-install-split.md)
(in the docs repo) for the rationale.

## Validate

`cargo truce validate` runs `auval`:

```sh
auval -v aufx MyFx MyCo
```

(Type code from `category`; subtype and manufacturer from
`truce.toml`.) AU v3 is validated by walking
`/Applications/*/Contents/PlugIns/*.appex` and running `auval`
against each.

## Logs

AU v3 runs in a separate appex process. `NSLog` output from the
extension doesn't land in the DAW's log — it goes to unified
logging. Stream it live with:

```sh
cargo truce log-stream-au
```

Useful when the AU v3 GUI is black, the appex crashes on load, or
parameters don't sync.

## Hosts

| Host | AU v2 | AU v3 |
|------|-------|-------|
| Logic Pro | ✅ | ✅ |
| GarageBand | ✅ | ✅ |
| Ableton Live | ✅ | ✅ |
| Reaper | ✅ | ✅ |
| Pro Tools | — (uses AAX) | — |

## Gotchas

- **`xcodebuild` is mandatory for AU v3.** Full Xcode, not just CLI
  tools. `xcode-select -p` must point at a real `Xcode.app`; if it
  points at `/Library/Developer/CommandLineTools`, v3 installs fail.
  Run `sudo xcode-select -s /Applications/Xcode.app`.
- **AU v3 ad-hoc signing is rejected.** The install path refuses
  `-` and prints a clear message. AU v2 is lenient and works
  ad-hoc.
- **System-wide install requires sudo.** Both AU v2 and v3 install
  under `/Library` / `/Applications` which need elevation. `cargo
  truce install` will prompt.
- **Host caches are sticky.** If a plugin appears broken after
  changing IDs or reinstalling, `cargo truce reset-au` flushes the AU
  caches and restarts `pkd` / `AudioComponentRegistrar` (use
  `cargo truce reset-aax` for the Pro Tools AAX cache). If that
  doesn't help, follow up with `cargo truce remove --au2 --au3 -p
  <crate>` and reinstall.
- **v2 and v3 collision.** If `au3_subtype` equals `fourcc`, hosts
  may only surface one of them. Use distinct subtypes (e.g. `MyFx`
  and `MyF3`) if both must coexist.
- **AU v3 is macOS-only.** iOS AU v3 is planned; the design mirrors
  macOS (same Swift `AudioUnitFactory`, `UIViewController` instead of
  `NSViewController`). Not implemented yet — see the iOS plan for
  scope.
