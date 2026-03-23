# truce-vst3

VST3 format wrapper for truce.

Uses a C++ shim that implements the real VST3 COM interfaces with correct
vtable layout. All plugin logic is delegated to Rust via C FFI callbacks.
