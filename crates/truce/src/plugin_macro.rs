//! The `plugin!` macro — one macro to export a PluginLogic plugin
//! to all formats with zero boilerplate.

/// Export a PluginLogic plugin to all active format targets.
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
/// # Hot-reload
///
/// Add a `dev` feature to your Cargo.toml and build the shell with
/// `--features dev --release`. The logic dylib is built normally
/// (`cargo build`). The shell watches for changes and hot-reloads.
///
/// ```toml
/// [features]
/// dev = []
/// ```
///
/// ```bash
/// cargo build --release --features dev  # one-time: install shell
/// cargo watch -x build                  # iterate: logic hot-reloads
/// ```
///
/// Zero code changes. Same `truce::plugin!` macro.
#[macro_export]
macro_rules! plugin {
    // Full form with bus_layouts
    (
        logic: $logic:ty,
        params: $params:ty,
        bus_layouts: [$($layout:expr),* $(,)?],
    ) => {
        $crate::__plugin_impl!($logic, $params, [$($layout),*]);
    };
    // Short form — defaults to stereo
    (
        logic: $logic:ty,
        params: $params:ty $(,)?
    ) => {
        $crate::__plugin_impl!($logic, $params, [$crate::prelude::BusLayout::stereo()]);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_impl {
    ($logic:ty, $params:ty, [$($layout:expr),*]) => {
        // Always export the PluginLogic for dylib use (hot-reload or testing).
        $crate::__reexport::export_plugin!($logic, $params);

        /// Type alias for use in tests and external references.
        pub type Plugin = __HotShellWrapper;

        // --- Static mode (default) ---
        // Embed the logic directly. Zero overhead.
        #[cfg(not(feature = "dev"))]
        $crate::__reexport::export_static! {
            params: $params,
            info: $crate::prelude::plugin_info!(),
            bus_layouts: [$($layout),*],
            logic: $logic,
        }

        // --- Dev mode (hot-reload) ---
        // Load the logic from a dylib. Same crate, debug build.
        #[cfg(feature = "dev")]
        $crate::__plugin_dev!($params, [$($layout),*]);

        /// FFI export driven by `cargo truce screenshot`. Renders the
        /// plugin's editor headlessly and writes the saved PNG path
        /// into `out_buf` (up to `out_cap` bytes). Returns the byte
        /// length the path required — the caller treats
        /// `result > out_cap` as truncation.
        ///
        /// # Safety
        /// `name_ptr` must point to `name_len` valid UTF-8 bytes.
        /// `out_buf` must be writable for at least `out_cap` bytes.
        #[doc(hidden)]
        #[no_mangle]
        pub unsafe extern "C" fn __truce_screenshot(
            name_ptr: *const u8,
            name_len: usize,
            out_buf: *mut u8,
            out_cap: usize,
        ) -> usize {
            let name_slice = ::std::slice::from_raw_parts(name_ptr, name_len);
            let name = ::std::str::from_utf8(name_slice).unwrap_or("screenshot");
            let path = $crate::core::screenshot::render::<Plugin>(name);
            let path_str = path.to_string_lossy();
            let bytes = path_str.as_bytes();
            if bytes.len() <= out_cap {
                ::std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf, bytes.len());
            }
            bytes.len()
        }

        // Format exports — same wrapper name in both modes.
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
    };
}

/// Dev mode: generate a hot-reload shell that loads the logic from
/// this same crate's debug-build dylib.
///
/// The developer builds the shell once with `--features dev --release`
/// and iterates with `cargo build` (debug, fast). The shell watches
/// `target/debug/lib{crate_name}.dylib` for changes.
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_dev {
    ($params:ty, [$($layout:expr),*]) => {
        pub struct __HotShellWrapper {
            inner: $crate::__reexport::HotShell<$params>,
        }

        impl __HotShellWrapper {
            fn dylib_path() -> std::path::PathBuf {
                // Check env var first.
                if let Ok(p) = std::env::var("TRUCE_LOGIC_PATH") {
                    return std::path::PathBuf::from(p);
                }

                // Find the workspace root by walking up from CARGO_MANIFEST_DIR
                // looking for a target/ directory.
                let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                loop {
                    if root.join("target").is_dir() {
                        break;
                    }
                    if !root.pop() {
                        root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                        break;
                    }
                }

                // Always look in target/debug/ — the shell is built in release,
                // the logic dylib is built in debug (fast compile).
                root.push("target");
                root.push("debug");

                // Derive dylib name from crate name.
                // CARGO_PKG_NAME = "truce-example-gain" → "truce_example_gain"
                let crate_name = env!("CARGO_PKG_NAME").replace('-', "_");

                #[cfg(target_os = "macos")]
                root.push(format!("lib{crate_name}.dylib"));
                #[cfg(target_os = "linux")]
                root.push(format!("lib{crate_name}.so"));
                #[cfg(target_os = "windows")]
                root.push(format!("{crate_name}.dll"));

                root
            }
        }

        impl $crate::core::plugin::Plugin for __HotShellWrapper {
            fn info() -> $crate::core::info::PluginInfo where Self: Sized {
                $crate::prelude::plugin_info!()
            }

            fn bus_layouts() -> Vec<$crate::core::bus::BusLayout> where Self: Sized {
                vec![$($layout),*]
            }

            fn init(&mut self) {
                self.inner.init();
            }

            fn reset(&mut self, sample_rate: f64, max_block_size: usize) {
                self.inner.reset(sample_rate, max_block_size);
            }

            fn process(
                &mut self,
                buffer: &mut $crate::core::buffer::AudioBuffer,
                events: &$crate::core::events::EventList,
                context: &mut $crate::core::process::ProcessContext,
            ) -> $crate::core::process::ProcessStatus {
                self.inner.process(buffer, events, context)
            }

            fn save_state(&self) -> Option<Vec<u8>> {
                self.inner.save_state()
            }

            fn load_state(&mut self, data: &[u8]) {
                self.inner.load_state(data);
            }

            fn editor(&mut self) -> Option<Box<dyn $crate::core::editor::Editor>> {
                self.inner.editor()
            }

            fn latency(&self) -> u32 { self.inner.latency() }
            fn tail(&self) -> u32 { self.inner.tail() }
            fn get_meter(&self, meter_id: u32) -> f32 { self.inner.get_meter(meter_id) }
        }

        impl $crate::core::export::PluginExport for __HotShellWrapper {
            type Params = $params;

            fn create() -> Self {
                let params = <$params>::new();
                let info = <Self as $crate::core::plugin::Plugin>::info();
                let bus_layouts = <Self as $crate::core::plugin::Plugin>::bus_layouts();
                let path = Self::dylib_path();
                Self {
                    inner: $crate::__reexport::HotShell::new(params, path, info, bus_layouts),
                }
            }

            fn params(&self) -> &$params {
                &self.inner.params
            }

            fn params_arc(&self) -> std::sync::Arc<$params> {
                std::sync::Arc::clone(&self.inner.params)
            }
        }
    };
}
