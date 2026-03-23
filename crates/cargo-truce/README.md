# cargo-truce

Build tool for truce audio plugins.

A cargo subcommand that wraps `truce-xtask` to provide a convenient CLI for
scaffolding, building, bundling, signing, installing, and validating plugins.

## Installation

```sh
cargo install --git https://github.com/truce-audio/truce cargo-truce
```

## Usage

```sh
cargo truce new my-plugin        # scaffold a new plugin project
cargo truce install              # build + bundle + sign + install
cargo truce install --clap       # single format
cargo truce validate             # run auval + pluginval
cargo truce doctor               # check environment
```
