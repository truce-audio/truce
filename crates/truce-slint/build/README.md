# truce-slint-build

Build-script helper for truce plugins with a Slint GUI.

## Overview

Plugin authors who use `truce-slint` write `.slint` UI files that
reference truce's widget library:

```slint
import { Knob, Meter, XYPad } from "@truce";
import "JetBrainsMono-Regular.ttf";
```

This crate bundles the widget library and the JetBrains Mono ttf,
materializes them into `OUT_DIR` at the consuming crate's build
time, and configures `slint-build` so the imports above resolve.

## Usage

`Cargo.toml`:

```toml
[build-dependencies]
truce-slint-build = "0.49"
```

`build.rs`:

```rust
fn main() {
    truce_slint_build::compile("ui/main.slint").unwrap();
}
```

That's the whole integration. No `library_paths`, no
`include_paths`, no knowledge of where truce's assets live on
disk.

## License

This crate ships JetBrains Mono under the SIL Open Font License
1.1 (see `fonts/OFL.txt`). The wrapper code itself is Apache-2.0
licensed.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
