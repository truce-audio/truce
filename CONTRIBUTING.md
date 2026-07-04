# Contributing to truce

Patches, bug reports, and feature requests are welcome.

## Code submissions and the dual-license contributor grant

truce ships under the dual license in `LICENSE`:

- The Author License — **Apache License, Version 2.0**
  (`LICENSE-APACHE`) — granted freely to plug-in authors, end-user
  audio software, **and** free, OSI-licensed, non-commercial
  framework / SDK / developer-tool projects built on top of truce
  (the Section 2.1 exemption).
- A Framework License granted only by separate written permission
  from the project maintainers, for **commercial** audio-plug-in
  frameworks built on truce — anything sold, subscription-gated,
  dual-licensed commercially, or bundled into a paid product.

For the dual-license model to work, code you contribute needs to be
licensable on both sides.

**By opening a pull request, issue patch, or other code contribution
to this repository, you agree that:**

1. You wrote the contribution yourself, or you have the legal right
   to submit it under the terms below.

2. Your contribution is licensed to the truce project and to all
   downstream recipients under the **Apache License, Version 2.0**
   (`LICENSE-APACHE`). This is identical to the standard "Inbound =
   Outbound" Apache 2.0 contribution rule per the Apache License
   §5 — your patch flows to users on the same terms as the rest of
   the project.

3. You grant the truce project the additional right to include your
   contribution under any Framework License the project grants under
   Section 2 of `LICENSE`, on whatever terms the project negotiates.

4. You retain copyright in your contribution. This grant does not
   transfer ownership; it grants the project the licensing rights
   needed to make the dual-license model work.

You do not need to sign a separate CLA document — opening the PR is
the agreement. If you can't agree to the above for any reason (an
employer claims rights to the code, you're unsure who owns it, etc.),
note that in the PR and a maintainer will work with you to sort it
out before merging.

If you're contributing on behalf of an employer or another legal
entity, please confirm in the PR that the entity authorizes the
above grant.

## Principles

These principles govern every change, alongside the mechanical gates
in the sections below.

### We do not break userspace

Within a major version, a plugin built against truce `N.x` keeps
compiling and running against every later `N.y`, and its users'
sessions and presets keep loading. "Userspace" is more than the Rust
API: the prelude / `truce::plugin!` / derive surface, the `truce.toml`
schema, the state-envelope wire format and everything plugin identity
derives from (`bundle_id`, `clap_id`, `vst3_id`, the envelope hash),
and host-visible behavior (parameter ids, preset containers, MIDI
wire). Breaks wait for a major version and ship with a migration path
in the changelog - mechanical where possible.

CI (`cli-backcompat.yml`) enforces only the compile-time slice, by
building last-release scaffolds against the PR's commit. Wire-format
and behavioral breaks have no tripwire and rely on review - if a
change touches state envelopes, id derivation, or preset containers,
say so loudly in the PR.

### All or nothing

A new capability lands in every format that can carry it, in the same
change - a CLAP-only feature is a discrepancy truce created, and
plugin authors inherit it invisibly. Real format gaps are fine: bridge
them where a faithful translation exists, document and log a skip
where it doesn't. What is never acceptable is the same truce event or
config meaning different things per format - same scaling, ranges, and
edge cases everywhere, with shared semantics in `truce-core` helpers
rather than re-derived per wrapper (duplicated constants drift). When
prioritizing, all-format work beats another single-format capability.

### Every feature has an example

Every feature lands with an example, and the example is how it gets
manually verified before merge: `cargo truce run` it, play it, hear
it - on top of the usual unit and `driver!` tests, not instead of
them. Examples are canonical reference code that plugin authors copy,
so each must be a small but real plugin demonstrating the feature's
idiomatic shape, not a synthetic stub. If no useful example can be
written, treat that as a design smell and raise it in the PR.

## Code quality

- `cargo clippy --workspace --all-targets -- -D warnings` must be
  clean.
- `cargo fmt --all --check` must be clean.
- `cargo test --workspace --lib` must pass.
- New crates / modules need rustdoc-warning-free `cargo doc
  --workspace --no-deps`.
- Comments explain **why**, not what. Don't reference past audits or
  PRs by name — those rot.
- Don't add error handling, fallbacks, or validation for scenarios
  that can't happen. Trust internal code.

## Testing

CI runs clippy, the unit + integration suite, screenshot pixel-diff
tests, and plugin validation (clap-validator, pluginval) across macOS,
Linux, and Windows; all of it must pass before merge. The notes below
cover what to add or check by hand on top of that.

- **DSP / processing changes** — cover them with `truce_test::driver!`
  scripts (sample-accurate, no host needed). Add to the relevant
  example crate's tests.
- **Layout changes** — regenerate the screenshot baselines with
  `cargo truce screenshot -p <crate> --out <path>`; the screenshot
  tests gate on a strict pixel diff.

### GUI framework changes

Changes to the GUI framework itself — window embedding, resize, focus,
DPI, the editor lifecycle — can't be covered by the screenshot tests,
which pin the built-in editor's pixel output but not its behavior once
embedded in a host. Verify them by loading a plugin in real DAWs,
following the tiered matrix below.

- **Tier 1** — must be tested and verified before merge. A bug found
  here is a blocker.
- **Tier 2** — should be tested and verified, but whether a given bug
  blocks the change is open to debate.
- **Tier 3** — not actively tested. Bugs are accepted as reports and
  fixed opportunistically.

| OS | Tier 1 hosts |
|----|--------------|
| macOS | Ableton Live, Logic Pro, Pro Tools, Cubase, Bitwig, REAPER |
| Windows | Ableton Live, Pro Tools, Cubase, Bitwig, REAPER |
| Linux | Bitwig, REAPER |

Hosts and OSes outside the Tier 1 table fall to Tier 2, except VST2,
which is Tier 3 everywhere — not actively tested.

## Releases

`main` always tracks the latest release. Feature branches and their
PRs don't target `main` directly; they merge into the branch for the
upcoming version, which then opens its own PR into `main` when the
release is cut.
