//! `cargo truce new` — scaffold a new plugin project under `examples/`.

use crate::{project_root, Res};
use std::fs;
use std::path::Path;

pub(crate) fn cmd_new(args: &[String]) -> Res {
    let mut name: Option<String> = None;
    let mut hot = false;

    for arg in args {
        match arg.as_str() {
            "--hot" => hot = true,
            s if !s.starts_with('-') && name.is_none() => name = Some(s.to_string()),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let name = name.ok_or("usage: cargo xtask new <name> [--hot]")?;
    let root = project_root();

    if hot {
        scaffold_hot_plugin(&root, &name)
    } else {
        scaffold_static_plugin(&root, &name)
    }
}

fn scaffold_static_plugin(root: &Path, name: &str) -> Res {
    let dir = root.join("examples").join(name);
    if dir.exists() {
        return Err(format!("examples/{name} already exists").into());
    }

    let crate_name = format!("truce-example-{name}");
    let struct_name = to_pascal_case(name);
    let _au_sub = to_fourcc(name);

    fs::create_dir_all(dir.join("src"))?;

    // Cargo.toml
    fs::write(dir.join("Cargo.toml"), format!(r#"[package]
name = "{crate_name}"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

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
dev = ["truce/dev"]

[dependencies]
truce = {{ workspace = true }}
truce-core = {{ workspace = true }}
truce-params = {{ workspace = true }}
truce-params-derive = {{ workspace = true }}
truce-loader = {{ workspace = true }}
truce-gui = {{ workspace = true }}
truce-clap = {{ workspace = true, optional = true }}
truce-vst3 = {{ workspace = true, optional = true }}
truce-vst2 = {{ workspace = true, optional = true }}
truce-lv2 = {{ workspace = true, optional = true }}
truce-au = {{ workspace = true, optional = true }}
truce-aax = {{ workspace = true, optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ workspace = true }}

[build-dependencies]
truce-build = {{ workspace = true }}
"#))?;

    // build.rs
    fs::write(dir.join("build.rs"), "fn main() { truce_build::emit_plugin_env(); }\n")?;

    // src/lib.rs
    fs::write(dir.join("src/lib.rs"), format!(r#"use truce::prelude::*;

#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}

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

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {{
        self.params.set_sample_rate(sample_rate);
    }}

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList, _context: &mut ProcessContext) -> ProcessStatus {{
        let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
        for ch in 0..buffer.channels() {{
            let (inp, out) = buffer.io(ch);
            for i in 0..buffer.num_samples() {{
                out[i] = inp[i] * gain;
            }}
        }}
        ProcessStatus::Normal
    }}

    fn layout(&self) -> truce_gui::layout::GridLayout {{
        use truce_gui::layout::{{GridLayout, GridWidget}};
        GridLayout::build("{struct_name}", "V0.1", 2, 80.0, vec![
            GridWidget::knob(0, "Gain").into(),
        ])
    }}
}}

truce::plugin! {{
    logic: {struct_name},
    params: {struct_name}Params,
}}
"#))?;

    eprintln!("Created examples/{name}/");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. Add \"{crate_name}\" to [workspace.members] in Cargo.toml");
    eprintln!("  2. Add a [[plugin]] entry to truce.toml with suffix = \"{name}\"");
    eprintln!("  3. cargo build -p {crate_name}");
    Ok(())
}

fn scaffold_hot_plugin(root: &Path, name: &str) -> Res {
    let dir = root.join("examples").join(name);
    if dir.exists() {
        return Err(format!("examples/{name} already exists").into());
    }

    let struct_name = to_pascal_case(name);
    let logic_crate = format!("{name}-logic");
    let shell_crate = format!("{name}-shell");
    let logic_lib = logic_crate.replace('-', "_");

    fs::create_dir_all(dir.join("logic/src"))?;
    fs::create_dir_all(dir.join("shell/src"))?;

    // --- Logic crate ---

    fs::write(dir.join("logic/Cargo.toml"), format!(r#"[package]
name = "{logic_crate}"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
truce = {{ workspace = true }}
truce-loader = {{ workspace = true }}
"#))?;

    fs::write(dir.join("logic/src/lib.rs"), format!(r#"use truce::prelude::*;

pub struct {struct_name} {{
    gain: f64,
}}

impl PluginLogic for {struct_name} {{
    fn new() -> Self {{
        Self {{ gain: 1.0 }}
    }}

    fn reset(&mut self, _sample_rate: f64, _max_block_size: usize) {{
        self.gain = 1.0;
    }}

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList, context: &mut ProcessContext) -> ProcessStatus {{
        let target = db_to_linear(context.param(0));
        for i in 0..buffer.num_samples() {{
            self.gain += 0.001 * (target - self.gain);
            let g = self.gain as f32;
            for ch in 0..buffer.channels() {{
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * g;
            }}
        }}
        ProcessStatus::Normal
    }}

    fn layout(&self) -> truce_gui::layout::GridLayout {{
        use truce_gui::layout::{{GridLayout, GridWidget}};
        GridLayout::build("{struct_name}", "V0.1", 2, 80.0, vec![
            GridWidget::knob(0, "Gain").into(),
        ])
    }}
}}

truce::export_plugin!({struct_name});
"#))?;

    // --- Shell crate ---

    fs::write(dir.join("shell/Cargo.toml"), format!(r#"[package]
name = "{shell_crate}"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = ["clap", "hot-reload"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
lv2 = ["dep:truce-lv2"]
au = ["dep:truce-au"]
aax = ["dep:truce-aax"]
hot-reload = []
static-logic = ["dep:{logic_crate}"]

[dependencies]
truce = {{ workspace = true }}
truce-core = {{ workspace = true }}
truce-params = {{ workspace = true }}
truce-params-derive = {{ workspace = true }}
truce-loader = {{ workspace = true, features = ["shell"] }}
truce-gui = {{ workspace = true }}
{logic_crate} = {{ path = "../logic", optional = true }}
truce-clap = {{ workspace = true, optional = true }}
truce-vst3 = {{ workspace = true, optional = true }}
truce-vst2 = {{ workspace = true, optional = true }}
truce-lv2 = {{ workspace = true, optional = true }}
truce-au = {{ workspace = true, optional = true }}
truce-aax = {{ workspace = true, optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[build-dependencies]
truce-build = {{ workspace = true }}
"#))?;

    fs::write(dir.join("shell/build.rs"), "fn main() { truce_build::emit_plugin_env(); }\n")?;

    fs::write(dir.join("shell/src/lib.rs"), format!(r#"use truce::prelude::*;

#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}

#[cfg(feature = "hot-reload")]
truce_loader::export_hot! {{
    params: {struct_name}Params,
    info: plugin_info!(),
    bus_layouts: [BusLayout::stereo()],
    logic_dylib: "{logic_lib}",
}}

#[cfg(feature = "static-logic")]
truce_loader::export_static! {{
    params: {struct_name}Params,
    info: plugin_info!(),
    bus_layouts: [BusLayout::stereo()],
    logic: {logic_lib}::{struct_name},
}}

#[cfg(feature = "clap")]
truce_clap::export_clap!(__HotShellWrapper);
#[cfg(feature = "vst3")]
truce_vst3::export_vst3!(__HotShellWrapper);
#[cfg(feature = "vst2")]
truce_vst2::export_vst2!(__HotShellWrapper);
#[cfg(feature = "lv2")]
truce_lv2::export_lv2!(__HotShellWrapper);
#[cfg(feature = "au")]
truce_au::export_au!(__HotShellWrapper);
#[cfg(feature = "aax")]
truce_aax::export_aax!(__HotShellWrapper);
"#))?;

    eprintln!("Created examples/{name}/shell/ and examples/{name}/logic/");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. Add \"{logic_crate}\" and \"{shell_crate}\" to [workspace.members] in Cargo.toml");
    eprintln!("  2. Add a [[plugin]] entry to truce.toml with suffix = \"{name}/shell\"");
    eprintln!("  3. cargo build -p {logic_crate}             # build the logic dylib");
    eprintln!("  4. cargo xtask install --clap -p {name}/shell  # install the shell once");
    eprintln!("  5. cargo watch -x \"build -p {logic_crate}\"   # iterate with hot-reload");
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

fn to_fourcc(s: &str) -> String {
    let segments: Vec<&str> = s.split('-').filter(|seg| !seg.is_empty()).collect();

    let mut code: Vec<char> = segments
        .iter()
        .map(|seg| {
            seg.chars()
                .next()
                .unwrap()
                .to_uppercase()
                .next()
                .unwrap()
        })
        .collect();

    if code.len() >= 4 {
        code.truncate(4);
        return code.into_iter().collect();
    }

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
