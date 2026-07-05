## What and why

<!-- What changes, and the problem it solves. -->

## Userspace impact

<!-- "We do not break userspace." Does this touch the state
envelope, id derivation (bundle_id / clap_id / vst3_id / envelope
hash), preset containers, the truce.toml schema, parameter ids, or
the MIDI wire? CI only catches the compile-time slice, so declare
wire-format and behavioral changes loudly. Breaking changes wait
for a major version and need a migration path in the changelog. -->

- [ ] No userspace impact
- [ ] Userspace impact, described below (with migration path if breaking)

## Format coverage

<!-- "All or nothing." A new capability lands in every format that
can carry it, in the same change: same scaling, ranges, and edge
cases, shared semantics in truce-core helpers. For real format
gaps, note whether each is bridged or documented + logged as a
skip. -->

| Format | Carried / bridged / skipped (why) |
|--------|-----------------------------------|
| CLAP / VST3 / VST2 / LV2 / AU v2 / AU v3 / AAX / standalone | |

## Example

<!-- Every feature lands with an example - a small but real plugin
showing the idiomatic shape, verified by running it: `cargo truce
run`, play it, hear it. If no useful example can be written, that's
a design smell: raise it here. Not applicable for pure fixes. -->

## Manual testing (required)

<!-- Required on every PR. Specs define correct behavior, but
host-specific behavior is unknown and hostile - list what you
loaded in which hosts and what you verified. "No host-facing
changes" is a valid entry when true.

Host-dependent changes (window embedding, resize, focus, DPI,
editor lifecycle, MIDI routing, state/preset recall, transport)
must cover the Tier 1 hosts for the affected OSes:
macOS: Ableton Live, Logic Pro, Pro Tools, Cubase, Bitwig, REAPER
Windows: Ableton Live, Pro Tools, Cubase, Bitwig, REAPER
Linux: Bitwig, REAPER
iOS: AUM -->

## Checklist

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo test --workspace --lib` passes
- [ ] `cargo doc --workspace --no-deps` warning-free (new crates / modules)
- [ ] Screenshot baselines regenerated for layout changes (`cargo truce screenshot -p <crate> --out <path>`)
- [ ] Unit tests / `truce_test::driver!` scripts cover the change
- [ ] Changelog entry (with migration steps if breaking)

<!-- Opening this PR is the contributor grant described in
CONTRIBUTING.md (Apache-2.0 inbound + Framework License grant). If
you can't agree to it, or you're contributing for an employer,
say so here so a maintainer can sort it out before merge. -->

