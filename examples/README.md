# Examples

Plugin examples for the truce framework. Most live here. Two GUI
backends carry their own examples inside their respective Cargo
sub-workspaces so that backend's pinned deps (skia-bindings for
slint, baseview for vizia) don't infect the parent lockfile:

- slint backend: [`crates/truce-slint/examples/`](../crates/truce-slint/examples/)
  - `truce-example-gain-slint`
  - `truce-example-gui-zoo-slint`
- vizia backend: [`crates/truce-vizia/examples/`](../crates/truce-vizia/examples/)
  - `truce-example-gain-vizia`
  - `truce-example-gui-zoo-vizia`

Run `cargo build`, `cargo test`, and `cargo truce screenshot` from
inside the relevant sub-workspace directory for slint / vizia
examples; from the workspace root for everything else.

Full docs: <https://truce.audio/docs/examples>
