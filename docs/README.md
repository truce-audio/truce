# Status

Updated 2026-04-27. Version 0.14.1. **Pre-1.0 — active development.**

Plugin authors can build, install, validate, and package across CLAP,
VST3, VST2, LV2, AU v2, AU v3, and AAX from a single Rust crate, on
macOS, Windows, and Linux. Standalone host + signed installers (.pkg
on macOS, .exe on Windows) work today. Hot-reload via `--shell` works
for every format except AU v3 and AAX, which have known caveats below.

## What works today

| Format | macOS | Windows | Linux | Hosts smoke-tested |
|---|---|---|---|---|
| CLAP | ✅ | ✅ | ✅ | Reaper (all three OSes) |
| VST3 | ✅ | ✅ | ✅ | Reaper, Ableton, FL Studio (macOS / Windows) |
| VST2 | ✅ | ✅ | ✅ | Reaper, Ableton, FL Studio (macOS / Windows) |
| LV2  | ✅ | ✅ | ✅ | Reaper (all three); Ardour / Carla pending |
| AU v2 | ✅ | — | — | Reaper, Logic, GarageBand, Ableton |
| AU v3 | ✅ | — | — | Reaper, Logic, Ableton |
| AAX  | ✅ | ✅† | — | Pro Tools Developer |

† Windows AAX builds and loads in Pro Tools Developer; retail Pro
Tools requires a PACE/iLok signature (untested with a retail
account — see below).

Hot-reload works on every cell except AAX (untested) and AU v3 (appex
sandbox blocks `dlopen` of `target/`; tracked in
`truce-docs/docs/internal/shell-hardening.md`).

## Immediate backlog

Things blocking confidence in the surface, roughly in priority order:

- **Retail iLok / PACE round-trip.** PACE wraptool is wired and
  exercised against a dev iLok account; we haven't yet round-tripped
  through a retail iLok + retail Pro Tools install. Needed before
  documenting AAX as production-ready.
- **Pro Tools shell-mode smoke test.** Manual: load a `--shell` AAX
  bundle in Pro Tools Developer, confirm hot-reload fires. Pro
  Tools' loader vs. dlopen behavior is the open question.
- **AU v3 + `--shell` sandbox-disable entitlement.** Branch the
  AUExt entitlements emitter on `shell_mode` and add
  `com.apple.security.app-sandbox = false` for dev builds only;
  production paths stay sandboxed. ~10 lines of code in
  `crates/cargo-truce/src/commands/install/au_v3.rs`. Item 5 in
  shell-hardening.md.
- **Authenticode round-trip with a real cert.** The Azure Trusted
  Signing / SHA1 thumbprint / `.pfx` paths are wired but haven't
  been exercised with a real EV / OV cert end-to-end.
- **Linux automation + preset round-trip testing in Bitwig and
  Ardour.** Reaper is verified; the others are pending.
- **`cargo truce package` for Linux.** `.deb` / `.rpm` / AppImage
  are not generated today. Linux distribution is "build the bundles
  and ship them yourself" until this lands.
- **Windows hot-reload memory-leak ceiling test.** The
  leak-don't-close pattern is correct but each rebuild leaks the
  previous DLL's address space. Need to confirm the upper bound is
  sane over a multi-hour session.
- **Cross-scope install collision smoke.** `cargo truce validate`
  warns when a plugin is in both user and system scopes; manual
  verification across all formats is open.

## Future

- WebView GUI backend.
- ARA support.
- Distribution-grade dynamic shell (today's `--shell` is dev-loop
  only; making it a shipping mechanism is a phase-2 question — see
  `shell-hardening.md` item 8).
- More example plugins (delay, compressor, reverb).
- crates.io publication once `baseview` ships a release.

## See also

- [`docs/reference/`](reference/) — install, first plugin, params,
  processing, GUI, hot reload, shipping.
- [`docs/formats/`](formats/) — per-format reference (CLAP, VST3,
  VST2, LV2, AU, AAX) with env vars, install paths, gotchas.
- [`docs/reference/hot-reload.md`](reference/hot-reload.md) — how
  `--shell` and the dynamic loader work end-to-end.
