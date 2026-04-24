//! `.clap-preset` on-disk format.
//!
//! CLAP deliberately leaves preset file bytes to the plugin. We use a
//! TOML envelope with a base64-encoded canonical truce state blob so
//! the files are human-inspectable + carry everything the runtime
//! preset-load path needs:
//!
//! ```toml
//! plugin_id = "com.truce.rir"
//! name = "Small Room"
//! category = "Rooms"
//! author = "Truce"
//! comment = "…"
//! tags = ["bright", "small"]
//! default = false
//! state_b64 = "T0FTVAEAAAAaQ5ZG…"
//! ```
//!
//! Hosts never parse this directly; `truce-clap`'s preset-load
//! extension opens it, decodes `state_b64`, and runs the same
//! `deserialize_state` path session recall uses.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::Preset;

/// On-disk representation of a CLAP preset.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClapPresetFile {
    /// Plugin ID string (not the hash) — lets a preset loader reject
    /// files emitted for a different plugin before even hashing.
    pub plugin_id: String,
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub default: bool,
    /// Base64-encoded output of `truce_core::serialize_state`.
    pub state_b64: String,
}

impl ClapPresetFile {
    pub fn from_preset(plugin_id: &str, preset: &Preset) -> Result<Self, crate::PresetError> {
        let hash = truce_core::state::hash_plugin_id(plugin_id);
        let blob = preset.to_state_blob(hash)?;
        Ok(ClapPresetFile {
            plugin_id: plugin_id.to_string(),
            name: preset.name.clone(),
            category: Some(preset.effective_category().to_string())
                .filter(|s| !s.is_empty()),
            author: preset.author.clone(),
            comment: preset.comment.clone(),
            tags: preset.tags.clone(),
            default: preset.default,
            state_b64: base64::engine::general_purpose::STANDARD.encode(&blob),
        })
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("ClapPresetFile serializes")
    }

    pub fn from_toml(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }

    /// Base64-decode the state blob.
    pub fn state_blob(&self) -> Result<Vec<u8>, base64::DecodeError> {
        base64::engine::general_purpose::STANDARD.decode(self.state_b64.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut preset = Preset {
            name: "Small Room".into(),
            category: Some("Rooms".into()),
            author: Some("Truce".into()),
            comment: None,
            tags: vec!["bright".into()],
            default: true,
            params: [("0".into(), 0.5), ("1".into(), -6.0)].into_iter().collect(),
            extra: None,
            path_stem: "rooms/small".into(),
        };
        let f = ClapPresetFile::from_preset("com.truce.test", &preset).unwrap();
        let toml_str = f.to_toml();
        let back = ClapPresetFile::from_toml(&toml_str).unwrap();
        assert_eq!(back.name, "Small Room");
        assert_eq!(back.tags, vec!["bright"]);
        assert!(back.default);
        let blob = back.state_blob().unwrap();
        let hash = truce_core::state::hash_plugin_id("com.truce.test");
        let de = truce_core::state::deserialize_state(&blob, hash).unwrap();
        assert_eq!(de.params.len(), 2);
        preset.category = None; // silence unused warning after take
    }
}
