# truce-aax

AAX format wrapper for truce.

Exports C ABI functions that the pre-built AAX template binary loads via
`dlopen`. No AAX SDK dependency — the Rust side only knows about the C bridge
types defined in `truce_aax_bridge.h`.

Part of [truce](https://github.com/truce-audio/truce).
