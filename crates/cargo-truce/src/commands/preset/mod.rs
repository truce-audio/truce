//! `cargo truce preset` - preset library management, format
//! conversion, and the in-DAW authoring round-trip.
//!
//! Subcommands:
//!
//! - `list` - every preset across factory (authored library), user,
//!   and pack scopes.
//! - `init` - stamp uuids into hand-authored `.preset` files.
//! - `convert <in> <out>` - re-envelope a preset between any two
//!   native formats (truce plugins only; the canonical state blob
//!   inside every container is the same).
//! - `import <file>` - one native preset file into the authored
//!   library as `.preset` TOML, or a `.zip` pack into the user
//!   pack directory.
//! - `export <out.zip>` - the authored library as a shareable pack
//!   of per-format native files.
//! - `pull [--watch]` - harvest host-saved presets from the OS
//!   preset locations into the authored library. The DAW's own
//!   "save preset" UI becomes the authoring frontend.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::commands::install::presets::{authored_presets_dir, resolved_name};
use crate::preset_codec::{PresetFormat, aupreset_xml, decode, fourcc_int, vstpreset_bytes};
use crate::util::fs_ctx;
use crate::{Config, PluginDef, Res, load_config, project_root};

use truce_build::presets::{AuthoredPreset, ParamAnnotation, read_presets_dir, render_preset_toml};
use truce_utils::preset::PresetMeta;
use truce_utils::presets::{PresetStore, mint_uuid};
use truce_utils::state::{deserialize_state, hash_plugin_id, serialize_state, vst3_cid};
use truce_utils::{safe_filename, slugify};

/// `(meta, params, extra)` for one decoded preset.
type PresetParts = (PresetMeta, Vec<(u32, f64)>, Vec<u8>);
/// `(params, extra)` straight out of a state envelope.
type StateParts = (Vec<(u32, f64)>, Vec<u8>);

pub(crate) fn cmd_preset(args: &[String]) -> Res {
    let Some(sub) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    let rest = &args[1..];
    match sub {
        "list" => cmd_list(rest),
        "init" => cmd_init(rest),
        "convert" => cmd_convert(rest),
        "import" => cmd_import(rest),
        "export" => cmd_export(rest),
        "pull" => cmd_pull(rest),
        "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        other => Err(format!(
            "unknown preset subcommand: {other:?} (see `cargo truce preset --help`)"
        )
        .into()),
    }
}

fn print_help() {
    eprintln!(
        "cargo truce preset - preset library management and conversion

USAGE:
  cargo truce preset list    [-p <crate>]
  cargo truce preset init    [-p <crate>]
  cargo truce preset convert <in> <out> [-p <crate>]
  cargo truce preset import  <file|pack.zip> [--category <c>] [-p <crate>]
  cargo truce preset export  <out.zip> [-p <crate>]
  cargo truce preset pull    [--category <c>] [--new] [--watch] [-p <crate>]

FORMATS (by extension): .preset (authored TOML), .trucepreset,
  .vstpreset, .aupreset, .ttl (LV2)

`pull` scans the OS preset locations hosts save into (Library/Audio/
Presets, VST3 Presets, ~/.lv2, the truce user root) for presets
belonging to the plugin, and converts them into the authored library.
A library preset with the same display name is updated in place
(keeping its uuid; regenerating the file drops hand-written
comments); pass --new to always create instead. --watch rescans
every 2 seconds - save in your DAW, watch presets land in git."
    );
}

// ---------------------------------------------------------------------------
// Shared context
// ---------------------------------------------------------------------------

/// Everything the subcommands derive from `truce.toml` for one
/// plugin: identity (the state-envelope hash and per-format ids),
/// the authored library directory, and param annotations.
struct PluginCtx<'a> {
    p: &'a PluginDef,
    config: &'a Config,
    root: PathBuf,
    plugin_id_hash: u64,
}

impl PluginCtx<'_> {
    fn library_dir(&self) -> Result<PathBuf, crate::CargoTruceError> {
        authored_presets_dir(&self.root, self.p).ok_or_else(|| {
            format!(
                "could not locate the crate directory for \"{}\"",
                self.p.name
            )
            .into()
        })
    }

    fn annotations(&self) -> BTreeMap<u32, ParamAnnotation> {
        let sidecars = truce_build::target_dir(&self.root)
            .join("lv2-meta")
            .join(&self.p.crate_name);
        truce_build::presets::read_param_annotations(&sidecars)
    }

    fn store(&self) -> PresetStore {
        PresetStore::new(&self.config.vendor.name, &self.p.name, self.plugin_id_hash)
    }

    fn library(&self) -> Result<Vec<AuthoredPreset>, crate::CargoTruceError> {
        let dir = self.library_dir()?;
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        read_presets_dir(&dir, true).map_err(Into::into)
    }
}

/// Split `-p <crate>` off an arg list; everything else passes
/// through positionally / as flags for the subcommand.
fn split_plugin_filter(
    args: &[String],
) -> Result<(Option<String>, Vec<String>), crate::CargoTruceError> {
    let mut filter = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-p" {
            filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    Ok((filter, rest))
}

fn single_plugin<'a>(
    config: &'a Config,
    filter: Option<&str>,
) -> Result<&'a PluginDef, crate::CargoTruceError> {
    let plugins = crate::commands::pick_plugins(config, filter)?;
    match plugins.as_slice() {
        [one] => Ok(one),
        _ => Err(format!(
            "this workspace has {} plugins - pick one with -p <crate>",
            plugins.len()
        )
        .into()),
    }
}

fn ctx<'a>(
    config: &'a Config,
    filter: Option<&str>,
) -> Result<PluginCtx<'a>, crate::CargoTruceError> {
    let p = single_plugin(config, filter)?;
    let clap_id = truce_build::plugin_id(&config.vendor.id, &p.name);
    Ok(PluginCtx {
        p,
        config,
        root: project_root(),
        plugin_id_hash: hash_plugin_id(&clap_id),
    })
}

// ---------------------------------------------------------------------------
// list / init
// ---------------------------------------------------------------------------

fn cmd_list(args: &[String]) -> Res {
    let (filter, rest) = split_plugin_filter(args)?;
    expect_no_args(&rest, "list")?;
    let config = load_config()?;
    let ctx = ctx(&config, filter.as_deref())?;

    // Same-uuid copies are one logical preset; the user-proximate
    // copy wins (the rule the runtime store applies across scopes),
    // so a user / pack override replaces the factory row.
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    let mut index_by_uuid: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for preset in ctx.library()? {
        index_by_uuid.insert(preset.meta.uuid.clone(), rows.len());
        rows.push((
            "factory".into(),
            preset.meta.category.clone(),
            preset.meta.name.clone(),
            preset.meta.uuid.clone(),
        ));
    }
    for preset in ctx.store().enumerate() {
        let scope = match preset.scope {
            truce_utils::presets::PresetScope::Factory => "factory",
            truce_utils::presets::PresetScope::User => "user",
            truce_utils::presets::PresetScope::Pack => "pack",
        };
        let row = (
            scope.to_string(),
            preset.category.unwrap_or_default(),
            preset.name,
            preset.uuid,
        );
        match index_by_uuid.get(&row.3) {
            Some(&i) if !row.3.is_empty() => rows[i] = row,
            _ => {
                if !row.3.is_empty() {
                    index_by_uuid.insert(row.3.clone(), rows.len());
                }
                rows.push(row);
            }
        }
    }

    if rows.is_empty() {
        eprintln!("no presets for {}", ctx.p.name);
        return Ok(());
    }
    eprintln!("{:<8} {:<12} {:<24} UUID", "SCOPE", "CATEGORY", "NAME");
    for (scope, category, name, uuid) in rows {
        eprintln!("{scope:<8} {category:<12} {name:<24} {uuid}");
    }
    Ok(())
}

fn cmd_init(args: &[String]) -> Res {
    let (filter, rest) = split_plugin_filter(args)?;
    expect_no_args(&rest, "init")?;
    let config = load_config()?;
    let ctx = ctx(&config, filter.as_deref())?;
    let presets = ctx.library()?;
    eprintln!(
        "{} preset(s) in {} - uuids stamped where missing",
        presets.len(),
        ctx.library_dir()?.display()
    );
    Ok(())
}

fn expect_no_args(rest: &[String], sub: &str) -> Res {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!("unexpected arguments for `preset {sub}`: {rest:?}").into())
    }
}

// ---------------------------------------------------------------------------
// convert
// ---------------------------------------------------------------------------

fn cmd_convert(args: &[String]) -> Res {
    let (filter, rest) = split_plugin_filter(args)?;
    let [input, output] = rest.as_slice() else {
        return Err("usage: cargo truce preset convert <in> <out> [-p <crate>]".into());
    };
    let config = load_config()?;
    let ctx = ctx(&config, filter.as_deref())?;

    let input = Path::new(input);
    let output = PathBuf::from(output);
    let (meta, params, extra) = read_native(&ctx, input)?;
    let bytes = encode_native(&ctx, &output, &meta, &params, &extra)?;
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs_ctx::create_dir_all(parent)?;
    }
    fs_ctx::write(&output, &bytes)?;
    eprintln!("{} -> {}", input.display(), output.display());
    Ok(())
}

/// Decode any supported input into `(meta, params, extra)`, with the
/// envelope validated against the plugin's identity hash.
fn read_native(ctx: &PluginCtx<'_>, path: &Path) -> Result<PresetParts, crate::CargoTruceError> {
    let format = PresetFormat::from_path(path)
        .ok_or_else(|| format!("unsupported preset extension: {}", path.display()))?;

    if format == PresetFormat::AuthoredToml {
        let authored = truce_build::presets::read_single_preset(path)?;
        return Ok((authored.meta, authored.params, authored.extra));
    }

    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let decoded = decode(format, &bytes)
        .ok_or_else(|| format!("{}: not a recognisable preset container", path.display()))?;
    let (params, extra) = blob_to_parts(ctx, &decoded.blob, path)?;
    let mut meta = decoded.meta.unwrap_or_default();
    if meta.name.is_empty() {
        meta.name = if decoded.name.is_empty() {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            decoded.name
        };
    }
    Ok((meta, params, extra))
}

fn blob_to_parts(
    ctx: &PluginCtx<'_>,
    blob: &[u8],
    path: &Path,
) -> Result<StateParts, crate::CargoTruceError> {
    let state = deserialize_state(blob, ctx.plugin_id_hash).ok_or_else(|| {
        format!(
            "{}: state belongs to a different plugin than \"{}\" (identity hash mismatch)",
            path.display(),
            ctx.p.name
        )
    })?;
    Ok((state.params, state.extra.unwrap_or_default()))
}

/// Encode `(meta, params, extra)` into the format `output`'s
/// extension selects.
fn encode_native(
    ctx: &PluginCtx<'_>,
    output: &Path,
    meta: &PresetMeta,
    params: &[(u32, f64)],
    extra: &[u8],
) -> Result<Vec<u8>, crate::CargoTruceError> {
    let format = PresetFormat::from_path(output)
        .ok_or_else(|| format!("unsupported preset extension: {}", output.display()))?;
    let ids: Vec<u32> = params.iter().map(|(id, _)| *id).collect();
    let values: Vec<f64> = params.iter().map(|(_, v)| *v).collect();
    let blob = serialize_state(ctx.plugin_id_hash, &ids, &values, extra);
    let mut meta = meta.clone();
    if meta.uuid.is_empty() {
        meta.uuid = mint_uuid();
    }

    Ok(match format {
        PresetFormat::TrucePreset => truce_utils::preset::write_preset_file(&meta, &blob),
        PresetFormat::Vst3 => {
            let clap_id = truce_build::plugin_id(&ctx.config.vendor.id, &ctx.p.name);
            vstpreset_bytes(&vst3_cid(&clap_id), &blob)
        }
        PresetFormat::Au => {
            let au_type = fourcc_int(ctx.p.resolved_au_type())?;
            let subtype = fourcc_int(ctx.p.resolved_fourcc())?;
            let manufacturer = fourcc_int(&ctx.config.vendor.au_manufacturer)?;
            aupreset_xml(au_type, subtype, manufacturer, &meta.name, &blob).into_bytes()
        }
        PresetFormat::Lv2 => {
            let uri = truce_build::lv2::plugin_uri(
                ctx.config.vendor.url.as_deref().unwrap_or(""),
                &ctx.p.bundle_id,
            );
            let label = if meta.category.is_empty() {
                meta.name.clone()
            } else {
                format!("{}/{}", meta.category, meta.name)
            };
            truce_build::lv2::render_preset_ttl(&uri, &meta.uuid, &label, &blob).into_bytes()
        }
        PresetFormat::AuthoredToml => {
            render_preset_toml(&meta, params, extra, &ctx.annotations()).into_bytes()
        }
    })
}

// ---------------------------------------------------------------------------
// import / pull
// ---------------------------------------------------------------------------

enum ImportOutcome {
    Created(PathBuf),
    Updated(PathBuf),
    Unchanged,
}

/// Land one decoded preset in the authored library. A library preset
/// with the same display name is regenerated in place (uuid and
/// metadata preserved, params / extra replaced) unless `always_new`;
/// an exact params + extra match is a no-op.
fn import_into_library(
    ctx: &PluginCtx<'_>,
    name: &str,
    src_meta: Option<&PresetMeta>,
    params: &[(u32, f64)],
    extra: &[u8],
    category: &str,
    always_new: bool,
) -> Result<ImportOutcome, crate::CargoTruceError> {
    let lib = ctx.library_dir()?;
    let library = ctx.library()?;

    let mut sorted: Vec<(u32, f64)> = params.to_vec();
    sorted.sort_by_key(|(id, _)| *id);

    if !always_new {
        for existing in &library {
            if existing.meta.name != name {
                continue;
            }
            let mut have: Vec<(u32, f64)> = existing.params.clone();
            have.sort_by_key(|(id, _)| *id);
            if have == sorted && existing.extra == extra {
                return Ok(ImportOutcome::Unchanged);
            }
            // Same display name, new values: the in-DAW edit loop.
            // Regenerating drops hand comments; uuid + metadata stay.
            let toml = render_preset_toml(&existing.meta, &sorted, extra, &ctx.annotations());
            fs_ctx::write(&existing.path, &toml)?;
            return Ok(ImportOutcome::Updated(existing.path.clone()));
        }
    }

    let mut meta = src_meta.cloned().unwrap_or_default();
    name.clone_into(&mut meta.name);
    if meta.uuid.is_empty() || library.iter().any(|p| p.meta.uuid == meta.uuid) {
        meta.uuid = mint_uuid();
    }
    // The directory carries the category; keep the field implicit.
    meta.category = String::new();
    meta.default = false;

    let dir = if category.is_empty() {
        lib
    } else {
        lib.join(safe_filename(category))
    };
    let stem = {
        let s = slugify(name);
        if s.is_empty() {
            "preset".to_string()
        } else {
            s
        }
    };
    let mut path = dir.join(format!("{stem}.preset"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}.preset"));
        n += 1;
    }

    let toml = render_preset_toml(&meta, &sorted, extra, &ctx.annotations());
    fs_ctx::create_dir_all(&dir)?;
    fs_ctx::write(&path, &toml)?;
    Ok(ImportOutcome::Created(path))
}

fn report_outcome(outcome: &ImportOutcome, source: &Path) {
    match outcome {
        ImportOutcome::Created(path) => {
            eprintln!("  new:       {} <- {}", path.display(), source.display());
        }
        ImportOutcome::Updated(path) => {
            eprintln!("  updated:   {} <- {}", path.display(), source.display());
        }
        ImportOutcome::Unchanged => {}
    }
}

fn cmd_import(args: &[String]) -> Res {
    let (filter, rest) = split_plugin_filter(args)?;
    let mut category = String::new();
    let mut file: Option<PathBuf> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--category" => {
                category = crate::util::arg_value(&rest, &mut i, "--category")?.to_string();
            }
            other if file.is_none() => file = Some(PathBuf::from(other)),
            other => return Err(format!("unexpected argument: {other}").into()),
        }
        i += 1;
    }
    let file = file.ok_or("usage: cargo truce preset import <file|pack.zip> [--category <c>]")?;
    let config = load_config()?;
    let ctx = ctx(&config, filter.as_deref())?;

    if file.extension().and_then(|e| e.to_str()) == Some("zip") {
        return import_pack(&ctx, &file);
    }

    let (meta, params, extra) = read_native(&ctx, &file)?;
    let outcome = import_into_library(
        &ctx,
        &meta.name.clone(),
        Some(&meta),
        &params,
        &extra,
        &category,
        false,
    )?;
    if matches!(outcome, ImportOutcome::Unchanged) {
        eprintln!(
            "  unchanged: library already has \"{}\" with these values",
            meta.name
        );
    }
    report_outcome(&outcome, &file);
    Ok(())
}

/// Unzip a pack's `.trucepreset` tree into the user pack directory.
/// Other per-format trees in the pack (`.vstpreset` / `.aupreset`)
/// are host-side conveniences; they're counted and left to the user
/// to place, keeping `import` from writing into host directories
/// unasked.
fn import_pack(ctx: &PluginCtx<'_>, file: &Path) -> Res {
    let store = ctx.store();
    let Some(user_root) = store.user_root() else {
        return Err("user preset directory could not be resolved".into());
    };
    let pack_name = file
        .file_stem()
        .map(|s| safe_filename(&s.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .ok_or("pack file has no usable name")?;
    let dest = user_root.join("packs").join(&pack_name);

    let reader = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("{}: {e}", file.display()))?;

    let mut installed = 0u32;
    let mut skipped = 0u32;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        let Some(rel) = entry.enclosed_name() else {
            skipped += 1;
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        if rel.extension().and_then(|e| e.to_str()) != Some("trucepreset") {
            skipped += 1;
            continue;
        }
        // Strip the conventional `trucepreset/` top-level tree name.
        let rel: PathBuf = match rel.strip_prefix("trucepreset") {
            Ok(stripped) => stripped.to_path_buf(),
            Err(_) => rel.clone(),
        };
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            fs_ctx::create_dir_all(parent)?;
        }
        let mut bytes = Vec::new();
        std::io::copy(&mut entry, &mut bytes).map_err(|e| format!("{}: {e}", file.display()))?;
        fs_ctx::write(&out, &bytes)?;
        installed += 1;
    }
    eprintln!(
        "pack \"{pack_name}\": {installed} preset(s) -> {} ({skipped} non-trucepreset entries left in the zip)",
        dest.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

fn cmd_export(args: &[String]) -> Res {
    let (filter, rest) = split_plugin_filter(args)?;
    let [out] = rest.as_slice() else {
        return Err("usage: cargo truce preset export <out.zip> [-p <crate>]".into());
    };
    let config = load_config()?;
    let ctx = ctx(&config, filter.as_deref())?;
    let presets = ctx.library()?;
    if presets.is_empty() {
        return Err(format!("no authored presets for \"{}\"", ctx.p.name).into());
    }

    let out_path = PathBuf::from(out);
    let file = std::fs::File::create(&out_path).map_err(|e| format!("{out}: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    let zip_err = |e: zip::result::ZipError| format!("{out}: {e}");

    for preset in &presets {
        let blob = preset.state_blob(ctx.plugin_id_hash);
        let category = safe_filename(&preset.meta.category);
        let dir = if category.is_empty() {
            String::new()
        } else {
            format!("{category}/")
        };
        let display = safe_filename(&preset.meta.name);

        zip.start_file(
            format!("trucepreset/{dir}{}.trucepreset", preset.stem),
            options,
        )
        .map_err(zip_err)?;
        zip.write_all(&truce_utils::preset::write_preset_file(&preset.meta, &blob))?;

        zip.start_file(format!("vstpreset/{dir}{display}.vstpreset"), options)
            .map_err(zip_err)?;
        let clap_id = truce_build::plugin_id(&ctx.config.vendor.id, &ctx.p.name);
        zip.write_all(&vstpreset_bytes(&vst3_cid(&clap_id), &blob))?;

        zip.start_file(format!("aupreset/{dir}{display}.aupreset"), options)
            .map_err(zip_err)?;
        let au = aupreset_xml(
            fourcc_int(ctx.p.resolved_au_type())?,
            fourcc_int(ctx.p.resolved_fourcc())?,
            fourcc_int(&ctx.config.vendor.au_manufacturer)?,
            &preset.meta.name,
            &blob,
        );
        zip.write_all(au.as_bytes())?;

        zip.start_file(format!("lv2/{}.ttl", preset.stem), options)
            .map_err(zip_err)?;
        let uri = truce_build::lv2::plugin_uri(
            ctx.config.vendor.url.as_deref().unwrap_or(""),
            &ctx.p.bundle_id,
        );
        let label = if preset.meta.category.is_empty() {
            preset.meta.name.clone()
        } else {
            format!("{}/{}", preset.meta.category, preset.meta.name)
        };
        zip.write_all(
            truce_build::lv2::render_preset_ttl(&uri, &preset.meta.uuid, &label, &blob).as_bytes(),
        )?;
    }
    zip.finish().map_err(zip_err)?;
    eprintln!(
        "{} preset(s) x 4 formats -> {}",
        presets.len(),
        out_path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// pull
// ---------------------------------------------------------------------------

fn cmd_pull(args: &[String]) -> Res {
    let (filter, rest) = split_plugin_filter(args)?;
    let mut category = String::new();
    let mut watch = false;
    let mut always_new = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--category" => {
                category = crate::util::arg_value(&rest, &mut i, "--category")?.to_string();
            }
            "--watch" => watch = true,
            "--new" => always_new = true,
            other => return Err(format!("unexpected argument: {other}").into()),
        }
        i += 1;
    }
    let config = load_config()?;
    let ctx = ctx(&config, filter.as_deref())?;

    if watch {
        eprintln!(
            "watching host preset locations for \"{}\" - save presets in your DAW, ctrl-c to stop",
            ctx.p.name
        );
        loop {
            pull_once(&ctx, &category, always_new)?;
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    let imported = pull_once(&ctx, &category, always_new)?;
    if imported == 0 {
        eprintln!(
            "nothing new - host-saved presets for \"{}\" are already in the library",
            ctx.p.name
        );
    }
    Ok(())
}

/// One scan over every host preset location. Idempotent: presets
/// already in the library (by name + identical values) are skipped,
/// which is what makes `--watch`'s rescan loop safe.
fn pull_once(
    ctx: &PluginCtx<'_>,
    category: &str,
    always_new: bool,
) -> Result<u32, crate::CargoTruceError> {
    let mut imported = 0u32;
    for path in host_preset_files(ctx) {
        let Some(format) = PresetFormat::from_path(&path) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Some(decoded) = decode(format, &bytes) else {
            continue;
        };
        // The identity hash is the gate: shared directories (and the
        // ~/.lv2 sweep) hold other plugins' presets.
        let Some(state) = deserialize_state(&decoded.blob, ctx.plugin_id_hash) else {
            continue;
        };
        let name = if decoded.name.is_empty() {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            decoded.name.clone()
        };
        if name.is_empty() {
            continue;
        }
        let outcome = import_into_library(
            ctx,
            &name,
            decoded.meta.as_ref(),
            &state.params,
            &state.extra.unwrap_or_default(),
            category,
            always_new,
        )?;
        if !matches!(outcome, ImportOutcome::Unchanged) {
            imported += 1;
        }
        report_outcome(&outcome, &path);
    }
    Ok(imported)
}

/// Candidate preset files in every location hosts save into. The
/// per-file identity check in `pull_once` filters out other
/// plugins' presets, so over-collection here is harmless.
fn host_preset_files(ctx: &PluginCtx<'_>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let vendor = safe_filename(&ctx.config.vendor.name);

    // Plugin-scoped host locations, per display name the host
    // groups under (VST3 + AU share the macOS tree).
    let mut names: Vec<String> = vec![
        safe_filename(resolved_name(ctx.p.vst3_name.as_deref(), &ctx.p.name)),
        safe_filename(resolved_name(ctx.p.au_name.as_deref(), &ctx.p.name)),
    ];
    names.dedup();

    #[cfg(target_os = "macos")]
    if let Some(home) = crate::dirs::home_dir() {
        for name in &names {
            collect_files(
                &home.join("Library/Audio/Presets").join(&vendor).join(name),
                3,
                &mut files,
            );
        }
        // Host-saved LV2 user presets (lilv writes one bundle per
        // preset); plugin-agnostic dir, the hash check filters.
        collect_files(&home.join(".lv2"), 2, &mut files);
    }
    #[cfg(target_os = "windows")]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        for name in &names {
            collect_files(
                &PathBuf::from(&profile)
                    .join("Documents")
                    .join("VST3 Presets")
                    .join(&vendor)
                    .join(name),
                3,
                &mut files,
            );
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for name in &names {
            collect_files(
                &home.join(".vst3/presets").join(&vendor).join(name),
                3,
                &mut files,
            );
        }
        collect_files(&home.join(".lv2"), 2, &mut files);
    }

    // The truce user root: presets saved through the CRUD API /
    // future in-editor menus.
    if let Some(user_root) = ctx.store().user_root() {
        collect_files(user_root, 3, &mut files);
    }

    files.sort();
    files.dedup();
    files
}

fn collect_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, depth - 1, out);
        } else if PresetFormat::from_path(&path).is_some_and(|f| f != PresetFormat::AuthoredToml) {
            out.push(path);
        }
    }
}
