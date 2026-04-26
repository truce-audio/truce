# 8. Shipping

From a plugin that works on your machine to a signed installer
users can double-click. Four commands get you there:

```sh
cargo truce build     # bundle formats into target/bundles/ without installing
cargo truce install   # build + install into your local plugin directories
cargo truce validate  # run auval + pluginval + clap-validator against installed bundles
cargo truce package   # produce a signed distributable installer (.pkg / .exe)
```

Per-format requirements (SDKs, env vars, install paths, signing
specifics) live in [docs/formats/](../formats/). This chapter
covers the cross-format `cargo truce` workflow and signing.

## Enabling formats

Scaffolded plugins ship CLAP + VST3 by default. Add more formats
to `[features].default` in `Cargo.toml`, or pass them explicitly:

```sh
cargo truce install --vst2
cargo truce install --lv2
cargo truce install --au3             # AU v3, macOS only
cargo truce install --aax             # AAX, needs AAX SDK
cargo truce install --clap --vst3 --lv2   # explicit subset
```

Flags add to the active feature set for this one invocation
— they don't modify `Cargo.toml`. To enable a format every time,
add it to `default`:

```toml
[features]
default = ["clap", "vst3", "lv2"]   # CLAP + VST3 + LV2 every time
```

Per-format setup (Xcode required for AU v3, PACE wraptool for
retail AAX, `AAX_SDK_PATH`, etc.) is in the per-format pages:
[clap](../formats/clap.md) · [vst3](../formats/vst3.md) ·
[vst2](../formats/vst2.md) · [lv2](../formats/lv2.md) ·
[au](../formats/au.md) · [aax](../formats/aax.md).

## `install`

```sh
cargo truce install                    # every format in your default features
cargo truce install --clap             # just CLAP
cargo truce install --no-build         # install the existing bundles, skip rebuild
cargo truce install -p my-gain         # single plugin in a workspace (cargo crate name)
```

Builds, bundles, codesigns on macOS, and writes into the standard
plugin directories on your machine:

- **macOS** user-scope for CLAP (`~/Library/Audio/Plug-Ins/CLAP/`),
  system-wide for everything else (`/Library/Audio/Plug-Ins/...` →
  `sudo`).
- **Windows** system-wide for all formats
  (`%COMMONPROGRAMFILES%\...`) — run from an Administrator prompt.
- **Linux** user-scope for everything (`~/.clap`, `~/.vst3`,
  `~/.vst`, `~/.lv2`) — no `sudo`.

Full per-platform table in [formats/README.md](../formats/).

## `build`

Same bundle layout as `install`, but written to
`target/bundles/<Plugin>.{clap,vst3,...}` instead of the system
plugin directories:

```sh
cargo truce build                    # every default format
cargo truce build --clap --vst3      # subset
cargo truce build --au3              # AU v3 .app, fully signed
cargo truce build --aax              # AAX .aaxplugin, fully signed
cargo truce build --hot-reload              # hot-reload shell build
```

Every format flag produces a complete, signed bundle in
`target/bundles/`. 

## `validate`

Runs the free validators:

```sh
cargo truce validate                 # auval + pluginval + clap-validator, all installed plugins
cargo truce validate --auval         # macOS AU only
cargo truce validate --pluginval     # VST3 only
cargo truce validate --clap-validator
```

- **auval** (macOS only, built into CoreAudio) exercises AU v2 +
  AU v3 lifecycle and parameter behaviour.
- **pluginval** (Tracktion,
  <https://github.com/Tracktion/pluginval>) runs at strictness 5
  against VST3 (and VST2 if installed).
- **clap-validator**
  (<https://github.com/free-audio/clap-validator>) exercises CLAP
  lifecycle, parameters, state, and process safety.

Override tool paths with `PLUGINVAL` / `CLAP_VALIDATOR` env vars
when the binaries aren't on `PATH`. `cargo truce doctor` tells you
what's found and what isn't.

## `package`

```sh
cargo truce package                          # every default format, universal arch, signed
cargo truce package -p my-gain               # single plugin (cargo crate name)
cargo truce package --formats clap,vst3,aax  # subset
cargo truce package --host-only              # skip the cross-arch build (dev iteration)
cargo truce package --no-sign                # skip signing (dev)
cargo truce package --no-notarize            # macOS: sign but skip Apple notarization
cargo truce package --no-installer           # Windows: stage files, skip ISCC
```

Output: `target/dist/<Name>-<version>-{macos.pkg,windows.exe}`. Version
comes from `[workspace.package] version` or `[package] version`
in `Cargo.toml`.

**Defaults to universal.** macOS bundles are `lipo`'d fat Mach-O
(`x86_64` + `aarch64`). Windows installers carry both `x64` and
`arm64` payloads and install the right one per machine.

### macOS flow

```
cargo truce package (on macOS)
    ↓
1. Build each format × arch    (x86_64 + aarch64 by default)
2. lipo per format             → fat Mach-O in target/release/
3. Stage into target/package/  (one fat bundle per format)
4. Codesign bundles            Developer ID Application + hardened runtime + timestamp
5. pkgbuild per format         → components/<name>-<format>.pkg
6. productbuild                → target/dist/<Name>-<version>-macos.pkg
                                 (signed with Developer ID Installer)
7. notarytool + staple         (if [macos.packaging].notarize = true)
```

Minimum config for signed + notarised macOS builds, in
`truce.toml`:

```toml
[macos.signing]
application_identity = "Developer ID Application: Your Name (TEAMID)"
installer_identity   = "Developer ID Installer: Your Name (TEAMID)"

[macos.packaging]
notarize = true
```

One-time notarisation keychain setup:

```sh
xcrun notarytool store-credentials TRUCE_NOTARY \
  --apple-id "you@example.com" \
  --team-id "TEAMID" \
  --password "<app-specific-password-from-appleid.apple.com>"
```

AU v2 post-install clears `AudioComponentRegistrar` caches
automatically — no manual step. AAX is built fat from the Avid
SDK (both Apple archs ship in the SDK).

### Windows flow

```
cargo truce package (on Windows, Admin prompt)
    ↓
1. Build each format × arch    (x86_64-pc-windows-msvc + aarch64-pc-windows-msvc)
2. Stage into target\package\  (VST3/AAX bundles carry both archs in arch subdirs;
                                CLAP/VST2 stage both DLLs side-by-side)
3. Authenticode-sign binaries  signtool.exe
4. PACE-sign AAX               wraptool.exe (skipped if PACE_ACCOUNT unset)
5. Render .iss                 target\package\windows\<bundle_id>\installer.iss
6. Compile installer           ISCC.exe → dist\<Name>-<version>-windows.exe
7. Authenticode-sign installer signtool.exe
```

Requirements:

- [Inno Setup 6](https://jrsoftware.org/isinfo.php) for `ISCC.exe`
  (6.3+ if packaging `--universal`).
- Windows 10/11 SDK for `signtool.exe`.
- PACE wraptool on `PATH` + `PACE_ACCOUNT` / `PACE_SIGN_ID` env
  vars, *only* if you're signing AAX for retail Pro Tools.

`cargo truce doctor` reports what's present.

Three Authenticode credential sources, tried in order:

1. **Azure Trusted Signing** (recommended, ~\$120/yr, no hardware
   token). Configure `[windows.signing].azure_account` +
   `azure_profile` and set `AZURE_TENANT_ID` / `AZURE_CLIENT_ID`
   / `AZURE_CLIENT_SECRET` in the environment.
2. **SHA1 cert thumbprint** — typical for OV/EV certs on a
   hardware token. Configure `sha1` + `cert_store`.
3. **`.pfx` file** — configure `pfx_path` + put the password in
   `TRUCE_PFX_PASSWORD`.

With no credentials `package` still runs; it prints a single
warning and emits unsigned binaries. Users get SmartScreen
"Unknown publisher" prompts.

### Universal (x64 + ARM64) Windows

Windows PE has no fat-binary equivalent to Mach-O. The installer
compiles both archs separately and installs the right one via Inno
Setup `Check:` directives for single-file formats, and side-by-side
arch sub-directories for bundle formats:

```
Plugin.vst3/Contents/x86_64-win/Plugin.vst3
Plugin.vst3/Contents/arm64-win/Plugin.vst3
Plugin.aaxplugin/Contents/x64/Plugin.aaxplugin
```

Requirements on the build host:

- VS Installer: "MSVC v143 - VS 2022 C++ ARM64/ARM64EC build
  tools" and "Windows 11 SDK (ARM64)".
- `rustup` installed. The missing `aarch64-pc-windows-msvc` target
  is auto-added on first use by `cargo truce package` (same
  preflight covers `x86_64-apple-darwin` / `aarch64-apple-darwin`
  on macOS). Homebrew's `rust` package shadows rustup and breaks
  this — `which cargo` must point at `~/.cargo/bin/cargo`.

AAX stays host-arch-only under `--universal`: Avid's Windows AAX
SDK ships x64 libs only. The package step stages AAX for the host
arch and prints a note.

### Linux

No signed-installer support yet. `.deb` / `.rpm` packaging is
planned. Meanwhile, distribute via a tarball produced by
`cargo truce build` plus a short `install.sh`, or have users run
`cargo truce install` themselves after `git clone`.

## Secrets belong in `.cargo/config.toml`

Signing identities, AAX SDK paths, notary credentials — anything
machine- or developer-specific — go in `.cargo/config.toml`
(gitignored), not `truce.toml` (committed):

```toml
# .cargo/config.toml
[env]
TRUCE_SIGNING_IDENTITY           = "Developer ID Application: Your Name (TEAMID)"
TRUCE_INSTALLER_SIGNING_IDENTITY = "Developer ID Installer: Your Name (TEAMID)"
AAX_SDK_PATH                     = "/Users/you/aax-sdk-2-9-0"
APPLE_ID                         = "you@example.com"
TEAM_ID                          = "TEAMID"
TRUCE_PFX_PASSWORD               = "…"          # if using .pfx
```

Env vars take precedence over equivalent `truce.toml` fields.

## CI

### macOS (GitHub Actions)

```yaml
jobs:
  package-macos:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable

      - name: Import certs
        env:
          CERT_P12: ${{ secrets.MACOS_CERT_P12_BASE64 }}
          CERT_PW:  ${{ secrets.MACOS_CERT_PASSWORD }}
        run: |
          echo "$CERT_P12" | base64 --decode > cert.p12
          security create-keychain -p "" build.keychain
          security import cert.p12 -k build.keychain -P "$CERT_PW" \
            -T /usr/bin/codesign -T /usr/bin/productbuild
          security set-key-partition-list -S apple-tool:,apple: -s -k "" build.keychain
          security default-keychain -s build.keychain

      - name: Store notarization creds
        run: |
          xcrun notarytool store-credentials TRUCE_NOTARY \
            --apple-id "${{ secrets.APPLE_ID }}" \
            --team-id "${{ secrets.TEAM_ID }}" \
            --password "${{ secrets.APPLE_APP_PASSWORD }}"

      - run: cargo truce package
      - uses: actions/upload-artifact@v4
        with: { name: macos-installer, path: target/dist/*.pkg }
```

### Windows (GitHub Actions)

```yaml
jobs:
  package-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: x86_64-pc-windows-msvc,aarch64-pc-windows-msvc }

      - name: Install Inno Setup
        run: choco install innosetup --no-progress

      - name: Trusted Signing auth
        env:
          AZURE_TENANT_ID:     ${{ secrets.AZURE_TENANT_ID }}
          AZURE_CLIENT_ID:     ${{ secrets.AZURE_CLIENT_ID }}
          AZURE_CLIENT_SECRET: ${{ secrets.AZURE_CLIENT_SECRET }}
        run: echo "Auth via env"

      - run: cargo truce package
      - uses: actions/upload-artifact@v4
        with: { name: windows-installer, path: target/dist/*.exe }
```

AAX CI needs the AAX SDK cached on the runner (via
`actions/cache`, keyed on a hash of the SDK tarball). Or exclude
AAX from `[packaging].formats` in CI and build it only locally
from a developer machine with the SDK installed.

## Troubleshooting

**`cargo truce package` says "no formats to package".**
No plugin has any of `clap`, `vst3`, `vst2`, `lv2`, `au`, `aax` in
its default Cargo features, and `[packaging].formats` isn't set.
Either add a format to `default`, or pass `--formats clap`
explicitly.

**macOS: notarisation says "Invalid".**
Run `cargo truce package --no-notarize`, then submit manually:

```sh
xcrun notarytool submit target/dist/<Name>-<version>-macos.pkg \
  --keychain-profile TRUCE_NOTARY --wait
xcrun notarytool log <submission-id> --keychain-profile TRUCE_NOTARY
```

Most "Invalid" results are an unsigned nested binary — often a
bundle was staged after signing, or ad-hoc signed at install but
not re-signed for `package`.

**Windows: "unknown publisher".** Authenticode isn't configured.
See the credential section above.

**Windows: `ISCC.exe not found`.** Install
[Inno Setup 6](https://jrsoftware.org/isinfo.php).

**Pro Tools rejects AAX with error -7054.** PACE signing required
for retail Pro Tools. Use Pro Tools Developer with a dev iLok
licence for local testing, or set up a paid PACE signing account
before shipping.

**Nothing rebuilds when I change a transitive dep.** Your plugin
probably depends on truce via git. Use
`[patch."https://github.com/truce-audio/truce"]` in `Cargo.toml`
to point at a local checkout during development.

---

<a id="truce-toml-reference"></a>

## Appendix: `truce.toml` reference

Every field `cargo truce` reads, grouped by table. `truce.toml`
lives at the project root alongside `Cargo.toml`. Per-developer
build settings (signing, SDK paths) live in `.cargo/config.toml`
or env vars — not here.

### `[vendor]` — required

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | Human company name. Shows in DAWs, installers, Apps & Features. |
| `id` | string | yes | Reverse-DNS prefix (`com.mycompany`). Used for AU/VST3/CLAP IDs and Windows installer AppId. |
| `url` | string | no | Vendor website. Surfaced in the Windows installer "Publisher URL" field. |
| `au_manufacturer` | string | yes | Exactly 4 ASCII characters. AU manufacturer code — must be unique per vendor. |

### `[[plugin]]` — one per plugin, at least one required

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | Human name. Used for bundle filenames and DAW display names. |
| `bundle_id` | string | yes | Short lowercase, no-dash identifier. Used internally for bundle / extension reverse-DNS IDs (`com.{vendor}.{bundle_id}.au`), install plist filenames, and scratch paths. Not used at the CLI. |
| `crate` | string | yes | Cargo package name. CLI uses this for `-p <crate>`. Hyphens become underscores in built `.dll`/`.dylib`. |
| `category` | string | yes | `"effect"` / `"instrument"` / `"midi"`. Drives AU/VST3/CLAP category metadata. |
| `fourcc` | string | yes† | Exactly 4 ASCII chars. AU subtype + cross-format unique ID. |
| `au_type` | string | no | Override AU type. Defaults: `"aumu"` for instruments, `"aumi"` for midi / note-effects, `"aufx"` for effects. |
| `au_subtype` | string | no | Synonym for `fourcc`. `fourcc` wins if both are set. |
| `au3_subtype` | string | no | 4-char subtype for AU v3 only. Set if v2/v3 must differ. |
| `au_tag` | string | no | AU category tag. Defaults to `"Effects"`. Common: `"Synthesizer"`, `"Dynamics"`, `"EQ"`, `"MIDI"`. |
| `{format}_name` | string | no | Per-format display-name override: `clap_name`, `vst3_name`, `vst2_name`, `au_name`, `au3_name`, `aax_name`, `lv2_name`. |

† One of `fourcc` / `au_subtype` is required.

Category → metadata mapping:

| `category` | CLAP features | VST3 category | AU type |
|------------|---------------|---------------|---------|
| `"effect"` | `audio-effect` | `Fx` | `aufx` |
| `"instrument"` | `instrument` | `Instrument\|Synth` | `aumu` |
| `"midi"` | `note-effect` | `Fx\|Event` | `aumi` |

`{format}_name` overrides the display name surfaced to hosts
while leaving bundle filenames, IDs, and install paths derived
from `name`. One exception: `au3_name` also overrides the
`/Applications/{au3_name}.app` install path.

### `[macos]` / `[windows]` — optional

| Field | Notes |
|-------|-------|
| `aax_sdk_path` | Absolute path to the AAX SDK root. Overridden by `AAX_SDK_PATH` env var if set. Prefer the env var. |

### `[macos.signing]` — optional

| Field | Default | Notes |
|-------|---------|-------|
| `application_identity` | `"-"` (ad-hoc) | `codesign -s` identity. Full `"Developer ID Application: Name (TEAMID)"` or `"-"`. Override via `TRUCE_SIGNING_IDENTITY`. |
| `installer_identity` | — | `productbuild --sign` identity. Required for a trusted `.pkg`. Override via `TRUCE_INSTALLER_SIGNING_IDENTITY`. |

### `[macos.packaging]` — optional

| Field | Default | Notes |
|-------|---------|-------|
| `notarize` | `false` | `true` → submit to Apple notary and staple. `--no-notarize` on the CLI skips it. |
| `apple_id` | — | Apple ID for notarization. Or `APPLE_ID` env var. |
| `team_id`  | — | Team ID for notarization. Or `TEAM_ID` env var. |

### `[windows.signing]` — optional

Three credential sources, tried in order; first wins.

| Field | Notes |
|-------|-------|
| `azure_account` / `azure_profile` | Azure Trusted Signing. Pair with `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` env vars. |
| `azure_dlib` | Override for `Azure.CodeSigning.Dlib.dll` location. |
| `sha1` + `cert_store` | SHA1 thumbprint of a cert in the Windows cert store. `cert_store` defaults to `"My"`. |
| `pfx_path` | Path to a `.pfx` cert. Password in `TRUCE_PFX_PASSWORD` env var. |
| `timestamp_url` | RFC 3161 timestamp server. Defaults to DigiCert. |

### `[windows.packaging]` — optional

| Field | Default | Notes |
|-------|---------|-------|
| `publisher` | `[vendor].name` | "Publisher" in installer and Apps & Features. |
| `publisher_url` | `[vendor].url` | Publisher URL in the installer. |
| `installer_icon` | — | Path to a `.ico` for the installer + uninstaller. |
| `welcome_bmp` | — | Path to a 164×314 `.bmp` for welcome/finish pages. |
| `license_rtf` | — | Path to `.rtf` or `.txt` license. |
| `app_id` | `{vendor.id}.{plugin.bundle_id}` | Inno Setup stable identifier. Only change on rename. |

### `[packaging]` — both platforms

| Field | Default | Notes |
|-------|---------|-------|
| `formats` | plugin's default features | Formats to include when packaging. Valid: `clap`, `vst3`, `vst2`, `lv2`, `au2`, `au3`, `aax`. `--formats` on the CLI overrides. |
| `welcome_html` | — | **macOS only** — welcome screen HTML in the `.pkg`. |
| `license_html` | — | **macOS only** — licence HTML in the `.pkg`. |

### Environment variables

These live outside `truce.toml` — in `.cargo/config.toml`
(gitignored) or your shell. They override the equivalent
`truce.toml` fields.

| Variable | Overrides | Purpose |
|----------|-----------|---------|
| `TRUCE_SIGNING_IDENTITY` | `[macos.signing].application_identity` | macOS codesign identity |
| `TRUCE_INSTALLER_SIGNING_IDENTITY` | `[macos.signing].installer_identity` | macOS productbuild identity |
| `AAX_SDK_PATH` | `[macos / windows].aax_sdk_path` | AAX SDK root |
| `APPLE_ID` | `[macos.packaging].apple_id` | Notarization Apple ID |
| `TEAM_ID` | `[macos.packaging].team_id` | Notarization team ID |
| `APP_SPECIFIC_PASSWORD` | — | Notarization password (never in `truce.toml`) |
| `TRUCE_NOTARY_PROFILE` | — | Keychain profile for `notarytool`. Default `TRUCE_NOTARY`. |
| `TRUCE_PFX_PASSWORD` | — | `.pfx` password (never in `truce.toml`) |
| `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` | — | Azure Trusted Signing auth |
| `PACE_ACCOUNT` / `PACE_SIGN_ID` | — | PACE wraptool auth (AAX retail signing) |

### Full example

```toml
[vendor]
name = "Acme Audio"
id = "com.acmeaudio"
url = "https://acmeaudio.example"
au_manufacturer = "Acme"

[macos.signing]
application_identity = "Developer ID Application: Acme Audio, LLC (TEAM123)"
installer_identity   = "Developer ID Installer: Acme Audio, LLC (TEAM123)"

[macos.packaging]
notarize = true

[windows.signing]
azure_account = "AcmeSigning"
azure_profile = "AcmeProfile"

[windows.packaging]
publisher = "Acme Audio, LLC"
installer_icon = "branding/installer.ico"
welcome_bmp = "branding/welcome.bmp"

[packaging]
formats = ["clap", "vst3", "aax"]

[[plugin]]
name = "Acme Gain"
bundle_id = "gain"
crate = "acme-gain"
category = "effect"
fourcc = "AGn1"
au_tag = "Dynamics"

[[plugin]]
name = "Acme Synth"
bundle_id = "synth"
crate = "acme-synth"
category = "instrument"
fourcc = "ASy1"
au_tag = "Synthesizer"
```
