//! End-to-end scaffold tests.
//!
//! Each user-visible `cargo truce new` / `cargo truce new-workspace`
//! permutation runs through an actual `cargo check`. Catches
//! cross-file scaffold bugs (workspace-dep vs plugin-dep mismatches,
//! missing `[build-dependencies]`, stale feature-flag lists, etc.)
//! that unit tests can't see because they only exercise one template
//! at a time.
//!
//! See `truce-docs/docs/internal/scaffold-e2e.md` for the design.
//!
//! Run: `cargo test -p cargo-truce --test scaffold_e2e`
//!
//! Tests self-serialize at the build step via a process-level mutex,
//! so `--test-threads=1` is optional — scaffolding and rewriting run
//! concurrently, only `cargo check` is single-threaded to share the
//! target dir safely.
//!
//! Most tests run `cargo check` only — ~7s warm. `workspace_full_build`
//! does a full `cargo build` on a multi-plugin workspace to catch
//! link-time regressions (format wrapper symbol exports, cdylib link
//! args, etc.) that `check` skips — ~60s cold, ~5-10s warm. Run just
//! that one when iterating on link-related code:
//!
//! ```
//! cargo test -p cargo-truce --test scaffold_e2e workspace_full_build
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `<truce-repo-root>/` — this crate's manifest lives at
/// `<truce>/crates/cargo-truce/Cargo.toml`, so two parents up is the
/// workspace root.
fn truce_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("CARGO_MANIFEST_DIR should be <truce>/crates/cargo-truce")
            .to_path_buf()
    })
}

fn cargo_truce_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-truce"))
}

/// Shared cargo target dir across every e2e test in this run. First
/// test compiles truce-* from source (~30s cold); subsequent tests
/// reuse the artifacts (~1–5s each).
fn shared_target() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let p = std::env::temp_dir().join("truce-scaffold-e2e-target");
        std::fs::create_dir_all(&p).unwrap();
        p
    })
}

/// Global mutex around `cargo check`. Scaffolding + rewriting can
/// parallelize (each test has its own scratch dir); only the
/// cache-sharing build step needs serialization.
fn build_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Unique per-test scratch dir. `pid + atomic counter` keeps parallel
/// test runs (and repeated local runs) from stepping on each other.
fn fresh_tempdir(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("truce-scaffold-e2e-{pid}-{n}-{label}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ---------------------------------------------------------------------------
// Scaffold harness
// ---------------------------------------------------------------------------

struct Scaffold {
    label: String,
    /// Directory the scaffold command runs inside. The generated
    /// project lands as a subdirectory of this.
    run_dir: PathBuf,
    /// `cargo-truce` subcommand + args.
    args: Vec<String>,
    /// Where the generated project lands after `run()`.
    generated: PathBuf,
}

impl Scaffold {
    /// `cargo truce new <name>` → generates `<tmp>/<name>/`.
    fn new(label: &str, name: &str) -> Self {
        let run_dir = fresh_tempdir(label);
        let generated = run_dir.join(name);
        Self {
            label: label.into(),
            run_dir,
            args: vec!["new".into(), name.into()],
            generated,
        }
    }

    /// `cargo truce new-workspace <ws> <p1> [pN..]` → generates
    /// `<tmp>/<ws>/` with one plugin crate per plugin name.
    fn new_workspace(label: &str, ws: &str, plugins: &[&str]) -> Self {
        let run_dir = fresh_tempdir(label);
        let generated = run_dir.join(ws);
        let mut args = vec!["new-workspace".into(), ws.into()];
        args.extend(plugins.iter().map(|s| s.to_string()));
        Self {
            label: label.into(),
            run_dir,
            args,
            generated,
        }
    }

    fn arg(mut self, s: &str) -> Self {
        self.args.push(s.into());
        self
    }

    fn run(&self) -> Result<(), String> {
        let out = Command::new(cargo_truce_bin())
            .args(&self.args)
            .current_dir(&self.run_dir)
            .output()
            .map_err(|e| format!("[{}] exec cargo-truce: {e}", self.label))?;
        if !out.status.success() {
            return Err(format!(
                "[{}] cargo-truce {} failed: {}\nstdout: {}\nstderr: {}",
                self.label,
                self.args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ));
        }
        if !self.generated.is_dir() {
            return Err(format!(
                "[{}] scaffold succeeded but {} is missing",
                self.label,
                self.generated.display()
            ));
        }
        Ok(())
    }

    /// Walk the generated tree and rewrite every
    /// `{ git = "https://github.com/truce-audio/truce", ... }` to
    /// `{ path = "<truce-root>/crates/<name>", ... }`. Keeps `cargo
    /// check` off the network.
    fn rewrite_git_to_path(&self) -> Result<(), String> {
        // Inject path deps with forward slashes even on Windows — TOML
        // basic strings (`path = "..."`) treat backslash as an escape
        // introducer, so a native `D:\a\truce\...` path would break
        // toml parsing with `missing escaped value`. Cargo accepts
        // forward slashes for path deps on Windows.
        let crates_dir = truce_root()
            .join("crates")
            .to_string_lossy()
            .replace('\\', "/");

        let mut files = Vec::new();
        walk_cargo_toml(&self.generated, &mut files);
        for f in files {
            let content =
                std::fs::read_to_string(&f).map_err(|e| format!("read {}: {e}", f.display()))?;
            let rewritten = rewrite_git_refs(&content, &crates_dir);
            if rewritten != content {
                std::fs::write(&f, rewritten).map_err(|e| format!("write {}: {e}", f.display()))?;
            }
        }
        Ok(())
    }

    fn cargo_check(&self) -> Result<(), String> {
        self.run_cargo("check")
    }

    fn cargo_build(&self) -> Result<(), String> {
        self.run_cargo("build")
    }

    /// Run `cargo test --workspace` against the scaffolded project.
    /// Compiles AND executes every `#[test]` block in the templates,
    /// so a broken default test rendered by scaffolding fails here.
    fn cargo_test(&self) -> Result<(), String> {
        self.run_cargo("test")
    }

    /// Shared body for `cargo check` / `cargo build` / `cargo test`.
    /// All hold `build_lock` (cache safety) and share the target dir.
    fn run_cargo(&self, subcommand: &str) -> Result<(), String> {
        let _guard = build_lock().lock().unwrap_or_else(|e| e.into_inner());
        let out = Command::new("cargo")
            .arg(subcommand)
            .arg("--workspace")
            // Explicit target dir so our shared cache survives across
            // tests and across test runs.
            .env("CARGO_TARGET_DIR", shared_target())
            .current_dir(&self.generated)
            .output()
            .map_err(|e| format!("[{}] exec cargo {subcommand}: {e}", self.label))?;
        if !out.status.success() {
            return Err(format!(
                "[{}] cargo {subcommand} failed: {}\nstdout:\n{}\nstderr:\n{}",
                self.label,
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ));
        }
        Ok(())
    }

    /// Run a `cargo truce <subcommand>` invocation against the
    /// scaffolded project. Exercises the actual `cargo-truce` binary
    /// (not just bare `cargo build`), so xtask-side regressions —
    /// bundle staging, per-format feature gating, project_root
    /// resolution from a child cwd — surface here.
    ///
    /// Sets `CARGO_TARGET_DIR` to the shared cache so artifacts
    /// land alongside the other tests' `cargo check` / `cargo build`
    /// outputs. xtask honors this env var for both inner cargo
    /// invocations and its own staging-path resolution.
    fn truce_subcommand(&self, args: &[&str]) -> Result<(), String> {
        let _guard = build_lock().lock().unwrap_or_else(|e| e.into_inner());
        // Wipe the staged-bundles dir so this test's assertions don't
        // pick up artifacts from earlier `truce_subcommand` runs.
        // Cargo build artifacts under `release/` survive — that's the
        // whole point of sharing the target dir.
        let _ = std::fs::remove_dir_all(shared_target().join("bundles"));
        let out = Command::new(cargo_truce_bin())
            .args(args)
            .env("CARGO_TARGET_DIR", shared_target())
            .current_dir(&self.generated)
            .output()
            .map_err(|e| format!("[{}] exec cargo-truce {args:?}: {e}", self.label))?;
        if !out.status.success() {
            return Err(format!(
                "[{}] cargo-truce {args:?} failed: {}\nstdout:\n{}\nstderr:\n{}",
                self.label,
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ));
        }
        // Scaffolded builds should be warning-clean. Templates that
        // accumulate `warning:` / `error:` lines in cargo output (rustc
        // warnings, unused-manifest-key, deprecated APIs) get caught
        // here instead of festering until a user files an issue.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let diagnostics = scan_for_diagnostics(&stdout, &stderr);
        if !diagnostics.is_empty() {
            return Err(format!(
                "[{}] cargo-truce {args:?} succeeded but emitted {} diagnostic(s):\n{}",
                self.label,
                diagnostics.len(),
                diagnostics.join("\n"),
            ));
        }
        // Echo cargo-truce output via eprintln so cargo test captures
        // it for the panic dump if a downstream assertion fails. No
        // output unless the test ultimately fails.
        eprintln!(
            "[{}] cargo-truce {args:?} succeeded.\nstdout:\n{stdout}\nstderr:\n{stderr}",
            self.label
        );
        Ok(())
    }

    /// Assert that `<shared-target>/bundles/` holds exactly `expected`
    /// entries whose name ends with `ext` (e.g. `.clap`, `.vst3`).
    /// Stronger end-to-end check than "cargo-truce exited 0" —
    /// catches silent staging regressions (e.g. format-flag honored at
    /// build time but bundle never materialized).
    ///
    /// Reads from the shared target dir because `truce_subcommand`
    /// runs with `CARGO_TARGET_DIR` pointed there.
    fn assert_bundle_count_by_ext(&self, ext: &str, expected: usize) {
        let bundles = shared_target().join("bundles");
        let names: Vec<String> = std::fs::read_dir(&bundles)
            .unwrap_or_else(|e| {
                panic!(
                    "[{}] target/bundles missing at {}: {e}\n{}",
                    self.label,
                    bundles.display(),
                    diagnose_target_layout(),
                )
            })
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        let count = names.iter().filter(|n| n.ends_with(ext)).count();
        assert_eq!(
            count,
            expected,
            "[{}] expected {expected} {ext} bundle(s) in {}, got: {names:?}\n{}",
            self.label,
            bundles.display(),
            diagnose_target_layout(),
        );
    }
}

/// Snapshot the shared target tree (top-level + `release/` + `debug/`
/// + `bundles/`) for inclusion in failure messages. Helps pinpoint
/// whether the build wrote to a different profile dir, the staging
/// step skipped, or cargo silently produced no output.
fn diagnose_target_layout() -> String {
    let target = shared_target();
    let mut out = format!("--- shared target dir: {} ---\n", target.display());
    for sub in ["", "release", "debug", "bundles"] {
        let dir = if sub.is_empty() {
            target.to_path_buf()
        } else {
            target.join(sub)
        };
        let label = if sub.is_empty() { "<root>" } else { sub };
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                let mut names: Vec<_> = entries
                    .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                    .collect();
                names.sort();
                out.push_str(&format!("  {label}/ ({} entries):\n", names.len()));
                for n in names.iter().take(40) {
                    out.push_str(&format!("    {n}\n"));
                }
                if names.len() > 40 {
                    out.push_str(&format!("    ... and {} more\n", names.len() - 40));
                }
            }
            Err(e) => {
                out.push_str(&format!("  {label}/: <not readable: {e}>\n"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Diagnostic scan
// ---------------------------------------------------------------------------

/// Pluck `warning:` / `error:` lines out of combined cargo output.
///
/// Matches the prefix at the start of a line (after optional ANSI color
/// codes and whitespace). Skips three benign cases:
///
/// - "Compiling …": not a diagnostic, just cargo progress.
/// - rustc's "warnings emitted" / "X warnings emitted" summary lines —
///   redundant with the underlying warnings we're already capturing.
/// - "warning: build failed, waiting for other jobs to finish…":
///   cargo's job-cancellation noise, not a real diagnostic.
fn scan_for_diagnostics(stdout: &str, stderr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for stream in [stdout, stderr] {
        for line in stream.lines() {
            // Strip ANSI color escapes that cargo emits.
            let stripped = strip_ansi(line);
            let trimmed = stripped.trim_start();
            let is_warning = trimmed.starts_with("warning:");
            let is_error = trimmed.starts_with("error:");
            if !(is_warning || is_error) {
                continue;
            }
            if trimmed.contains("warnings emitted")
                || trimmed.contains("warning emitted")
                || trimmed.contains("build failed, waiting for other jobs")
            {
                continue;
            }
            out.push(stripped.into_owned());
        }
    }
    out
}

fn strip_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\x1b') {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC [ ... <letter>
        if chars.next() != Some('[') {
            continue;
        }
        for inner in chars.by_ref() {
            if inner.is_ascii_alphabetic() {
                break;
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Git → path rewrite
// ---------------------------------------------------------------------------

fn walk_cargo_toml(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_cargo_toml(&p, out);
            } else if p.file_name().map(|n| n == "Cargo.toml").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

/// Line-based rewrite:
///
/// ```text
/// <key> = { git = "https://github.com/truce-audio/truce"[, ...] }
///                           ↓
/// <key> = { path = "<crates>/<key>"[, ...] }
/// ```
///
/// Skips commented-out lines (so the workspace `[workspace.dependencies]`
/// block's commented "Uncomment to opt in" entries pass through
/// unchanged). Scaffolded Cargo.tomls always use the single-line form,
/// so a regex-less line scan suffices.
fn rewrite_git_refs(content: &str, crates_dir: &str) -> String {
    const NEEDLE: &str = r#"{ git = "https://github.com/truce-audio/truce""#;
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || !line.contains(NEEDLE) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // Extract the key (the `truce-foo` in `truce-foo = { git = ... }`).
        let eq_idx = match line.find('=') {
            Some(i) => i,
            None => {
                out.push_str(line);
                out.push('\n');
                continue;
            }
        };
        let key = line[..eq_idx].trim();
        let replacement = format!(r#"{{ path = "{crates_dir}/{key}""#);
        out.push_str(&line.replacen(NEEDLE, &replacement, 1));
        out.push('\n');
    }
    // Preserve trailing-newline state — `lines()` drops the final \n.
    if !content.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Test matrix
// ---------------------------------------------------------------------------

#[test]
fn single_plugin_effect() {
    let s = Scaffold::new("single-effect", "demo_effect");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn single_plugin_instrument() {
    let s = Scaffold::new("single-inst", "demo_inst").arg("--instrument");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn single_plugin_midi() {
    let s = Scaffold::new("single-midi", "demo_midi").arg("--midi");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn workspace_one_plugin() {
    let s = Scaffold::new_workspace("ws-one", "acme", &["gain"]);
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn workspace_three_plugins() {
    let s = Scaffold::new_workspace("ws-three", "acme", &["gain", "reverb", "delay"]);
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn workspace_mixed_types() {
    let s = Scaffold::new_workspace("ws-mixed", "acme", &["gain", "synth", "arp"])
        .arg("--type:synth=instrument")
        .arg("--type:arp=midi");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn workspace_with_vendor() {
    let s = Scaffold::new_workspace("ws-vendor", "acme", &["gain"])
        .arg("--vendor")
        .arg("Demo Audio")
        .arg("--vendor-id")
        .arg("com.demo");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_check().unwrap();
}

#[test]
fn single_plugin_no_standalone() {
    let s = Scaffold::new("single-no-standalone", "demo_bare").arg("--no-standalone");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    // No `src/main.rs` — the standalone host shouldn't be scaffolded.
    assert!(
        !s.generated.join("src/main.rs").exists(),
        "[single-no-standalone] src/main.rs leaked into a --no-standalone scaffold"
    );
    s.cargo_check().unwrap();
}

#[test]
fn workspace_no_standalone() {
    let s = Scaffold::new_workspace("ws-no-standalone", "acme", &["gain", "reverb"])
        .arg("--no-standalone");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    for p in ["gain", "reverb"] {
        assert!(
            !s.generated
                .join(format!("plugins/{p}/src/main.rs"))
                .exists(),
            "[ws-no-standalone] plugins/{p}/src/main.rs leaked into a --no-standalone scaffold"
        );
    }
    s.cargo_check().unwrap();
}

// Full `cargo build` of a multi-plugin workspace. Catches link-time
// regressions (format wrapper cdylib symbols, force-load /
// exported-symbol link args, Mach-O / PE export tables) that
// `cargo check` can't see. Cold runtime ~60s; warm ~5-10s if the
// shared target dir is still populated.
#[test]
fn workspace_full_build() {
    let s = Scaffold::new_workspace("ws-full-build", "acme", &["gain", "reverb", "delay"]);
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_build().unwrap();
}

// `cargo test --workspace` on a single-plugin scaffold. The
// scaffolded project ships a default `#[test]` block exercising the
// templated DSP + bus config + state round-trip; compiling the test
// binary AND running the tests catches both compile-time regressions
// and broken assertions that templates accumulate.
#[test]
fn single_plugin_tests_pass() {
    let s = Scaffold::new("single-tests", "demo_effect");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_test().unwrap();
}

// `cargo test --workspace` on a multi-plugin workspace mixing every
// plugin kind (effect + instrument + midi). Doubles as a link-time
// build check — `cargo test` compiles and links every cdylib like
// `cargo build` does — and verifies the per-kind default test
// templates (`render_effect` vs `render_instrument` vs note-effect
// silence assertion) all pass.
#[test]
fn workspace_mixed_types_tests_pass() {
    let s = Scaffold::new_workspace("ws-mixed-tests", "acme", &["gain", "synth", "arp"])
        .arg("--type:synth=instrument")
        .arg("--type:arp=midi");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_test().unwrap();
}

// `cargo truce build --clap` on a single-plugin scaffold. Exercises
// the actual `cargo-truce` binary (not bare `cargo build`), so
// xtask-side regressions — `project_root` resolution from a child
// cwd, per-format feature gating in `detect_default_features`,
// bundle staging into `target/bundles/` — surface here. Single
// plugin + CLAP only to keep the test under a minute. Does not use
// the shared target dir; see `truce_subcommand` for why.
#[test]
fn scaffold_cargo_truce_build_clap() {
    let s = Scaffold::new("truce-build-clap", "demo_effect");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.truce_subcommand(&["build", "--clap"]).unwrap();
    s.assert_bundle_count_by_ext(".clap", 1);
}

// `cargo truce build --clap --vst3` on a two-plugin workspace.
// Doubles the matrix coverage of the build integration: workspace
// (vs single plugin), multi-format invocation (vs one format),
// and the VST3 C++ shim compile path (vs CLAP-only). One test
// rather than three so we pay for the truce framework compile once.
#[test]
fn scaffold_cargo_truce_build_workspace_multi_format() {
    let s = Scaffold::new_workspace("truce-build-multi", "acme", &["gain", "reverb"]);
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.truce_subcommand(&["build", "--clap", "--vst3"]).unwrap();
    s.assert_bundle_count_by_ext(".clap", 2);
    s.assert_bundle_count_by_ext(".vst3", 2);
}

// `cargo truce screenshot` on a fresh scaffold. Regression guard for
// a bug where `truce_core::screenshot::workspace_screenshot_dir`
// resolved against compile-time `CARGO_MANIFEST_DIR` — when truce
// was consumed as a git dep, that baked in the cargo git checkout
// path, so PNGs landed in `~/.cargo/git/checkouts/.../target/...`
// instead of the user's project. The fix walks from runtime cwd.
//
// Test exercises this path-rewritten-to-local: the rewrite makes
// the truce deps look like a sibling crate rather than the truce
// repo's `[workspace]`, so a regression would dump the PNG into the
// in-tree `target/screenshots/` of the truce-xtask test run.
#[test]
fn scaffold_cargo_truce_screenshot() {
    let s = Scaffold::new("truce-screenshot", "demo_effect");
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    // Pre-clean the truce repo's screenshot output for the unique
    // name we'll use, so the leak-detection assertion below is
    // sensitive only to *this* test run.
    let truce_pic = truce_root().join("target/screenshots/scaffold_smoke.png");
    let _ = std::fs::remove_file(&truce_pic);

    s.truce_subcommand(&["screenshot", "--name", "scaffold_smoke"])
        .unwrap();

    // Screenshot must land in the SCAFFOLDED project's target dir,
    // not the truce repo's. `workspace_screenshot_dir` walks from
    // runtime cwd; the cargo-truce CLI runs in the scaffold dir, so
    // the dlopen'd cdylib's `__truce_screenshot` should resolve there.
    let project_pic = s.generated.join("target/screenshots/scaffold_smoke.png");
    assert!(
        project_pic.exists(),
        "[truce-screenshot] expected PNG at {} but it's missing — \
         workspace_screenshot_dir resolved somewhere else",
        project_pic.display()
    );
    assert!(
        !truce_pic.exists(),
        "[truce-screenshot] PNG leaked into truce checkout at {} — \
         workspace_screenshot_dir reverted to compile-time CARGO_MANIFEST_DIR?",
        truce_pic.display()
    );
}

// ---------------------------------------------------------------------------
// Unit tests for the rewrite helper
// ---------------------------------------------------------------------------

#[test]
fn rewrite_simple_git_ref() {
    let input = r#"truce = { git = "https://github.com/truce-audio/truce" }
"#;
    let expected = r#"truce = { path = "/abs/crates/truce" }
"#;
    assert_eq!(rewrite_git_refs(input, "/abs/crates"), expected);
}

#[test]
fn rewrite_preserves_features_and_optional() {
    let input = r#"truce-clap = { git = "https://github.com/truce-audio/truce", optional = true }
truce-standalone = { git = "https://github.com/truce-audio/truce", features = ["gui"] }
"#;
    let expected = r#"truce-clap = { path = "/abs/crates/truce-clap", optional = true }
truce-standalone = { path = "/abs/crates/truce-standalone", features = ["gui"] }
"#;
    assert_eq!(rewrite_git_refs(input, "/abs/crates"), expected);
}

#[test]
fn scan_diagnostics_picks_up_warning_and_error() {
    let stderr = "\
   Compiling foo v0.1.0
warning: unused import: `Foo`
  --> src/lib.rs:3:5
error: cannot find function `bar` in this scope
  --> src/lib.rs:7:5
";
    let got = scan_for_diagnostics("", stderr);
    assert_eq!(got.len(), 2, "got: {got:?}");
    assert!(got[0].contains("warning: unused import"));
    assert!(got[1].contains("error: cannot find function"));
}

#[test]
fn scan_diagnostics_skips_summary_and_cancellation_lines() {
    let stderr = "\
warning: 3 warnings emitted

warning: build failed, waiting for other jobs to finish...
warning: real diagnostic here
";
    let got = scan_for_diagnostics("", stderr);
    assert_eq!(got.len(), 1, "got: {got:?}");
    assert!(got[0].contains("real diagnostic"));
}

#[test]
fn scan_diagnostics_strips_ansi_color() {
    // ESC[33m = yellow, ESC[0m = reset (typical rustc warning coloring).
    let stderr = "\x1b[1m\x1b[33mwarning\x1b[0m: unused variable\n";
    let got = scan_for_diagnostics("", stderr);
    assert_eq!(got.len(), 1, "got: {got:?}");
    assert!(got[0].contains("warning: unused variable"));
}

#[test]
fn rewrite_leaves_commented_lines_alone() {
    let input = r#"# truce-lv2 = { git = "https://github.com/truce-audio/truce" }
#   truce-au = { git = "https://github.com/truce-audio/truce" }
truce = { git = "https://github.com/truce-audio/truce" }
"#;
    let got = rewrite_git_refs(input, "/abs/crates");
    assert!(got.contains(r#"# truce-lv2 = { git = "https://github.com/truce-audio/truce" }"#));
    assert!(got.contains(r#"#   truce-au = { git = "https://github.com/truce-audio/truce" }"#));
    assert!(got.contains(r#"truce = { path = "/abs/crates/truce" }"#));
}
