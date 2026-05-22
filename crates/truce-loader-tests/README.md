# truce-loader-tests

Integration-test host for `truce-loader`. Internal to the truce
workspace — `publish = false`, never goes to crates.io.

## Why this crate exists

The integration tests for `truce-loader` need
`#[derive(Params)]` / `#[derive(State)]`, which expand to
`::truce::params::*` / `::truce::core::*` paths — so the tests
need the umbrella `truce` crate in scope. But `truce` already
depends on `truce-loader` (for the `truce::plugin!` macro's
HotShell wiring), and adding `truce` as a `[dev-dependencies]`
entry on `truce-loader` would form a `truce <-> truce-loader`
cycle in cargo metadata.

Holding the tests in this separate crate breaks the loop: the
edges run `truce-loader-tests -> truce-loader` and
`truce-loader-tests -> truce`, with no back-edge into either.

Part of [truce](https://github.com/truce-audio/truce).
