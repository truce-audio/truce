//! `cargo truce test` — run all plugin tests + the VST2 binary smoke test.

use crate::{deployment_target, load_config, project_root, Res};
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
            // Count tests from stderr (cargo test output goes to stderr)
            let test_line = stderr.lines().find(|l| l.contains("test result"));
            if let Some(line) = test_line {
                eprintln!("{}", line.trim());
            } else {
                eprintln!("PASS");
            }
        } else {
            eprintln!("FAIL");
            eprint!("{}", String::from_utf8_lossy(&status.stderr));
            all_passed = false;
        }
    }

    // --- VST2 binary tests ---
    let root = project_root();
    let test_src = root.join("tests/test_vst2_binary.c");
    let test_bin = root.join("target/test_vst2");
    if test_src.exists() {
        eprintln!("Running VST2 binary tests...\n");
        let cc_status = Command::new("cc")
            .args(["-o", test_bin.to_str().unwrap(), test_src.to_str().unwrap()])
            .status()?;
        if cc_status.success() {
            // Build VST2 plugins
            for p in &config.plugin {
                eprint!("  VST2 {} ... ", p.name);
                let build = Command::new("cargo")
                    .args(["build", "--release", "-p", &p.crate_name,
                           "--no-default-features", "--features", "vst2"])
                    .env("MACOSX_DEPLOYMENT_TARGET", &deployment_target())
                    .output()?;
                if !build.status.success() {
                    eprintln!("BUILD FAILED");
                    all_passed = false;
                    continue;
                }
                let dylib = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
                let is_synth = p.resolved_au_type() == "aumu";
                let mut cmd = Command::new(test_bin.to_str().unwrap());
                cmd.arg(dylib.to_str().unwrap());
                if is_synth { cmd.arg("--synth"); }
                let output = cmd.output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if output.status.success() {
                    if let Some(line) = stdout.lines().last() {
                        eprintln!("{}", line);
                    }
                } else {
                    eprintln!("FAIL");
                    eprint!("{}", stdout);
                    all_passed = false;
                }
            }
            eprintln!();
        }
    }

    if all_passed {
        eprintln!("All tests passed.");
        Ok(())
    } else {
        Err("Some tests failed".into())
    }
}
