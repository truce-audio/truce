//! `cargo truce new` — scaffold a new plugin project under `plugins/`.

use crate::{project_root, Res};
use std::fs;
use std::path::Path;

pub(crate) fn cmd_new(args: &[String]) -> Res {
    let mut name: Option<String> = None;

    for arg in args {
        match arg.as_str() {
            s if !s.starts_with('-') && name.is_none() => name = Some(s.to_string()),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let name = name.ok_or("usage: cargo truce new <name>")?;
    let root = project_root();
    scaffold_static_plugin(&root, &name)
}

fn scaffold_static_plugin(root: &Path, name: &str) -> Res {
    let dir = root.join("plugins").join(name);
    if dir.exists() {
        return Err(format!("plugins/{name} already exists").into());
    }

    let crate_name = name.to_string();
    let struct_name = to_pascal_case(name);

    fs::create_dir_all(dir.join("src"))?;

    // Cargo.toml
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[[bin]]
name = "{name}-standalone"
path = "src/main.rs"
required-features = ["standalone"]

[features]
# CLAP and VST3 are enabled by default. VST2, LV2, AU, and AAX are opt-in.
# NOTE: VST2 is a legacy format. The Steinberg VST2 SDK was deprecated
# in 2018 and distributing VST2 plugins may require agreement with
# Steinberg's licensing terms. Enable `vst2` only if you understand
# the implications.
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
lv2 = ["dep:truce-lv2"]
au = ["dep:truce-au"]
aax = ["dep:truce-aax"]
standalone = ["dep:truce-standalone"]
dev = ["truce/dev"]

# Direct git refs keep the plugin self-contained — works in any
# workspace regardless of which `truce-*` crates its root
# `[workspace.dependencies]` happens to declare. If your workspace
# already pins a specific revision, flip these to
# `{{ workspace = true }}`.
[dependencies]
truce = {{ git = "{repo}" }}
truce-gui = {{ git = "{repo}" }}
truce-loader = {{ git = "{repo}" }}
truce-clap = {{ git = "{repo}", optional = true }}
truce-vst3 = {{ git = "{repo}", optional = true }}
truce-vst2 = {{ git = "{repo}", optional = true }}
truce-lv2 = {{ git = "{repo}", optional = true }}
truce-au = {{ git = "{repo}", optional = true }}
truce-aax = {{ git = "{repo}", optional = true }}
truce-standalone = {{ git = "{repo}", features = ["gui"], optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ git = "{repo}", features = ["in-process"] }}

[build-dependencies]
truce-build = {{ git = "{repo}" }}
"#,
            repo = "https://github.com/truce-audio/truce",
        ),
    )?;

    // build.rs — emits TRUCE_PLUGIN_* env vars + check-cfg for format features
    // so plugin code doesn't trip `unexpected_cfgs` warnings for features
    // it hasn't opted in to.
    fs::write(
        dir.join("build.rs"),
        "fn main() { truce_build::emit_plugin_env(); }\n",
    )?;

    // src/lib.rs
    fs::write(
        dir.join("src/lib.rs"),
        format!(
            r#"use truce::prelude::*;
use truce_gui::layout::{{knob, widgets, GridLayout}};

#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}

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
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {{
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }}

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {{
        for i in 0..buffer.num_samples() {{
            let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
            for ch in 0..buffer.channels() {{
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }}
        }}
        ProcessStatus::Normal
    }}

    fn layout(&self) -> GridLayout {{
        GridLayout::build("{upper_name}", "V0.1", 2, 50.0, vec![widgets(vec![
            knob(P::Gain, "Gain"),
        ])])
    }}
}}

truce::plugin! {{
    logic: {struct_name},
    params: {struct_name}Params,
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn info_is_valid() {{
        truce_test::assert_valid_info::<Plugin>();
    }}

    #[test]
    fn renders_nonzero_output() {{
        let result = truce_test::render_effect::<Plugin>(512, 44100.0);
        truce_test::assert_nonzero(&result.output);
        truce_test::assert_no_nans(&result.output);
    }}

    #[test]
    fn state_round_trips() {{
        truce_test::assert_state_round_trip::<Plugin>();
    }}

    #[test]
    fn bus_config_effect() {{
        truce_test::assert_bus_config_effect::<Plugin>();
    }}

    #[test]
    fn param_defaults_match() {{
        truce_test::assert_param_defaults_match::<Plugin>();
    }}

    #[test]
    fn gui_screenshot() {{
        let params = Arc::new({struct_name}Params::new());
        let p = {struct_name}::new(Arc::clone(&params));
        let layout = p.layout();
        // Reference PNGs live in the project root's `snapshots/` dir.
        // First run logs a `cp`-based promote hint with the exact
        // command to commit the freshly-rendered reference.
        let (pixels, w, h) = truce_gpu::screenshot::render_to_pixels(params, layout);
        truce_test::assert_screenshot(
            "{name}_default", &pixels, w, h, 0, "snapshots",
        );
    }}
}}
"#,
            upper_name = struct_name.to_uppercase(),
        ),
    )?;

    // src/main.rs — standalone binary, gated behind the `standalone` feature
    // so release bundles don't drag in the standalone host.
    fs::write(
        dir.join("src/main.rs"),
        format!(
            r#"use {crate_lib}::Plugin;

fn main() {{
    truce_standalone::run::<Plugin>();
}}
"#,
            crate_lib = crate_name.replace('-', "_"),
        ),
    )?;

    // presets/ — empty directory for factory preset `.preset` TOML
    // files. When populated, the install pipeline emits per-format
    // native preset files alongside the plugin bundle. See
    // `truce-docs/docs/internal/presets.md` for the authoring schema.
    fs::create_dir_all(dir.join("presets"))?;

    eprintln!("Created plugins/{name}/");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. Add \"plugins/{name}\" to [workspace.members] in Cargo.toml");
    eprintln!("  2. Add a [[plugin]] entry to truce.toml with bundle_id = \"{name}\"");
    eprintln!("  3. cargo truce install -p {name}");
    Ok(())
}

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
