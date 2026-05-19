[workspace]
resolver = "2"
members = [
{{- for m in members }}
    "{m}",
{{- endfor }}
]

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
truce = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-gui = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-clap = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-vst3 = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
{{ if has_standalone -}}
truce-standalone = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
{{ endif -}}
clap-sys = "0.5"

# Uncomment to opt in. After uncommenting here, add the matching
# feature + optional dep to each plugin's Cargo.toml.
# truce-lv2 = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
# truce-au  = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
# truce-aax = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
#
# VST2 is a legacy format - the Steinberg VST2 SDK was deprecated in
# 2018 and distributing VST2 plugins may require agreement with
# Steinberg's licensing terms. Enable only if you understand the
# implications:
# truce-vst2 = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }

# Custom profile for `cargo truce install --shell`. The shell-mode
# build lands at `target/shell/lib<crate>.dylib`, independent of
# `target/release/` and `target/debug/`. Cargo profiles are workspace-
# level so this entry covers every plugin in the workspace.
[profile.shell]
inherits = "release"
