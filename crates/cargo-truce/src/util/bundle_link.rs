//! Link a plugin static archive into a macOS loadable bundle.
//!
//! Rust's `cdylib` link path produces `MH_DYLIB`, which `CFBundle`'s
//! loader (every JUCE-hosted VST3 host, pluginval, `DawDreamer`)
//! rejects. The cleanest fix is to skip the cdylib for macOS bundle
//! formats (VST3, CLAP, VST2) and instead link a Rust `staticlib`
//! through `clang -bundle` to produce a real `MH_BUNDLE`.
//!
//! AU v2 / AAX / Linux / Windows continue to use the cdylib path:
//! AU's component loader and AAX's `dlopen`-from-C++ shim are happy
//! with `MH_DYLIB`, and ELF / PE don't carry the bundle vs dylib
//! distinction.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::build::MacArch;

/// Symbols a CLAP bundle must export so the host can find the
/// descriptor table. The wrapper crate emits the symbol via
/// `#[no_mangle]`; clang `-bundle` would otherwise dead-strip it.
pub(crate) const CLAP_EXPORTS: &[&str] = &["_clap_entry"];

/// Symbols a macOS VST3 bundle must export. `GetPluginFactory` is the
/// factory entry; `BundleEntry`/`BundleExit` (plus the lower-cased
/// variants the SDK ships) get the dyld init/teardown callbacks. The
/// `ModuleEntry`/`ModuleExit` (Linux) and `InitDll`/`ExitDll` (Windows)
/// counterparts aren't defined on macOS in `truce-vst3`, so listing
/// them here would fail the link with "undefined exported symbol".
pub(crate) const VST3_EXPORTS: &[&str] = &[
    "_GetPluginFactory",
    "_BundleEntry",
    "_bundleEntry",
    "_BundleExit",
    "_bundleExit",
];

/// Symbols a VST2 bundle must export. `VSTPluginMain` is the modern
/// entry; `main_macho` is the legacy alias older Steinberg hosts
/// probe.
pub(crate) const VST2_EXPORTS: &[&str] = &["_VSTPluginMain", "_main_macho"];

/// Single source of truth for the "no staticlib emitted" error.
///
/// Plugins scaffolded before 0.44.0 ship `crate-type = ["cdylib",
/// "rlib"]`. The 0.44.0 macOS bundle-link pipeline reads
/// `lib<stem>.a` (a Rust `staticlib` archive) and feeds it to
/// `clang -bundle` to produce an `MH_BUNDLE`, so the missing
/// staticlib makes the install / package step fail. We surface the
/// exact one-line `Cargo.toml` fix here so plugin authors don't
/// have to hunt for it in release notes.
pub(crate) fn missing_staticlib_error(staticlib_path: &Path) -> String {
    format!(
        "macOS bundle link needs a Rust staticlib at\n  \
           {path}\n\
         but cargo didn't emit one.\n\
         \n\
         Starting with truce 0.44.0, macOS bundle formats (CLAP / VST3 / VST2) \
         are linked from `lib<stem>.a` via `clang -bundle`. Plugins scaffolded \
         before 0.44.0 only declared `[\"cdylib\", \"rlib\"]` and need to add \
         `\"staticlib\"` to the `crate-type` array in their plugin crate's \
         `Cargo.toml`.\n\
         \n\
         Exact change:\n\
         \n\
             # before\n\
             [lib]\n\
             crate-type = [\"cdylib\", \"rlib\"]\n\
         \n\
             # after\n\
             [lib]\n\
             crate-type = [\"cdylib\", \"staticlib\", \"rlib\"]\n\
         \n\
         Then re-run the failing command.",
        path = staticlib_path.display(),
    )
}

/// System frameworks the bundle needs at load time. Mirrors what the
/// equivalent cdylib build pulls in from `objc2-app-kit`,
/// `objc2-foundation`, `objc2-quartz-core`, `truce-gpu` (`Metal`),
/// `truce-au` shim (`AudioToolbox` / `AVFAudio` / `CoreAudio` /
/// `CoreMIDI`), and `core-graphics` deps.
///
/// Even though `-Wl,-undefined,dynamic_lookup` would let dyld resolve
/// these symbols when the host already has the frameworks mapped, a
/// non-DAW caller (e.g. `clap-validator`, a CLI) doesn't have `AppKit`
/// pre-loaded. Linking the frameworks here makes `LC_LOAD_DYLIB`
/// commands land in the bundle's Mach-O header, so dyld loads them
/// before symbol resolution kicks in - exactly what the cdylib path
/// does on its own.
const MACOS_PLUGIN_FRAMEWORKS: &[&str] = &[
    "AppKit",
    "Foundation",
    "CoreFoundation",
    "QuartzCore",
    "Metal",
    "AudioToolbox",
    "AVFAudio",
    "CoreAudio",
    "CoreMIDI",
    "CoreGraphics",
];

/// Link one or more per-arch Rust static archives into a single
/// macOS bundle binary at `out_bundle_bin`. Per-arch link via clang;
/// multi-arch is merged with `lipo`.
///
/// `exports` is the set of symbols clang must keep in the output
/// (e.g. [`CLAP_EXPORTS`] / [`VST3_EXPORTS`]). Everything else can be
/// dead-stripped. `-undefined dynamic_lookup` defers system framework
/// references (`CoreFoundation`, `AudioToolbox`, `AppKit`, ...) to the
/// host's dyld at load time; this is the standard pattern for macOS
/// audio plugins and avoids us re-declaring every framework the
/// staticlib's Rust deps would otherwise bring in via
/// `cargo:rustc-link-lib`.
pub(crate) fn link_macos_bundle(
    staticlibs: &[(MacArch, PathBuf)],
    exports: &[&str],
    deployment_target: &str,
    out_bundle_bin: &Path,
) -> crate::Res {
    if staticlibs.is_empty() {
        return Err("link_macos_bundle: no input static archives".into());
    }
    for (_, p) in staticlibs {
        if !p.exists() {
            return Err(
                format!("link_macos_bundle: missing static archive {}", p.display()).into(),
            );
        }
    }

    if let Some(parent) = out_bundle_bin.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if staticlibs.len() == 1 {
        let (arch, staticlib) = &staticlibs[0];
        return clang_bundle_single(*arch, staticlib, exports, deployment_target, out_bundle_bin);
    }

    // Multi-arch: link each slice next to the output, then `lipo` them.
    let mut per_arch_outputs: Vec<PathBuf> = Vec::with_capacity(staticlibs.len());
    for (arch, staticlib) in staticlibs {
        let slice_out = out_bundle_bin.with_extension(format!("{}-slice", arch.triple()));
        clang_bundle_single(*arch, staticlib, exports, deployment_target, &slice_out)?;
        per_arch_outputs.push(slice_out);
    }
    super::build::lipo_into(&per_arch_outputs, out_bundle_bin)?;
    for slice in &per_arch_outputs {
        let _ = std::fs::remove_file(slice);
    }
    Ok(())
}

fn clang_bundle_single(
    arch: MacArch,
    staticlib: &Path,
    exports: &[&str],
    deployment_target: &str,
    out: &Path,
) -> crate::Res {
    let arch_flag = match arch {
        MacArch::Arm64 => "arm64",
        MacArch::X86_64 => "x86_64",
    };
    // Some C-dep chains (notably skia-bindings, which carries a full
    // harfbuzz inside libskia.a) end up bundled into the rustc-emitted
    // staticlib *twice*: once via the depending rlib's embedded native
    // archive and once via the staticlib's own native-dep pass. The
    // duplicate members are byte-identical, but `-Wl,-all_load` below
    // pulls every member and clang errors on the resulting hundreds of
    // duplicate symbols before `-dead_strip` ever runs. Dedup the
    // archive ahead of time; the `_deduped` binding keeps the temp
    // dir alive for the clang invocation and the cleanup runs on its
    // `Drop`.
    let deduped = dedupe_archive_members(staticlib)?;
    let staticlib = deduped.path();
    let mut cmd = Command::new("clang");
    cmd.args([
        "-bundle",
        "-arch",
        arch_flag,
        // Clang spells the min-version flag as a single `=`-joined
        // token; the space-separated form gets parsed as `-m` + a
        // bare version string.
        &format!("-mmacosx-version-min={deployment_target}"),
        // Catch-all for any symbol we didn't explicitly link a
        // framework for (e.g. Rust deps that pull in obscure
        // CoreServices APIs). DAW hosts already have most system
        // frameworks mapped, so the deferred lookup succeeds at load
        // time. We still link the common framework set below so
        // non-DAW callers (`clap-validator`, headless test harnesses)
        // don't fail on `_NSFilenamesPboardType` and friends.
        "-Wl,-undefined,dynamic_lookup",
        // Pull every object from the archive so format-specific
        // entry points (declared with `#[no_mangle]` deep inside
        // truce-{clap,vst3,vst2}) aren't dead-stripped before we get
        // a chance to mark them exported below.
        "-Wl,-all_load",
    ]);
    for framework in MACOS_PLUGIN_FRAMEWORKS {
        cmd.args(["-framework", framework]);
    }
    // C++ runtime - truce-gpu pulls in wgpu/Metal which transitively
    // depends on libc++ symbols. libobjc + libSystem come implicitly
    // via clang's driver defaults; libc++ is the one extra runtime we
    // have to ask for by name.
    cmd.arg("-lc++");
    for sym in exports {
        cmd.arg(format!("-Wl,-exported_symbol,{sym}"));
    }
    // `-all_load` pulls every staticlib object into the link, then
    // `-dead_strip` removes everything not reachable from the
    // `-exported_symbol` roots. Without this the bundle ships every
    // monomorphization and dep the staticlib brought in - roughly
    // double the size of the equivalent cdylib (AU2 / AAX), whose
    // rustc-driven link gets `-dead_strip` for free on apple-darwin.
    cmd.arg("-Wl,-dead_strip");
    cmd.arg(staticlib);
    cmd.arg("-o").arg(out);

    let output = cmd.output().map_err(|e| -> crate::CargoTruceError {
        format!("invoking clang for bundle link: {e}").into()
    })?;
    if !output.status.success() {
        return Err(format!(
            "clang -bundle failed for {} ({arch_flag}):\n{}",
            staticlib.display(),
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }
    Ok(())
}

/// Self-cleaning wrapper around the deduplicated archive. Holds the
/// temp dir we extracted into so the cleanup happens after clang
/// finishes consuming the archive.
struct DedupedArchive {
    archive_path: PathBuf,
    temp_dir: PathBuf,
}

impl DedupedArchive {
    fn path(&self) -> &Path {
        &self.archive_path
    }
}

impl Drop for DedupedArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Extract `staticlib`'s members into a temp directory then recompose
/// a fresh archive with byte-identical duplicate members removed.
///
/// Why this exists: some sys-crate chains (skia-bindings carrying a
/// full harfbuzz inside libskia.a is the live example) end up bundled
/// into the rustc-emitted staticlib *twice* - once via the depending
/// rlib's embedded native archive and once via the staticlib's own
/// native-dep pass. Those duplicate members are byte-identical and can
/// be dropped before `-all_load`.
///
/// Some native archives also contain multiple different object files with
/// the same member name. A plain `ar -x` overwrites those on extraction,
/// which silently drops required objects. We therefore read the archive
/// directly, drop only exact byte duplicates, and give preserved members
/// unique filesystem names before recomposing the archive.
fn dedupe_archive_members(staticlib: &Path) -> Result<DedupedArchive, crate::CargoTruceError> {
    let parent = staticlib
        .parent()
        .ok_or_else(|| -> crate::CargoTruceError {
            format!(
                "staticlib path has no parent directory: {}",
                staticlib.display()
            )
            .into()
        })?;
    let stem = staticlib
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("staticlib");
    // Append PID so concurrent installs (rare but possible: `cargo
    // truce install -p A -p B` against the same workspace) don't
    // collide on the same temp dir.
    let temp_dir = parent.join(format!("{stem}.dedup-{}", std::process::id()));
    if temp_dir.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
    std::fs::create_dir_all(&temp_dir)?;

    let mut members: Vec<PathBuf> = Vec::new();
    let mut seen: HashMap<String, Vec<SeenArchiveMember>> = HashMap::new();
    for member in archive_members(staticlib)? {
        if member.name.is_empty() || member.name.starts_with("__.SYMDEF") || member.name == "/" {
            continue;
        }

        let len = member.bytes.len() as u64;
        let hash = member_hash(&member.bytes);
        let entry = seen.entry(member.name.clone()).or_default();
        let duplicate = entry.iter().any(|existing| {
            existing.len == len
                && existing.hash == hash
                && std::fs::read(&existing.path).is_ok_and(|bytes| bytes == member.bytes)
        });
        if duplicate {
            continue;
        }

        let unique = temp_dir.join(format!(
            "{:06}_{}",
            members.len(),
            archive_member_filename(&member.name)
        ));
        std::fs::write(&unique, &member.bytes)?;
        entry.push(SeenArchiveMember {
            len,
            hash,
            path: unique.clone(),
        });
        members.push(unique);
    }
    if members.is_empty() {
        return Err(format!(
            "archive dedupe found no object members in {} - archive may be empty or corrupt",
            staticlib.display()
        )
        .into());
    }

    let archive_path = parent.join(format!("{stem}.dedup-{}.a", std::process::id()));
    if archive_path.exists() {
        let _ = std::fs::remove_file(&archive_path);
    }
    // `-rcs`: replace (`-r`) + create-if-missing (`-c`) + write symbol
    // table (`-s`). One pass; no separate `ranlib` step.
    let mut compose = Command::new("ar");
    compose.arg("-rcs").arg(&archive_path);
    for m in &members {
        compose.arg(m);
    }
    let composed = compose.output().map_err(|e| -> crate::CargoTruceError {
        format!("invoking ar -rcs for archive dedupe: {e}").into()
    })?;
    if !composed.status.success() {
        return Err(format!(
            "ar -rcs failed for {}:\n{}",
            archive_path.display(),
            String::from_utf8_lossy(&composed.stderr),
        )
        .into());
    }

    Ok(DedupedArchive {
        archive_path,
        temp_dir,
    })
}

struct SeenArchiveMember {
    len: u64,
    hash: u64,
    path: PathBuf,
}

fn member_hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn archive_member_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '_',
            ch => ch,
        })
        .collect()
}

struct ArchiveMember {
    name: String,
    bytes: Vec<u8>,
}

fn archive_members(archive: &Path) -> Result<Vec<ArchiveMember>, crate::CargoTruceError> {
    const MAGIC: &[u8] = b"!<arch>\n";
    const HEADER_LEN: usize = 60;

    let data = std::fs::read(archive)?;
    if !data.starts_with(MAGIC) {
        return Err(format!("{} is not an ar archive", archive.display()).into());
    }

    let mut members = Vec::new();
    let mut gnu_names: Option<Vec<u8>> = None;
    let mut offset = MAGIC.len();
    while offset + HEADER_LEN <= data.len() {
        let header = &data[offset..offset + HEADER_LEN];
        if &header[58..60] != b"`\n" {
            return Err(format!(
                "{} has an invalid ar header at byte {offset}",
                archive.display()
            )
            .into());
        }

        let raw_name = ascii_field(&header[0..16]);
        let size = ascii_field(&header[48..58]).parse::<usize>().map_err(
            |e| -> crate::CargoTruceError {
                format!(
                    "{} has an invalid ar member size at byte {offset}: {e}",
                    archive.display()
                )
                .into()
            },
        )?;
        let payload_start = offset + HEADER_LEN;
        let payload_end =
            payload_start
                .checked_add(size)
                .ok_or_else(|| -> crate::CargoTruceError {
                    format!("{} has an overflowing ar member size", archive.display()).into()
                })?;
        if payload_end > data.len() {
            return Err(format!(
                "{} has a truncated ar member at byte {offset}",
                archive.display()
            )
            .into());
        }

        let payload = &data[payload_start..payload_end];
        let (name, bytes) = parse_archive_member(&raw_name, payload, gnu_names.as_deref())?;
        if raw_name == "//" {
            gnu_names = Some(payload.to_vec());
        } else {
            members.push(ArchiveMember { name, bytes });
        }

        offset = payload_end + (payload_end % 2);
    }

    if offset != data.len() {
        return Err(format!(
            "{} has trailing bytes after the final ar member",
            archive.display()
        )
        .into());
    }

    Ok(members)
}

fn parse_archive_member(
    raw_name: &str,
    payload: &[u8],
    gnu_names: Option<&[u8]>,
) -> Result<(String, Vec<u8>), crate::CargoTruceError> {
    if let Some(name_len) = raw_name.strip_prefix("#1/") {
        let name_len = name_len
            .parse::<usize>()
            .map_err(|e| -> crate::CargoTruceError {
                format!("invalid BSD ar extended-name length `{name_len}`: {e}").into()
            })?;
        if name_len > payload.len() {
            return Err(format!(
                "BSD ar extended-name length {name_len} exceeds member payload size {}",
                payload.len()
            )
            .into());
        }
        let name = archive_member_name(&String::from_utf8_lossy(&payload[..name_len]));
        return Ok((name, payload[name_len..].to_vec()));
    }

    if let Some(offset) = raw_name.strip_prefix('/')
        && raw_name != "/"
        && raw_name != "//"
        && offset.chars().all(|ch| ch.is_ascii_digit())
    {
        let offset = offset
            .parse::<usize>()
            .map_err(|e| -> crate::CargoTruceError {
                format!("invalid GNU ar long-name offset `{offset}`: {e}").into()
            })?;
        let names = gnu_names.ok_or_else(|| -> crate::CargoTruceError {
            "GNU ar long-name member appeared before the string table".into()
        })?;
        if offset >= names.len() {
            return Err(format!(
                "GNU ar long-name offset {offset} exceeds string table size {}",
                names.len()
            )
            .into());
        }
        let end = names[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|idx| offset + idx)
            .unwrap_or(names.len());
        let name = archive_member_name(&String::from_utf8_lossy(&names[offset..end]));
        return Ok((name, payload.to_vec()));
    }

    Ok((archive_member_name(raw_name), payload.to_vec()))
}

fn ascii_field(field: &[u8]) -> String {
    String::from_utf8_lossy(field)
        .trim_matches(|ch| ch == ' ' || ch == '\0')
        .to_string()
}

fn archive_member_name(name: &str) -> String {
    name.trim_end_matches(|ch| ch == '\0' || ch == '/')
        .to_string()
}
