# truce-font

Bundled fonts for the truce audio plugin framework.

Exposes `JETBRAINS_MONO` — JetBrains Mono Regular as `&'static [u8]`
of TTF bytes — for use by the built-in GUI rasterizer (`truce-gui`),
the egui / iced / slint editor backends, and the headless snapshot
rendering pipelines.

## Why a separate crate

Lets advanced users override the bundled font via Cargo's `[patch]`
section instead of forking `truce-gui`, and keeps the font's binary
payload + license file out of the framework's main crates.

## License

This crate is dual-licensed `MIT OR Apache-2.0`, matching the rest
of the truce workspace.

The bundled JetBrains Mono font is itself licensed under the **SIL
Open Font License, Version 1.1**. The full license text is at
`fonts/OFL.txt`. Downstream redistribution must keep the OFL with
the font and preserve its embedded copyright notice
("Copyright 2020 The JetBrains Mono Project Authors").
