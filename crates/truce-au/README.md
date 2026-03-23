# truce-au

Audio Unit v3 format wrapper for truce.

Uses an Objective-C shim compiled via `cc` that implements the `AUAudioUnit`
subclass. The shim calls back into Rust for all plugin logic via C FFI.
