## Channel layouts

Most plugins are stereo, but if you need other configurations:

```rust
fn bus_layouts() -> Vec<BusLayout> {
    vec![
        // Stereo in -> Stereo out (most common; also the default)
        BusLayout::stereo(),

        // Or build custom layouts:
        BusLayout::new()
            .with_input("Main", ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Mono),

        // Mono -> Stereo (e.g., a stereo widener)
        BusLayout::new()
            .with_input("Main", ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Stereo),
    ]
}
```

For instruments (no audio input):

```rust
fn bus_layouts() -> Vec<BusLayout> {
    vec![
        BusLayout::new()
            .with_output("Main", ChannelConfig::Stereo),
    ]
}
```

For sidechain effects:

```rust
fn bus_layouts() -> Vec<BusLayout> {
    vec![
        BusLayout::new()
            .with_input("Main", ChannelConfig::Stereo)
            .with_input("Sidechain", ChannelConfig::Stereo)
            .with_output("Main", ChannelConfig::Stereo),
    ]
}
```

---


---

[← Previous](05-processing.md) | [Next →](07-synth.md) | [Index](README.md)
