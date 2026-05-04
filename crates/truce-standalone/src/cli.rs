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
    /// Print status chatter (device picks, toggles, save / load
    /// confirmations, transport state). Off by default; set with
    /// `--verbose` / `-v`. Errors and `--list-*` output ignore
    /// this flag and always print.
    pub verbose: bool,
    /// `.wav` file to feed into the plugin's input bus, summed
    /// with mic input when both are active. Only populated when
    /// the `playback` feature is enabled.
    #[cfg(feature = "playback")]
    pub input_file: Option<PathBuf>,
    /// `.wav` file to write the plugin's output bus to. Forces
    /// headless. Only populated when the `playback` feature is
    /// enabled.
    #[cfg(feature = "playback")]
    pub output_file: Option<PathBuf>,
    /// Bypass cpal entirely and render as fast as the CPU
    /// allows. Only meaningful with both `input_file` and
    /// `output_file` set; ignored with a warning otherwise.
    #[cfg(feature = "playback")]
    pub no_playback: bool,
    pub help: bool,
}

const HELP_HEAD: &str = "\
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
  -v, --verbose             Print status chatter (device picks, toggles,
                            save/load notices, transport state). Errors
                            and --list-* output always print.
";

#[cfg(feature = "playback")]
const HELP_PLAYBACK: &str = "\
  --input-file <path>       Decode <path>.wav and feed it into the
                            plugin's input bus. One-shot — plays once,
                            then plugin sees silence on the file
                            channel. Mic + file sum when both are
                            enabled. Linear-interp resample if file SR
                            doesn't match device SR; channel-count
                            mismatches are soft-warned and adapted.
  --output-file <path>      Capture the plugin's output bus to <path>.wav.
                            32-bit float; pre-mute. Implies --headless.
                            Real-time by default; pair with --no-playback
                            for offline rendering.
  --no-playback             Bypass cpal entirely; render as fast as the
                            CPU allows. Requires both --input-file and
                            --output-file (otherwise ignored with a warn).
";

const HELP_TAIL: &str = "\
  -h, --help                Show this message

PRECEDENCE (first match wins):
  CLI flag > TRUCE_STANDALONE_* env var > plugin-author Defaults
   > runtime default (input off, output on, cpal-picked devices)

  Plugin-author defaults are set in code by calling
  `truce_standalone::run_with::<Plugin>(Defaults { … })` instead
  of `truce_standalone::run::<Plugin>()`.
";

fn print_help() {
    print!("{HELP_HEAD}");
    #[cfg(feature = "playback")]
    print!("{HELP_PLAYBACK}");
    print!("{HELP_TAIL}");
}

/// Parse argv + env and return resolved options. Plugin-author
/// defaults are not handled here — [`crate::run_with`] applies them
/// after parsing. When `--help` / `-h` is present, prints help and
/// returns options with `help = true` so the caller exits cleanly
/// at the binary boundary (no `process::exit` from library code,
/// which would short-circuit any test that drives this entry point).
/// Returns `Err` on parse failure.
///
/// # Errors
///
/// Returns `Err(String)` if a value-bearing flag is missing its
/// argument, fails the type-coercion (`f64`, `usize`, etc.), or an
/// unrecognized positional / leftover token slips through.
pub fn parse() -> Result<Options, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut args = pico_args::Arguments::from_vec(args);

    // Presence flags first (affect the rest of parsing).
    let help = args.contains(["-h", "--help"]);
    if help {
        print_help();
        return Ok(Options {
            help: true,
            ..Options::default()
        });
    }

    let headless = args.contains("--headless");
    let list_devices = args.contains("--list-devices");
    let list_midi = args.contains("--list-midi");
    let verbose = args.contains(["-v", "--verbose"]);

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
    #[cfg(feature = "playback")]
    let input_file = args
        .opt_value_from_str::<_, PathBuf>("--input-file")
        .map_err(|e| format!("--input-file: {e}"))?;
    #[cfg(feature = "playback")]
    let output_file = args
        .opt_value_from_str::<_, PathBuf>("--output-file")
        .map_err(|e| format!("--output-file: {e}"))?;
    #[cfg(feature = "playback")]
    let no_playback = args.contains("--no-playback");

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
        verbose: verbose
            || env("VERBOSE")
                .is_some_and(|s| matches!(s.trim(), "1" | "true" | "on" | "yes")),
        #[cfg(feature = "playback")]
        input_file: input_file.or_else(|| env("INPUT_FILE").map(PathBuf::from)),
        #[cfg(feature = "playback")]
        output_file: output_file.or_else(|| env("OUTPUT_FILE").map(PathBuf::from)),
        #[cfg(feature = "playback")]
        no_playback,
        help: false,
    };

    Ok(opts)
}

/// Parse an on/off boolean. `source` is the human-facing label
/// prepended to error messages — pass the originating CLI flag
/// (`"--input-enabled"`) when called from argv parsing, or the env
/// var name (`"TRUCE_STANDALONE_INPUT_ENABLED"`) when called from the
/// env-fallback layer. Either way, the user sees a parse error
/// pointing at exactly where the bad value came from.
fn parse_on_off(s: &str, source: &str) -> Result<bool, String> {
    match s.trim().to_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("{source}: expected `on` or `off` (got `{other}`)")),
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(format!("TRUCE_STANDALONE_{name}")).ok()
}
