# AAX

AAX (Avid Audio eXtension) is the only plugin format Pro Tools
loads. Shipping into pro-audio studios means shipping AAX. The
format is proprietary to Avid, requires a licensed SDK, and —
for retail distribution — PACE/iLok signing via `wraptool`.

## Status

Opt-in. macOS and Windows; no Linux support (Avid doesn't ship an
AAX SDK for Linux). Truce's AAX support has been tested in Pro
Tools Developer Build; retail Pro Tools requires PACE signing.

## Enable

```toml
[features]
aax = ["dep:truce-aax"]
```

```sh
cargo truce install --aax
```

## Requirements

### 1. Avid AAX SDK

Obtain the AAX SDK from [developer.avid.com]. Requires a free
developer account. Unpack to a path of your choice — common
conventions are `~/sdk/aax-sdk-2-9-0/` on macOS / Linux or
`C:\sdk\aax-sdk-2-9-0\` on Windows.

[developer.avid.com]: https://developer.avid.com/

Tell `cargo truce` where it lives via the `AAX_SDK_PATH` env var
(preferred — gitignored):

```toml
# .cargo/config.toml  (gitignored)
[env]
AAX_SDK_PATH = "/Users/you/sdk/aax-sdk-2-9-0"
```

Or export it:

```sh
export AAX_SDK_PATH=/Users/you/sdk/aax-sdk-2-9-0
```

Or — least preferred, because it's repo-committed — under
`[macos]` / `[windows]` in `truce.toml`:

```toml
[macos]
aax_sdk_path = "/Users/you/sdk/aax-sdk-2-9-0"
```

The env var takes precedence over the toml field if both are set.

### 2. A compiler

- **macOS**: Xcode CLI tools.
- **Windows**: Visual Studio 2019+ with the "Desktop development
  with C++" workload. CMake and Ninja for the AAX SDK's own build.

### 3. (Retail only) PACE wraptool + iLok signing account

Pro Tools **retail** refuses to load AAX plugins that aren't signed
by PACE. For developer testing you can use Pro Tools Developer
(free from Avid with a developer license on your iLok) and skip
PACE signing entirely.

For retail signing:

- `wraptool.exe` on `$PATH` / `%PATH%`.
- Environment variables for your PACE account (read at
  `cargo truce package` time):

```sh
# macOS / Linux
export PACE_ACCOUNT=your-account
export PACE_SIGN_ID=your-sign-id
```

```cmd
REM Windows
set PACE_ACCOUNT=your-account
set PACE_SIGN_ID=your-sign-id
```

If any of these are missing, `cargo truce package` prints a
warning, skips PACE signing, and includes the unsigned AAX in the
installer. Pro Tools Developer still loads it; retail Pro Tools
doesn't.

## Install paths

| Platform | Path (system-wide, **sudo / admin required**) |
|----------|-----------------------------------------------|
| macOS | `/Library/Application Support/Avid/Audio/Plug-Ins/{Name}.aaxplugin/` |
| Windows | `%COMMONPROGRAMFILES%\Avid\Audio\Plug-Ins\{Name}.aaxplugin\` |

Like VST3, `.aaxplugin` is a bundle directory (`Contents/MacOS/...`
on macOS; `Contents/x64/...` on Windows).

## Signing

Three layers, in order:

1. **Codesign / Authenticode.** `cargo truce install` codesigns the
   macOS bundle with `$TRUCE_SIGNING_IDENTITY`. Windows binaries
   are Authenticode-signed at `cargo truce package` time via
   `signtool`.
2. **PACE wrap.** Runs before Authenticode on the
   `.aaxplugin` binary when `PACE_ACCOUNT` / `PACE_SIGN_ID` are set
   and `wraptool` is on PATH. PACE wraps the binary; Authenticode
   signs the wrapped result.
3. **Installer signing.** On macOS the `.pkg` is Developer ID
   Installer-signed and optionally notarized; on Windows the
   `.exe` is Authenticode-signed.

## Build / install / package

```sh
cargo truce install --aax          # build + install (requires sudo/admin)
cargo truce build --aax            # bundle into target/bundles/ without
                                    # installing — useful for CI / artifact
                                    # pipelines
cargo truce package --formats aax  # signed installer (adds PACE if
                                    # PACE_ACCOUNT/SIGN_ID set, else warns)
```

Note: AAX is **not** in the scaffold default feature set. You must
either add it to `[features].default` or pass `--aax` explicitly
every time.

## Validate

No first-party AAX validator is wired into `cargo truce validate`.
Verify manually by opening Pro Tools Developer and loading the
plugin.

### Pro Tools error `-7054`

The retail Pro Tools equivalent of "plugin unsigned" is error code
`-7054`. If you hit this, the plugin isn't PACE-signed. Either:

- Use Pro Tools Developer with a developer iLok license (free for
  testing); or
- Set up a paid PACE signing account and re-package with
  `PACE_ACCOUNT` / `PACE_SIGN_ID` set.

## truce.toml fields

```toml
[[plugin]]
name = "My Effect"
suffix = "effect"
fourcc = "MyFx"
aax_category = "Dynamics"        # Avid's category string — shows in the
                                  # Pro Tools plug-in menu grouping
```

Valid `aax_category` values: `None`, `EQ`, `Dynamics`, `PitchShift`,
`Reverb`, `Delay`, `Modulation`, `Harmonic`, `NoiseReduction`,
`Dither`, `SoundField`, `HardwareGenerators`, `SWGenerators`,
`WrappedPlugin`, `Effect`.

## Hosts

| Host | Platform | Status |
|------|----------|--------|
| Pro Tools Developer | macOS / Windows | working — loads unsigned AAX |
| Pro Tools (retail) | macOS / Windows | requires PACE signing |
| Pro Tools First | — | no longer supported by Avid |
| Any non-Avid host | — | AAX is Pro Tools only |

## Gotchas

- **Linux is unsupported.** Avid doesn't ship an AAX SDK for Linux.
  `cargo truce install --aax` on Linux fails with a clear error
  before building.
- **`AAX_SDK_PATH` must point to the SDK root**, the directory
  that contains `Interfaces/`, `Libs/`, `Utilities/`. Pointing at
  a subdirectory fails with an unhelpful build error from the
  AAX SDK's own CMake.
- **Windows AAX is host-arch only.** The AAX SDK doesn't support
  cross-compiled universal binaries on Windows — `cargo truce
  install --aax` builds for the host architecture only. macOS
  produces universal (arm64 + x86_64) bundles normally.
- **PACE signing is not optional for retail.** Pro Tools "real"
  (the paid user-facing product) refuses unsigned plug-ins.
  Budget the time/cost of a PACE account before committing to a
  retail release.
- **System-wide install requires elevation.** macOS: `sudo`.
  Windows: Administrator command prompt. `cargo truce package`
  avoids that for end users by producing an installer that handles
  elevation at run time.
- **Four-char codes must be stable.** `fourcc` (+ `au_manufacturer`
  from `[vendor]`) is how Pro Tools identifies the plugin in saved
  sessions. Don't change it after release.
