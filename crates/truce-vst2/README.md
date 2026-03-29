# truce-vst2

VST2 format wrapper for truce.

Uses a C shim that implements the `AEffect` interface. The shim calls back
into Rust for all plugin logic via C FFI. Clean-room implementation — no
Steinberg SDK headers.

Part of [truce](https://github.com/truce-audio/truce).
