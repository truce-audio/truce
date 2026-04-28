# Development

Notes for contributors working on truce itself. End-user documentation
lives in [`docs/reference/`](../../docs/reference/) and the rendered API docs
at <https://truce-audio.github.io/truce/>.

## Workflow rules

**Never push directly to `main`.** Every change reaches `main` through
a pull request. CI must pass on the PR, and at least one review is
expected before merge. `main` is protected — direct pushes will be
rejected.

**The active development branch is `dev/latest`.** Branch your work
off `dev/latest`, not `main`. PRs target `dev/latest`. Periodically,
`dev/latest` is merged into `main` as part of the release process
(see below). This keeps `main` close to the latest tagged release —
users who scaffold new plugins should be able to track `main` without
seeing in-progress work.

```sh
git checkout dev/latest
git pull --ff-only
git checkout -b feat/your-change
# ... commits ...
git push -u origin feat/your-change
gh pr create --base dev/latest
```

## Building and testing

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

How to cut a `truce` release that scaffolded plugins can pin to.
Every release is **both** a Git tag (`v{major}.{minor}.{patch}`) and
a release branch (`preview/{major}.{minor}` while pre-1.0;
`release/{major}.{minor}` post-1.0):

- The **tag** is the immutable snapshot. CI artifacts, the
  `cargo install --tag v0.15.0 cargo-truce` recipe, and any
  `tag = "v0.15.0"` pin in a user's `Cargo.toml` resolve to it.
- The **branch** is what users pin via `branch = "preview/0.15"`
  to float patches automatically. Each `0.15.x` patch release
  fast-forwards `preview/0.15` to the new tag.

The branch costs one extra `git push` per release and gives users
the cargo-flavoured upgrade path they expect from semver.

### When to release

- **Patch (`0.15.0` → `0.15.1`):** bug fixes, doc changes, and any
  *additive* API change that doesn't break existing scaffolded
  plugins. Goes onto the existing `preview/0.15` branch.
- **Minor (`0.15.x` → `0.16.0`):** new features that change the
  surface in a forwards-compatible way for new code, or fix a
  bug in a way that requires a recompile (e.g. ABI change in a
  format wrapper). Cuts a new `preview/0.16` branch; the old
  `preview/0.15` branch stays alive for users who haven't migrated.
- **Major (`0.x` → `1.0`):** deferred until the surface settles.

Today truce is on the `0.15.x` train. The current
`preview/{major}.{minor}` branch stays open for **one minor release
after the next** — i.e. when `0.16.0` lands, `preview/0.15` keeps
receiving compat patches until `0.17.0` cuts, then sunsets.
`preview/0.14` is now sunset-pending: it keeps receiving any
remaining `0.14.x` hotfixes until `0.16.0` ships, then no more.

### One-time setup

```sh
cargo login <token>             # token from https://crates.io/me
gh auth login                   # GitHub web flow
```

The crates.io token is scoped to "publish-update" (and "publish-new"
until both crates have shipped their first version). Both
credentials live on the maintainer's machine; verify with
`cat ~/.cargo/credentials.toml | grep token` and `gh auth status`.

### Cutting a release

Two scripts under `development/scripts/`:

- **`bump.sh`** — opens the version-bump PR from `dev/latest` to
  `main`. Runs locally; pre-flight asserts you're on `dev/latest`
  with a clean tree.
- **`release.sh`** — runs *after* the bump PR is merged. Tags from
  `main`, publishes both crates to crates.io with the inter-publish
  index-propagation sleep, fast-forwards the preview branch, pushes
  everything, and creates the GitHub Release.

Neither script makes any judgment calls (semver, changelog prose,
etc.) — those stay with the maintainer.

#### Patch release (most common: 0.15.0 → 0.15.1)

```sh
# 1. Bump.
git checkout dev/latest && git pull --ff-only
./development/scripts/bump.sh patch

# 2. Review + merge the opened PR via the GitHub UI. CI runs across
#    all three platforms + docs. Diff should be limited to the two
#    version strings in Cargo.toml (workspace.package + the
#    truce-shim-types workspace dep) and the matching Cargo.lock
#    entries — reject anything else.

# 3. Publish.
git checkout main && git pull --ff-only
./development/scripts/release.sh

# 4. Smoke-test from a clean install.
cargo install --force cargo-truce --version 0.15.1
cargo truce --help
```

Total wall-clock: ~5 minutes of maintainer attention spread over
~30 minutes (CI runs dominate).

#### Minor release (new train: 0.15.x → 0.16.0)

Same flow, with one extra step between merge and `release.sh`: cut
the new preview branch from `main`, since `release.sh` assumes the
branch already exists.

```sh
# 1. Bump.
./development/scripts/bump.sh minor

# 2. Review + merge.

# 3. Cut the new preview branch BEFORE release.sh.
git checkout main && git pull --ff-only
git branch preview/0.16 main          # main HEAD = v0.16.0 commit

# 4. Publish.
./development/scripts/release.sh

# 5. Mark the previous train as sunset-pending in CHANGELOG. The
#    branch keeps receiving 0.15.x patches for one minor cycle
#    (i.e. until 0.17.0 cuts).
```

Don't delete `preview/0.15` — users still have
`branch = "preview/0.15"` in their `Cargo.toml`. Sunset by stopping
new patches, not by removing the ref.

### Hotfixes

`bump.sh`'s pre-flight rejects anything other than `dev/latest`, so
hotfixes are still manual. Branches from the existing release line,
applied via PR, version bump + tag happen on the release branch:

Example: shipping a fix on the now-sunset-pending `preview/0.14`
train (last release was `0.14.3`):

```sh
# 1. Branch off the release line for the fix.
git checkout preview/0.14 && git pull --ff-only
git checkout -b hotfix/0.14.4-loader-crash

# 2. Apply the minimal fix; resist scope creep — anything beyond the
#    bug should land on dev/latest and wait for the next minor.
$EDITOR crates/truce-loader/...
git commit -am "Fix: loader crash on AAX session reload (#1234)"

# 3. Bump (single sed, same shape bump.sh would use).
sed -i '' 's/"0.14.3"/"0.14.4"/g' Cargo.toml
cargo check --workspace
git commit -am "Release v0.14.4"
git push -u origin hotfix/0.14.4-loader-crash

# 4. Open PR targeting preview/0.14. Review + merge via merge-commit.
gh pr create --base preview/0.14 --title "Hotfix v0.14.4"

# 5. After merge, run release.sh from preview/0.14.
git checkout preview/0.14 && git pull --ff-only
./development/scripts/release.sh      # tags + publishes + FFs

# 6. Backport the fix commit (NOT the version bump) to dev/latest.
git checkout dev/latest && git pull --ff-only
git cherry-pick <fix-commit-sha>      # not the "Release v0.14.4" commit
git push origin dev/latest

# 7. Clean up the hotfix branch.
git branch -d hotfix/0.14.4-loader-crash
git push origin --delete hotfix/0.14.4-loader-crash
```

`release.sh` reads the version from `Cargo.toml` and derives the
train name from it, so running it from `preview/0.14` correctly tags
`v0.14.4` and pushes to `preview/0.14`. The script's first two lines
(`git checkout main && git pull --ff-only`) need to be skipped or
the script needs a one-line edit for the hotfix — accept that
friction or write a `hotfix.sh` variant once you've shipped one and
felt it.

### When release.sh fails partway

The script is linear, not idempotent. Recovery depends on which step
failed:

- **Before any `cargo publish`** — nothing irreversible happened.
  Delete the local tag (`git tag -d vX.Y.Z`), fix, re-run.
- **After `truce-shim-types` publish, before `cargo-truce` publish** —
  shim-types is now permanent on crates.io at this version. Either
  retry just the cargo-truce publish (typical: index-lag failure;
  wait a minute and re-run `cargo publish -p cargo-truce`), or yank
  shim-types and bump to a new patch end-to-end.
- **After both publishes, before push** — no user-visible state yet
  (no tag pushed, no preview-branch FF, no GitHub Release). Run the
  remaining steps manually: `git push origin main preview/X.Y vX.Y.Z`
  then `gh release create vX.Y.Z --generate-notes`.
- **`cargo install` fails post-publish** — yank
  (`cargo yank -p cargo-truce --version X.Y.Z`) and bump-and-release
  again. crates.io versions are immutable.

### What scaffolded plugins resolve to

After the `git push`, a user's `Cargo.toml` resolves as:

| Pin form (in user's `Cargo.toml`) | Resolves to |
|---|---|
| `git = "https://github.com/truce-audio/truce"` | latest commit on `main` (no pin — every `cargo update` moves) |
| `git = "...", tag = "v0.14.2"` | exact tag, immutable |
| `git = "...", rev = "<sha>"` | exact commit, immutable |
| `git = "...", branch = "preview/0.14"` | latest patch in the `0.14.x` train (auto-tracks `0.14.3`, `0.14.4`, …; stops at `0.15`) |

`cargo truce new` emits the **train branch** form:

```toml
truce = { git = "https://github.com/truce-audio/truce", branch = "preview/0.14" }
```

This auto-tracks patch releases on the train the user scaffolded
against and stops at the next minor — the lowest-friction upgrade
path that's still bounded by semver. Users who want bit-for-bit
reproducibility can pin to a tag manually after scaffolding.

The branch name is derived at scaffold time from `cargo-truce`'s
version (which inherits from `[workspace.package].version`). When
the workspace bumps from 0.15.x to 0.16.0, scaffolds automatically
emit `branch = "preview/0.16"` from the next compiled `cargo-truce`
binary onward — no parallel edit to `scaffold.rs` required.

### Tag hygiene

- **Annotated tags only** (`git tag -a`), never lightweight. Annotated
  tags carry a tagger identity, date, and message; they show up in
  `git describe` and GitHub's release UI; they survive
  `git push --tags`. Lightweight tags don't.
- **Never force-move a tag.** Once `v0.14.2` is pushed it's
  immutable. If the release is broken, cut `v0.14.3` with the fix —
  forcing a tag breaks every user who already pinned to it.
- **Sign tags** (`git tag -s`) once a release-signing key is set up.
  Not blocking pre-1.0.

### Branch hygiene

- **Fast-forward only.** The release branch is a moving pointer at
  the *latest patch tag* on its train. `git merge --ff-only` is the
  invariant; a merge that wouldn't fast-forward indicates drift
  (handled by the hotfix workflow above), not a code-review situation
  to resolve on the branch.
- **Never delete a release branch.** Once a user pins to it, the
  ref is part of the project's public API. Sunsetting means stopping
  pushes, not removing the branch.
- **Don't squash-merge into release branches.** Always preserve the
  exact tagged commit so `git log preview/0.14 --first-parent` reads
  as a clean list of releases.

### What gets published to crates.io

`cargo-truce` is the one crate users `cargo install`, so it lives on
crates.io. The framework crates (`truce`, `truce-gui`, format
wrappers, etc.) stay git-only — they transitively depend on
`baseview`, which is git-only — and scaffolded plugins consume them
via the git ref + release-branch pin documented above.

| Crate | Why on crates.io |
|---|---|
| `truce-shim-types` | Direct dep of `cargo-truce`; cargo strips the `path =` half of the workspace dep on publish, so the version must already be on crates.io. |
| `cargo-truce` | The `cargo install cargo-truce` target. |

If a future release adds a new dep to `cargo-truce` that lives in
this repo, it joins the publish list (and must publish before
`cargo-truce`). `release.sh` would need a parallel addition.

### Maintainer responsibilities

Things the scripts don't decide:

- [ ] Patch vs. minor — semver judgment about whether the change is
      additive on the existing train or breaking enough to need a
      new train
- [ ] CHANGELOG entry written before bump (the bump PR carries it)
- [ ] CI green on all three platforms + docs before merging the
      bump PR
- [ ] For minor releases: new `preview/X.Y` branch cut from `main`
      *before* `release.sh`
- [ ] For minor releases: previous train marked sunset-pending in
      CHANGELOG
- [ ] For hotfixes: fix commit cherry-picked back to `dev/latest`
      (without it, the next minor regresses the fix)
- [ ] After release: `cargo install --force cargo-truce` smoke-tested
- [ ] After release: GitHub Release auto-generated notes lightly
      edited if anything significant got buried in the PR list

What `bump.sh` checks: clean tree, on `dev/latest`, remote up to
date, version strings in `Cargo.toml` correctly bumped together,
`Cargo.lock` refreshed, branch + PR opened.

What `release.sh` checks: on `main`, version drift between the two
`Cargo.toml` strings, no pre-existing local tag with this version,
`truce-shim-types --dry-run` (catches metadata gaps before the
immutable upload), inter-publish 30s sleep, FF preview branch only
after both publishes succeed.
