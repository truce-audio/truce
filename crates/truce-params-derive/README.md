# truce-params-derive

Derive macros for truce parameter structs.

Provides `#[derive(Params)]` and `#[derive(ParamEnum)]` to generate the
boilerplate needed to expose parameter structs to the host. Used internally
by the `truce` crate and re-exported through `truce::prelude`.

Part of [truce](https://github.com/truce-audio/truce).
