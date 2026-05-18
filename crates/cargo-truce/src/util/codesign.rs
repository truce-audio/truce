//! macOS code-signing helpers: `codesign_bundle` (inside-out signing
//! across every Mach-O in a bundle), `verify_signed_for_notarization`
//! (mirrors Apple's notarization-server checks locally), and PACE /
//! wraptool signing for AAX. Cross-platform stubs short-circuit on
//! Linux / Windows so callers can invoke unconditionally.

#[cfg(target_os = "macos")]
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use super::{run_codesign, tag_fail, tmp_manifests};

/// Whether the signing identity is a real Developer ID (not ad-hoc).
#[cfg(target_os = "macos")]
pub(crate) fn is_production_identity(identity: &str) -> bool {
    identity != "-"
}

/// Write the entitlements plist used by Developer-ID-signed bundles.
/// Returns the path to the temp file.
///
/// Entitlements:
/// - `allow-unsigned-executable-memory` - JIT / dynamically generated
///   code (egui's wgpu shader cache, hot-reload trampolines).
/// - `device.audio-input` - required for the standalone host on
///   hardened-runtime builds. cpal opens the input device through
///   `CoreAudio` HAL, which (unlike `AVAudioEngine`) is blocked silently
///   without this entitlement: no TCC prompt appears, no error
///   surfaces, the callback just receives zeros. Plugin bundles
///   (CLAP / VST3 / AU / AAX) never open the mic themselves - their
///   DAW does - so the entitlement is a no-op there but cheap to
///   carry, and a single entitlements file keeps the codesign call
///   sites identical across formats.
#[cfg(target_os = "macos")]
pub(crate) fn write_entitlements_plist() -> PathBuf {
    let path = tmp_manifests().join("entitlements.plist");
    let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
    <key>com.apple.security.device.audio-input</key>
    <true/>
</dict>
</plist>"#;
    let _ = fs::write(&path, content);
    path
}

/// Code-sign a bundle (or a single Mach-O). When `identity` is a
/// Developer ID, adds hardened runtime, timestamp, and entitlements
/// (required for notarization). When ad-hoc (`"-"`), performs a
/// simple ad-hoc sign.
/// If `use_sudo` is true the codesign command runs via sudo.
///
/// **Inside-out signing.** When `path` is a directory (a bundle),
/// every Mach-O in the bundle is enumerated and signed explicitly
/// before the bundle's outer seal is applied. This bypasses Apple's
/// `codesign --deep` traversal - which doesn't recurse into
/// `Contents/Resources/` for AAX (`TDMw`) and other non-app bundle
/// types, leaving inner dylibs with their linker-applied ad-hoc
/// signature and breaking notarization. Apple has been deprecating
/// `--deep` for years anyway; enumerate ourselves to be sure.
// On non-macOS targets the body is a no-op `Ok(())` so all three
// args are unused - silence the warnings only on those targets.
// `unnecessary_wraps` likewise fires only off-macOS where the body
// can't actually fail; the `Result` return is required by the
// cross-platform staging callers.
#[cfg_attr(
    not(target_os = "macos"),
    allow(unused_variables, clippy::unnecessary_wraps)
)]
pub(crate) fn codesign_bundle(bundle: &str, identity: &str, use_sudo: bool) -> crate::Res {
    // macOS-only: `codesign` is an Apple tool, and the entitlements plist
    // we write is consumed only by it. On Linux / Windows this is a no-op
    // so the cross-platform `stage_*` helpers can call us unconditionally.
    #[cfg(target_os = "macos")]
    {
        let production = is_production_identity(identity);
        let entitlements = write_entitlements_plist();
        let bundle_path = Path::new(bundle);

        let sign_one = |target: &OsStr| -> crate::Res {
            let mut args: Vec<&OsStr> = vec![
                OsStr::new("--force"),
                OsStr::new("--sign"),
                OsStr::new(identity),
            ];
            if production {
                args.extend_from_slice(&[
                    OsStr::new("--options"),
                    OsStr::new("runtime"),
                    OsStr::new("--timestamp"),
                    OsStr::new("--entitlements"),
                    entitlements.as_os_str(),
                ]);
            }
            args.push(target);
            run_codesign(&args, use_sudo)
        };

        // Inside-out: sign each Mach-O in the bundle's tree before
        // sealing the bundle itself. For a single-file path, this
        // enumeration is empty and the path goes straight to the
        // bundle-level sign below.
        if bundle_path.is_dir() {
            let mach_os = enumerate_mach_os(bundle_path);
            for mach_o in &mach_os {
                sign_one(mach_o.as_os_str())?;
            }
        }

        // Bundle-level (or single-file) seal. With the inner Mach-Os
        // already signed inside-out, we don't need `--deep` here -
        // codesign will validate the inner signatures and stamp the
        // outer Info.plist seal.
        sign_one(OsStr::new(bundle))?;

        if production {
            run_codesign(
                &[
                    OsStr::new("--verify"),
                    OsStr::new("--strict"),
                    OsStr::new(bundle),
                ],
                use_sudo,
            )?;
        }
    }
    Ok(())
}

/// Detect a Mach-O file by its 4-byte magic. Catches 32 / 64-bit
/// thin Mach-O and FAT (universal) binaries in either endianness.
#[cfg(target_os = "macos")]
fn is_mach_o_file(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    if f.read_exact(&mut buf).is_err() {
        return false;
    }
    let magic_be = u32::from_be_bytes(buf);
    matches!(
        magic_be,
        0xFEED_FACE      // thin Mach-O 32-bit, BE
        | 0xFEED_FACF    // thin Mach-O 64-bit, BE
        | 0xCEFA_EDFE    // thin Mach-O 32-bit, LE
        | 0xCFFA_EDFE    // thin Mach-O 64-bit, LE
        | 0xCAFE_BABE    // FAT/universal, BE
        | 0xBEBA_FECA // FAT/universal, LE
    )
}

/// Walk a directory recursively and return every Mach-O file found.
/// Used by `codesign_bundle` to drive inside-out signing and by the
/// notarization-readiness check.
#[cfg(target_os = "macos")]
fn enumerate_mach_os(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_mach_os(dir, &mut out);
    out
}

#[cfg(target_os = "macos")]
fn walk_mach_os(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            walk_mach_os(&path, out);
        } else if metadata.is_file() && is_mach_o_file(&path) {
            out.push(path);
        }
    }
}

/// Verify that every Mach-O under `path` is signed for notarization:
///   - signed with a Developer ID Application Authority,
///   - has a secure timestamp,
///   - has the hardened runtime enabled.
///
/// These mirror the checks Apple's notarization service runs server-
/// side; running them locally before submission catches issues
/// (unsigned Mach-Os, missing `--timestamp`, missing
/// `--options runtime`, ad-hoc cert leakage) without a six-minute
/// round-trip to Apple's servers.
///
/// No-op when `identity` is ad-hoc - ad-hoc bundles are deliberately
/// not notarization-ready and the checks would all fail by design.
#[cfg(target_os = "macos")]
pub(crate) fn verify_signed_for_notarization(path: &Path, identity: &str) -> crate::Res {
    if !is_production_identity(identity) {
        return Ok(());
    }

    let mach_os = enumerate_mach_os(path);
    if mach_os.is_empty() {
        return Ok(());
    }

    let mut failures: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for mach_o in &mach_os {
        let issues = check_mach_o_signing(mach_o)?;
        if !issues.is_empty() {
            failures.push((mach_o.clone(), issues));
        }
    }

    if failures.is_empty() {
        return Ok(());
    }

    eprintln!();
    eprintln!(
        "{} Notarization-readiness check failed for {} Mach-O(s) under {}:",
        tag_fail(),
        failures.len(),
        path.display()
    );
    for (path, issues) in &failures {
        eprintln!("    {}", path.display());
        for issue in issues {
            eprintln!("      - {issue}");
        }
    }
    eprintln!();
    eprintln!(
        "These issues mirror Apple's notarization-server checks. \
         Submitting now would fail the same way, with a ~6-minute \
         round-trip per attempt."
    );
    Err("notarization-readiness check failed".into())
}

/// Inspect a single Mach-O via `codesign -d -vvvv` and return any
/// notarization-blocking issues. Empty Vec = passes.
#[cfg(target_os = "macos")]
fn check_mach_o_signing(path: &Path) -> Result<Vec<String>, crate::BoxErr> {
    let path_str = path.to_str().ok_or("Mach-O path is not UTF-8")?;
    let output = Command::new("codesign")
        .args(["-d", "-vvvv", path_str])
        .output()?;
    // codesign writes its detail report to stderr.
    let report = String::from_utf8_lossy(&output.stderr);

    let mut issues = Vec::new();

    if report.contains("code object is not signed at all")
        || report.contains("is not signed at all")
    {
        issues.push("not signed".to_string());
        return Ok(issues);
    }

    if !report.contains("Authority=Developer ID Application:") {
        if report.contains("Signature=adhoc") {
            issues.push("ad-hoc signature (not a Developer ID cert)".to_string());
        } else {
            issues.push("not signed with a Developer ID Application certificate".to_string());
        }
    }

    // Timestamp line shows e.g. "Timestamp=Apr 28, 2026 at ..." or
    // "Signed Time=...". Absence (or "Timestamp=none") means no
    // secure timestamp.
    let has_timestamp = report
        .lines()
        .any(|l| l.starts_with("Timestamp=") && !l.contains("Timestamp=none"));
    if !has_timestamp {
        issues.push("missing secure timestamp (--timestamp)".to_string());
    }

    // Hardened runtime: codesign reports it on the CodeDirectory
    // flags line, e.g. "flags=0x10000(runtime)".
    if !report.contains("(runtime)") {
        issues.push("hardened runtime not enabled (--options runtime)".to_string());
    }

    Ok(issues)
}

/// PACE / iLok wraptool, the canonical macOS install path. Eden 5 ships under
/// `Versions/5/`; `Current` is a stable symlink Eden maintains across version
/// bumps. Users who symlinked `wraptool` onto `$PATH` are picked up first.
#[cfg(target_os = "macos")]
pub(crate) fn locate_wraptool_macos() -> Option<PathBuf> {
    if let Ok(p) = which_unix("wraptool") {
        return Some(p);
    }
    for canonical in [
        "/Applications/PACEAntiPiracy/Eden/Fusion/Current/bin/wraptool",
        "/Applications/PACEAntiPiracy/Eden/Fusion/Versions/5/bin/wraptool",
    ] {
        let p = PathBuf::from(canonical);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// Only `locate_wraptool_macos` calls this; gating to macOS keeps Linux
// from warning on the otherwise-cross-platform Unix `PATH` walker.
#[cfg(target_os = "macos")]
pub(crate) fn which_unix(name: &str) -> std::result::Result<PathBuf, std::io::Error> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PATH not set"))?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        name.to_string(),
    ))
}

/// PACE-sign an AAX bundle on macOS. No-ops cleanly when wraptool isn't
/// installed or `PACE_ACCOUNT` / `PACE_SIGN_ID` aren't set - Pro Tools
/// Developer loads unsigned AAX, retail rejects with `-14013` → `-7054`.
///
/// Must run **after** Apple codesign on the bundle: PACE wraps the binary
/// and `--dsigharden` re-signs with hardened-runtime + secure timestamp,
/// which is what notarization wants. Apple-signing afterwards would be
/// detected as PACE tampering at load time.
///
/// Must be the **last** step that touches the bundle: the signed
/// bundle contains a symlink that `cp -r` (and most copy helpers
/// without `-H`) silently turn into a regular file, which breaks the
/// digital seal at load time.
#[cfg(target_os = "macos")]
pub(crate) fn pace_sign_aax_macos(bundle: &Path) -> crate::Res {
    let Some(wraptool) = locate_wraptool_macos() else {
        eprintln!(
            "    wraptool not found - AAX bundle is unsigned for PACE. \
             Pro Tools Developer will load it; retail Pro Tools won't."
        );
        return Ok(());
    };
    let Ok(account) = std::env::var("PACE_ACCOUNT") else {
        eprintln!("    PACE_ACCOUNT not set - skipping PACE signing.");
        return Ok(());
    };
    let Ok(signid) = std::env::var("PACE_SIGN_ID") else {
        eprintln!("    PACE_SIGN_ID not set - skipping PACE signing.");
        return Ok(());
    };

    eprintln!("    wraptool: PACE-signing {}", bundle.display());
    let bundle_str = bundle
        .to_str()
        .ok_or("AAX bundle path is not valid UTF-8")?;
    let status = Command::new(&wraptool)
        .args([
            "sign",
            "--account",
            &account,
            "--signid",
            &signid,
            "--allowsigningservice",
            "--dsigharden",
            "--dsig1-compat",
            "off",
            "--in",
            bundle_str,
            "--out",
            bundle_str,
        ])
        .status()?;
    if !status.success() {
        return Err("wraptool failed".into());
    }
    Ok(())
}
