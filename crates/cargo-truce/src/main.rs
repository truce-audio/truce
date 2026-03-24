//! cargo-truce — build tool for truce audio plugins.
//!
//! Install:
//!   cargo install --git https://github.com/truce-audio/truce cargo-truce
//!
//! Usage:
//!   cargo truce new my-plugin          # scaffold a new plugin project
//!   cargo truce install                # build + bundle + sign + install
//!   cargo truce install --clap         # single format
//!   cargo truce validate               # run auval + pluginval
//!   cargo truce doctor                 # check environment

use std::fs;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "truce")
        .collect();

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        // Scaffold commands — handled here
        "new" => match cmd_new(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => { eprintln!("Error: {e}"); ExitCode::FAILURE }
        },

        // Build/install commands — forwarded to truce-xtask
        "install" | "build" | "run" | "test" | "status" | "clean"
        | "nuke" | "validate" | "doctor" | "log" => {
            truce_xtask::run(&args)
        },

        "help" | "--help" | "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("Unknown command: {other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "\
cargo-truce — build tool for truce audio plugins

USAGE:
  cargo truce new <name> [--instrument] [--midi]
      Scaffold a new plugin project.

  cargo truce install [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [-p <name>]
      Build, bundle, sign, and install plugins.

  cargo truce build [--clap] [--vst3] [-p <name>]
      Build plugin bundles without installing.

  cargo truce validate [--auval] [--pluginval]
      Run plugin validators.

  cargo truce test
      Run in-process regression tests.

  cargo truce clean
      Clear AU and DAW plugin caches.

  cargo truce status
      Show installed plugins.

  cargo truce doctor
      Check development environment.

  cargo truce log
      Stream AU v3 appex logs.
"
    );
}

type Res = Result<(), Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// new
// ---------------------------------------------------------------------------

fn cmd_new(args: &[String]) -> Res {
    let mut name: Option<String> = None;
    let mut kind = "effect";

    for arg in args {
        match arg.as_str() {
            "--instrument" => kind = "instrument",
            "--midi" => kind = "midi",
            s if !s.starts_with('-') && name.is_none() => name = Some(s.to_string()),
            other => return Err(format!("Unknown argument: {other}").into()),
        }
    }

    let name = name.ok_or("Usage: cargo truce new <name> [--instrument] [--midi]")?;

    if Path::new(&name).exists() {
        return Err(format!("Directory '{name}' already exists").into());
    }

    let struct_name = to_pascal_case(&name);
    let crate_name = name.clone();

    fs::create_dir_all(format!("{name}/src"))?;

    // Cargo.toml
    fs::write(
        format!("{name}/Cargo.toml"),
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
            crate_name = crate_name,
        ),
    )?;

    // truce.toml
    let au_type = match kind {
        "instrument" => "aumu",
        _ => "aufx",
    };
    let au_tag = match kind {
        "instrument" => "Synthesizer",
        _ => "Effects",
    };
    let au_sub = to_fourcc(&name);

    fs::write(
        format!("{name}/truce.toml"),
        format!(
            r#"[macos]
# Ad-hoc signing works for CLAP, VST3, VST2, AU v2.
# AU v3 requires a Developer ID: "Developer ID Application: Your Name (TEAMID)"
signing_identity = "-"

[vendor]
name = "My Company"
id = "com.mycompany"
url = "https://mycompany.com"
au_manufacturer = "MyCo"

[[plugin]]
name = "{display}"
suffix = "{name}"
crate = "{crate_name}"
au_type = "{au_type}"
fourcc = "{au_sub}"
au_tag = "{au_tag}"
"#,
            display = struct_name,
        ),
    )?;

    // src/lib.rs
    let process_body = match kind {
        "instrument" => r#"    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
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
        "midi" => r#"    fn process(&mut self, _buffer: &mut AudioBuffer, events: &EventList,
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
        _ => r#"    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
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

    let params = match kind {
        "midi" => format!(
            r#"#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(id = 0, name = "Semitones", range = "discrete(-12, 12)")]
    pub semitones: FloatParam,
}}"#
        ),
        _ => format!(
            r#"#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}"#
        ),
    };

    let layout_knob = match kind {
        "midi" => "GridWidget::knob(0u32, \"Semitones\")",
        _ => "GridWidget::knob(0u32, \"Gain\")",
    };

    let bus_layouts = match kind {
        "instrument" => "BusLayout::new().with_output(\"Main\", ChannelConfig::Stereo)",
        _ => "BusLayout::stereo()",
    };

    let test_fn = match kind {
        "instrument" => "render_instrument",
        _ => "render_effect",
    };

    fs::write(
        format!("{name}/src/lib.rs"),
        format!(
            r#"use truce::prelude::*;

{params}

pub struct {struct_name} {{
    params: {struct_name}Params,
}}

impl PluginLogic for {struct_name} {{
    fn new() -> Self {{
        Self {{ params: {struct_name}Params::new() }}
    }}

    fn params_mut(&mut self) -> Option<&mut dyn Params> {{
        Some(&mut self.params)
    }}

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
        let result = truce_test::{test_fn}::<Plugin>(512, 44100.0);
        truce_test::assert_no_nans(&result.output);
    }}
}}
"#,
        ),
    )?;

    // .gitignore
    fs::write(
        format!("{name}/.gitignore"),
        "/target\n",
    )?;

    eprintln!("Created {name}/");
    eprintln!();
    eprintln!("  cd {name}");
    eprintln!("  cargo truce install --clap      # build + install CLAP");
    eprintln!("  cargo truce install              # all formats");
    eprintln!("  cargo truce doctor               # check environment");
    eprintln!();
    eprintln!("Edit src/lib.rs to add your DSP.");
    eprintln!("Edit truce.toml to configure vendor info and AU metadata.");

    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn to_pascal_case(s: &str) -> String {
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

fn to_fourcc(s: &str) -> String {
    let pascal = to_pascal_case(s);
    let chars: Vec<char> = pascal.chars().take(4).collect();
    if chars.len() >= 4 {
        chars.into_iter().collect()
    } else {
        format!("{:X<4}", pascal.chars().take(4).collect::<String>())
    }
}
