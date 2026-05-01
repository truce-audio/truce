//! CLI parser with strict precedence.
//!
//! Resolution order, first match wins:
//!
//! 1. CLI flag (`--output "…"`)
//! 2. Environment variable (`TRUCE_STANDALONE_OUTPUT`)
//! 3. Plugin-author defaults supplied via
//!    [`crate::run_with`] / [`crate::Defaults`] (only `input_enabled`
//!    and `output_enabled` participate at this tier)
//! 4. Compiled runtime default (input off, output on, cpal-picked
//!    devices)

use std::path::PathBuf;

/// Resolved CLI + env + project-baked + runtime defaults.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub headless: bool,
    pub list_devices: bool,
    pub list_midi: bool,
    pub output_device: Option<String>,
    pub input_device: Option<String>,
    /// Whether the mic input is enabled at launch. `None` →
    /// privacy default (off). Set explicitly via `--input-enabled
    /// on|off`, the env var, or `[plugin.standalone].input_enabled`
    /// in `truce.toml`.
    pub input_enabled: Option<bool>,
    /// Whether the speaker output is enabled at launch. `None` →
    /// runtime default (on — the user launched standalone to hear
    /// the plugin). Set explicitly via `--output-enabled on|off`,
    /// the env var, or `[plugin.standalone].output_enabled` in
    /// `truce.toml`.
    pub output_enabled: Option<bool>,
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
    pub midi_input: Option<String>,
    pub bpm: Option<f64>,
    pub state_path: Option<PathBuf>,
    pub help: bool,
}

const HELP: &str = "\
truce standalone

USAGE:
  <plugin>-standalone [OPTIONS]

OPTIONS:
  --headless                Run audio only; no window
  --list-devices            List audio output + input devices and exit
  --list-midi               List MIDI input devices and exit
  --output <name>           Audio output device (substring match)
  --input <name>            Audio input device (effect plugins)
  --input-enabled <on|off>  Enable mic input at launch (default: off).
                            Press `I` in the window to toggle live.
  --output-enabled <on|off> Enable speaker output at launch (default: on).
                            Toggle live from the Plugin menu (Cmd+O / Ctrl+O).
  --sample-rate <hz>        e.g. 44100, 48000, 96000
  --buffer <frames>         Audio buffer size (power of two recommended)
  --midi-input <name>       MIDI input device (substring match)
  --bpm <n>                 Transport BPM (default 120)
  --state <path>            Load plugin state from this file on launch
  -h, --help                Show this message

PRECEDENCE (first match wins):
  CLI flag > TRUCE_STANDALONE_* env var > plugin-author Defaults
   > runtime default (input off, output on, cpal-picked devices)

  Plugin-author defaults are set in code by calling
  `truce_standalone::run_with::<Plugin>(Defaults { … })` instead
  of `truce_standalone::run::<Plugin>()`.
";

/// Parse argv + env and return resolved options. Plugin-author
/// defaults are not handled here — [`crate::run_with`] applies them
/// after parsing. Prints help and exits if `--help` / `-h` seen;
/// returns `Err` on parse failure.
pub fn parse() -> Result<Options, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut args = pico_args::Arguments::from_vec(args);

    // Presence flags first (affect the rest of parsing).
    let help = args.contains(["-h", "--help"]);
    if help {
        print!("{HELP}");
        std::process::exit(0);
    }

    let headless = args.contains("--headless");
    let list_devices = args.contains("--list-devices");
    let list_midi = args.contains("--list-midi");

    // Parse values — each `Option<...>` short-circuits to None if absent.
    let output_device = args
        .opt_value_from_str::<_, String>("--output")
        .map_err(|e| format!("--output: {e}"))?;
    let input_device = args
        .opt_value_from_str::<_, String>("--input")
        .map_err(|e| format!("--input: {e}"))?;
    let input_enabled = args
        .opt_value_from_str::<_, String>("--input-enabled")
        .map_err(|e| format!("--input-enabled: {e}"))?
        .map(|s| parse_on_off(&s, "--input-enabled"))
        .transpose()?;
    let output_enabled = args
        .opt_value_from_str::<_, String>("--output-enabled")
        .map_err(|e| format!("--output-enabled: {e}"))?
        .map(|s| parse_on_off(&s, "--output-enabled"))
        .transpose()?;
    let sample_rate = args
        .opt_value_from_str::<_, u32>("--sample-rate")
        .map_err(|e| format!("--sample-rate: {e}"))?;
    let buffer_size = args
        .opt_value_from_str::<_, u32>("--buffer")
        .map_err(|e| format!("--buffer: {e}"))?;
    let midi_input = args
        .opt_value_from_str::<_, String>("--midi-input")
        .map_err(|e| format!("--midi-input: {e}"))?;
    let bpm = args
        .opt_value_from_str::<_, f64>("--bpm")
        .map_err(|e| format!("--bpm: {e}"))?;
    let state_path = args
        .opt_value_from_str::<_, PathBuf>("--state")
        .map_err(|e| format!("--state: {e}"))?;

    let leftover = args.finish();
    if !leftover.is_empty() {
        return Err(format!("unknown arguments: {leftover:?}"));
    }

    // Layer env variables beneath CLI. Plugin-author defaults are
    // applied later by `run_with` (only `input_enabled` /
    // `output_enabled` participate at that tier).
    let opts = Options {
        headless,
        list_devices,
        list_midi,
        output_device: output_device.or_else(|| env("OUTPUT")),
        input_device: input_device.or_else(|| env("INPUT")),
        input_enabled: input_enabled.or_else(|| {
            env("INPUT_ENABLED")
                .and_then(|s| parse_on_off(&s, "TRUCE_STANDALONE_INPUT_ENABLED").ok())
        }),
        output_enabled: output_enabled.or_else(|| {
            env("OUTPUT_ENABLED")
                .and_then(|s| parse_on_off(&s, "TRUCE_STANDALONE_OUTPUT_ENABLED").ok())
        }),
        sample_rate: sample_rate.or_else(|| env("SAMPLE_RATE").and_then(|s| s.parse().ok())),
        buffer_size: buffer_size.or_else(|| env("BUFFER").and_then(|s| s.parse().ok())),
        midi_input: midi_input.or_else(|| env("MIDI_INPUT")),
        bpm: bpm.or_else(|| env("BPM").and_then(|s| s.parse().ok())),
        state_path: state_path.or_else(|| env("STATE").map(PathBuf::from)),
        help: false,
    };

    Ok(opts)
}

fn parse_on_off(s: &str, flag: &str) -> Result<bool, String> {
    match s.trim().to_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("{flag}: expected `on` or `off` (got `{other}`)")),
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(format!("TRUCE_STANDALONE_{name}")).ok()
}
