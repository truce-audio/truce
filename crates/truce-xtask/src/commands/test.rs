//! `cargo truce test` — per-plugin `cargo test` driver.
//!
//! Iterates `[[plugin]]` entries in `truce.toml` and runs
//! `cargo test -p <crate> -- --quiet` for each, printing one summary
//! line per plugin. Functionally a subset of `cargo test --workspace`
//! (which also runs non-plugin crate tests like `truce-core`,
//! `truce-params`, `truce-loader`); kept for the per-plugin output
//! shape and to filter the workspace down to plugin crates only.
//!
//! Format-spec validation (auval, pluginval, clap-validator, the VST2
//! binary smoke) lives under `cargo truce validate` — see
//! `commands/validate.rs`.

use crate::{load_config, Res};
use std::process::Command;

pub(crate) fn cmd_test() -> Res {
    let config = load_config()?;

    eprintln!("Running plugin tests...\n");
    let mut all_passed = true;

    for p in &config.plugin {
        eprint!("  {} ... ", p.name);
        let status = Command::new("cargo")
            .args(["test", "-p", &p.crate_name, "--", "--quiet"])
            .output()?;
        let stderr = String::from_utf8_lossy(&status.stderr);
        if status.status.success() {
            // Count tests from stderr (cargo test output goes to stderr).
            let test_line = stderr.lines().find(|l| l.contains("test result"));
            if let Some(line) = test_line {
                eprintln!("{}", line.trim());
            } else {
                eprintln!("PASS");
            }
        } else {
            eprintln!("FAIL");
            eprint!("{}", stderr);
            all_passed = false;
        }
    }

    if all_passed {
        eprintln!("All tests passed.");
        Ok(())
    } else {
        Err("Some tests failed".into())
    }
}
