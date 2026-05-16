[package]
name = "{crate_name}"
{{ if is_workspace -}}
version.workspace = true
edition.workspace = true
{{- else -}}
version = "0.1.0"
edition = "2024"
{{- endif }}

[lib]
crate-type = ["cdylib", "rlib"]
{{ if has_standalone }}
[[bin]]
name = "{crate_name}-standalone"
path = "src/main.rs"
required-features = ["standalone"]
{{ endif }}
{{ if is_workspace -}}
# Scaffolded default: {default_label}. To add LV2 / AU / AAX / VST2,
# uncomment the matching line in the root `Cargo.toml`'s
# `[workspace.dependencies]`, then add the feature + optional dep
# below (e.g. `lv2 = ["dep:truce-lv2", "truce/lv2"]` +
# `truce-lv2 = \{ workspace = true, optional = true }`).
{{- else -}}
# Scaffolded default: {default_label}. To add LV2 / AU / AAX / VST2,
# add the matching feature + optional dep below (e.g.
# `lv2 = ["dep:truce-lv2", "truce/lv2"]` +
# `truce-lv2 = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}", optional = true }`).
# VST2 is a legacy format — the Steinberg VST2 SDK was deprecated in
# 2018 and distributing VST2 plugins may require agreement with
# Steinberg's licensing terms.
{{- endif }}
# Format features pair `"dep:truce-<format>"` (pulls in the
# wrapper crate) with `"truce/<format>"` (turns on the matching
# feature on the truce umbrella so `truce::<format>_wrapper` is
# in scope and the macro's per-format expansion arm fires).
[features]
default = {default_features | unescaped}
clap = ["dep:truce-clap", "dep:clap-sys", "truce/clap"]
vst3 = ["dep:truce-vst3", "truce/vst3"]
{{ if has_standalone -}}
standalone = ["dep:truce-standalone"]
{{ endif -}}
shell = ["truce/shell"]

[dependencies]
truce = \{ {dep_args | unescaped} }
truce-gui = \{ {dep_args | unescaped} }
truce-clap = \{ {dep_args | unescaped}, optional = true }
truce-vst3 = \{ {dep_args | unescaped}, optional = true }
{{ if has_standalone -}}
truce-standalone = \{ {dep_args | unescaped}, features = ["gui"], optional = true }
{{ endif -}}
clap-sys = \{ version = "0.5", optional = true }
{{ if is_workspace }}{{ else }}
# Custom profile for `cargo truce install --shell`. The shell-mode
# build (`cargo build --profile shell --features ...,shell`) lands the
# shell binary at `target/shell/lib<crate>.dylib`, independent of
# `target/release/` (where regular `cargo build --release` writes) and
# `target/debug/` (where `cargo build` writes). Inherits release for
# DSP perf parity.
[profile.shell]
inherits = "release"
{{ endif }}