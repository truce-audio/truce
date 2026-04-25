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
//! The default matrix runs `cargo check` only — ~7s warm. There's
//! also an `#[ignore]`d full-build test that runs `cargo build` on a
//! multi-plugin workspace to catch link-time regressions (format
//! wrapper symbol exports, cdylib link args, etc.) that `check`
//! skips. Opt in:
//!
//! ```
//! cargo test -p cargo-truce --test scaffold_e2e -- --ignored
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
        let crates_dir = truce_root().join("crates").to_string_lossy().into_owned();

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

    /// Shared body for `cargo check` and `cargo build`. Both hold
    /// `build_lock` (cache safety) and share the target dir.
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

// Off by default (ignored) — full `cargo build` of a multi-plugin
// workspace. Catches link-time regressions (format wrapper cdylib
// symbols, force-load / exported-symbol link args, Mach-O / PE
// export tables) that `cargo check` can't see. Enable explicitly:
//
//     cargo test -p cargo-truce --test scaffold_e2e -- --ignored
//
// Cold runtime: ~60s (full release-profile link graph for truce-*).
// Warm: ~5–10s if the shared target dir is still populated.
#[test]
#[ignore = "full build — run with `cargo test -- --ignored`"]
fn workspace_full_build() {
    let s = Scaffold::new_workspace("ws-full-build", "acme", &["gain", "reverb", "delay"]);
    s.run().unwrap();
    s.rewrite_git_to_path().unwrap();
    s.cargo_build().unwrap();
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
