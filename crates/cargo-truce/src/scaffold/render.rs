//! Template renderer — wraps `tinytemplate` so the rest of the
//! scaffold module talks to a small `Renderer` API instead of the
//! tinytemplate machinery directly.

use serde::Serialize;
use tinytemplate::TinyTemplate;

/// Templates registered by name. The names are referenced by the
/// `Scaffolder` driver; keeping them as constants in one place
/// catches typos at compile time.
pub mod tpl {
    pub const PLUGIN_CARGO_TOML: &str = "plugin/Cargo.toml";
    pub const PLUGIN_BUILD_RS: &str = "plugin/build.rs";
    pub const PLUGIN_LIB_RS: &str = "plugin/src/lib.rs";
    pub const PLUGIN_MAIN_RS: &str = "plugin/src/main.rs";
    pub const PLUGIN_GITIGNORE: &str = "plugin/.gitignore";
    pub const PLUGIN_CARGO_CONFIG: &str = "plugin/.cargo/config.toml";
    pub const WORKSPACE_CARGO_TOML: &str = "workspace/Cargo.toml";
    pub const TRUCE_TOML: &str = "truce.toml";
}

pub struct Renderer {
    tt: TinyTemplate<'static>,
}

impl Renderer {
    pub fn new() -> Self {
        let mut tt = TinyTemplate::new();
        // We're emitting Rust / TOML / shell-style text. Default
        // HTML escaping is wrong for every template we have, so
        // unwire it once.
        tt.set_default_formatter(&tinytemplate::format_unescaped);

        // Register every template by name. `add_template` borrows
        // the &'static str; `include_str!` produces one, so the
        // tinytemplate lifetime stays anchored to the binary's
        // rodata.
        tt.add_template(
            tpl::PLUGIN_CARGO_TOML,
            include_str!("../../templates/scaffold/plugin/Cargo.toml.tpl"),
        )
        .expect("scaffold template parse: plugin/Cargo.toml");
        tt.add_template(
            tpl::PLUGIN_BUILD_RS,
            include_str!("../../templates/scaffold/plugin/build.rs.tpl"),
        )
        .expect("scaffold template parse: plugin/build.rs");
        tt.add_template(
            tpl::PLUGIN_LIB_RS,
            include_str!("../../templates/scaffold/plugin/src/lib.rs.tpl"),
        )
        .expect("scaffold template parse: plugin/src/lib.rs");
        tt.add_template(
            tpl::PLUGIN_MAIN_RS,
            include_str!("../../templates/scaffold/plugin/src/main.rs.tpl"),
        )
        .expect("scaffold template parse: plugin/src/main.rs");
        tt.add_template(
            tpl::PLUGIN_GITIGNORE,
            include_str!("../../templates/scaffold/plugin/.gitignore.tpl"),
        )
        .expect("scaffold template parse: plugin/.gitignore");
        tt.add_template(
            tpl::PLUGIN_CARGO_CONFIG,
            include_str!("../../templates/scaffold/plugin/.cargo/config.toml.tpl"),
        )
        .expect("scaffold template parse: plugin/.cargo/config.toml");
        tt.add_template(
            tpl::WORKSPACE_CARGO_TOML,
            include_str!("../../templates/scaffold/workspace/Cargo.toml.tpl"),
        )
        .expect("scaffold template parse: workspace/Cargo.toml");
        tt.add_template(
            tpl::TRUCE_TOML,
            include_str!("../../templates/scaffold/truce.toml.tpl"),
        )
        .expect("scaffold template parse: truce.toml");

        Self { tt }
    }

    pub fn render<C: Serialize>(&self, name: &str, ctx: &C) -> String {
        self.tt
            .render(name, ctx)
            .unwrap_or_else(|e| panic!("template '{name}' failed to render: {e}"))
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
