//! Corpus replay: re-run every accumulated fuzz input through its
//! harness in a plain binary, so the corpus can execute under Miri
//! (`cargo +nightly miri run --bin replay`, with
//! `-Zmiri-disable-isolation` for the corpus-file reads - the code
//! under test does no I/O). Native fuzzing finds the inputs; this
//! promotes them to UB-checked.
//!
//! Missing corpus directories are skipped, not errors: a fresh
//! checkout has no corpus until `cargo fuzz run` builds one (or CI
//! restores its cache).

use std::path::Path;

/// A named fuzz harness the replay walks a corpus directory for.
type Target = (&'static str, fn(&[u8]));

fn main() {
    let targets: &[Target] = &[
        ("state_envelope", truce_fuzz::state_envelope),
        ("preset_container", truce_fuzz::preset_container),
        ("midi_short", truce_fuzz::midi_short),
        ("ump_decode", truce_fuzz::ump_decode),
        ("sysex_assembler", truce_fuzz::sysex_assembler),
    ];

    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    let mut total = 0usize;
    for (name, harness) in targets {
        let dir = corpus_root.join(name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            println!("{name}: no corpus, skipped");
            continue;
        };
        let mut ran = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let bytes =
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            harness(&bytes);
            ran += 1;
        }
        println!("{name}: replayed {ran} inputs");
        total += ran;
    }
    println!("total: {total} inputs replayed");
}
