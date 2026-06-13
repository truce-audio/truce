//! Per-format preset container codecs.
//!
//! Encode and decode the native preset files for truce plugins. The
//! decode direction is what makes `cargo truce preset pull` /
//! `convert` possible: every container truce emits wraps the same
//! canonical state envelope, so a host-saved preset converts to any
//! other format by re-enveloping the embedded blob. This only holds
//! for truce plugins - other vendors' preset content is opaque.
//!
//! Emit fns live here (rather than `commands/install/presets.rs`) so
//! the install pipeline and the `preset` CLI share one
//! implementation.

use std::fmt::Write as _;
use std::path::Path;

use base64::Engine as _;

/// Container formats the codec understands, detected by file
/// extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetFormat {
    /// `.trucepreset` - truce's native container.
    TrucePreset,
    /// `.vstpreset` - Steinberg container, `Comp` chunk = envelope.
    Vst3,
    /// `.aupreset` - Apple plist, `truce_state` data = envelope.
    Au,
    /// `.ttl` - LV2 preset, `state:state` base64 literal = envelope.
    Lv2,
    /// `.preset` - the authored TOML source format.
    AuthoredToml,
}

impl PresetFormat {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "trucepreset" => Some(Self::TrucePreset),
            "vstpreset" => Some(Self::Vst3),
            "aupreset" => Some(Self::Au),
            "ttl" => Some(Self::Lv2),
            "preset" => Some(Self::AuthoredToml),
            _ => None,
        }
    }
}

/// A preset decoded from any native container: the canonical state
/// envelope plus whatever display metadata the container carried.
pub(crate) struct DecodedPreset {
    /// Display name. Containers that don't embed one leave it empty;
    /// callers fall back to the file stem.
    pub(crate) name: String,
    /// Full metadata when the container carries it (`.trucepreset`).
    pub(crate) meta: Option<truce_utils::preset::PresetMeta>,
    /// The canonical state envelope.
    pub(crate) blob: Vec<u8>,
}

/// Decode a native preset file's bytes by format. Returns `None`
/// when the container doesn't parse or carries no envelope.
pub(crate) fn decode(format: PresetFormat, bytes: &[u8]) -> Option<DecodedPreset> {
    match format {
        PresetFormat::TrucePreset => {
            let (meta, blob) = truce_utils::preset::parse_preset_file(bytes)?;
            Some(DecodedPreset {
                name: meta.name.clone(),
                meta: Some(meta),
                blob,
            })
        }
        PresetFormat::Vst3 => parse_vstpreset(bytes).map(|blob| DecodedPreset {
            name: String::new(),
            meta: None,
            blob,
        }),
        PresetFormat::Au => parse_aupreset(bytes),
        PresetFormat::Lv2 => parse_lv2_preset_ttl(std::str::from_utf8(bytes).ok()?),
        PresetFormat::AuthoredToml => None, // parsed via truce_build::presets instead
    }
}

// ---------------------------------------------------------------------------
// VST3 (.vstpreset)
// ---------------------------------------------------------------------------

const VSTPRESET_HEADER_LEN: usize = 48;

/// Serialize one `.vstpreset`: the Steinberg container with a single
/// `Comp` chunk holding the canonical state envelope (the same bytes
/// `truce-vst3`'s component `setState` consumes).
///
/// Layout: `"VST3"` magic, `i32` version, 32 ASCII hex chars of the
/// class ID (see [`encode_class_id`]), `i64` offset to the chunk
/// list, the chunk data, then a `"List"` section of `(id, offset,
/// size)` entries - all integers little-endian, per the VST3 SDK's
/// `PresetFile` implementation.
pub(crate) fn vstpreset_bytes(class_id: &[u8; 16], blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(VSTPRESET_HEADER_LEN + blob.len() + 36);
    out.extend_from_slice(b"VST3");
    out.extend_from_slice(&1i32.to_le_bytes());
    // The host that loads this file is the one we're installing for
    // (presets are emitted locally), so match its platform.
    out.extend_from_slice(encode_class_id(class_id, cfg!(target_os = "windows")).as_bytes());
    let list_offset = VSTPRESET_HEADER_LEN + blob.len();
    out.extend_from_slice(&(list_offset as u64).to_le_bytes());
    out.extend_from_slice(blob);
    out.extend_from_slice(b"List");
    out.extend_from_slice(&1i32.to_le_bytes());
    out.extend_from_slice(b"Comp");
    out.extend_from_slice(&(VSTPRESET_HEADER_LEN as u64).to_le_bytes());
    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    out
}

/// Hex-encode the 16-byte class ID into the 32-char header string the
/// way the host will read it back. The SDK's `PresetFile` writes
/// `FUID::toString(classID)` and hosts decode with `FUID::fromString`,
/// so we must reproduce that platform-dependent ordering or the host
/// recovers a different TUID than the plugin's factory reports and
/// rejects the file ("...doesn't appear to be for this plugin").
///
/// On macOS/Linux it is a straight per-byte hex dump. On Windows the
/// SDK is `COM_COMPATIBLE`: `toString` reinterprets the first eight
/// bytes as a little-endian Windows GUID (`Data1` u32, `Data2` /
/// `Data3` u16), reversing bytes 0..4, 4..6 and 6..8; bytes 8..16 stay
/// in order. `truce`'s factory reports the raw FNV bytes on every
/// platform, so the file's class string is genuinely platform-specific
/// - which is fine, presets are emitted per-install on the target.
fn encode_class_id(class_id: &[u8; 16], com_compatible: bool) -> String {
    const RAW: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    const GUID: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let order = if com_compatible { GUID } else { RAW };
    let mut hex = String::with_capacity(32);
    for i in order {
        let _ = write!(hex, "{:02X}", class_id[i]);
    }
    hex
}

/// Extract the `Comp` chunk (the canonical envelope) from a
/// `.vstpreset`. The class id in the header is deliberately not
/// validated here - the envelope's own plugin-id hash is the
/// authoritative identity check and callers verify it.
pub(crate) fn parse_vstpreset(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < VSTPRESET_HEADER_LEN || &bytes[0..4] != b"VST3" {
        return None;
    }
    let read_u64 = |at: usize| -> Option<usize> {
        usize::try_from(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?)).ok()
    };
    let list_offset = read_u64(40)?;
    if bytes.get(list_offset..list_offset + 4)? != b"List" {
        return None;
    }
    let count = i32::from_le_bytes(
        bytes
            .get(list_offset + 4..list_offset + 8)?
            .try_into()
            .ok()?,
    );
    let count = usize::try_from(count).ok()?;
    for i in 0..count {
        let entry = list_offset + 8 + i * 20;
        let id = bytes.get(entry..entry + 4)?;
        if id == b"Comp" {
            let offset = read_u64(entry + 4)?;
            let size = read_u64(entry + 12)?;
            return bytes
                .get(offset..offset.checked_add(size)?)
                .map(<[u8]>::to_vec);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// AU (.aupreset)
// ---------------------------------------------------------------------------

/// Pack a 4-char code into the integer representation `.aupreset`
/// plists carry (`'aufx'` → big-endian u32).
pub(crate) fn fourcc_int(code: &str) -> Result<u32, crate::CargoTruceError> {
    let bytes = code.as_bytes();
    let four: [u8; 4] = bytes
        .try_into()
        .map_err(|_| format!("four-char code \"{code}\" is not exactly 4 bytes"))?;
    Ok(u32::from_be_bytes(four))
}

/// Render one `.aupreset` XML plist. The standard identity keys let
/// hosts match the preset to the component; the state itself rides
/// the `truce_state` key - the slot `truce-au`'s `ClassInfo` property
/// handler reads (and writes) the canonical envelope through.
pub(crate) fn aupreset_xml(
    au_type: u32,
    subtype: u32,
    manufacturer: u32,
    name: &str,
    blob: &[u8],
) -> String {
    // 0x0001_0000: matches both the AudioComponents version in the
    // installed Info.plist and the registration descriptor.
    const AU_VERSION: u32 = 0x0001_0000;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>manufacturer</key>
    <integer>{manufacturer}</integer>
    <key>name</key>
    <string>{}</string>
    <key>subtype</key>
    <integer>{subtype}</integer>
    <key>truce_state</key>
    <data>{}</data>
    <key>type</key>
    <integer>{au_type}</integer>
    <key>version</key>
    <integer>{AU_VERSION}</integer>
</dict>
</plist>
"#,
        xml_escape(name),
        base64::engine::general_purpose::STANDARD.encode(blob),
    )
}

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Extract name + envelope from an `.aupreset`. Goes through the
/// `plist` crate so host-written binary plists (Logic saves those)
/// parse the same as our XML emission.
fn parse_aupreset(bytes: &[u8]) -> Option<DecodedPreset> {
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    let dict = value.as_dictionary()?;
    let blob = dict.get("truce_state")?.as_data()?.to_vec();
    let name = dict
        .get("name")
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_string();
    Some(DecodedPreset {
        name,
        meta: None,
        blob,
    })
}

// ---------------------------------------------------------------------------
// LV2 preset TTL
// ---------------------------------------------------------------------------

/// Extract name + envelope from an LV2 preset TTL: the
/// `<urn:truce:state-blob>` property's base64 literal (the State
/// extension key the runtime stores under), plus `rdfs:label` when
/// present. Tolerant line-oriented scan rather than a Turtle parser:
/// both our emitter and lilv (which writes host-saved user presets)
/// keep the property and its literal on one statement.
fn parse_lv2_preset_ttl(text: &str) -> Option<DecodedPreset> {
    let after_key = text.split("<urn:truce:state-blob>").nth(1)?;
    let b64 = extract_quoted(after_key)?;
    let compact: String = b64.split_whitespace().collect();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .ok()?;

    let name = text
        .split("rdfs:label")
        .nth(1)
        .and_then(extract_quoted)
        .unwrap_or_default()
        .to_string();
    Some(DecodedPreset {
        name,
        meta: None,
        blob,
    })
}

fn extract_quoted(s: &str) -> Option<&str> {
    let start = s.find('"')? + 1;
    let end = start + s.get(start..)?.find('"')?;
    s.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use truce_utils::state::{hash_plugin_id, serialize_state};

    fn sample_blob() -> Vec<u8> {
        serialize_state(
            hash_plugin_id("com.acme.x"),
            &[0, 7],
            &[0.5, -6.0],
            b"extra",
        )
    }

    #[test]
    fn vstpreset_round_trips() {
        let blob = sample_blob();
        let bytes = vstpreset_bytes(&[0xCD; 16], &blob);
        assert_eq!(parse_vstpreset(&bytes).unwrap(), blob);
        assert!(parse_vstpreset(b"not a preset").is_none());

        let mut truncated = bytes.clone();
        truncated.truncate(50);
        assert!(parse_vstpreset(&truncated).is_none());
    }

    #[test]
    fn vstpreset_layout_is_stable() {
        let blob = vec![7u8; 10];
        let bytes = vstpreset_bytes(&[0xAB; 16], &blob);
        assert_eq!(&bytes[0..4], b"VST3");
        assert_eq!(&bytes[8..40], "AB".repeat(16).as_bytes());
        assert_eq!(&bytes[48..58], &blob[..]);
    }

    #[test]
    fn class_id_encoding_matches_fuid_tostring() {
        // 0x00..0x0F so each byte is distinguishable.
        let cid: [u8; 16] = std::array::from_fn(|i| i as u8);
        // Non-COM: straight per-byte dump.
        assert_eq!(
            encode_class_id(&cid, false),
            "000102030405060708090A0B0C0D0E0F"
        );
        // COM_COMPATIBLE: first 8 bytes reordered as a little-endian
        // Windows GUID (Data1 u32, Data2/Data3 u16), tail unchanged.
        assert_eq!(
            encode_class_id(&cid, true),
            "030201000504070608090A0B0C0D0E0F"
        );
    }

    #[test]
    fn aupreset_round_trips_including_escapes() {
        let blob = sample_blob();
        let xml = aupreset_xml(
            fourcc_int("aufx").unwrap(),
            fourcc_int("TGan").unwrap(),
            fourcc_int("Trce").unwrap(),
            "Bright & <Saw>",
            &blob,
        );
        assert!(xml.contains("<integer>1635083896</integer>")); // 'aufx'
        let decoded = decode(PresetFormat::Au, xml.as_bytes()).unwrap();
        assert_eq!(decoded.name, "Bright & <Saw>");
        assert_eq!(decoded.blob, blob);
    }

    #[test]
    fn lv2_ttl_round_trips() {
        let blob = sample_blob();
        let ttl = truce_build::lv2::render_preset_ttl(
            "https://example.com/lv2/x",
            "u-1",
            "pad/Glass",
            &blob,
            &[("cutoff".to_string(), 8000.0), ("reso".to_string(), 0.5)],
        );
        let decoded = decode(PresetFormat::Lv2, ttl.as_bytes()).unwrap();
        assert_eq!(decoded.name, "pad/Glass");
        // The `state:state` blob still round-trips even with port
        // values present (the decoder reads the chunk, ignoring ports).
        assert_eq!(decoded.blob, blob);
    }

    #[test]
    fn trucepreset_round_trips_with_meta() {
        let blob = sample_blob();
        let meta = truce_utils::preset::PresetMeta {
            uuid: "u-tp".into(),
            name: "Native".into(),
            ..truce_utils::preset::PresetMeta::default()
        };
        let bytes = truce_utils::preset::write_preset_file(&meta, &blob);
        let decoded = decode(PresetFormat::TrucePreset, &bytes).unwrap();
        assert_eq!(decoded.name, "Native");
        assert_eq!(decoded.meta.unwrap().uuid, "u-tp");
        assert_eq!(decoded.blob, blob);
    }

    #[test]
    fn format_detection() {
        let f = |p: &str| PresetFormat::from_path(Path::new(p));
        assert_eq!(f("a.vstpreset"), Some(PresetFormat::Vst3));
        assert_eq!(f("a.aupreset"), Some(PresetFormat::Au));
        assert_eq!(f("a.trucepreset"), Some(PresetFormat::TrucePreset));
        assert_eq!(f("a.ttl"), Some(PresetFormat::Lv2));
        assert_eq!(f("a.preset"), Some(PresetFormat::AuthoredToml));
        assert_eq!(f("a.wav"), None);
    }
}
