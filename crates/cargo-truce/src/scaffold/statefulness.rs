//! Statefulness axis - whether the scaffolded plugin implements the
//! stateless `PurePluginLogic` sugar or the full `PluginLogic` with an
//! explicit `#[derive(Default)]` DSP-state struct. Drives the impl
//! header, the DSP-state declaration, and the `process` signature the
//! scaffold's lib.rs emits.

/// Which leaf trait the scaffold's plugin implements.
#[derive(Clone, Copy, PartialEq)]
pub enum Statefulness {
    /// `impl PurePluginLogic` - no `DspState`, no `state` argument, for
    /// a params-only effect.
    Pure,
    /// `impl PluginLogic` with `type DspState = <Name>DspState` and a
    /// `state: &mut Self::DspState` argument on `process`. The default:
    /// most plugins grow DSP state, so the plumbing is pre-wired.
    Stateful,
}

impl Statefulness {
    /// Leaf trait named in the `impl ... for` header.
    #[must_use]
    pub fn impl_trait(self) -> &'static str {
        match self {
            Self::Pure => "PurePluginLogic",
            Self::Stateful => "PluginLogic",
        }
    }

    /// Text emitted immediately above `pub struct <Name>;` - the
    /// DSP-state struct (stateful only) plus the descriptor's doc
    /// comment. Always ends in a newline so the struct declaration
    /// starts its own line.
    #[must_use]
    pub fn descriptor_block(self, struct_name: &str) -> String {
        match self {
            Self::Pure => PURE_DESCRIPTOR.to_string(),
            Self::Stateful => STATEFUL_DESCRIPTOR.replace("{struct_name}", struct_name),
        }
    }

    /// The `type DspState = ...;` line inside the impl block, or empty
    /// for a pure plugin. Ends in a newline when present.
    #[must_use]
    pub fn dsp_state_type(self, struct_name: &str) -> String {
        match self {
            Self::Pure => String::new(),
            Self::Stateful => format!("    type DspState = {struct_name}DspState;\n"),
        }
    }

    /// Inject the DSP-state argument into a per-kind `process` body for
    /// a stateful plugin; leave a pure body untouched. Named `_state`
    /// because a fresh scaffold has no state to read yet - rename it
    /// once the state struct grows a field.
    #[must_use]
    pub fn wrap_process(self, body: &str) -> String {
        match self {
            Self::Pure => body.to_string(),
            Self::Stateful => body.replacen(
                "fn process(\n",
                "fn process(\n        _state: &mut Self::DspState,\n",
                1,
            ),
        }
    }
}

const PURE_DESCRIPTOR: &str = "\
// Stateless descriptor. When your DSP needs per-instance state
// (filters, voices, phase), scaffold with `--stateful` - or add a
// `#[derive(Default)]` DSP-state struct, switch the impl header to
// `PluginLogic`, add `type DspState`, and take `state: &mut
// Self::DspState` as the first `process` argument.
";

const STATEFUL_DESCRIPTOR: &str = "\
// Per-instance DSP state - filters, delay lines, phase counters. The
// shell owns it and keeps it alive across a hot-reload, so a code-only
// reload preserves reverb tails and oscillator phase. Fields need
// `Default`: derive it, or hand-write `Default` when a fresh state has
// non-zero values.
#[derive(Default)]
pub struct {struct_name}DspState {}

// Stateless descriptor - the DSP state above lives in the shell.
";
