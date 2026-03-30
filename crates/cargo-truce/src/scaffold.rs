//! Shared scaffolding templates for `cargo truce new` and `cargo truce new-workspace`.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq)]
pub enum PluginKind {
    Effect,
    Instrument,
    Midi,
}

impl PluginKind {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "effect" => Ok(Self::Effect),
            "instrument" => Ok(Self::Instrument),
            "midi" => Ok(Self::Midi),
            other => Err(format!("Unknown plugin type: {other} (expected effect, instrument, or midi)")),
        }
    }

    fn category(self) -> &'static str {
        match self {
            Self::Instrument => "instrument",
            _ => "effect",
        }
    }

    fn au_tag(self) -> &'static str {
        match self {
            Self::Instrument => "Synthesizer",
            _ => "Effects",
        }
    }

    fn bus_layouts(self) -> &'static str {
        match self {
            Self::Instrument => "BusLayout::new().with_output(\"Main\", ChannelConfig::Stereo)",
            _ => "BusLayout::stereo()",
        }
    }

    fn test_body(self) -> &'static str {
        match self {
            Self::Instrument => "truce_test::render_instrument::<Plugin>(512, 44100.0, &[])",
            _ => "truce_test::render_effect::<Plugin>(512, 44100.0)",
        }
    }
}

pub struct PluginSpec {
    pub name: String,
    pub kind: PluginKind,
}

// ---------------------------------------------------------------------------
// Template generators
// ---------------------------------------------------------------------------

pub fn plugin_cargo_toml_standalone(crate_name: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
au = ["dep:truce-au"]
aax = ["dep:truce-aax"]
dev = ["truce/dev"]

[dependencies]
truce = {{ git = "https://github.com/truce-audio/truce" }}
truce-gui = {{ git = "https://github.com/truce-audio/truce" }}
truce-clap = {{ git = "https://github.com/truce-audio/truce", optional = true }}
truce-vst3 = {{ git = "https://github.com/truce-audio/truce", optional = true }}
truce-vst2 = {{ git = "https://github.com/truce-audio/truce", optional = true }}
truce-au = {{ git = "https://github.com/truce-audio/truce", optional = true }}
truce-aax = {{ git = "https://github.com/truce-audio/truce", optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ git = "https://github.com/truce-audio/truce" }}
"#,
    )
}

pub fn plugin_cargo_toml_workspace(crate_name: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
au = ["dep:truce-au"]
aax = ["dep:truce-aax"]
dev = ["truce/dev"]

[dependencies]
truce = {{ workspace = true }}
truce-gui = {{ workspace = true }}
truce-clap = {{ workspace = true, optional = true }}
truce-vst3 = {{ workspace = true, optional = true }}
truce-vst2 = {{ workspace = true, optional = true }}
truce-au = {{ workspace = true, optional = true }}
truce-aax = {{ workspace = true, optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ workspace = true }}
"#,
    )
}

pub fn plugin_lib_rs(struct_name: &str, kind: PluginKind) -> String {
    let params = match kind {
        PluginKind::Midi => format!(
            r#"#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(name = "Semitones", range = "discrete(-12, 12)")]
    pub semitones: FloatParam,
}}"#
        ),
        _ => format!(
            r#"#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}"#
        ),
    };

    let layout_knob = match kind {
        PluginKind::Midi => "GridWidget::knob(P::Semitones, \"Semitones\")",
        _ => "GridWidget::knob(P::Gain, \"Gain\")",
    };

    let process_body = match kind {
        PluginKind::Instrument => r#"    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
               _context: &mut ProcessContext) -> ProcessStatus {
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { note, velocity, .. } => {
                    // TODO: start a voice
                    let _ = (note, velocity);
                }
                EventBody::NoteOff { note, .. } => {
                    // TODO: release the voice
                    let _ = note;
                }
                _ => {}
            }
        }

        for ch in 0..buffer.num_output_channels() {
            for i in 0..buffer.num_samples() {
                buffer.output(ch)[i] = 0.0;
            }
        }
        ProcessStatus::Normal
    }"#,
        PluginKind::Midi => r#"    fn process(&mut self, _buffer: &mut AudioBuffer, events: &EventList,
               context: &mut ProcessContext) -> ProcessStatus {
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { channel, note, velocity } => {
                    let shifted = (*note as i16 + self.params.semitones.value() as i16)
                        .clamp(0, 127) as u8;
                    context.output_events.push(Event {
                        sample_offset: event.sample_offset,
                        body: EventBody::NoteOn {
                            channel: *channel, note: shifted, velocity: *velocity,
                        },
                    });
                }
                EventBody::NoteOff { channel, note, velocity } => {
                    let shifted = (*note as i16 + self.params.semitones.value() as i16)
                        .clamp(0, 127) as u8;
                    context.output_events.push(Event {
                        sample_offset: event.sample_offset,
                        body: EventBody::NoteOff {
                            channel: *channel, note: shifted, velocity: *velocity,
                        },
                    });
                }
                _ => {}
            }
        }
        ProcessStatus::Normal
    }"#,
        PluginKind::Effect => r#"    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
               _context: &mut ProcessContext) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }"#,
    };

    let bus_layouts = kind.bus_layouts();
    let test_body = kind.test_body();

    format!(
        r#"use truce::prelude::*;

{params}

use {struct_name}ParamsParamId as P;

pub struct {struct_name} {{
    params: Arc<{struct_name}Params>,
}}

impl {struct_name} {{
    pub fn new(params: Arc<{struct_name}Params>) -> Self {{
        Self {{ params }}
    }}
}}

impl PluginLogic for {struct_name} {{
    fn reset(&mut self, sr: f64, _bs: usize) {{
        self.params.set_sample_rate(sr);
    }}

{process_body}

    fn layout(&self) -> truce_gui::layout::GridLayout {{
        use truce_gui::layout::{{GridLayout, GridWidget}};
        GridLayout::build("{struct_name}", "V0.1", 2, 50.0, vec![
            {layout_knob}.into(),
        ])
    }}
}}

truce::plugin! {{
    logic: {struct_name},
    params: {struct_name}Params,
    bus_layouts: [{bus_layouts}],
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn builds_and_runs() {{
        let result = {test_body};
        truce_test::assert_no_nans(&result.output);
    }}
}}
"#,
    )
}

pub fn truce_toml(
    vendor_name: &str,
    vendor_id: &str,
    plugins: &[PluginSpec],
    workspace_name: &str,
    fourcc_map: &HashMap<String, String>,
) -> String {
    let mut s = format!(
        r#"[macos]
# Ad-hoc signing works for CLAP, VST3, VST2, AU v2.
# AU v3 requires a Developer ID: "Developer ID Application: Your Name (TEAMID)"
signing_identity = "-"

[vendor]
name = "{vendor_name}"
id = "{vendor_id}"
url = "https://example.com"
au_manufacturer = "{au_mfr}"
"#,
        au_mfr = to_fourcc(vendor_name),
    );

    for p in plugins {
        let struct_name = to_pascal_case(&p.name);
        let crate_name = format!("{workspace_name}-{}", p.name);
        let fourcc = &fourcc_map[&p.name];
        s.push_str(&format!(
            r#"
[[plugin]]
name = "{display}"
suffix = "{suffix}"
crate = "{crate_name}"
category = "{category}"
fourcc = "{fourcc}"
au_tag = "{au_tag}"
"#,
            display = struct_name,
            suffix = p.name,
            category = p.kind.category(),
            au_tag = p.kind.au_tag(),
        ));
    }
    s
}

pub fn workspace_cargo_toml(workspace_name: &str, plugins: &[PluginSpec]) -> String {
    let members: Vec<String> = plugins
        .iter()
        .map(|p| format!("    \"plugins/{}\"", p.name))
        .collect();
    let members_str = members.join(",\n");

    format!(
        r#"[workspace]
resolver = "2"
members = [
{members_str},
]

[workspace.package]
name = "{workspace_name}"
version = "0.1.0"
edition = "2021"

[workspace.dependencies]
truce = {{ git = "https://github.com/truce-audio/truce" }}
truce-gui = {{ git = "https://github.com/truce-audio/truce" }}
truce-clap = {{ git = "https://github.com/truce-audio/truce" }}
truce-vst3 = {{ git = "https://github.com/truce-audio/truce" }}
truce-vst2 = {{ git = "https://github.com/truce-audio/truce" }}
truce-au = {{ git = "https://github.com/truce-audio/truce" }}
truce-aax = {{ git = "https://github.com/truce-audio/truce" }}
truce-test = {{ git = "https://github.com/truce-audio/truce" }}
clap-sys = "0.5"
"#,
    )
}

pub fn gitignore() -> &'static str {
    "/target\n"
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Assign collision-free fourcc codes to all plugins. When two plugins produce
/// the same code, the later one gets its last character replaced with '2'–'9',
/// then 'A'–'Z' until a unique code is found.
pub fn resolve_fourccs(plugins: &[PluginSpec]) -> HashMap<String, String> {
    let mut assignments: HashMap<String, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();

    for p in plugins {
        let mut fc = to_fourcc(&p.name);
        if !used.contains(&fc) {
            used.insert(fc.clone());
            assignments.insert(p.name.clone(), fc);
            continue;
        }
        // Collision — mutate last character
        let base: String = fc.chars().take(3).collect();
        let mut resolved = false;
        for suffix in ('2'..='9').chain('A'..='Z') {
            let candidate = format!("{base}{suffix}");
            if !used.contains(&candidate) {
                fc = candidate;
                resolved = true;
                break;
            }
        }
        if !resolved {
            // Extremely unlikely: 34 slots exhausted. Panic is acceptable here
            // since it means 35+ plugins share the same 3-char prefix.
            panic!("cannot resolve fourcc collision for '{}'", p.name);
        }
        used.insert(fc.clone());
        assignments.insert(p.name.clone(), fc);
    }

    assignments
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn to_pascal_case(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Generate a 4-character code from a plugin name using segment initials.
///
/// 1. Split on hyphens, take the first character (uppercased) of each segment.
/// 2. If fewer than 4 initials, backfill from the last segment's remaining
///    characters first (the differentiator), then earlier segments.
/// 3. Pad with 'X' if still short.
pub fn to_fourcc(s: &str) -> String {
    let segments: Vec<&str> = s.split('-').filter(|seg| !seg.is_empty()).collect();

    let mut code: Vec<char> = segments
        .iter()
        .map(|seg| {
            seg.chars()
                .next()
                .unwrap()
                .to_uppercase()
                .next()
                .unwrap()
        })
        .collect();

    if code.len() >= 4 {
        code.truncate(4);
        return code.into_iter().collect();
    }

    // Backfill from segments in reverse order (last segment = differentiator)
    let needed = 4 - code.len();
    let mut fill: Vec<char> = Vec::new();
    for seg in segments.iter().rev() {
        fill.extend(seg.chars().skip(1));
        if fill.len() >= needed {
            break;
        }
    }
    code.extend(fill.into_iter().take(needed));

    while code.len() < 4 {
        code.push('X');
    }

    code.into_iter().collect()
}

#[cfg(test)]
mod fourcc_tests {
    use super::*;

    // --- to_fourcc: segment-initials algorithm ---

    #[test]
    fn single_short_word() {
        assert_eq!(to_fourcc("gain"), "Gain");
    }

    #[test]
    fn single_long_word() {
        assert_eq!(to_fourcc("synth"), "Synt");
    }

    #[test]
    fn single_short_word_padded() {
        assert_eq!(to_fourcc("eq"), "EqXX");
    }

    #[test]
    fn multi_segment_uses_initials() {
        let fc = to_fourcc("delay-mono");
        // D from delay, M from mono, then backfill from "mono"
        assert_eq!(fc, "DMon");
    }

    #[test]
    fn multi_segment_differentiates_suffixes() {
        // These collided before the fix (both produced "Dela")
        assert_ne!(to_fourcc("delay-mono"), to_fourcc("delay-stereo"));
    }

    #[test]
    fn multi_segment_backfills_from_last() {
        assert_eq!(to_fourcc("delay-stereo"), "DSte");
    }

    #[test]
    fn four_plus_segments_truncated() {
        let fc = to_fourcc("a-b-c-d-e");
        assert_eq!(fc.len(), 4);
        assert_eq!(fc, "ABCD");
    }

    #[test]
    fn always_four_chars() {
        for name in ["a", "ab", "abc-d", "very-long-plugin-name"] {
            assert_eq!(to_fourcc(name).len(), 4, "failed for {name}");
        }
    }

    // --- resolve_fourccs: collision handling ---

    #[test]
    fn no_collision() {
        let plugins = vec![
            PluginSpec { name: "gain".into(), kind: PluginKind::Effect },
            PluginSpec { name: "synth".into(), kind: PluginKind::Instrument },
        ];
        let map = resolve_fourccs(&plugins);
        assert_eq!(map["gain"], to_fourcc("gain"));
        assert_eq!(map["synth"], to_fourcc("synth"));
    }

    #[test]
    fn collision_produces_unique_codes() {
        // Two names that produce the same initials + backfill
        let plugins = vec![
            PluginSpec { name: "aa".into(), kind: PluginKind::Effect },
            PluginSpec { name: "ab".into(), kind: PluginKind::Effect },
        ];
        let map = resolve_fourccs(&plugins);
        assert_ne!(map["aa"], map["ab"]);
        assert_eq!(map["aa"].len(), 4);
        assert_eq!(map["ab"].len(), 4);
    }

    #[test]
    fn three_way_collision_all_unique() {
        let plugins = vec![
            PluginSpec { name: "soft-clip".into(), kind: PluginKind::Effect },
            PluginSpec { name: "soft-comp".into(), kind: PluginKind::Effect },
            PluginSpec { name: "soft-crush".into(), kind: PluginKind::Effect },
        ];
        let map = resolve_fourccs(&plugins);
        let mut codes: Vec<&String> = map.values().collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), 3);
    }

    #[test]
    fn first_plugin_keeps_natural_code() {
        let plugins = vec![
            PluginSpec { name: "soft-clip".into(), kind: PluginKind::Effect },
            PluginSpec { name: "soft-comp".into(), kind: PluginKind::Effect },
        ];
        let map = resolve_fourccs(&plugins);
        // First plugin should keep its natural code
        assert_eq!(map["soft-clip"], to_fourcc("soft-clip"));
    }
}
