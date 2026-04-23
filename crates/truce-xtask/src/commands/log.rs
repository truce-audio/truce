//! `cargo truce log` — stream AU v3 appex logs.

use crate::Res;
use std::process::Command;

pub(crate) fn cmd_log() -> Res {
    eprintln!("Streaming AU v3 appex logs (Ctrl-C to stop)...\n");
    let status = Command::new("/usr/bin/log")
        .args([
            "stream",
            "--predicate",
            "subsystem == \"com.truce.au3\"",
            "--style", "compact",
            "--level", "debug",
        ])
        .status()?;
    if !status.success() {
        return Err("log stream exited with error".into());
    }
    Ok(())
}
