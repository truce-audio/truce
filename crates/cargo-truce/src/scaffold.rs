//! Shared scaffolding templates for `cargo truce new` and `cargo truce new-workspace`.

use std::collections::HashSet;

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
        GridLayout::build("{struct_name}", "V0.1", 2, 80.0, vec![
            {layout_knob},
        ], vec![])
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

pub fn truce_toml(vendor_name: &str, vendor_id: &str, plugins: &[PluginSpec], workspace_name: &str) -> String {
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
        let fourcc = to_fourcc(&p.name);
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

/// Check for fourcc collisions among plugin names. Returns an error message
/// listing the collisions, or Ok(()) if none.
pub fn check_fourcc_collisions(plugins: &[PluginSpec]) -> Result<(), String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut collisions: Vec<String> = Vec::new();
    for p in plugins {
        let fc = to_fourcc(&p.name);
        if !seen.insert(fc.clone()) {
            collisions.push(format!("'{}' (fourcc: {fc})", p.name));
        }
    }
    if collisions.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Fourcc collision detected — these plugins produce the same 4-char code: {}. \
             Rename one to avoid the collision.",
            collisions.join(", "),
        ))
    }
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

pub fn to_fourcc(s: &str) -> String {
    let pascal = to_pascal_case(s);
    let chars: Vec<char> = pascal.chars().take(4).collect();
    if chars.len() >= 4 {
        chars.into_iter().collect()
    } else {
        format!("{:X<4}", pascal.chars().take(4).collect::<String>())
    }
}
