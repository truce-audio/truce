//! The `.trucepreset` container - truce's native on-disk preset file.
//!
//! One file per preset: a small metadata block followed by the
//! canonical state envelope ([`crate::state`]). `cargo-truce` writes
//! these into plugin bundles at install time (factory scope) and the
//! CLAP wrapper reads them back at host scan / load time, so both the
//! writer and the parser live in the dependency-free tier.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! "TRPS"  magic           4 bytes
//! u32     format version  (currently 1)
//! u32     meta_len
//! [meta]  UTF-8 `key=value` lines (see below)
//! u64     blob_len
//! [blob]  canonical state envelope (crate::state wire format)
//! ```
//!
//! The metadata block is deliberately not TOML: runtime consumers
//! (format wrappers enumerating presets during a host scan) must not
//! need a TOML dependency. One `key=value` pair per `\n`-terminated
//! line, values taken verbatim to end-of-line. The writer strips
//! newlines from values; `tags` joins entries with `,` (commas inside
//! a tag are stripped at write).

/// File extension for truce-native preset files, without the dot.
pub const PRESET_FILE_EXT: &str = "trucepreset";

const PRESET_MAGIC: &[u8; 4] = b"TRPS";
const PRESET_FORMAT_VERSION: u32 = 1;

/// Preset metadata carried in the container's header block.
///
/// `uuid` is the preset's stable identity: generated once when the
/// preset is authored and never changed afterwards, so renames and
/// recategorisation don't break host-side references. `category` is
/// the explicit value only; consumers fall back to the parent
/// directory name when it's empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetMeta {
    pub uuid: String,
    pub name: String,
    pub category: String,
    pub author: String,
    pub comment: String,
    pub tags: Vec<String>,
    /// The library's "init sound". At most one preset per library
    /// sets this; consumers order it first where a format has a
    /// positional default (the AU factory list).
    pub default: bool,
}

fn push_meta_line(buf: &mut String, key: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    buf.push_str(key);
    buf.push('=');
    // Newlines would corrupt the line framing; values are display
    // strings, so dropping them is acceptable.
    buf.extend(value.chars().filter(|c| *c != '\n' && *c != '\r'));
    buf.push('\n');
}

/// Serialize a preset file from metadata + a canonical state blob.
#[must_use]
pub fn write_preset_file(meta: &PresetMeta, state_blob: &[u8]) -> Vec<u8> {
    let mut meta_block = String::new();
    push_meta_line(&mut meta_block, "uuid", &meta.uuid);
    push_meta_line(&mut meta_block, "name", &meta.name);
    push_meta_line(&mut meta_block, "category", &meta.category);
    push_meta_line(&mut meta_block, "author", &meta.author);
    push_meta_line(&mut meta_block, "comment", &meta.comment);
    let tags = meta
        .tags
        .iter()
        .map(|t| t.replace(',', ""))
        .collect::<Vec<_>>()
        .join(",");
    push_meta_line(&mut meta_block, "tags", &tags);
    if meta.default {
        push_meta_line(&mut meta_block, "default", "1");
    }

    let mut out = Vec::with_capacity(16 + meta_block.len() + state_blob.len() + 8);
    out.extend_from_slice(PRESET_MAGIC);
    out.extend_from_slice(&PRESET_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&crate::cast::len_u32(meta_block.len()).to_le_bytes());
    out.extend_from_slice(meta_block.as_bytes());
    out.extend_from_slice(&(state_blob.len() as u64).to_le_bytes());
    out.extend_from_slice(state_blob);
    out
}

/// Parse just the metadata of a preset file. Cheap path for
/// enumeration during a host scan - the state blob is not copied.
#[must_use]
pub fn parse_preset_meta(data: &[u8]) -> Option<PresetMeta> {
    parse_sections(data).map(|(meta, _)| meta)
}

/// Parse a preset file into metadata + the contained state blob.
#[must_use]
pub fn parse_preset_file(data: &[u8]) -> Option<(PresetMeta, Vec<u8>)> {
    parse_sections(data).map(|(meta, blob)| (meta, blob.to_vec()))
}

fn parse_sections(data: &[u8]) -> Option<(PresetMeta, &[u8])> {
    if data.len() < 12 || &data[0..4] != PRESET_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(data[4..8].try_into().ok()?);
    if version != PRESET_FORMAT_VERSION {
        return None;
    }
    let meta_len = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
    let meta_end = 12usize.checked_add(meta_len)?;
    if meta_end + 8 > data.len() {
        return None;
    }
    let meta_text = std::str::from_utf8(&data[12..meta_end]).ok()?;

    let mut meta = PresetMeta::default();
    for line in meta_text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "uuid" => meta.uuid = value.to_string(),
            "name" => meta.name = value.to_string(),
            "category" => meta.category = value.to_string(),
            "author" => meta.author = value.to_string(),
            "comment" => meta.comment = value.to_string(),
            "tags" => {
                meta.tags = value
                    .split(',')
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "default" => meta.default = value == "1",
            // Unknown keys are forward-compatible: skip.
            _ => {}
        }
    }

    // The wire format encodes `blob_len` as `u64`; the checked_add
    // below validates against the buffer length, which bounds it to
    // usize on every target.
    #[allow(clippy::cast_possible_truncation)]
    let blob_len = u64::from_le_bytes(data[meta_end..meta_end + 8].try_into().ok()?) as usize;
    let blob_start = meta_end + 8;
    let blob_end = blob_start.checked_add(blob_len)?;
    if blob_end > data.len() {
        return None;
    }
    Some((meta, &data[blob_start..blob_end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> PresetMeta {
        PresetMeta {
            uuid: "9a2f6c1e-3b44-4f1d-9b7a-1de0c4a51b22".into(),
            name: "Bright Saw".into(),
            category: "Lead".into(),
            author: "JK".into(),
            comment: "Classic bright lead".into(),
            tags: vec!["analog".into(), "lead".into()],
            default: true,
        }
    }

    #[test]
    fn round_trips() {
        let blob = vec![1u8, 2, 3, 4, 5];
        let file = write_preset_file(&sample_meta(), &blob);
        let (meta, parsed_blob) = parse_preset_file(&file).unwrap();
        assert_eq!(meta, sample_meta());
        assert_eq!(parsed_blob, blob);
        assert_eq!(parse_preset_meta(&file).unwrap(), sample_meta());
    }

    #[test]
    fn empty_fields_round_trip() {
        let meta = PresetMeta {
            name: "Init".into(),
            ..PresetMeta::default()
        };
        let file = write_preset_file(&meta, &[]);
        let (parsed, blob) = parse_preset_file(&file).unwrap();
        assert_eq!(parsed, meta);
        assert!(blob.is_empty());
    }

    #[test]
    fn newlines_in_values_are_stripped() {
        let meta = PresetMeta {
            name: "Two\nLines".into(),
            ..PresetMeta::default()
        };
        let file = write_preset_file(&meta, &[]);
        assert_eq!(parse_preset_meta(&file).unwrap().name, "TwoLines");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_preset_meta(b"nope").is_none());
        let mut file = write_preset_file(&sample_meta(), &[1, 2, 3]);
        file.truncate(file.len() - 1);
        assert!(parse_preset_file(&file).is_none());
        file[0] = b'X';
        assert!(parse_preset_meta(&file).is_none());
    }
}
