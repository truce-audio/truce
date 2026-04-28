# Development

Notes for contributors working on truce itself. End-user documentation
lives in [`docs/reference/`](../../docs/reference/) and the rendered API docs
at <https://truce-audio.github.io/truce/>.

## Workflow rules

**Never push directly to `main`.** Every change reaches `main` through
a pull request. CI must pass on the PR, and at least one review is
expected before merge. `main` is protected — direct pushes will be
rejected.

**`main` is the only long-lived branch.** Feature work lives on
short-lived feature branches (e.g., `feat/your-change`) created off
`main`, PR'd to `main`, and deleted on merge. Release bumps live on
short-lived `bump/vX.Y.Z` branches managed by `bump.sh`. Releases
themselves are tags (`vX.Y.Z`).

```sh
git checkout main
git pull --ff-only
git checkout -b feat/your-change
# ... commits ...
git push -u origin feat/your-change
gh pr create --base main
```

**Use "Rebase and merge" for PRs to `main`.** Keeps `main`'s
history linear — every commit is a discrete, reviewable change.
Squash-merging collapses meaningful commits into one (loses commit
identity); merge-commits add noise. Branch protection on `main`
should be configured to disable squash + merge-commit so the green
PR button only offers rebase.

(This is the only enforced merge style. We've kept rebase-merge
after a recent move from a two-branch model to a single-branch one
— rebase preserves linear history, which makes `git log main` read
as a clean sequence of changes.)

## Building and testing

[![macOS](https://github.com/truce-audio/truce/actions/workflows/ci-macos.yml/badge.svg)](https://github.com/truce-audio/truce/actions/workflows/ci-macos.yml)
[![Windows](https://github.com/truce-audio/truce/actions/workflows/ci-windows.yml/badge.svg)](https://github.com/truce-audio/truce/actions/workflows/ci-windows.yml)
[![Linux](https://github.com/truce-audio/truce/actions/workflows/ci-linux.yml/badge.svg)](https://github.com/truce-audio/truce/actions/workflows/ci-linux.yml)

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The three CI workflows (`ci-macos.yml`, `ci-windows.yml`,
`ci-linux.yml`) run the same checks on every PR. The docs workflow
(`docs.yml`) builds rustdoc with `RUSTDOCFLAGS="-D warnings"` —
broken intra-doc links fail the build.

## Release process

Two scripts under `dev/scripts/`:

- **`bump.sh`** — opens the version-bump PR. Branches off
  `origin/main`, bumps `Cargo.toml`, refreshes `Cargo.lock`, pushes
  on `bump/vX.Y.Z`, opens the PR. Idempotent: re-running with the
  same version resets the bump branch and reuses the open PR.
- **`release.sh`** — runs *after* the bump PR is merged. Reads the
  version from main HEAD, tags, publishes both crates with the
  inter-publish sleep, pushes the tag, creates the GitHub Release.
  **Idempotent at every step** — if a step is already done (tag on
  origin, crate on crates.io, GitHub Release exists), it skips
  ahead. Re-run after a partial failure to resume.

Neither makes judgment calls (semver, changelog prose, hotfix base
commit) — those stay with the maintainer.

### One-time setup

```sh
cargo login <token>             # token from https://crates.io/me
gh auth login                   # GitHub web flow
```

The crates.io token is scoped to "publish-update" (and "publish-new"
until both crates have shipped their first version). Verify with
`cat ~/.cargo/credentials.toml | grep token` and `gh auth status`.

### Patch / minor / major release

```sh
# 1. Bump.
./dev/scripts/bump.sh patch       # or `minor` or `major`

# 2. Review + merge the opened PR via the GitHub UI. Diff should be
#    limited to the two version strings in Cargo.toml + Cargo.lock
#    entries — reject anything else. Merge using "Rebase and merge"
#    (the only style allowed by branch protection on `main`).

# 3. Publish.
git checkout main && git pull --ff-only
./dev/scripts/release.sh

# 4. Smoke-test from a clean install.
cargo install --force cargo-truce --version <X.Y.Z>
cargo truce --help
```

### Pre-release builds

For RCs (`v1.0.0-rc.1`, etc.), pass the explicit version to
`bump.sh`:

```sh
./dev/scripts/bump.sh 1.0.0-rc.1
# review, merge, ./dev/scripts/release.sh — ships v1.0.0-rc.1
./dev/scripts/bump.sh 1.0.0-rc.2
# ...
./dev/scripts/bump.sh 1.0.0       # finalize
```

Cargo handles SemVer pre-release semantics — `tag = "v1.0.0-rc.1"`
resolves to that specific pre-release, and downstream consumers
don't get pre-releases via `version = "1.0"` requirements unless
they opt in.

### Hotfix release

Same flow as a patch release, with a different starting commit.
Branch off the tag you want to fix, apply the fix, bump version
manually (since `bump.sh` would compute from `main`'s current
version, not the older tag), PR, merge, then `release.sh`.

```sh
# 1. Branch off the tag.
git checkout -b hotfix/0.15.4-loader-crash v0.15.3

# 2. Apply the fix and bump.
$EDITOR crates/truce-loader/...
git commit -am "Fix: loader crash on AAX session reload"
sed -i '' 's/"0.15.3"/"0.15.4"/g' Cargo.toml
cargo check --workspace
git commit -am "Release v0.15.4"
git push -u origin hotfix/0.15.4-loader-crash

# 3. Open the PR. Base depends on whether the fix should also flow
#    forward to current main:
#       --base main             — typical (fix flows forward)
#       --base v0.15.3          — back-train only (rare; tags as base
#                                 require enabling on the repo)
gh pr create --base main --title "Hotfix v0.15.4"

# 4. Review + merge.

# 5. release.sh from main.
git checkout main && git pull --ff-only
./dev/scripts/release.sh

# 6. Clean up.
git push origin --delete hotfix/0.15.4-loader-crash
```

### When release.sh fails partway

`release.sh` is idempotent — **re-run it.** Each step checks if it's
already done (tag on origin, crate on crates.io, GitHub Release
exists) and skips ahead. Almost any partial-failure recovery is just
re-running the script.

The exceptions:

- **Cargo.toml metadata gap surfaces during `cargo publish`** — fix
  on a feature branch + PR + merge, then delete the local tag
  (`git tag -d vX.Y.Z`) before re-running release.sh so it re-tags
  HEAD with the fix.
- **`cargo install` fails post-publish** — yank the broken version
  (`cargo yank -p cargo-truce --version X.Y.Z`) and bump-and-release
  again. crates.io versions are immutable.

### What gets published to crates.io

`cargo-truce` is the one crate users `cargo install`, so it lives on
crates.io. Framework crates (`truce`, `truce-gui`, format wrappers,
etc.) stay git-only — they transitively depend on `baseview`, which
is git-only — and scaffolded plugins consume them via tag pins.

| Crate | Why on crates.io |
|---|---|
| `truce-shim-types` | Direct dep of `cargo-truce`; cargo strips the `path =` half of the workspace dep on publish, so the version must already be on crates.io. |
| `cargo-truce` | The `cargo install cargo-truce` target. |

If a future release adds a new dep to `cargo-truce` that lives in
this repo, it joins the publish list (and must publish before
`cargo-truce`). `release.sh` would need a parallel addition.

### Maintainer responsibilities

Things the scripts don't decide:

- [ ] Patch vs. minor vs. major — semver judgment about whether the
      change is additive, breaking, or a rewrite
- [ ] CHANGELOG entry written before bump (the bump PR carries it)
- [ ] CI green on all three platforms + docs before merging the
      bump PR
- [ ] For hotfixes: which tag / commit to branch from
- [ ] After release: GitHub Release auto-generated notes lightly
      edited if anything significant got buried in the PR list

What `bump.sh` checks: clean working tree, version strings in
`Cargo.toml` correctly bumped together, `Cargo.lock` refreshed,
branch + PR opened (or surfaces the existing PR if one's open).

What `release.sh` checks: version drift between the two `Cargo.toml`
strings, idempotent skip for tag (already pushed), publishes
(already on crates.io), and Release (already exists). Inter-publish
30s sleep only if shim-types was actually just published.

### What scaffolded plugins resolve to

`cargo truce new` emits the **tag** form, pinned to the current
patch:

```toml
truce = { git = "https://github.com/truce-audio/truce", tag = "v0.16.1" }
```

The tag pin is reproducible by default — a fresh `cargo build`
resolves to the exact same SHAs every time. Users wanting to upgrade
edit the tag string by hand (or run a future `cargo truce upgrade`
helper).

The tag is derived at scaffold time from `cargo-truce`'s version
(which inherits from `[workspace.package].version`). When
`cargo-truce` is rebuilt after a workspace bump, new scaffolds emit
the new tag — no parallel edit to `scaffold.rs` required.

| Pin form (in user's `Cargo.toml`) | Resolves to |
|---|---|
| `git = "https://github.com/truce-audio/truce"` | latest commit on `main` (no pin — every `cargo update` moves) |
| `git = "...", tag = "vX.Y.Z"` | exact tag, immutable (**scaffold default**) |
| `git = "...", rev = "<sha>"` | exact commit, immutable |

Older scaffolds that pinned to historical train branches keep
resolving — those refs still exist in the repo's history — but
they're frozen at the simpler-model cutover point. Existing
plugins should migrate by editing their `Cargo.toml` to a
`tag = "vX.Y.Z"` pin.

### Tag hygiene

- **Annotated tags only** (`git tag -a`), never lightweight.
- **Never force-move a tag.** Once `vX.Y.Z` is pushed it's
  immutable. If a release is broken, cut `vX.Y.(Z+1)` with the fix.
- **Sign tags** (`git tag -s`) once a release-signing key is set up.
  Not blocking pre-1.0.
