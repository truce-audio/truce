//! Post-link DPI-aware manifest embed for the standalone host exe.
//!
//! Without an embedded application manifest declaring per-monitor v2
//! DPI awareness, Windows treats the standalone process as DPI-unaware
//! and the plugin editor renders blurry / wrong-sized on non-100%
//! displays. baseview does call `SetProcessDpiAwarenessContext` at
//! runtime, but that only gives PMA v1 and runs after the first HWND
//! is created - too late to influence initial sizing.
//!
//! Embedding here (rather than via a `build.rs` in the user's plugin
//! crate) keeps `truce-standalone` and the user's crate free of any
//! manifest plumbing - the truce framework owns the canonical DPI
//! policy in one place. Runs after the staged exe is in place, so
//! it's idempotent across repeated `cargo truce run` invocations.
//!
//! Embed strategy: prefer `mt.exe` from the Windows 10/11 SDK to write
//! a real `RT_MANIFEST` resource into the PE. If `mt.exe` isn't found,
//! drop a `<exe>.manifest` sidecar - Windows loads that automatically
//! when the exe launches, but it's brittle if the bare exe is ever
//! redistributed without the file alongside.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::fs_ctx;
use crate::{Res, tmp_manifests};

/// Per-monitor v2 with v1 fallback for older Win10 builds. The
/// `supportedOS` Windows 10 GUID opts in to non-virtualized DPI APIs.
const MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="truce.standalone" version="1.0.0.0"/>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">True/PM</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2,PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

/// Embed the DPI-aware manifest into `exe`. Falls back to a sidecar
/// `<exe>.manifest` if `mt.exe` isn't available. Best-effort: warnings
/// are printed but the function doesn't fail the build.
pub(crate) fn embed_dpi_manifest(exe: &Path) -> Res {
    let manifest_path = tmp_manifests().join("truce-standalone.manifest");
    fs_ctx::write(&manifest_path, MANIFEST_XML)?;

    if let Some(mt) = locate_mt_exe() {
        // `;#1` → resource id 1, the slot Windows reads for an exe's
        // application manifest at process start.
        let resource_arg = format!("-outputresource:{};#1", exe.display());
        let result = Command::new(&mt)
            .args([
                "-nologo",
                "-manifest",
                &manifest_path.display().to_string(),
                &resource_arg,
            ])
            .status();
        match result {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => eprintln!(
                "  warning: mt.exe exited with {s} embedding manifest into {} \
                 - falling back to sidecar",
                exe.display()
            ),
            Err(e) => {
                eprintln!("  warning: failed to invoke mt.exe ({e}) - falling back to sidecar");
            }
        }
    } else {
        eprintln!(
            "  note: mt.exe not found (install the Windows 10/11 SDK to embed \
             the DPI manifest); writing sidecar instead."
        );
    }

    fs_ctx::copy(&manifest_path, sidecar_path(exe))?;
    Ok(())
}

fn sidecar_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".manifest");
    PathBuf::from(s)
}

/// Locate `mt.exe`. Mirrors `locate_signtool` in `packaging_windows.rs`:
/// PATH first, then the highest-versioned subdir under the Win10 SDK.
fn locate_mt_exe() -> Option<PathBuf> {
    if let Ok(p) = which("mt.exe") {
        return Some(p);
    }
    let sdk_bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    // SDK subdirs are named by version (e.g. `10.0.22621.0`); lexical
    // max picks the newest because Windows SDK versions sort
    // correctly as strings.
    std::fs::read_dir(&sdk_bin)
        .ok()?
        .flatten()
        .map(|e| e.path().join(r"x64\mt.exe"))
        .filter(|p| p.exists())
        .max()
}

fn which(name: &str) -> std::io::Result<PathBuf> {
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
        format!("{name} not found"),
    ))
}

// ---------------------------------------------------------------------------
// Icon embed (RT_GROUP_ICON + RT_ICON resources)
// ---------------------------------------------------------------------------
//
// .ico → PE resource transform. The file format and the in-PE resource
// layout differ in exactly one field (`dwImageOffset` → `nID`), so we
// parse the file's directory once and re-emit it with sequential
// resource IDs. Same `BeginUpdateResource` / `UpdateResource` /
// `EndUpdateResource` triad rcedit uses; staying inside the workspace
// avoids shipping a third-party binary or wiring a per-plugin
// `build.rs`. Resource type constants and resource name `1` follow the
// `MAKEINTRESOURCE` convention - Win32 treats pointer-sized integers
// below 0x10000 as numeric IDs.

// `MAKEINTRESOURCEW`: Win32 treats pointer-sized integers below
// 0x10000 as numeric resource IDs (vs pointers to wide strings).
// `ptr::without_provenance` is the strict-provenance-clean way to
// express that "address but no allocation" - keeps clippy happy
// without `#[allow(clippy::manual_dangling_ptr)]`.
fn make_int_resource(id: u16) -> *const u16 {
    std::ptr::without_provenance(id as usize)
}

/// Convert `usize` → `u16` for resource counts/IDs with a contextual
/// error. An .ico file can carry up to 65535 images, but in practice
/// the truce icon has 7 - overflow here means a malformed input.
fn u16_or(value: usize, ctx: &str) -> std::result::Result<u16, crate::BoxErr> {
    u16::try_from(value)
        .map_err(|_| -> crate::BoxErr { format!("{ctx}: value {value} exceeds u16 range").into() })
}

fn u32_or(value: usize, ctx: &str) -> std::result::Result<u32, crate::BoxErr> {
    u32::try_from(value)
        .map_err(|_| -> crate::BoxErr { format!("{ctx}: value {value} exceeds u32 range").into() })
}

/// Build the `RT_GROUP_ICON` resource payload from the parsed `.ico`
/// directory. Same 6-byte `ICONDIR` header as the file, then a 14-byte
/// `GRPICONDIRENTRY` per image (the 16-byte on-disk entry differs in
/// its last field: file offset → resource ID).
fn build_group_icon_blob(entries: &[IcoEntry]) -> std::result::Result<Vec<u8>, crate::BoxErr> {
    let count = u16_or(entries.len(), "RT_GROUP_ICON entry count")?;
    let mut grp = Vec::with_capacity(6 + entries.len() * 14);
    grp.extend_from_slice(&[0, 0]); // Reserved
    grp.extend_from_slice(&1u16.to_le_bytes()); // Type = 1 (icon)
    grp.extend_from_slice(&count.to_le_bytes());
    for (i, e) in entries.iter().enumerate() {
        grp.push(e.width);
        grp.push(e.height);
        grp.push(e.color_count);
        grp.push(0); // Reserved
        grp.extend_from_slice(&e.planes.to_le_bytes());
        grp.extend_from_slice(&e.bit_count.to_le_bytes());
        grp.extend_from_slice(&e.size_bytes.to_le_bytes());
        // Resource IDs start at 1; `RT_GROUP_ICON` itself takes name 1
        // too but lives in a separate type namespace, so no collision.
        let res_id = u16_or(i + 1, "RT_ICON resource id")?;
        grp.extend_from_slice(&res_id.to_le_bytes());
    }
    Ok(grp)
}

/// Embed `ico` into `exe` as an `RT_GROUP_ICON` + N `RT_ICON`
/// resources, so Windows Explorer / Start Menu show the truce logo
/// for the staged standalone host. Preserves the existing
/// `RT_MANIFEST` (DPI awareness) that `embed_dpi_manifest` wrote.
//
// Pedantic too-many-lines: the body is straight-line FFI calls with
// per-step error handling, and splitting further would just create
// artificial helpers around the resource-update triad. Keeping it
// flat so the SAFETY contract sits next to all three Win32 calls.
#[allow(clippy::too_many_lines)]
pub(crate) fn embed_icon(exe: &Path, ico: &Path) -> Res {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW,
    };

    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    const LANG_NEUTRAL: u16 = 0;

    let ico_bytes = std::fs::read(ico)
        .map_err(|e| -> crate::BoxErr { format!("read {}: {e}", ico.display()).into() })?;
    let entries = parse_ico_directory(&ico_bytes, ico)?;
    let grp = build_group_icon_blob(&entries)?;
    let grp_len = u32_or(grp.len(), "RT_GROUP_ICON payload size")?;

    let exe_w: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `BeginUpdateResource` accepts a null-terminated wide
    // pointer; we own `exe_w` for the duration. `UpdateResource` /
    // `EndUpdateResource` consume the returned handle on success.
    // `make_int_resource` builds the `MAKEINTRESOURCE` integer-as-ptr
    // sentinel - values < 0x10000 are interpreted as IDs by Win32.
    unsafe {
        let h = BeginUpdateResourceW(exe_w.as_ptr(), 0);
        if h.is_null() {
            return Err(format!(
                "BeginUpdateResource failed for {} (error {})",
                exe.display(),
                std::io::Error::last_os_error()
            )
            .into());
        }

        let group_ok = UpdateResourceW(
            h,
            make_int_resource(RT_GROUP_ICON),
            make_int_resource(1),
            LANG_NEUTRAL,
            grp.as_ptr().cast(),
            grp_len,
        );
        if group_ok == 0 {
            // Abandon the update so the .exe isn't left in a half-
            // updated state. `EndUpdateResource` with `fDiscard=TRUE`.
            EndUpdateResourceW(h, 1);
            return Err(format!(
                "UpdateResource(RT_GROUP_ICON) failed for {}: {}",
                exe.display(),
                std::io::Error::last_os_error()
            )
            .into());
        }

        for (i, e) in entries.iter().enumerate() {
            let img = &ico_bytes[e.offset..e.offset + e.size_bytes as usize];
            let img_len = u32_or(img.len(), "RT_ICON payload size")?;
            let res_id = u16_or(i + 1, "RT_ICON resource id")?;
            let ok = UpdateResourceW(
                h,
                make_int_resource(RT_ICON),
                make_int_resource(res_id),
                LANG_NEUTRAL,
                img.as_ptr().cast(),
                img_len,
            );
            if ok == 0 {
                EndUpdateResourceW(h, 1);
                return Err(format!(
                    "UpdateResource(RT_ICON #{}) failed for {}: {}",
                    i + 1,
                    exe.display(),
                    std::io::Error::last_os_error()
                )
                .into());
            }
        }

        let commit_ok = EndUpdateResourceW(h, 0);
        if commit_ok == 0 {
            return Err(format!(
                "EndUpdateResource (commit) failed for {}: {}",
                exe.display(),
                std::io::Error::last_os_error()
            )
            .into());
        }
    }
    Ok(())
}

#[derive(Debug)]
struct IcoEntry {
    width: u8,
    height: u8,
    color_count: u8,
    planes: u16,
    bit_count: u16,
    size_bytes: u32,
    offset: usize,
}

fn parse_ico_directory(
    bytes: &[u8],
    path: &Path,
) -> std::result::Result<Vec<IcoEntry>, crate::BoxErr> {
    // ICONDIR: u16 reserved (=0), u16 type (=1 icon), u16 count.
    // Then `count` × 16-byte `ICONDIRENTRY`.
    if bytes.len() < 6 {
        return Err(format!("{}: truncated .ico (no header)", path.display()).into());
    }
    let reserved = u16::from_le_bytes([bytes[0], bytes[1]]);
    let kind = u16::from_le_bytes([bytes[2], bytes[3]]);
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if reserved != 0 || kind != 1 {
        return Err(format!(
            "{}: not an icon file (reserved={reserved}, type={kind})",
            path.display()
        )
        .into());
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 6 + i * 16;
        if off + 16 > bytes.len() {
            return Err(format!("{}: truncated ICONDIRENTRY {}", path.display(), i).into());
        }
        let size_bytes = u32::from_le_bytes([
            bytes[off + 8],
            bytes[off + 9],
            bytes[off + 10],
            bytes[off + 11],
        ]);
        let image_off = u32::from_le_bytes([
            bytes[off + 12],
            bytes[off + 13],
            bytes[off + 14],
            bytes[off + 15],
        ]) as usize;
        if image_off + size_bytes as usize > bytes.len() {
            return Err(format!(
                "{}: ICONDIRENTRY {} points past end of file",
                path.display(),
                i
            )
            .into());
        }
        out.push(IcoEntry {
            width: bytes[off],
            height: bytes[off + 1],
            color_count: bytes[off + 2],
            // bytes[off+3] is the reserved field, dropped here - the
            // 14-byte resource entry zeros it anyway.
            planes: u16::from_le_bytes([bytes[off + 4], bytes[off + 5]]),
            bit_count: u16::from_le_bytes([bytes[off + 6], bytes[off + 7]]),
            size_bytes,
            offset: image_off,
        });
    }
    Ok(out)
}
