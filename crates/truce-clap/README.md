# truce-clap

CLAP format wrapper for the truce audio plugin framework.

Provides the `export_clap!` macro to expose any `PluginExport` implementation
as a [CLAP](https://cleveraudio.org/) plugin. Handles the CLAP entry point,
descriptor, parameter mapping, and audio processing bridge.
