//! `cargo truce reset-aax` — flush the Pro Tools AAX plugin cache.
//!
//! macOS-only (Pro Tools' AAX cache lives at
//! `/Users/Shared/Pro Tools/AAXPlugInCache`). Removes the per-vendor
//! cache files Pro Tools writes there; on next launch Pro Tools
//! re-scans the AAX plugin directory. Does **not** touch Audio Unit
//! caches — see `cargo truce reset-au` for that.

use crate::Res;

#[cfg(not(target_os = "macos"))]
pub(crate) fn cmd_reset_aax(_args: &[String]) -> Res {
    Err(
        "`cargo truce reset-aax` is macOS-only — it flushes the Pro \
         Tools AAX cache under `/Users/Shared/Pro Tools/`, which \
         doesn't exist on Linux or Windows. On Windows, restart Pro \
         Tools to rescan."
            .into(),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn cmd_reset_aax(args: &[String]) -> Res {
    use crate::{confirm_prompt, load_config};
    use std::fs;
    use std::path::PathBuf;

    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            other => return Err(format!("Unknown flag: {other}").into()),
        }
    }

    if !yes
        && !confirm_prompt(
            "Wipe this vendor's entries from the Pro Tools AAX cache? \
             Pro Tools will re-scan AAX plugins on next launch.",
        )
    {
        eprintln!("Cancelled.");
        return Ok(());
    }

    let aax_cache = PathBuf::from("/Users/Shared/Pro Tools/AAXPlugInCache");
    if !aax_cache.exists() {
        eprintln!("No AAX cache at {} — nothing to do.", aax_cache.display());
        return Ok(());
    }

    let config = load_config()?;
    let mut removed = 0usize;
    if let Ok(entries) = fs::read_dir(&aax_cache) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains(&config.vendor.name) {
                let _ = fs::remove_file(entry.path());
                eprintln!("  Removed AAX cache: {}", name);
                removed += 1;
            }
        }
    }

    if removed == 0 {
        eprintln!(
            "No AAX cache entries matched vendor '{}'.",
            config.vendor.name
        );
    } else {
        eprintln!("Done. Restart Pro Tools to rescan.");
    }
    Ok(())
}
