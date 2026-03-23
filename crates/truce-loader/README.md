# truce-loader

Hot-reloadable plugin logic for truce.

Splits a plugin into a static shell (loaded by the DAW) and a hot-reloadable
logic dylib that reloads on recompile. The developer implements the
`PluginLogic` trait — a safe Rust trait — and exports it via `#[no_mangle]`
functions. The shell loads the dylib, verifies ABI compatibility, and delegates
audio processing and GUI rendering to the trait object.

## Features

| Feature | Description |
|---------|-------------|
| `shell` | Enable dylib loading via `libloading` |
| `gpu` | GPU rendering support in the shell |
