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
            other => Err(format!(
                "Unknown plugin type: {other} (expected effect, instrument, or midi)"
            )),
        }
    }

    fn category(self) -> &'static str {
        match self {
            Self::Instrument => "instrument",
            Self::Midi => "midi",
            Self::Effect => "effect",
        }
    }

    fn au_tag(self) -> &'static str {
        match self {
            Self::Instrument => "Synthesizer",
            Self::Midi => "MIDI",
            Self::Effect => "Effects",
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

pub fn plugin_cargo_toml_standalone(crate_name: &str, with_standalone: bool) -> String {
    let bin_block = if with_standalone {
        format!(
            "\n[[bin]]\nname = \"{crate_name}-standalone\"\n\
             path = \"src/main.rs\"\n\
             required-features = [\"standalone\"]\n"
        )
    } else {
        String::new()
    };
    let default_features = if with_standalone {
        r#"["clap", "vst3", "standalone"]"#
    } else {
        r#"["clap", "vst3"]"#
    };
    let standalone_feature = if with_standalone {
        "standalone = [\"dep:truce-standalone\"]\n"
    } else {
        ""
    };
    let standalone_dep = if with_standalone {
        "truce-standalone = { git = \"https://github.com/truce-audio/truce\", features = [\"gui\"], optional = true }\n"
    } else {
        ""
    };
    let default_label = if with_standalone {
        "CLAP + VST3 + standalone"
    } else {
        "CLAP + VST3"
    };
    format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]
{bin_block}
# Scaffolded default: {default_label}. To add LV2 / AU / AAX / VST2,
# add the matching feature + optional dep below (e.g.
# `lv2 = ["dep:truce-lv2"]` +
# `truce-lv2 = {{ git = "https://github.com/truce-audio/truce", optional = true }}`).
# VST2 is a legacy format — the Steinberg VST2 SDK was deprecated in
# 2018 and distributing VST2 plugins may require agreement with
# Steinberg's licensing terms.
[features]
default = {default_features}
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
{standalone_feature}hot-reload = ["truce/hot-reload"]

[dependencies]
truce = {{ git = "https://github.com/truce-audio/truce" }}
truce-gui = {{ git = "https://github.com/truce-audio/truce" }}
truce-clap = {{ git = "https://github.com/truce-audio/truce", optional = true }}
truce-vst3 = {{ git = "https://github.com/truce-audio/truce", optional = true }}
{standalone_dep}clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ git = "https://github.com/truce-audio/truce" }}

# `truce-build` emits `TRUCE_PLUGIN_*` env vars (consumed by
# `plugin_info!()`) + a `rustc-check-cfg` declaration covering every
# format feature the `truce::plugin!` macro references. Without it,
# rustc fires `unexpected_cfgs` warnings for every format this
# crate doesn't declare.
[build-dependencies]
truce-build = {{ git = "https://github.com/truce-audio/truce" }}
"#,
    )
}

pub fn plugin_cargo_toml_workspace(crate_name: &str, with_standalone: bool) -> String {
    let bin_block = if with_standalone {
        format!(
            "\n[[bin]]\nname = \"{crate_name}-standalone\"\n\
             path = \"src/main.rs\"\n\
             required-features = [\"standalone\"]\n"
        )
    } else {
        String::new()
    };
    let default_features = if with_standalone {
        r#"["clap", "vst3", "standalone"]"#
    } else {
        r#"["clap", "vst3"]"#
    };
    let standalone_feature = if with_standalone {
        "standalone = [\"dep:truce-standalone\"]\n"
    } else {
        ""
    };
    let standalone_dep = if with_standalone {
        "truce-standalone = { workspace = true, features = [\"gui\"], optional = true }\n"
    } else {
        ""
    };
    let default_label = if with_standalone {
        "CLAP + VST3 + standalone"
    } else {
        "CLAP + VST3"
    };
    format!(
        r#"[package]
name = "{crate_name}"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]
{bin_block}
# Scaffolded default: {default_label}. To add LV2 / AU / AAX / VST2,
# uncomment the matching line in the root `Cargo.toml`'s
# `[workspace.dependencies]`, then add the feature + optional dep
# below (e.g. `lv2 = ["dep:truce-lv2"]` +
# `truce-lv2 = {{ workspace = true, optional = true }}`).
[features]
default = {default_features}
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
{standalone_feature}hot-reload = ["truce/hot-reload"]

[dependencies]
truce = {{ workspace = true }}
truce-gui = {{ workspace = true }}
truce-clap = {{ workspace = true, optional = true }}
truce-vst3 = {{ workspace = true, optional = true }}
{standalone_dep}clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ workspace = true }}

# `truce-build` emits `TRUCE_PLUGIN_*` env vars (consumed by
# `plugin_info!()`) + a `rustc-check-cfg` declaration covering every
# format feature the `truce::plugin!` macro references. Without it,
# rustc fires `unexpected_cfgs` warnings for every format this
# crate doesn't declare.
[build-dependencies]
truce-build = {{ workspace = true }}
"#,
    )
}

/// Standalone-host bin source. `cargo truce run` builds this with
/// `--features standalone`, stages the binary into `target/bundles/`,
/// and launches it. Gated behind `required-features = ["standalone"]`
/// in Cargo.toml so release plugin bundles don't drag in the host.
pub fn plugin_main_rs(crate_name: &str) -> String {
    let crate_lib = crate_name.replace('-', "_");
    format!(
        r#"//! Entry point for standalone mode — run the plugin as a regular
//! desktop app via `cargo truce run`, no DAW needed. Only compiled
//! when the `standalone` feature is enabled (see `[[bin]]` in
//! Cargo.toml).
//!
//! Safe to delete this file (and the `standalone` feature + bin
//! entry in Cargo.toml) if you don't want a standalone build.

use {crate_lib}::Plugin;

fn main() {{
    truce_standalone::run::<Plugin>();
}}
"#
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
        PluginKind::Midi => "knob(P::Semitones, \"Semitones\")",
        _ => "knob(P::Gain, \"Gain\")",
    };

    let process_body = match kind {
        PluginKind::Instrument => {
            r#"    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
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
    }"#
        }
        PluginKind::Midi => {
            r#"    fn process(&mut self, _buffer: &mut AudioBuffer, events: &EventList,
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
    }"#
        }
        PluginKind::Effect => {
            r#"    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
               _context: &mut ProcessContext) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }"#
        }
    };

    let bus_layouts = kind.bus_layouts();
    let test_body = kind.test_body();

    let effect_only_tests = match kind {
        PluginKind::Instrument => "",
        _ => {
            r#"
    #[test]
    fn renders_nonzero_output() {
        let result = TEST_BODY;
        truce_test::assert_nonzero(&result.output);
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }"#
        }
    };
    let effect_only_tests = effect_only_tests.replace("TEST_BODY", test_body);

    let plugin_macro = match kind {
        PluginKind::Instrument => format!(
            r#"truce::plugin! {{
    logic: {struct_name},
    params: {struct_name}Params,
    bus_layouts: [{bus_layouts}],
}}"#
        ),
        _ => format!(
            r#"truce::plugin! {{
    logic: {struct_name},
    params: {struct_name}Params,
}}"#
        ),
    };

    let upper_name = struct_name.to_uppercase();

    format!(
        r#"use truce::prelude::*;
use truce_gui::layout::{{GridLayout, knob, widgets}};

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
        self.params.snap_smoothers();
    }}

{process_body}

    fn layout(&self) -> truce_gui::layout::GridLayout {{
        GridLayout::build("{upper_name}", "V0.1", 2, 50.0, vec![widgets(vec![
            {layout_knob},
        ])])
    }}
}}

{plugin_macro}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn builds_and_runs() {{
        let result = {test_body};
        truce_test::assert_no_nans(&result.output);
    }}
{effect_only_tests}
    #[test]
    fn info_is_valid() {{
        truce_test::assert_valid_info::<Plugin>();
    }}

    #[test]
    fn has_editor() {{
        truce_test::assert_has_editor::<Plugin>();
    }}

    #[test]
    fn state_round_trips() {{
        truce_test::assert_state_round_trip::<Plugin>();
    }}

    #[test]
    fn param_defaults_match() {{
        truce_test::assert_param_defaults_match::<Plugin>();
    }}

    #[test]
    fn no_duplicate_param_ids() {{
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }}

    #[test]
    fn corrupt_state_no_crash() {{
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }}

    #[test]
    fn param_normalized_clamped() {{
        truce_test::assert_param_normalized_clamped::<Plugin>();
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
    is_workspace: bool,
) -> String {
    let mut s = format!(
        r#"[vendor]
name = "{vendor_name}"
id = "{vendor_id}"
url = "https://example.com"
au_manufacturer = "{au_mfr}"
"#,
        au_mfr = to_fourcc(vendor_name),
    );

    for p in plugins {
        let struct_name = to_pascal_case(&p.name);
        let crate_name = if is_workspace {
            format!("{workspace_name}-{}", p.name)
        } else {
            p.name.clone()
        };
        let fourcc = &fourcc_map[&p.name];
        s.push_str(&format!(
            r#"
[[plugin]]
name = "{display}"
bundle_id = "{bundle_id}"
crate = "{crate_name}"
category = "{category}"
fourcc = "{fourcc}"
au_tag = "{au_tag}"
"#,
            display = struct_name,
            bundle_id = p.name,
            category = p.kind.category(),
            au_tag = p.kind.au_tag(),
        ));
    }
    s
}

pub fn workspace_cargo_toml(
    workspace_name: &str,
    plugins: &[PluginSpec],
    with_standalone: bool,
) -> String {
    let members: Vec<String> = plugins
        .iter()
        .map(|p| format!("    \"plugins/{}\"", p.name))
        .collect();
    let members_str = members.join(",\n");

    let standalone_dep = if with_standalone {
        "truce-standalone = { git = \"https://github.com/truce-audio/truce\" }\n"
    } else {
        ""
    };

    let _ = workspace_name; // reserved for future per-workspace config
    format!(
        r#"[workspace]
resolver = "2"
members = [
{members_str},
]

[workspace.package]
version = "0.1.0"
edition = "2021"

[workspace.dependencies]
truce = {{ git = "https://github.com/truce-audio/truce" }}
truce-gui = {{ git = "https://github.com/truce-audio/truce" }}
truce-clap = {{ git = "https://github.com/truce-audio/truce" }}
truce-vst3 = {{ git = "https://github.com/truce-audio/truce" }}
{standalone_dep}truce-test = {{ git = "https://github.com/truce-audio/truce" }}
truce-build = {{ git = "https://github.com/truce-audio/truce" }}
clap-sys = "0.5"

# Uncomment to opt in. After uncommenting here, add the matching
# feature + optional dep to each plugin's Cargo.toml.
# truce-lv2 = {{ git = "https://github.com/truce-audio/truce" }}
# truce-au  = {{ git = "https://github.com/truce-audio/truce" }}
# truce-aax = {{ git = "https://github.com/truce-audio/truce" }}
#
# VST2 is a legacy format — the Steinberg VST2 SDK was deprecated in
# 2018 and distributing VST2 plugins may require agreement with
# Steinberg's licensing terms. Enable only if you understand the
# implications:
# truce-vst2 = {{ git = "https://github.com/truce-audio/truce" }}
"#,
    )
}

pub fn gitignore() -> &'static str {
    "/target\n\
     /dist\n\
     # Per-developer build env (signing identities, SDK paths). cargo truce\n\
     # reads this file for [env] values — keep it out of the repo.\n\
     /.cargo/config.toml\n"
}

/// Initial `.cargo/config.toml` written into a fresh scaffold. Pre-seeded
/// with commented env-var stubs so developers have one obvious place to put
/// signing and SDK settings instead of hardcoding them into `truce.toml`.
///
/// This file is gitignored (see `gitignore()` above) — a fresh clone won't
/// have one, and developers create their own by filling in the blanks.
pub fn cargo_config_toml() -> &'static str {
    r#"# Local build-environment config. Gitignored — set your own values.
# `cargo truce install` and `cargo truce package` both read from here.

[env]
# macOS code signing (see `cargo truce doctor`):
# TRUCE_SIGNING_IDENTITY           = "Developer ID Application: Your Name (TEAMID)"
# TRUCE_INSTALLER_SIGNING_IDENTITY = "Developer ID Installer: Your Name (TEAMID)"

# AAX SDK location (macOS and Windows):
# AAX_SDK_PATH = "/path/to/aax-sdk-2-9-0"
# AAX_SDK_PATH = 'C:\Users\you\aax-sdk-2-9-0'

# macOS notarization (alternative to using a keychain profile):
# APPLE_ID              = "you@example.com"
# TEAM_ID               = "ABCDEFG123"
# APP_SPECIFIC_PASSWORD = "xxxx-xxxx-xxxx-xxxx"

# Windows Authenticode via Azure Trusted Signing:
# AZURE_TENANT_ID     = "..."
# AZURE_CLIENT_ID     = "..."
# AZURE_CLIENT_SECRET = "..."

# Windows .pfx password (when using [windows.signing].pfx_path):
# TRUCE_PFX_PASSWORD = "..."

# Screenshot testing — which OS owns the committed reference PNGs?
# Defaults to `macos`. Other platforms render and report diffs but
# don't fail the test. See docs/reference/gui/screenshot-testing.md.
# TRUCE_SCREENSHOT_REFERENCE_OS = "macos"
"#
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
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
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
/// 1. Split on any non-alphanumeric (`-`, `_`, `.`, etc.), take the
///    first character (uppercased) of each segment.
/// 2. If fewer than 4 initials, backfill from the last segment's remaining
///    characters first (the differentiator), then earlier segments.
/// 3. Pad with 'X' if still short.
pub fn to_fourcc(s: &str) -> String {
    let segments: Vec<&str> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|seg| !seg.is_empty())
        .collect();

    let mut code: Vec<char> = segments
        .iter()
        .map(|seg| seg.chars().next().unwrap().to_uppercase().next().unwrap())
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
mod pascal_case_tests {
    use super::*;

    #[test]
    fn single_word() {
        assert_eq!(to_pascal_case("gain"), "Gain");
    }

    #[test]
    fn hyphenated() {
        assert_eq!(to_pascal_case("demo-effect"), "DemoEffect");
    }

    #[test]
    fn snake_case_is_camelcased() {
        // Regression: scaffolded crate names with underscores
        // (`demo_effect`) used to pass through as `Demo_effect`,
        // triggering rustc's `non_camel_case_types` warning on every
        // generated struct. Splitting on any non-alphanumeric fixes it.
        assert_eq!(to_pascal_case("demo_effect"), "DemoEffect");
    }

    #[test]
    fn mixed_separators() {
        assert_eq!(to_pascal_case("foo_bar-baz"), "FooBarBaz");
    }

    #[test]
    fn empty_segments_dropped() {
        assert_eq!(to_pascal_case("foo--bar"), "FooBar");
        assert_eq!(to_pascal_case("__foo"), "Foo");
    }
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
    fn snake_case_separator() {
        // Regression: only `-` was treated as a segment separator,
        // so `demo_effect` collapsed to a single 11-char run instead
        // of two segments. Now it produces "DE" + backfill.
        assert_eq!(to_fourcc("demo_effect"), "DEff");
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
            PluginSpec {
                name: "gain".into(),
                kind: PluginKind::Effect,
            },
            PluginSpec {
                name: "synth".into(),
                kind: PluginKind::Instrument,
            },
        ];
        let map = resolve_fourccs(&plugins);
        assert_eq!(map["gain"], to_fourcc("gain"));
        assert_eq!(map["synth"], to_fourcc("synth"));
    }

    #[test]
    fn collision_produces_unique_codes() {
        // Two names that produce the same initials + backfill
        let plugins = vec![
            PluginSpec {
                name: "aa".into(),
                kind: PluginKind::Effect,
            },
            PluginSpec {
                name: "ab".into(),
                kind: PluginKind::Effect,
            },
        ];
        let map = resolve_fourccs(&plugins);
        assert_ne!(map["aa"], map["ab"]);
        assert_eq!(map["aa"].len(), 4);
        assert_eq!(map["ab"].len(), 4);
    }

    #[test]
    fn three_way_collision_all_unique() {
        let plugins = vec![
            PluginSpec {
                name: "soft-clip".into(),
                kind: PluginKind::Effect,
            },
            PluginSpec {
                name: "soft-comp".into(),
                kind: PluginKind::Effect,
            },
            PluginSpec {
                name: "soft-crush".into(),
                kind: PluginKind::Effect,
            },
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
            PluginSpec {
                name: "soft-clip".into(),
                kind: PluginKind::Effect,
            },
            PluginSpec {
                name: "soft-comp".into(),
                kind: PluginKind::Effect,
            },
        ];
        let map = resolve_fourccs(&plugins);
        // First plugin should keep its natural code
        assert_eq!(map["soft-clip"], to_fourcc("soft-clip"));
    }
}
