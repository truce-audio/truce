//! Plugin kind - drives the per-kind text fragments injected into
//! the scaffold's lib.rs (params struct, process body, layout knob,
//! plugin macro args, effect-only tests).
//!
//! Every per-kind axis lives as a method on `PluginKind` so adding
//! a fourth variant (or tweaking one variant's `process` template)
//! is a one-place change.

#[derive(Clone, Copy, PartialEq)]
pub enum PluginKind {
    Effect,
    Instrument,
    Midi,
}

impl std::str::FromStr for PluginKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "effect" => Ok(Self::Effect),
            "instrument" => Ok(Self::Instrument),
            "midi" => Ok(Self::Midi),
            other => Err(format!(
                "Unknown plugin type: {other} (expected effect, instrument, or midi)"
            )),
        }
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.category())
    }
}

impl PluginKind {
    #[must_use]
    pub fn category(self) -> &'static str {
        match self {
            Self::Instrument => "instrument",
            Self::Midi => "midi",
            Self::Effect => "effect",
        }
    }

    #[must_use]
    pub fn au_tag(self) -> &'static str {
        match self {
            Self::Instrument => "Synthesizer",
            Self::Midi => "MIDI",
            Self::Effect => "Effects",
        }
    }

    /// Default VST3 secondary subcategory baked into the scaffolded
    /// `truce.toml`. Without it, hosts like Cubase bucket the plug-in
    /// under "Other". The user is expected to tighten this to the
    /// actual effect kind (`Delay`, `Reverb`, `Distortion`, …) once
    /// the DSP shape is settled; `Tools` is the safe placeholder for
    /// generic effects. `NoteEffect` / Midi plug-ins get a value too,
    /// but `truce-vst3` overrides them to `Fx|Event` regardless.
    #[must_use]
    pub fn vst3_subcategory(self) -> &'static str {
        match self {
            Self::Instrument => "Synth",
            Self::Effect | Self::Midi => "Tools",
        }
    }

    /// `PluginLogic::bus_layouts()` method body for non-stereo
    /// kinds. Empty string for stereo (the trait default), so
    /// the scaffold doesn't emit a redundant override.
    #[must_use]
    pub fn bus_layouts_method(self) -> &'static str {
        match self {
            Self::Instrument => {
                "    fn bus_layouts() -> Vec<BusLayout> {\n        \
                 vec![BusLayout::new().with_output(\"Main\", ChannelConfig::Stereo)]\n    \
                 }\n\n"
            }
            _ => "",
        }
    }

    /// Per-kind `Params` struct, with `{struct_name}` substituted.
    #[must_use]
    pub fn params_struct(self, struct_name: &str) -> String {
        let tpl = match self {
            Self::Midi => MIDI_PARAMS_STRUCT,
            _ => DEFAULT_PARAMS_STRUCT,
        };
        tpl.replace("{struct_name}", struct_name)
    }

    #[must_use]
    pub fn layout_knob(self) -> &'static str {
        match self {
            Self::Midi => "knob(P::Semitones, \"Semitones\")",
            _ => "knob(P::Gain, \"Gain\")",
        }
    }

    #[must_use]
    pub fn process_body(self) -> &'static str {
        match self {
            Self::Instrument => INSTRUMENT_PROCESS_BODY,
            Self::Midi => MIDI_PROCESS_BODY,
            Self::Effect => EFFECT_PROCESS_BODY,
        }
    }

    /// `truce::plugin!` invocation. Same shape across kinds; bus
    /// layouts come from `PluginLogic::bus_layouts()` (see
    /// [`Self::bus_layouts_method`]).
    #[must_use]
    pub fn plugin_macro(self, struct_name: &str) -> String {
        let _ = self;
        format!(
            "truce::plugin! {{\n    \
             logic: {struct_name},\n    \
             params: {struct_name}Params,\n\
             }}"
        )
    }
}

// ---------------------------------------------------------------------------
// Per-kind text fragments
// ---------------------------------------------------------------------------

const DEFAULT_PARAMS_STRUCT: &str = r#"#[derive(Params)]
pub struct {struct_name}Params {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}"#;

const MIDI_PARAMS_STRUCT: &str = r#"#[derive(Params)]
pub struct {struct_name}Params {
    #[param(name = "Semitones", range = "discrete(-12, 12)")]
    pub semitones: IntParam,
}"#;

const EFFECT_PROCESS_BODY: &str = r"    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
               _context: &mut ProcessContext) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain = db_to_linear(self.params.gain.read());
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }";

const INSTRUMENT_PROCESS_BODY: &str = r"    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
               _context: &mut ProcessContext) -> ProcessStatus {
        // Trigger / release your voices here. Note events arrive at
        // sample-accurate offsets via `event.frame_offset`; in-block
        // dispatch is up to you.
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { note, velocity, .. } => {
                    let _ = (note, velocity);
                }
                EventBody::NoteOff { note, .. } => {
                    let _ = note;
                }
                _ => {}
            }
        }

        // Render your voices into the output channels here. The
        // scaffold ships silence so a fresh `cargo truce run` is
        // immediately audible (and visibly silent) for sanity-checking.
        for ch in 0..buffer.num_output_channels() {
            for i in 0..buffer.num_samples() {
                buffer.output(ch)[i] = 0.0;
            }
        }
        ProcessStatus::Normal
    }";

const MIDI_PROCESS_BODY: &str = r"    fn process(&mut self, _buffer: &mut AudioBuffer, events: &EventList,
               context: &mut ProcessContext) -> ProcessStatus {
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { group, channel, note, velocity } => {
                    let shifted = (i32::from(*note) + self.params.semitones.value_i32())
                        .clamp(0, 127) as u8;
                    context.output_events.push(Event {
                        sample_offset: event.sample_offset,
                        body: EventBody::NoteOn {
                            group: *group, channel: *channel, note: shifted, velocity: *velocity,
                        },
                    });
                }
                EventBody::NoteOff { group, channel, note, velocity } => {
                    let shifted = (i32::from(*note) + self.params.semitones.value_i32())
                        .clamp(0, 127) as u8;
                    context.output_events.push(Event {
                        sample_offset: event.sample_offset,
                        body: EventBody::NoteOff {
                            group: *group, channel: *channel, note: shifted, velocity: *velocity,
                        },
                    });
                }
                _ => {}
            }
        }
        ProcessStatus::Normal
    }";
