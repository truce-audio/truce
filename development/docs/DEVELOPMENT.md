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
  `cargo install --tag v0.14.2 cargo-truce` recipe, and any
  `tag = "v0.14.2"` pin in a user's `Cargo.toml` resolve to it.
- The **branch** is what users pin via `branch = "preview/0.14"`
  to float patches automatically. Each `0.14.x` patch release
  fast-forwards `preview/0.14` to the new tag.

The branch costs one extra `git push` per release and gives users
the cargo-flavoured upgrade path they expect from semver.

### When to release

- **Patch (`0.14.1` → `0.14.2`):** bug fixes, doc changes, and any
  *additive* API change that doesn't break existing scaffolded
  plugins. Goes onto the existing `preview/0.14` branch.
- **Minor (`0.14.x` → `0.15.0`):** new features that change the
  surface in a forwards-compatible way for new code, or fix a
  bug in a way that requires a recompile (e.g. ABI change in a
  format wrapper). Cuts a new `preview/0.15` branch; the old
  `preview/0.14` branch stays alive for users who haven't migrated.
- **Major (`0.x` → `1.0`):** deferred until the surface settles.

Today truce is on the `0.14.x` train. The current
`preview/{major}.{minor}` branch stays open for **one minor release
after the next** — i.e. when `0.15.0` lands, `preview/0.14` keeps
receiving compat patches until `0.16.0` cuts, then sunsets.

### Cutting a release

Pre-flight from a clean working tree on `main`:

```sh
git checkout main
git pull --ff-only
git status                                   # working tree must be clean
cargo test --workspace                       # full test suite green
cargo clippy --workspace -- -D warnings      # no clippy warnings
```

#### Patch release (most common)

You want `0.14.1` → `0.14.2`, branch `preview/0.14` already exists.
The release commit lands on `main` via PR from `dev/latest`; the
steps below assume `dev/latest` has been merged and `main` carries
the version bump.

```sh
# 1. Bump the version. Two strings in Cargo.toml track the release:
#    `[workspace.package].version` (the source of truth — every
#    member crate inherits this) and the `truce-shim-types` entry in
#    `[workspace.dependencies]` (load-bearing because cargo-truce
#    consumes it via `workspace = true` and ships to crates.io).
sed -i '' 's/"0.14.1"/"0.14.2"/g' Cargo.toml

# 2. Update CHANGELOG
$EDITOR CHANGELOG.md

# 3. Refresh Cargo.lock with the bumped versions
cargo check --workspace

# 4. Open a PR from dev/latest to main with the bump + changelog
#    entry. Land it via squash- or merge-commit per the project
#    convention. (No direct push to main.)

# 5. Once the release commit is on main, tag and fast-forward the
#    release branch.
git checkout main
git pull --ff-only
git tag -a v0.14.2 -m "truce 0.14.2"
git checkout preview/0.14
git merge --ff-only main                     # branch was at v0.14.1; FF to main
git checkout main

# 6. Publish to crates.io (must happen before the push so a failed
#    publish doesn't leave a tag without a matching crates.io
#    artifact).
cargo publish -p truce-shim-types
sleep 30                                     # crates.io index lag
cargo publish -p cargo-truce

# 7. Push branch, release branch, and tag in one go.
git push origin main preview/0.14 v0.14.2
```

If `git merge --ff-only` rejects (release branch has commits not on
`main`), you've drifted — see [Hotfixes](#hotfixes) below.

#### Minor release (new release branch)

You want `0.14.x` → `0.15.0`. Same first three steps, then:

```sh
# 4. Land the version bump on main via PR from dev/latest.

# 5. Cut the new release branch and tag.
git checkout main
git pull --ff-only
git tag -a v0.15.0 -m "truce 0.15.0"
git branch preview/0.15 v0.15.0              # new branch from the tag

# 6. Publish to crates.io.
cargo publish -p truce-shim-types
sleep 30
cargo publish -p cargo-truce

# 7. Push everything.
git push origin main preview/0.15 v0.15.0

# 8. Mark the previous train as sunset-pending in CHANGELOG. The
#    branch keeps receiving 0.14.x patches for one minor cycle
#    (i.e. until 0.16.0 cuts).
```

Don't delete `preview/0.14` — users still have
`branch = "preview/0.14"` in their `Cargo.toml`. Sunset by stopping
new patches, not by removing the ref.

### Hotfixes

The release branch and `main` can diverge when a security or
correctness fix needs to ship before the next normal release on
`main` is ready. Hotfixes branch from the existing release line, are
applied via PR (no direct push), and the version bump + tag happen
on the release branch:

```sh
# 1. Branch off the existing release line for the fix.
git checkout preview/0.14
git pull --ff-only
git checkout -b hotfix/0.14.3-loader-crash

# 2. Apply the minimal fix; resist scope creep — anything beyond the
#    bug should land on dev/latest and wait for the next minor.
$EDITOR crates/truce-loader/...
git commit -am "Fix: loader crash on AAX session reload (#1234)"

# 3. Bump to 0.14.3 on the hotfix branch.
sed -i '' 's/"0.14.2"/"0.14.3"/g' Cargo.toml
cargo check --workspace
git commit -am "Release v0.14.3"

# 4. Open a PR targeting preview/0.14. Land it via merge-commit.

# 5. Tag from preview/0.14 once the merge lands.
git checkout preview/0.14
git pull --ff-only
git tag -a v0.14.3 -m "truce 0.14.3 (hotfix)"

# 6. Backport to dev/latest. Cherry-pick the fix commit (not the
#    version bump — dev/latest is on whatever 0.15.0-dev version
#    it's tracking).
git checkout dev/latest
git pull --ff-only
git cherry-pick <fix-commit-sha>             # not the version bump
git push origin dev/latest

# 7. Publish from the release branch (only re-publish crates whose
#    bytes actually changed).
git checkout preview/0.14
cargo publish -p truce-shim-types --dry-run  # confirm whether re-publish is needed
cargo publish -p cargo-truce
git checkout main

# 8. Push everything.
git push origin preview/0.14 v0.14.3
git branch -d hotfix/0.14.3-loader-crash
```

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

The branch name is hard-coded in `crates/cargo-truce/src/scaffold.rs`
today. When cutting a new minor (`preview/0.15`, `preview/0.16`, …)
the scaffold templates need a parallel bump.

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

### Crates.io publishing

`cargo-truce` is the one crate users `cargo install`, so it lives on
crates.io. The framework crates (`truce`, `truce-gui`, format
wrappers, etc.) stay git-only — they transitively depend on
`baseview`, which is git-only — and scaffolded plugins consume them
via the git ref + release-branch pin documented above.

#### What gets published

| Crate | Why |
|---|---|
| `truce-shim-types` | Direct dep of `cargo-truce`; cargo strips the `path =` half of the workspace dep on publish, so the version must already be on crates.io. |
| `cargo-truce` | The `cargo install cargo-truce` target. |

If a future release adds a new dep to `cargo-truce` that lives in
this repo, it joins the publish list (and must publish before
`cargo-truce`).

#### One-time setup

- Run `cargo login <token>` once per release machine. Token comes
  from <https://crates.io/me> — scope it to "publish-update" plus
  "publish-new" for the first-time publish of `truce-shim-types`.
- The crates.io account must own both crate names. First publish
  claims them.
- `truce-shim-types/Cargo.toml` needs `repository.workspace = true`,
  `homepage.workspace = true`, `categories.workspace = true`, and
  `keywords` set. crates.io rejects the upload without
  license + description + repository.

#### Publish recipe

Runs from the version-bumped commit on `main`. The tag should
already exist locally so a publish failure is recoverable (delete
the local tag, fix, retry — nothing has been pushed yet).

```sh
# Sanity check what each upload will contain. --dry-run is a full
# package + verify pass; catches missing metadata, .gitignore'd
# files, dirty trees, version conflicts.
cargo publish -p truce-shim-types --dry-run
cargo publish -p cargo-truce --dry-run

# Real publish, in dependency order. crates.io's index has up to
# ~30s of CDN lag between accepting a publish and making the new
# version visible to a downstream resolver, so insert a sleep
# between the two — otherwise `cargo publish -p cargo-truce` can
# fail to find the just-published `truce-shim-types`.
cargo publish -p truce-shim-types
sleep 30
cargo publish -p cargo-truce
```

#### Failure modes

- **`error: failed to verify package`** during `cargo publish` —
  almost always a `Cargo.toml` metadata gap (missing `description`,
  `license`, or `repository`). Fix on `main` via PR, retry. The
  local tag has not been pushed; either re-tag or move the tag to
  the amended commit with `git tag -fa vX.Y.Z`.
- **`error: api errors: crate version X.Y.Z is already uploaded`** —
  someone already published this version. Bump to the next patch and
  start again. crates.io versions are immutable; you cannot
  re-publish over them.
- **`error: failed to select a version for ...`** during the
  `cargo-truce` publish — the index hasn't propagated
  `truce-shim-types` yet. Wait 30–60s and retry.
- **Yanking.** If a published `cargo-truce` turns out to be broken,
  `cargo yank --version X.Y.Z -p cargo-truce` hides it from new
  installs without removing it (existing `Cargo.lock` files keep
  resolving). Fix forward with the next patch — never re-publish the
  same version.

### Checklist

Pin this on the wall before any release:

- [ ] PR from `dev/latest` → `main` carries the version bump +
      CHANGELOG entry, and is merged (no direct push to `main`)
- [ ] Working tree clean on `main` after merge
- [ ] `cargo test --workspace` green
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] All three platform CI runs green on the release commit
- [ ] Both version strings in `Cargo.toml` bumped to the same value
      (`[workspace.package].version` and the `truce-shim-types`
      entry in `[workspace.dependencies]`)
- [ ] `Cargo.lock` regenerated (`cargo check --workspace`)
- [ ] Annotated tag created (`git tag -a vX.Y.Z`)
- [ ] Release branch fast-forwarded to the tag
- [ ] `cargo publish -p truce-shim-types --dry-run` clean
- [ ] `cargo publish -p cargo-truce --dry-run` clean
- [ ] `truce-shim-types` published to crates.io
- [ ] `cargo-truce` published to crates.io (after the 30s index wait)
- [ ] `main`, `preview/X.Y`, and `vX.Y.Z` all pushed in one
      `git push` (atomic from the user's perspective — they never
      see a tag without its branch update)
- [ ] GitHub release notes drafted from CHANGELOG
- [ ] `cargo install cargo-truce` smoke-tested from a clean machine
      (or `cargo install --force cargo-truce` locally) so the
      crates.io artifact is verified end-to-end
