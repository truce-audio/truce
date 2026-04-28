//! CLI parser + config-file loader with strict precedence.
//!
//! Resolution order, first match wins:
//!
//! 1. CLI flag (`--output "…"`)
//! 2. Environment variable (`TRUCE_STANDALONE_OUTPUT`)
//! 3. Config file (`~/Library/Application Support/truce/standalone.toml` on
//!    macOS, `$XDG_CONFIG_HOME/truce/standalone.toml` on Linux,
//!    `%APPDATA%\truce\standalone.toml` on Windows)
//! 4. Compiled default (usually: whatever cpal picks)

use serde::Deserialize;
use std::path::PathBuf;

/// Resolved CLI + config-file + environment + defaults.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub headless: bool,
    pub list_devices: bool,
    pub list_midi: bool,
    pub output_device: Option<String>,
    pub input_device: Option<String>,
    /// Whether the mic input is enabled at launch. `None` →
    /// privacy default (off). Set explicitly via `--input-enabled
    /// on|off` or `default_input_enabled` in the config file.
    pub input_enabled: Option<bool>,
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
  --sample-rate <hz>        e.g. 44100, 48000, 96000
  --buffer <frames>         Audio buffer size (power of two recommended)
  --midi-input <name>       MIDI input device (substring match)
  --bpm <n>                 Transport BPM (default 120)
  --state <path>            Load plugin state from this file on launch
  -h, --help                Show this message

CONFIG FILE:
  macOS   ~/Library/Application Support/truce/standalone.toml
  Linux   $XDG_CONFIG_HOME/truce/standalone.toml (or ~/.config/...)
  Windows %APPDATA%\\truce\\standalone.toml

PRECEDENCE (first match wins):
  CLI flag > TRUCE_STANDALONE_* env var > config file > cpal default
";

/// Parse argv + env + config file and return resolved options.
/// Prints help and exits if `--help` / `-h` seen; prints error and
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

    // Layer env variables beneath CLI.
    let mut opts = Options {
        headless,
        list_devices,
        list_midi,
        output_device: output_device.or_else(|| env("OUTPUT")),
        input_device: input_device.or_else(|| env("INPUT")),
        input_enabled: input_enabled.or_else(|| {
            env("INPUT_ENABLED")
                .and_then(|s| parse_on_off(&s, "TRUCE_STANDALONE_INPUT_ENABLED").ok())
        }),
        sample_rate: sample_rate.or_else(|| env("SAMPLE_RATE").and_then(|s| s.parse().ok())),
        buffer_size: buffer_size.or_else(|| env("BUFFER").and_then(|s| s.parse().ok())),
        midi_input: midi_input.or_else(|| env("MIDI_INPUT")),
        bpm: bpm.or_else(|| env("BPM").and_then(|s| s.parse().ok())),
        state_path: state_path.or_else(|| env("STATE").map(PathBuf::from)),
        help: false,
    };

    // Config file — anything CLI+env left as None gets filled here.
    if let Some(config) = load_config() {
        opts.output_device = opts.output_device.or(config.default_output);
        opts.input_device = opts.input_device.or(config.default_input);
        opts.input_enabled = opts.input_enabled.or(config.default_input_enabled);
        opts.sample_rate = opts.sample_rate.or(config.default_sample_rate);
        opts.buffer_size = opts.buffer_size.or(config.default_buffer);
        opts.midi_input = opts.midi_input.or(config.default_midi_input);
        opts.bpm = opts.bpm.or(config.default_bpm);
    }

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

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    default_output: Option<String>,
    #[serde(default)]
    default_input: Option<String>,
    /// Privacy default: `None` (unset in config) means the runtime
    /// default takes effect (off). Set to `true` / `false` in config
    /// to override at launch.
    #[serde(default)]
    default_input_enabled: Option<bool>,
    #[serde(default)]
    default_sample_rate: Option<u32>,
    #[serde(default)]
    default_buffer: Option<u32>,
    #[serde(default)]
    default_midi_input: Option<String>,
    #[serde(default)]
    default_bpm: Option<f64>,
}

fn load_config() -> Option<Config> {
    let path = config_path()?;
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<Config>(&contents) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!("[truce-standalone] {} parse error: {e}", path.display());
            None
        }
    }
}

fn config_path() -> Option<PathBuf> {
    // dirs::config_dir() gives the platform-correct base:
    //   macOS   ~/Library/Application Support
    //   Linux   $XDG_CONFIG_HOME or ~/.config
    //   Windows %APPDATA% (roaming)
    Some(dirs::config_dir()?.join("truce").join("standalone.toml"))
}
