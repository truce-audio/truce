# Packaging & Distribution

`cargo truce install` installs a plugin on your machine. `cargo truce package` produces a single signed installer file you can hand to anyone.

On macOS this is a `.pkg`. On Windows it's an Inno Setup `.exe`. Both end up in `dist/{PluginName}-{version}-{platform}.{ext}`.

---

## Quick reference

```sh
cargo truce package                          # all default-feature formats, sign, notarize
cargo truce package -p gain                  # single plugin
cargo truce package --formats clap,vst3,aax  # subset of formats
cargo truce package --universal              # Windows: dual-arch x64+ARM64 installer
cargo truce package --no-sign                # skip Authenticode/codesign (dev)
cargo truce package --no-installer           # Windows: stage files, skip ISCC
cargo truce package --no-notarize            # macOS: skip Apple notarization
```

Output: `dist/<PluginName>-<version>-macos.pkg` or `dist/<PluginName>-<version>-windows.exe`. Version comes from `[workspace.package] version` or `[package] version` in `Cargo.toml`.

---

## macOS

### Flow

```
cargo truce package                (on macOS)
    ↓
1. Build each format               cargo build --release with --features per format
2. Stage into                      target/package/<suffix>/
3. Codesign bundles                Developer ID Application + hardened runtime + timestamp
4. pkgbuild per format             components/<name>-<format>.pkg
5. Generate distribution.xml       (format-selection UI)
6. productbuild                    dist/<Name>-<version>-macos.pkg, signed with Developer ID Installer
7. Notarize + staple               xcrun notarytool submit --wait; xcrun stapler staple
```

### Install paths

| Format | Path |
|---|---|
| CLAP | `/Library/Audio/Plug-Ins/CLAP/` |
| VST3 | `/Library/Audio/Plug-Ins/VST3/` |
| VST2 | `/Library/Audio/Plug-Ins/VST/` |
| AU v2 | `/Library/Audio/Plug-Ins/Components/` |
| AU v3 | `/Applications/` (hosts `.appex`) |
| AAX | `/Library/Application Support/Avid/Audio/Plug-Ins/` |

### Minimum config for signed + notarized builds

```toml
[macos.signing]
application_identity = "Developer ID Application: Your Name (TEAMID)"
installer_identity   = "Developer ID Installer: Your Name (TEAMID)"

[macos.packaging]
notarize = true
```

One-time setup for notarization credentials (keychain profile avoids storing the password anywhere else):

```sh
xcrun notarytool store-credentials TRUCE_NOTARY \
  --apple-id "your@apple.id" \
  --team-id "TEAMID" \
  --password "<app-specific-password>"
```

App-specific passwords come from [appleid.apple.com](https://appleid.apple.com/account/manage) → Sign-In and Security → App-Specific Passwords.

### AU v2 cache post-install

Every `.pkg` that installs AU v2 includes a post-install script that kills `AudioComponentRegistrar` and clears `~/Library/Caches/AudioUnitCache/` so the host re-scans. You don't need to do anything — it's wired in automatically.

### Dev builds

```sh
cargo truce package --no-notarize    # sign but skip notarization (faster)
```

Or set `notarize = false` in `[macos.packaging]`.

---

## Windows

### Flow

```
cargo truce package                (on Windows)
    ↓
1. Build each format               cargo build --release --features per format
2. Stage into                      target\package\windows\<suffix>\
3. Authenticode-sign binaries      signtool.exe (skipped if [windows.signing] empty)
4. PACE-sign AAX bundles           wraptool.exe (skipped if PACE_ACCOUNT unset)
5. Render .iss                     target\package\windows\<suffix>\installer.iss
6. Compile installer               ISCC.exe → dist\<Name>-<version>-windows-x64.exe
7. Authenticode-sign installer     signtool.exe
```

### Install paths

| Format | Path |
|---|---|
| CLAP | `%COMMONPROGRAMFILES%\CLAP\{name}.clap` |
| VST3 | `%COMMONPROGRAMFILES%\VST3\{name}.vst3\Contents\x86_64-win\{name}.vst3` |
| VST2 | `%PROGRAMFILES%\Steinberg\VstPlugins\{name}.dll` |
| AAX | `%COMMONPROGRAMFILES%\Avid\Audio\Plug-Ins\{name}.aaxplugin` |

All require admin to install (`PrivilegesRequired=admin` is set in the `.iss`).

### Requirements

- [Inno Setup 6](https://jrsoftware.org/isinfo.php) — for `ISCC.exe`. Auto-discovered from `%PATH%` or `C:\Program Files (x86)\Inno Setup 6\`.
- Windows 10/11 SDK — for `signtool.exe`. Auto-discovered.
- PACE wraptool — only needed if you're signing AAX for retail Pro Tools.

`cargo truce doctor` reports which of these are found.

### Authenticode credentials

Three credential sources, tried in order. First one that's configured wins. Full field reference in [12-truce-toml.md](12-truce-toml.md#windowssigning--optional).

#### Azure Trusted Signing (recommended)

~$120/yr, no hardware token, scales to CI.

```toml
[windows.signing]
azure_account = "YourSigningAccount"
azure_profile = "YourProfile"
```

Install the [Trusted Signing Client Tools](https://learn.microsoft.com/en-us/azure/trusted-signing/how-to-signing-integrations) (provides `Azure.CodeSigning.Dlib.dll`). Set `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` in the environment, or use `az login` in dev.

#### Existing cert in Windows cert store (SHA1 thumbprint)

Typical for OV/EV certs on a hardware token (YubiKey, SafeNet). Install the token driver; cert appears in the current user's `My` store.

```sh
certutil -user -store My        # find the thumbprint
```

```toml
[windows.signing]
sha1 = "abc123..."
cert_store = "My"
```

#### .pfx file

```toml
[windows.signing]
pfx_path = "C:\\signing\\cert.pfx"
```

Set `TRUCE_PFX_PASSWORD` in the environment before `cargo truce package`.

#### No credentials

`cargo truce package` still runs. It prints a single warning, emits unsigned binaries, and produces an unsigned installer. Users get SmartScreen "Unknown publisher" prompts. Fine for dev builds; pass `--no-sign` to silence the warning.

### PACE / iLok (AAX only)

PACE-sign happens *before* Authenticode on the `.aaxplugin` bundle — PACE wraps the binary and Authenticode signs the wrapped result. Pro Tools validates PACE at load time.

```sh
# Before running cargo truce package:
set PACE_ACCOUNT=your-account
set PACE_SIGN_ID=your-sign-id
```

Plus `wraptool.exe` on `%PATH%`. If any of those are missing, PACE signing is skipped with a warning and the unsigned bundle still goes into the installer — Pro Tools Developer will load it; retail Pro Tools won't.

### The `.iss` Inno Setup script

Rendered by `cargo truce package` into `target/package/windows/<suffix>/installer.iss` (kept around for debugging). It's human-readable — you can open it to see exactly what the installer is going to do.

Relevant fields come from `[windows.packaging]`:

```toml
[windows.packaging]
publisher = "Your Company"
publisher_url = "https://..."
installer_icon = "branding/installer.ico"
welcome_bmp = "branding/welcome.bmp"       # 164×314 px
license_rtf = "LICENSE.rtf"
app_id = "{custom-guid}"                   # default: {vendor.id}.{plugin.suffix}
```

The `app_id` is Inno Setup's stable identifier for "same product." Users who installed v0.1.0 and run the v0.2.0 installer get an in-place upgrade instead of a second copy. Only change `app_id` if you rename the plugin or vendor.

### Uninstaller

Inno Setup generates `unins000.exe` next to the install directory and registers it under `HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\`. "Apps & Features" / "Add or Remove Programs" shows it.

```sh
# Silent uninstall from the CLI
"C:\Program Files\<publisher>\<plugin>\unins000.exe" /VERYSILENT /SUPPRESSMSGBOXES
```

### Universal (x64 + ARM64) installers

`cargo truce package --universal` produces a single Windows installer that runs on both x64 and ARM64 machines. Conceptually it's two complete builds stitched into one `.exe`:

```
Truce Gain-0.3.0-windows.exe            # one installer, both archs
├── CLAP / VST2 (single-file formats)
│     Inno Setup's Check: directive installs only the matching DLL
│     (Check: IsArm64 vs Check: not IsArm64)
└── VST3 / AAX (bundle formats)
      Both arch sub-directories installed side-by-side:
        Plugin.vst3/Contents/x86_64-win/Plugin.vst3
        Plugin.vst3/Contents/arm64-win/Plugin.vst3
        Plugin.aaxplugin/Contents/x64/Plugin.aaxplugin
        Plugin.aaxplugin/Contents/arm64/Plugin.aaxplugin
      The host chooses the right one at load time.
```

Windows PE doesn't have a fat-binary equivalent to macOS Mach-O, so every approach compiles both arches separately. `--universal` just bundles and installs them correctly.

Requirements on the build machine:

1. **Rust target**: `rustup target add aarch64-pc-windows-msvc`
2. **VS ARM64 MSVC toolchain**: install "MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools" and "Windows 11 SDK (ARM64)" via the Visual Studio Installer.

`cargo truce doctor` reports whether both are in place.

Limitations:

- **AAX**: Avid's AAX SDK 2.9 ships x64 libs only. An ARM64 AAX build is attempted with `--universal` but usually fails to link — drop `aax` from `--formats` if you're not interested in ARM64 AAX. Universal VST3/VST2/CLAP works fully.
- **Installer size**: roughly 1.7–2× a single-arch installer (lzma2 compresses well but it's still two full Rust binaries).
- **AAX Resources directory**: because the Rust cdylib filename would clash between archs in a shared `Resources/` dir, `--universal` AAX builds tag the filename with the arch (`{stem}_aax_x64.dll`, `{stem}_aax_arm64.dll`). The bridge C++ code discovers them via `FindFirstFileA` so the naming doesn't matter to the host.

Example `.iss` fragment for CLAP with `--universal`:

```ini
[Files]
Source: "...\clap\x64\Plugin.clap"; DestDir: "{commoncf}\CLAP";
  Components: clap; Check: not IsArm64; Flags: ignoreversion overwritereadonly
Source: "...\clap\arm64\Plugin.clap"; DestDir: "{commoncf}\CLAP";
  Components: clap; Check: IsArm64; Flags: ignoreversion overwritereadonly
```

`IsArm64` is a Pascal predicate available from Inno Setup 6.3 onward — the xtask requires Inno Setup 6.3+ when `--universal` is passed.

### aarch64-apple-darwin from Windows?

Not supported. Apple's toolchain is required, and the SDK isn't redistributable outside macOS. Build the macOS universal on a Mac.

---

## What gets packaged

`cargo truce package` includes a format if **all** of:

1. It's enabled in the plugin's Cargo features (either in `default` or explicitly).
2. It's selected via `[packaging].formats` in `truce.toml`, or `--formats` on the CLI. If neither is set, every format from the plugin's default features is included.
3. On Windows, AU v2/v3 are filtered out silently (macOS-only).

Packaging a single plugin vs. the whole project:

```sh
cargo truce package               # every [[plugin]] in truce.toml → one installer each
cargo truce package -p gain       # just the plugin with suffix = "gain"
```

---

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
          CERT_P12:     ${{ secrets.MACOS_CERT_P12_BASE64 }}
          CERT_PW:      ${{ secrets.MACOS_CERT_PASSWORD }}
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
        with: { name: macos-installer, path: dist/*.pkg }
```

### Windows (GitHub Actions)

```yaml
jobs:
  package-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: x86_64-pc-windows-msvc }

      - name: Install Inno Setup
        run: choco install innosetup --no-progress

      - name: Trusted Signing creds
        env:
          AZURE_TENANT_ID:     ${{ secrets.AZURE_TENANT_ID }}
          AZURE_CLIENT_ID:     ${{ secrets.AZURE_CLIENT_ID }}
          AZURE_CLIENT_SECRET: ${{ secrets.AZURE_CLIENT_SECRET }}
        run: echo "Auth via env"

      - run: cargo truce package
      - uses: actions/upload-artifact@v4
        with: { name: windows-installer, path: dist/*.exe }
```

AAX builds need the Avid AAX SDK cached on the runner. Skip AAX for CI by omitting it from `[packaging].formats`, or cache the SDK via `actions/cache`.

---

## Troubleshooting

**`cargo truce package` says "no formats to package"** — No plugin has any of `clap`, `vst3`, `vst2`, `au`, `aax` in its default Cargo features, and `[packaging].formats` isn't set. Add one, or pass `--formats clap` explicitly.

**macOS: notarization says "Invalid"** — Run `cargo truce package --no-notarize`, then manually submit and read the log:
```sh
xcrun notarytool log <submission-id> --keychain-profile TRUCE_NOTARY
```
Usually it's an unsigned nested binary (fix: bundle was staged after signing).

**Windows: installer says "unknown publisher"** — Authenticode isn't configured. See [signing credentials](#authenticode-credentials) above.

**Windows: `ISCC.exe not found`** — Install [Inno Setup 6](https://jrsoftware.org/isinfo.php).

**Windows: Pro Tools rejects AAX with error -7054** — PACE signing required for retail Pro Tools. Use Pro Tools Developer with a dev iLok license for local testing, or set up a paid PACE signing account for shipping.

**Nothing rebuilds when I change a dependency** — The plugin probably depends on truce via git, not path. Use `[patch."https://github.com/truce-audio/truce"]` to point at a local checkout during development.

---

[← Previous](12-truce-toml.md) | [Index](README.md)
