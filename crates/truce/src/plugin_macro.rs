//! The `plugin!` macro - one macro to export a `PluginLogic` impl
//! to all formats with zero boilerplate.

/// Export a plugin to all active format targets.
///
/// This is the only macro a developer needs. It generates all format
/// exports (CLAP, VST3, etc.) based on Cargo features.
///
/// # Usage
///
/// ```ignore
/// truce::plugin! {
///     logic: Gain,
///     params: GainParams,
/// }
/// ```
///
/// # Feature-combination matrix
///
/// The macro doesn't statically require *any* feature gate - every
/// arm is `#[cfg(feature = "...")]`-guarded so consumers can mix and
/// match. The four legitimate combinations:
///
/// | Features                    | Result                                      |
/// |-----------------------------|---------------------------------------------|
/// | none                        | `cargo check` / test-only logic dylib       |
/// | one or more of `clap`, `vst3`, `vst2`, `lv2`, `aax`, `au` | multi-format cdylib that exports every enabled format from one binary |
/// | `shell` only                | shell-mode loader (logic dylib loaded at runtime) |
/// | `shell` + format(s)         | shell-mode cdylib that re-exports the loaded logic to the enabled formats |
///
/// `cargo truce build` / `cargo truce install` set the appropriate
/// features per format on each invocation, so the multi-format case
/// is the typical end-user shape; `cargo check` without features is
/// the typical dev-iteration shape and intentionally produces no
/// format exports. There is no static "exactly one format" check
/// because zero is legitimate (cargo check) and many is intentional
/// (multi-format cdylib).
///
/// # Hot-reload
///
/// Add a `shell` feature to your Cargo.toml and build the shell with
/// `--features shell --release`. The logic dylib is built normally
/// (`cargo build`). The shell watches for changes and hot-reloads.
///
/// ```toml
/// [features]
/// shell = ["truce/shell"]
/// ```
///
/// ```bash
/// cargo build --release --features shell  # one-time: install shell
/// cargo watch -x build                    # iterate: logic hot-reloads
/// ```
///
/// Zero code changes. Same `truce::plugin!` macro.
/// Both `logic:` and `params:` are required. `params:` names the
/// `#[derive(Params)]` struct; the LV2 TTL renderer is a proc-macro and
/// can only find that struct's metadata by its literal name at
/// expansion time, so it must be spelled out here.
#[macro_export]
macro_rules! plugin {
    (
        logic: $logic:ty,
        params: $params:ty $(,)?
    ) => {
        $crate::__plugin_impl!($logic, $params,);
    };
    // Opt-in managed background tasks: each listed type implements
    // `BackgroundTask` and gets its own lane (queue + mode). See the
    // `tasks` module.
    (
        logic: $logic:ty,
        params: $params:ty,
        tasks: [$($task:ty),+ $(,)?] $(,)?
    ) => {
        $crate::__plugin_impl!($logic, $params, tasks: [$($task),+],);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_impl {
    ($logic:ty, $params:ty, $(tasks: [$($task:ty),+],)?) => {
        // Compile-time LV2 TTL emission. Walks the params type's
        // sidecar tree (written by `derive(Params)`) and produces
        // `manifest.ttl` / `plugin.ttl` next to it. cargo-truce's
        // stage_lv2 reads those files at package time - no dlopen.
        $crate::__reexport::__truce_lv2_emit_root!($params);

        // Always export the PluginLogic for dylib use (shell-mode or
        // testing). Static-mode shells ignore these exports.
        $crate::__reexport::export_plugin!($logic, $params);

        // The static / dynamic-shell `__HotShellWrapper` definition
        // lives inside this synthetic module so a single
        // `#[allow(unexpected_cfgs)]` covers the `feature = "shell"`
        // gate. Without the wrap, downstream crates that don't
        // declare a `shell` Cargo feature emit `unexpected_cfgs`
        // warnings at the `truce::plugin!` invocation site (the lint
        // is attributed to the macro, not the cfg attribute, so
        // per-item `#[allow]` doesn't suppress it).
        #[allow(unexpected_cfgs)]
        mod __truce_runtime {
            // `$logic` / `$params` are paths from the user crate root
            // (e.g. `Gain`, `GainParams`); pull the parent scope in so
            // they resolve from inside the synthetic module.
            use super::*;

            // --- Static mode (default, and always under `cfg(test)`) ---
            // Embed the logic directly. Zero overhead.
            //
            // `cfg(test)` forces static mode even when the `shell` feature
            // is on: shell mode loads from a release dylib that doesn't
            // exist during `cargo test`, so the runtime would have no
            // logic to delegate to and `editor()` / `process()` would
            // silently no-op. Tests need the in-process logic.
            #[cfg(any(not(feature = "shell"), test))]
            $crate::__reexport::export_static! {
                params: $params,
                info: $crate::prelude::plugin_info!(),
                logic: $logic,
                $(tasks: [$($task),+],)?
            }

            // --- Shell mode (hot-reload) ---
            // Load the logic from a dylib. Same crate, debug build.
            #[cfg(all(feature = "shell", not(test)))]
            $crate::__plugin_hot_reload!($logic, $params);
        }

        // Re-export the wrapper so `pub type Plugin`, the screenshot
        // FFI shim, and the per-format `export_*!` macros below all
        // resolve `__HotShellWrapper` at the macro invocation scope.
        #[doc(hidden)]
        pub use __truce_runtime::__HotShellWrapper;

        /// Type alias for use in tests and external references.
        pub type Plugin = __HotShellWrapper;

        /// FFI export driven by `cargo truce screenshot`. Renders
        /// the plugin's editor (optionally after loading a
        /// `.pluginstate` blob) and writes the PNG to the caller-
        /// specified path. Returns 0 on success, non-zero on
        /// failure (error message is printed to stderr).
        ///
        /// # FFI contract
        ///
        /// **Signature must stay byte-identical to
        /// `cargo-truce::commands::screenshot::ScreenshotFn`:**
        /// `unsafe extern "C" fn(*const u8, usize, *const u8, usize, f64) -> u32`.
        /// The CLI dlopens this plugin's cdylib and casts the
        /// `__truce_screenshot` symbol to that type - any drift (extra
        /// arg, reordered args, return-type change) becomes silent UB
        /// at the call site rather than a link-time error. Update both
        /// sides together.
        ///
        /// `scale` is the render scale factor. `0.0` (or any
        /// non-finite / `<= 0` value) is the sentinel for "use the
        /// default": rendering falls back to
        /// `truce_core::screenshot::DEFAULT_SCREENSHOT_SCALE` (2.0)
        /// so reference PNGs render at identical physical
        /// dimensions on every host.
        ///
        /// # Safety
        /// - `state_ptr` may be null when `state_len == 0`.
        /// - `state_ptr` (if non-null) must point to `state_len`
        ///   readable bytes (the contents of a `.pluginstate` file).
        /// - `out_path_ptr` must point to `out_path_len` valid
        ///   UTF-8 bytes - the absolute path the caller wants the
        ///   PNG written to.
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn __truce_screenshot(
            state_ptr: *const u8,
            state_len: usize,
            out_path_ptr: *const u8,
            out_path_len: usize,
            scale: f64,
        ) -> u32 {
            let state: Option<&[u8]> = if state_len == 0 {
                None
            } else {
                Some(::std::slice::from_raw_parts(state_ptr, state_len))
            };
            let path_bytes = ::std::slice::from_raw_parts(out_path_ptr, out_path_len);
            let path_str = match ::std::str::from_utf8(path_bytes) {
                Ok(s) => s,
                Err(e) => {
                    ::std::eprintln!("[truce] __truce_screenshot: invalid UTF-8 in out_path: {e}");
                    return 1;
                }
            };
            let path = ::std::path::Path::new(path_str);
            if let Some(parent) = path.parent() {
                let _ = ::std::fs::create_dir_all(parent);
            }
            let resolved_scale = if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                $crate::core::screenshot::DEFAULT_SCREENSHOT_SCALE
            };
            let (pixels, w, h) = $crate::core::screenshot::render_with_state_at_scale::<Plugin>(
                state,
                resolved_scale,
            );
            $crate::core::screenshot::save_png(path, &pixels, w, h);
            0
        }

        // Format exports - same wrapper name in both modes.
        //
        // Wrapped in a synthetic module so a single
        // `#[allow(unexpected_cfgs)]` covers every cfg-feature gate
        // below. Without this, downstream crates that don't declare
        // every truce format as a Cargo feature (e.g. analyzers that
        // ship without LV2) emit `unexpected_cfgs` warnings at the
        // `truce::plugin!` invocation site. Per-item `#[allow]`
        // doesn't suppress it because the lint is attributed to the
        // macro invocation, not the cfg attribute. Symbols emitted
        // inside this module are still `#[unsafe(no_mangle)] extern "C"` so
        // they're visible to host loaders regardless of module scope.
        #[allow(unexpected_cfgs)]
        mod __truce_format_exports {
            use super::__HotShellWrapper;

            #[cfg(feature = "clap")]
            ::truce_clap::export_clap!(__HotShellWrapper);

            #[cfg(feature = "vst3")]
            ::truce_vst3::export_vst3!(__HotShellWrapper);

            #[cfg(feature = "vst2")]
            ::truce_vst2::export_vst2!(__HotShellWrapper);

            #[cfg(feature = "lv2")]
            ::truce_lv2::export_lv2!(__HotShellWrapper);

            #[cfg(feature = "aax")]
            ::truce_aax::export_aax!(__HotShellWrapper);

            #[cfg(feature = "au")]
            ::truce_au::export_au!(__HotShellWrapper);
        }
    };
}

/// Shell mode: generate a dynamic shell that loads the logic from
/// this same crate's debug-build dylib.
///
/// The developer builds the shell once with
/// `--features shell --release` and iterates with `cargo build`
/// (debug, fast). The shell watches `target/debug/lib{crate_name}.dylib`
/// for changes.
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_hot_reload {
    ($logic:ty, $params:ty) => {
        pub struct __HotShellWrapper {
            // `Sample` is the prelude's type alias (`f32` for
            // `prelude` / `prelude32` / `prelude64m`, `f64` for
            // `prelude64`). `HotShell` is generic over the sample
            // type so a `prelude64` plugin can hot-reload too; the
            // matching dylib must have been built against the same
            // prelude (its `AbiCanary::sample_precision` is checked
            // at load time, so a mismatch is a clean canary failure
            // rather than a silent UB on the first audio block).
            inner: $crate::__reexport::HotShell<$params, Sample>,
        }

        impl __HotShellWrapper {
            fn dylib_path() -> std::path::PathBuf {
                // Runtime escape hatch - point the shell at any
                // dylib (advanced; only works when the DAW inherits
                // the env, e.g. launched from the same terminal).
                if let Ok(p) = std::env::var("TRUCE_LOGIC_PATH") {
                    return std::path::PathBuf::from(p);
                }

                // Sidecar written by `cargo truce install --shell` at
                // install time: a single line containing the absolute
                // path to the logic dylib.
                let crate_name = env!("CARGO_PKG_NAME");
                if let Some(sidecar) = $crate::__reexport::shell_sidecar_path(crate_name) {
                    if let Ok(contents) = std::fs::read_to_string(&sidecar) {
                        let trimmed = contents.trim();
                        if !trimmed.is_empty() {
                            let p = std::path::PathBuf::from(trimmed);
                            // Skip the sidecar if the dylib it points
                            // at no longer exists (stale path from a
                            // prior `--target` build, or a manual
                            // `cargo clean`). Letting `Library::new`
                            // fail surfaces as a generic "plugin
                            // failed to load" in the DAW; falling
                            // through to the manifest-relative
                            // fallback below at least gives the
                            // in-tree dev workflow a chance.
                            if p.is_file() {
                                return p;
                            }
                            eprintln!(
                                "[truce] sidecar {} points at missing dylib {}; \
                                 falling back to manifest-relative search",
                                sidecar.display(),
                                p.display(),
                            );
                        }
                    }
                }

                panic!(
                    "truce hot-reload: no logic dylib path resolved. \
                     The shell sidecar at $HOME/.truce/shell/{}.path is \
                     missing or empty and TRUCE_LOGIC_PATH is unset. \
                     Run `cargo truce install --shell` to write the \
                     sidecar, or set TRUCE_LOGIC_PATH explicitly.",
                    crate_name,
                );
            }
        }

        impl $crate::core::plugin::PluginRuntime for __HotShellWrapper {
            type Sample = Sample;

            fn supports_in_place() -> bool
            where
                Self: Sized,
            {
                <$logic as $crate::__reexport::truce_plugin::PluginLogicCore<Sample>>::supports_in_place()
            }

            fn info() -> $crate::core::info::PluginInfo
            where
                Self: Sized,
            {
                $crate::prelude::plugin_info!()
            }

            fn bus_layouts() -> Vec<$crate::core::bus::BusLayout>
            where
                Self: Sized,
            {
                // Hot-reload mode reads bus layouts from the
                // shell's *baked-in* `$logic` rather than the
                // running dylib's: bus layouts are queried during
                // plugin discovery (host port enumeration) and
                // changes to them require a host-level
                // re-discovery anyway. Reloading the logic dylib
                // can iterate DSP and GUI freely; bus layouts
                // changes warrant a shell rebuild + DAW rescan.
                <$logic as $crate::__reexport::truce_plugin::PluginLogicCore<Sample>>::bus_layouts()
            }

            fn init(&mut self) {
                self.inner.init();
            }

            fn reset(&mut self, config: &$crate::core::config::AudioConfig) {
                self.inner.reset(config);
            }

            fn process(
                &mut self,
                buffer: &mut $crate::core::buffer::AudioBuffer<Sample>,
                events: &$crate::core::events::EventList,
                context: &mut $crate::core::process::ProcessContext,
            ) -> $crate::core::process::ProcessStatus {
                self.inner.process(buffer, events, context)
            }

            fn save_state(&self) -> Vec<u8> {
                self.inner.save_state()
            }

            fn load_state(
                &mut self,
                data: &[u8],
            ) -> Result<(), $crate::core::state::StateLoadError> {
                self.inner.load_state(data)
            }

            fn latency(&self) -> u32 {
                self.inner.latency()
            }
            fn tail(&self) -> u32 {
                self.inner.tail()
            }
            fn get_meter(&self, meter_id: u32) -> f32 {
                self.inner.get_meter(meter_id)
            }
        }

        impl $crate::core::export::PluginExport for __HotShellWrapper {
            type Params = $params;

            fn create() -> Self {
                let params = <$params>::new();
                let path = Self::dylib_path();
                Self {
                    inner: $crate::__reexport::HotShell::new(params, path),
                }
            }

            fn params(&self) -> &$params {
                &self.inner.params
            }

            fn params_arc(&self) -> std::sync::Arc<$params> {
                std::sync::Arc::clone(&self.inner.params)
            }

            fn meter_store(&self) -> std::sync::Arc<$crate::core::meters::MeterStore> {
                self.inner.meter_store()
            }

            fn snapshot_slot(&self) -> std::sync::Arc<$crate::core::snapshot::SnapshotSlot> {
                self.inner.snapshot_slot()
            }

            fn editor_builder(&self) -> $crate::core::editor::EditorBuilder<$params> {
                // Builds from the *reloaded* dylib (not the shell's
                // baked-in logic), so GUI edits hot-reload - picked up on
                // the next editor close+open. Takes the loader lock the
                // audio thread only `try_lock`s, so it never stalls audio.
                self.inner.editor_builder()
            }
        }
    };
}
