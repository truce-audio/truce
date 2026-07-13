//! Statefulness axis - whether the scaffolded plugin implements the
//! stateless `PurePluginLogic` sugar or the full `PluginLogic` where the
//! descriptor is its own DSP state (`type DspState = Self`). Drives the
//! descriptor struct, the `type DspState` line, and the `process`
//! signature the scaffold's lib.rs emits.

/// Which leaf trait the scaffold's plugin implements.
#[derive(Clone, Copy, PartialEq)]
pub enum Statefulness {
    /// `impl PurePluginLogic` - no `DspState`, no `state` argument, for
    /// a params-only effect.
    Pure,
    /// `impl PluginLogic` where the descriptor is its own DSP state
    /// (`type DspState = Self`) and `process` takes `state: &mut
    /// Self::DspState`. The default: most plugins grow DSP state, so the
    /// plumbing is pre-wired.
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

    /// Doc comment emitted immediately above the descriptor struct.
    /// Always ends in a newline so the struct starts its own line.
    #[must_use]
    pub fn descriptor_block(self) -> String {
        match self {
            Self::Pure => PURE_DESCRIPTOR.to_string(),
            Self::Stateful => STATEFUL_DESCRIPTOR.to_string(),
        }
    }

    /// The descriptor struct declaration itself. A pure plugin is a unit
    /// struct; a stateful plugin folds its DSP state into the descriptor
    /// (`type DspState = Self`), so the struct carries `#[derive(Default)]`
    /// and a field block. No trailing newline - the template supplies it.
    #[must_use]
    pub fn struct_decl(self, struct_name: &str) -> String {
        match self {
            Self::Pure => format!("pub struct {struct_name};"),
            Self::Stateful => STATEFUL_STRUCT.replace("{struct_name}", struct_name),
        }
    }

    /// The `type DspState = ...;` line inside the impl block, or empty
    /// for a pure plugin. Ends in a newline when present.
    #[must_use]
    pub fn dsp_state_type(self) -> String {
        match self {
            Self::Pure => String::new(),
            Self::Stateful => "    type DspState = Self;\n".to_string(),
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
// (filters, voices, phase), scaffold with `--stateful` - or give this
// struct fields, add `#[derive(Default)]`, switch the impl header to
// `PluginLogic`, add `type DspState = Self`, and take `state: &mut
// Self::DspState` as the first `process` argument.
";

const STATEFUL_DESCRIPTOR: &str = "\
// The plugin struct is its own DSP state (`type DspState = Self`). The
// shell owns it and preserves it across a hot-reload, so a code-only
// reload keeps reverb tails and oscillator phase alive.
";

const STATEFUL_STRUCT: &str = "\
#[derive(Default)]
pub struct {struct_name} {
    // Per-instance DSP state - filters, delay lines, phase counters.
    // Fields need `Default`. Add them as your DSP grows.
}";
