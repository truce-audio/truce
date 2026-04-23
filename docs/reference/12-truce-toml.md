# truce.toml Reference

Every field `cargo truce` reads, grouped by table.

`truce.toml` lives at your project root alongside `Cargo.toml`. It describes who you are, what plugins you ship, and how they should be packaged. Per-developer build settings (signing, SDK paths) live in `.cargo/config.toml` or environment variables — not here. See [Environment variables](#environment-variables) at the bottom.

---

## `[vendor]` — required

Who publishes the plugin.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `name` | string | yes | — | Human-readable company name. Shows up in DAW plugin lists, installers, and Apps & Features / Finder. |
| `id` | string | yes | — | Reverse-DNS vendor prefix (`com.mycompany`). Used for AU/VST3/CLAP IDs and for Windows installer AppId. |
| `url` | string | no | — | Vendor website URL. Surfaced in the Windows installer's "Publisher URL" field. |
| `au_manufacturer` | string | yes | — | Exactly 4 ASCII characters. The AU manufacturer code. Used in the AU subtype tuple and must be unique per vendor. |

```toml
[vendor]
name = "My Company"
id = "com.mycompany"
url = "https://mycompany.example"
au_manufacturer = "MyCo"
```

---

## `[[plugin]]` — one entry per plugin, required

Each `[[plugin]]` describes one plugin to build and install. At least one is required.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `name` | string | yes | — | Human-readable plugin name. Used for bundle filenames (`{name}.clap`, `{name}.vst3`, etc.) and DAW display names. |
| `suffix` | string | yes | — | Short kebab-case identifier used in CLI (`cargo truce install -p <suffix>`), dylib stems, and install paths. Typically matches the `Cargo.toml` package name. |
| `crate` | string | yes | — | The Cargo package name for this plugin (`crate = "my-effect"`). Hyphens become underscores in the built `.dll`/`.dylib` filename. |
| `category` | string | yes | — | One of `"effect"`, `"instrument"`, `"midi"`. Determines AU/VST3/CLAP category metadata. |
| `fourcc` | string | no† | — | Exactly 4 ASCII characters. AU subtype code, used as the plugin's unique ID across formats. Required unless `au_subtype` is given. |
| `au_type` | string | no | derived | Override the AU type. Defaults to `"aumu"` for instruments, `"aufx"` for effects/midi. Rarely set manually. |
| `au_subtype` | string | no | = `fourcc` | Synonym for `fourcc`. If both are set, `fourcc` wins. |
| `au3_subtype` | string | no | = `fourcc` | 4-char subtype for AU v3 specifically. Override if you want v2 and v3 to differ (useful during migration). |
| `au_tag` | string | no | `"Effects"` | AU category tag. Common values: `"Effects"`, `"Synthesizer"`, `"Dynamics"`, `"EQ"`, `"Filter"`, `"MIDI"`. |
| `clap_name` | string | no | = `name` | Override the plugin name shown to CLAP hosts. See [Per-format display names](#per-format-display-names). |
| `vst3_name` | string | no | = `name` | Override the plugin name shown to VST3 hosts. |
| `vst2_name` | string | no | = `name` | Override the plugin name shown to VST2 hosts. |
| `au_name`   | string | no | = `name` | Override the plugin name shown to AU v2 hosts. |
| `au3_name`  | string | no | = `"{name} v3"` | Override the AU v3 display name **and** the `/Applications/{au3_name}.app` install path. |
| `aax_name`  | string | no | = `name` | Override the plugin name shown to AAX (Pro Tools). |
| `lv2_name`  | string | no | = `name` | Override the LV2 `doap:name` written into `plugin.ttl`. |

† One of `fourcc` or `au_subtype` must be present.

```toml
[[plugin]]
name = "My Effect"
suffix = "effect"
crate = "my-effect"
category = "effect"
fourcc = "MyFx"
au_tag = "Dynamics"

[[plugin]]
name = "My Synth"
suffix = "synth"
crate = "my-synth"
category = "instrument"
fourcc = "MySy"
au3_subtype = "MySz"      # different v3 code during migration
au_tag = "Synthesizer"
```

### Category → format-specific metadata

| `category` | CLAP features | VST3 category | AU type |
|---|---|---|---|
| `"effect"` | `audio-effect` | `Fx` | `aufx` |
| `"instrument"` | `instrument` | `Instrument\|Synth` | `aumu` |
| `"midi"` | `note-effect` | `Fx\|Event` | `aumi` |

### Per-format display names

By default every format shows the plugin under `name`. The optional
`{format}_name` fields let you override that display name per host
format — useful when you need to disambiguate side-by-side installs
(AU v2 vs AU v3, beta vs stable) or work around a host that already
ships a plugin with the same name.

```toml
[[plugin]]
name = "Acme Gain"
suffix = "gain"
crate = "acme-gain"
category = "effect"
fourcc = "AGn1"

# AU v2 keeps the bare name; AU v3 ships side-by-side under a different
# label so both show up in Logic without one shadowing the other.
au3_name = "Acme Gain (AUv3)"

# Beta build for VST3-only testers — production binaries omit this.
vst3_name = "Acme Gain BETA"
```

Behavior per format:

- **CLAP / VST3 / VST2 / AU v2 / AAX / LV2** — overrides only the
  *display name* surfaced to hosts. Bundle filenames, IDs, and on-disk
  install paths still derive from `name` (so the host re-discovers an
  upgraded build cleanly).
- **AU v3** — `au3_name` overrides both the display name **and** the
  containing app bundle path: `/Applications/{au3_name}.app` instead of
  the legacy `/Applications/{name} v3.app`.

The override is applied at install time via a per-format
`TRUCE_{FORMAT}_NAME_OVERRIDE` environment variable that
`cargo truce install` and `cargo truce package` pass through to the
build, so bundles built for distribution carry the override too.

---

## `[macos]` — optional

macOS-specific build settings.

> **Prefer `.cargo/config.toml` (gitignored).** Everything in this section is per-developer / machine-specific — set the equivalent env vars in `[env]` there instead of committing paths to the repo. See [Environment variables](#environment-variables) at the bottom.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `aax_sdk_path` | string | no | — | Absolute path to the AAX SDK root. Overridden by `AAX_SDK_PATH` env var if that's set. |

```toml
[macos]
# aax_sdk_path = "/path/to/aax-sdk-2-9-0"   # prefer .cargo/config.toml [env].AAX_SDK_PATH
```

---

## `[macos.signing]` — optional

macOS code-signing identities. Parallels `[windows.signing]`: credentials live here, installer appearance and notarization live in `[macos.packaging]`.

> **Prefer `.cargo/config.toml` (gitignored).** Signing identities are per-developer — use `TRUCE_SIGNING_IDENTITY` / `TRUCE_INSTALLER_SIGNING_IDENTITY` in `[env]` rather than committing them here. See [Environment variables](#environment-variables).

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `application_identity` | string | no | `"-"` (ad-hoc) | `codesign -s` identity for bundles. Full "Developer ID Application: Name (TEAMID)" string or `"-"` for ad-hoc. Override via `TRUCE_SIGNING_IDENTITY` env var. |
| `installer_identity` | string | no | — | `productbuild --sign` identity. "Developer ID Installer: Name (TEAMID)". Required to produce a trusted `.pkg`. Override via `TRUCE_INSTALLER_SIGNING_IDENTITY` env var. |

```toml
[macos.signing]
application_identity = "Developer ID Application: Your Name (TEAMID)"
installer_identity   = "Developer ID Installer: Your Name (TEAMID)"
```

---

## `[macos.packaging]` — optional

Notarization + post-sign steps for `cargo truce package` on macOS.

> **`notarize` is project policy and belongs in tracked `truce.toml`. `apple_id` / `team_id` are per-developer — prefer `APPLE_ID` / `TEAM_ID` in `.cargo/config.toml` `[env]`, or use a keychain profile (`xcrun notarytool store-credentials TRUCE_NOTARY`).**

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `notarize` | bool | no | `false` | When `true`, submit the `.pkg` to Apple's notary service and staple the ticket. `--no-notarize` on the CLI skips this. |
| `apple_id` | string | no | — | Apple ID used for notarization. Can also come from `APPLE_ID` env var. |
| `team_id` | string | no | — | Team ID used for notarization. Can also come from `TEAM_ID` env var. |

If `apple_id`/`team_id` are absent and no keychain profile named `TRUCE_NOTARY` exists, notarization fails with instructions.

```toml
[macos.packaging]
notarize = true
# apple_id = "you@example.com"     # or use APPLE_ID env var
# team_id  = "ABCDEFG123"          # or use TEAM_ID env var
```

---

## `[windows]` — optional

Windows-specific build settings.

> **Prefer `.cargo/config.toml` (gitignored).** Everything here is per-developer / machine-specific — set `AAX_SDK_PATH` in `[env]` rather than committing a Windows path to the repo. See [Environment variables](#environment-variables).

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `aax_sdk_path` | string | no | — | Absolute path to the AAX SDK root. Overridden by `AAX_SDK_PATH` env var. |

```toml
[windows]
# aax_sdk_path = 'C:\Users\you\aax-sdk-2-9-0'   # prefer .cargo/config.toml [env].AAX_SDK_PATH
```

---

## `[windows.signing]` — optional

Authenticode signing credentials for `cargo truce package`. First configured source wins, in order: Azure → SHA1 thumbprint → `.pfx` file. Absence is fine — binaries and installer ship unsigned with a warning.

> **Prefer `.cargo/config.toml` (gitignored) or CI secrets.** Signing credentials are per-developer / per-environment and should never be committed. Azure uses `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` from the environment; `.pfx` files use `TRUCE_PFX_PASSWORD`. See [Environment variables](#environment-variables).

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `azure_account` | string | no† | — | Azure Trusted Signing account name. |
| `azure_profile` | string | no† | — | Azure Trusted Signing certificate profile name. |
| `azure_dlib` | string | no | standard install path | Override for `Azure.CodeSigning.Dlib.dll` location. Default: `C:\Program Files\Microsoft Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll`. |
| `sha1` | string | no† | — | SHA1 thumbprint of a cert in the Windows cert store. Paired with `cert_store`. |
| `cert_store` | string | no | `"My"` | Windows cert store to search. Usually `"My"` (current user's personal store). |
| `pfx_path` | string | no† | — | Path to a `.pfx` code-signing cert. Password read from `TRUCE_PFX_PASSWORD` env var. |
| `timestamp_url` | string | no | DigiCert | RFC 3161 timestamp server. Default: `http://timestamp.digicert.com`. |

† Must set one of: `azure_account` + `azure_profile`, or `sha1`, or `pfx_path`.

```toml
# Option A: Azure Trusted Signing (recommended for new setups)
[windows.signing]
azure_account = "MySigningAccount"
azure_profile = "MyProfile"

# Option B: existing cert in Windows cert store
[windows.signing]
sha1 = "ABC123DEF456..."

# Option C: .pfx file
[windows.signing]
pfx_path = 'C:\signing\cert.pfx'
# Set TRUCE_PFX_PASSWORD env var separately
```

---

## `[windows.packaging]` — optional

Inno Setup installer customization.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `publisher` | string | no | `[vendor].name` | "Publisher" field in the installer and Apps & Features. |
| `publisher_url` | string | no | `[vendor].url` | URL shown in the installer's "Publisher" area. |
| `installer_icon` | string | no | — | Path (relative to workspace root) to a `.ico` for the installer window and the uninstaller. |
| `welcome_bmp` | string | no | — | Path to a 164×314 `.bmp` shown on the wizard's welcome/finish pages. |
| `license_rtf` | string | no | — | Path to a `.rtf` or `.txt` license shown on a dedicated page. |
| `app_id` | string | no | `{vendor.id}.{plugin.suffix}` | Stable identifier Inno Setup uses to recognize upgrades. Change only if renaming vendor/plugin. |

```toml
[windows.packaging]
publisher = "My Company, LLC"
publisher_url = "https://mycompany.example"
installer_icon = "branding/installer.ico"
welcome_bmp = "branding/welcome.bmp"
license_rtf = "LICENSE.rtf"
```

---

## `[packaging]` — optional (both platforms)

Cross-platform packaging options.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `formats` | array of strings | no | plugin's Cargo default features | Which formats to include when packaging. Valid values: `"clap"`, `"vst3"`, `"vst2"`, `"au2"`, `"au3"`, `"aax"`. The CLI `--formats` flag overrides this. |
| `welcome_html` | string | no | — | **macOS only** — path to an HTML welcome screen for the `.pkg` installer. |
| `license_html` | string | no | — | **macOS only** — path to an HTML license for the `.pkg` installer. |

```toml
[packaging]
formats = ["clap", "vst3", "aax"]
welcome_html = "installer/welcome.html"   # macOS
license_html = "installer/license.html"   # macOS
```

---

## Environment variables

These live outside `truce.toml` — in `.cargo/config.toml` (gitignored) or your shell profile. They override the corresponding `truce.toml` fields when set.

| Variable | Overrides | Purpose |
|---|---|---|
| `TRUCE_SIGNING_IDENTITY` | `[macos.signing].application_identity` | macOS codesign identity |
| `TRUCE_INSTALLER_SIGNING_IDENTITY` | `[macos.signing].installer_identity` | macOS productbuild identity |
| `AAX_SDK_PATH` | `[macos].aax_sdk_path` / `[windows].aax_sdk_path` | AAX SDK root |
| `APPLE_ID` | `[macos.packaging].apple_id` | Notarization Apple ID |
| `TEAM_ID` | `[macos.packaging].team_id` | Notarization team ID |
| `APP_SPECIFIC_PASSWORD` | — | App-specific password for notarization (never goes in `truce.toml`) |
| `TRUCE_NOTARY_PROFILE` | — | Keychain profile name for `notarytool`. Default: `TRUCE_NOTARY`. |
| `TRUCE_PFX_PASSWORD` | — | Password for the Windows `.pfx` cert (never goes in `truce.toml`) |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | — | Azure Trusted Signing auth (via `DefaultAzureCredential`) |

Set them in `.cargo/config.toml`:

```toml
[env]
TRUCE_SIGNING_IDENTITY = "Developer ID Application: Your Name (TEAMID)"
AAX_SDK_PATH = "/path/to/aax-sdk-2-9-0"
```

`cargo truce` reads both `[env]` (when invoked via `cargo run`) and parses `.cargo/config.toml` directly, so the same config works whether you invoke it as `cargo truce` or `cargo xtask`.

---

## Full example

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
suffix = "gain"
crate = "acme-gain"
category = "effect"
fourcc = "AGn1"
au_tag = "Dynamics"

[[plugin]]
name = "Acme Synth"
suffix = "synth"
crate = "acme-synth"
category = "instrument"
fourcc = "ASy1"
au_tag = "Synthesizer"
```

---

[← Previous](11-building.md) | [Index](README.md) | [Next →](13-packaging.md)
