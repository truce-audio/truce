//! LV2 format wrapper for the truce framework.
//!
//! Exports a `PluginExport` implementation as an LV2 plugin via the
//! [`export_lv2!`] macro. LV2's C ABI is small and stable, so we
//! hand-roll the bindings rather than pulling in a large `lv2-sys` crate.
//!
//! Port layout (default):
//!   - `0..num_in` — audio input (one port per channel)
//!   - `num_in..num_in+num_out` — audio output (one port per channel)
//!   - next N — control input (one port per parameter, float)
//!   - `atom_in_port` — single AtomPort for MIDI input (if plugin accepts MIDI)
//!
//! MIDI, State, and UI support live in sibling modules.

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
}

mod atom;
mod state;
mod ttl;
mod types;
mod ui;
mod urid;

pub use ttl::emit_bundle;
pub use types::*;

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::{PluginCategory, PluginInfo};
use truce_core::process::ProcessContext;
use truce_core::state::hash_plugin_id;
use truce_params::{ParamInfo, Params};

use crate::atom::AtomSequenceReader;
use crate::urid::UridMap;

// ---------------------------------------------------------------------------
// Port layout
// ---------------------------------------------------------------------------

/// Describes where each logical port sits in the flat LV2 port-index space.
/// Filled in once at `instantiate()` time.
#[derive(Clone, Debug)]
pub struct PortLayout {
    pub num_audio_in: u32,
    pub num_audio_out: u32,
    pub num_params: u32,
    pub num_meters: u32,
    pub has_midi_in: bool,
    pub has_midi_out: bool,
}

impl PortLayout {
    pub fn audio_in_start(&self) -> u32 {
        0
    }
    pub fn audio_out_start(&self) -> u32 {
        self.num_audio_in
    }
    pub fn control_start(&self) -> u32 {
        self.num_audio_in + self.num_audio_out
    }
    pub fn meter_start(&self) -> u32 {
        self.control_start() + self.num_params
    }
    pub fn midi_in_port(&self) -> Option<u32> {
        if self.has_midi_in {
            Some(self.meter_start() + self.num_meters)
        } else {
            None
        }
    }
    pub fn midi_out_port(&self) -> Option<u32> {
        let after_midi_in = self.meter_start()
            + self.num_meters
            + if self.has_midi_in { 1 } else { 0 };
        if self.has_midi_out {
            Some(after_midi_in)
        } else {
            None
        }
    }
    /// Index of the DSP→UI notification atom port. Always present: the
    /// DSP writes host transport (and any future plugin-defined notify
    /// messages) here, and the UI listens via `ui:portNotification`.
    pub fn notify_out_port(&self) -> u32 {
        self.meter_start()
            + self.num_meters
            + if self.has_midi_in { 1 } else { 0 }
            + if self.has_midi_out { 1 } else { 0 }
    }
    pub fn total(&self) -> u32 {
        self.notify_out_port() + 1
    }
}

// ---------------------------------------------------------------------------
// Instance
// ---------------------------------------------------------------------------

/// Live instance of an LV2 plugin. Held as `LV2_Handle` for the host.
pub struct Lv2Instance<P: PluginExport> {
    plugin: P,
    sample_rate: f64,
    max_block_size: usize,
    plugin_id_hash: u64,
    param_infos: Vec<ParamInfo>,
    layout: PortLayout,

    // Port pointers populated by connect_port().
    audio_inputs: Vec<*const f32>,
    audio_outputs: Vec<*mut f32>,
    control_ports: Vec<*const f32>,
    /// Output control ports — one per `#[meter]` slot. We write the
    /// latest meter reading here at the end of each `run()` so the host
    /// forwards it to the UI via `port_event`.
    meter_ports: Vec<*mut f32>,
    /// Parameter/meter IDs for the meter slots, in port order.
    meter_ids: Vec<u32>,
    midi_in_port: *const AtomSequence,
    midi_out_port: *mut AtomSequence,
    notify_out_port: *mut AtomSequence,

    /// Last observed value on each control port; used to emit ParamChange
    /// events only when the host actually moved a knob.
    last_control: Vec<f32>,

    event_list: EventList,
    output_events: EventList,

    urid_map: UridMap,

    /// Scratch vectors so we don't allocate on the audio thread.
    input_slices: Vec<&'static [f32]>,
    output_slices: Vec<&'static mut [f32]>,

    /// Shared transport slot — audio thread writes each block. LV2 UIs
    /// are out-of-process so the UI side still reads `None`; this slot
    /// exists so an in-process consumer (tests / DSP-side code) can
    /// observe host transport.
    transport_slot: std::sync::Arc<truce_core::TransportSlot>,
}

// Raw pointers only — we never share an instance between threads. LV2 hosts
// drive a single instance from one thread at a time (audio thread for
// run(), main thread for everything else).
unsafe impl<P: PluginExport> Send for Lv2Instance<P> {}

// ---------------------------------------------------------------------------
// LV2 lifecycle callbacks
// ---------------------------------------------------------------------------

/// Build a PortLayout from the plugin's declared bus layout + params.
pub fn derive_port_layout<P: PluginExport>() -> PortLayout {
    let layouts = P::bus_layouts();
    let default_layout = layouts
        .first()
        .expect("Plugin must declare at least one bus layout");
    let plugin = P::create();
    let params = plugin.params();
    let param_count = params.param_infos().len() as u32;
    let meter_count = params.meter_ids().len() as u32;
    let category = P::info().category;
    let has_midi_in = matches!(
        category,
        PluginCategory::Instrument | PluginCategory::NoteEffect
    );
    let has_midi_out = matches!(category, PluginCategory::NoteEffect);
    PortLayout {
        num_audio_in: default_layout.total_input_channels(),
        num_audio_out: default_layout.total_output_channels(),
        num_params: param_count,
        num_meters: meter_count,
        has_midi_in,
        has_midi_out,
    }
}

/// # Safety
/// Called by the LV2 host during plugin instantiation. `features` must be
/// a null-terminated array of `LV2_Feature` pointers (or null if none).
pub unsafe fn instantiate<P: PluginExport>(
    sample_rate: f64,
    _bundle_path: *const c_char,
    features: *const *const LV2Feature,
) -> *mut Lv2Instance<P> {
    let layout = derive_port_layout::<P>();
    let plugin = P::create();
    let info = P::info();
    let param_infos = plugin.params().param_infos();

    let control_port_count = layout.num_params as usize;
    let audio_in_count = layout.num_audio_in as usize;
    let audio_out_count = layout.num_audio_out as usize;
    let meter_ids = plugin.params().meter_ids();
    let meter_count = meter_ids.len();

    let urid_map = UridMap::from_features(features);

    let instance = Box::new(Lv2Instance::<P> {
        plugin,
        sample_rate,
        max_block_size: 0,
        plugin_id_hash: hash_plugin_id(info.clap_id),
        param_infos,
        layout,

        audio_inputs: vec![ptr::null(); audio_in_count],
        audio_outputs: vec![ptr::null_mut(); audio_out_count],
        control_ports: vec![ptr::null(); control_port_count],
        meter_ports: vec![ptr::null_mut(); meter_count],
        meter_ids,
        midi_in_port: ptr::null(),
        midi_out_port: ptr::null_mut(),
        notify_out_port: ptr::null_mut(),

        last_control: vec![f32::NAN; control_port_count],

        event_list: EventList::new(),
        output_events: EventList::new(),

        urid_map,

        input_slices: Vec::with_capacity(audio_in_count),
        output_slices: Vec::with_capacity(audio_out_count),

        transport_slot: truce_core::TransportSlot::new(),
    });
    Box::into_raw(instance)
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer previously returned
/// from `instantiate::<P>()`.
pub unsafe fn connect_port<P: PluginExport>(
    handle: *mut Lv2Instance<P>,
    port: u32,
    data: *mut c_void,
) {
    let inst = &mut *handle;
    let layout = inst.layout.clone();

    if port < layout.audio_out_start() {
        inst.audio_inputs[(port - layout.audio_in_start()) as usize] = data as *const f32;
    } else if port < layout.control_start() {
        inst.audio_outputs[(port - layout.audio_out_start()) as usize] = data as *mut f32;
    } else if port < layout.meter_start() {
        inst.control_ports[(port - layout.control_start()) as usize] = data as *const f32;
    } else if port < layout.meter_start() + layout.num_meters {
        inst.meter_ports[(port - layout.meter_start()) as usize] = data as *mut f32;
    } else if Some(port) == layout.midi_in_port() {
        inst.midi_in_port = data as *const AtomSequence;
    } else if Some(port) == layout.midi_out_port() {
        inst.midi_out_port = data as *mut AtomSequence;
    } else if port == layout.notify_out_port() {
        inst.notify_out_port = data as *mut AtomSequence;
    }
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer.
pub unsafe fn activate<P: PluginExport>(handle: *mut Lv2Instance<P>) {
    let inst = &mut *handle;
    // LV2 doesn't tell us max block size up front; use a generous default.
    // run() passes n_samples each call, so we can resize if it ever exceeds.
    let max_block = 8192usize;
    inst.max_block_size = max_block;
    inst.plugin.reset(inst.sample_rate, max_block);
    inst.plugin.params().set_sample_rate(inst.sample_rate);
    inst.plugin.params().snap_smoothers();
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer with port connections
/// established by prior calls to `connect_port()`. Audio and control port
/// memory must be valid for `n_samples`.
pub unsafe fn run<P: PluginExport>(handle: *mut Lv2Instance<P>, n_samples: u32) {
    let inst = &mut *handle;
    let n = n_samples as usize;
    if n == 0 {
        return;
    }
    if n > inst.max_block_size {
        inst.plugin.reset(inst.sample_rate, n);
        inst.max_block_size = n;
    }

    inst.event_list.clear();
    inst.output_events.clear();

    // Emit ParamChange events for any control port that moved since last
    // run. The event carries the PLAIN value — format wrappers agree on
    // plain (see `HotShell::process`'s comment). Writing plain directly
    // also lets the plugin see the value immediately via its params Arc;
    // the event is only there so `PluginLogic`s that observe param
    // changes via events (rather than reading atomics) pick the change up
    // at the right sample offset.
    for (i, &port_ptr) in inst.control_ports.iter().enumerate() {
        if port_ptr.is_null() {
            continue;
        }
        let v = *port_ptr;
        if !v.is_finite() {
            continue;
        }
        let last = inst.last_control[i];
        if last.is_nan() || (v - last).abs() > f32::EPSILON {
            inst.last_control[i] = v;
            let pid = inst.param_infos[i].id;
            let plain = v as f64;
            inst.plugin.params().set_plain(pid, plain);
            inst.event_list.push(Event {
                sample_offset: 0,
                body: EventBody::ParamChange { id: pid, value: plain },
            });
        }
    }

    // Decode MIDI + time:Position from the atom sequence port, if connected.
    let mut transport = TransportInfo::default();
    if !inst.midi_in_port.is_null() {
        let reader = AtomSequenceReader::new(inst.midi_in_port, &inst.urid_map);
        reader.for_each_midi(|sample_offset, bytes| {
            if let Some(event) = atom::midi_bytes_to_event(sample_offset, bytes) {
                inst.event_list.push(event);
            }
        });
        reader.apply_time_position(&mut transport);
    }

    // Build AudioBuffer from port pointers.
    inst.input_slices.clear();
    inst.output_slices.clear();
    for &ptr in &inst.audio_inputs {
        if !ptr.is_null() {
            let sl: &[f32] = std::slice::from_raw_parts(ptr, n);
            inst.input_slices
                .push(std::mem::transmute::<&[f32], &'static [f32]>(sl));
        }
    }
    for &ptr in &inst.audio_outputs {
        if !ptr.is_null() {
            let sl: &mut [f32] = std::slice::from_raw_parts_mut(ptr, n);
            inst.output_slices
                .push(std::mem::transmute::<&mut [f32], &'static mut [f32]>(sl));
        }
    }

    // Copy input to output for in-place effects (matches CLAP/VST2 convention).
    let copy_ch = inst.input_slices.len().min(inst.output_slices.len());
    for ch in 0..copy_ch {
        inst.output_slices[ch][..n].copy_from_slice(&inst.input_slices[ch][..n]);
    }

    let mut audio = AudioBuffer::from_slices(&inst.input_slices, &mut inst.output_slices, n);
    inst.transport_slot.write(&transport);
    let mut ctx = ProcessContext::new(&transport, inst.sample_rate, n, &mut inst.output_events);
    let _ = inst.plugin.process(&mut audio, &inst.event_list, &mut ctx);

    // Copy meter readings out to the host. The plugin's process() has
    // already written the latest peaks into the HotShell via
    // `ctx.set_meter`; reading them back via `plugin.get_meter` picks
    // up those atomics. Hosts forward the updated port value to the UI
    // through `port_event` so the editor's meter widget animates.
    for (slot, &id) in inst.meter_ports.iter().zip(inst.meter_ids.iter()) {
        if slot.is_null() {
            continue;
        }
        let v = inst.plugin.get_meter(id);
        **slot = v;
    }

    // Write MIDI output to the atom sequence port, if connected.
    if !inst.midi_out_port.is_null() {
        atom::write_midi_out_sequence(
            inst.midi_out_port,
            &inst.output_events,
            &inst.urid_map,
        );
    }

    // Forward transport to the UI as a time:Position atom on the
    // notify-out port. Hosts deliver this to the UI's port_event each
    // block; the UI decodes it and updates its shared `TransportSlot`.
    if !inst.notify_out_port.is_null() {
        atom::write_time_position_sequence(
            inst.notify_out_port,
            &transport,
            &inst.urid_map,
        );
    }
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer.
pub unsafe fn deactivate<P: PluginExport>(_handle: *mut Lv2Instance<P>) {
    // No-op: LV2 activate/deactivate bracketing is advisory. We keep the
    // plugin ready to go; another activate() will reset again.
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer. After this call the
/// pointer is dangling and must not be used.
pub unsafe fn cleanup<P: PluginExport>(handle: *mut Lv2Instance<P>) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// # Safety
/// `uri` must be a valid null-terminated C string or null.
pub unsafe fn extension_data<P: PluginExport>(uri: *const c_char) -> *const c_void {
    if uri.is_null() {
        return ptr::null();
    }
    let uri = match CStr::from_ptr(uri).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null(),
    };
    if uri == state::LV2_STATE__INTERFACE_URI {
        return state::state_interface::<P>() as *const _ as *const c_void;
    }
    ptr::null()
}

// ---------------------------------------------------------------------------
// Plugin URI
// ---------------------------------------------------------------------------

/// Derive the plugin's LV2 URI from its `PluginInfo`. Prefers an `http://`
/// URI under the vendor's URL so that LV2 hosts that expect well-formed
/// web URIs (notably the lilv reference loader used by Ardour/Reaper) are
/// happy. Falls back to `urn:truce:{id}` if the vendor URL is empty.
pub fn plugin_uri(info: &PluginInfo) -> String {
    if info.url.is_empty() {
        return format!("urn:truce:{}", info.clap_id);
    }
    let base = info.url.trim_end_matches('/');
    format!("{base}/lv2/{}", info.clap_id)
}

// ---------------------------------------------------------------------------
// Descriptor holder
// ---------------------------------------------------------------------------

/// Holds the static LV2 descriptor plus its owned URI string. One per
/// plugin type per process.
pub struct DescriptorHolder {
    pub descriptor: LV2Descriptor,
    _uri: CString,
}

unsafe impl Send for DescriptorHolder {}
unsafe impl Sync for DescriptorHolder {}

impl DescriptorHolder {
    pub fn new(
        info: &PluginInfo,
        instantiate: InstantiateFn,
        connect_port: ConnectPortFn,
        activate: LifecycleFn,
        run: RunFn,
        deactivate: LifecycleFn,
        cleanup: LifecycleFn,
        extension_data: ExtensionDataFn,
    ) -> Self {
        let uri = CString::new(plugin_uri(info)).unwrap();
        let descriptor = LV2Descriptor {
            uri: uri.as_ptr(),
            instantiate,
            connect_port,
            activate: Some(activate),
            run,
            deactivate: Some(deactivate),
            cleanup,
            extension_data,
        };
        Self {
            descriptor,
            _uri: uri,
        }
    }
}

// ---------------------------------------------------------------------------
// Export macro
// ---------------------------------------------------------------------------

/// Export a plugin as LV2.
///
/// ```ignore
/// truce_lv2::export_lv2!(MyPlugin);
/// ```
#[macro_export]
macro_rules! export_lv2 {
    ($plugin_type:ty) => {
        mod _lv2_entry {
            use super::*;
            use std::ffi::{c_char, c_void};
            use std::sync::OnceLock;

            use ::truce_lv2::__macro_deps::truce_core::plugin::Plugin;
            use ::truce_lv2::{
                DescriptorHolder, LV2Descriptor, LV2Feature, Lv2Instance,
            };

            static DESCRIPTOR: OnceLock<DescriptorHolder> = OnceLock::new();

            fn get_descriptor() -> &'static LV2Descriptor {
                let holder = DESCRIPTOR.get_or_init(|| {
                    let info = <$plugin_type as Plugin>::info();
                    DescriptorHolder::new(
                        &info,
                        instantiate,
                        connect_port,
                        activate,
                        run,
                        deactivate,
                        cleanup,
                        extension_data,
                    )
                });
                &holder.descriptor
            }

            unsafe extern "C" fn instantiate(
                _descriptor: *const LV2Descriptor,
                sample_rate: f64,
                bundle_path: *const c_char,
                features: *const *const LV2Feature,
            ) -> *mut c_void {
                ::truce_lv2::instantiate::<$plugin_type>(sample_rate, bundle_path, features)
                    as *mut c_void
            }

            unsafe extern "C" fn connect_port(handle: *mut c_void, port: u32, data: *mut c_void) {
                ::truce_lv2::connect_port::<$plugin_type>(
                    handle as *mut Lv2Instance<$plugin_type>,
                    port,
                    data,
                );
            }

            unsafe extern "C" fn activate(handle: *mut c_void) {
                ::truce_lv2::activate::<$plugin_type>(handle as *mut Lv2Instance<$plugin_type>);
            }

            unsafe extern "C" fn run(handle: *mut c_void, n_samples: u32) {
                ::truce_lv2::run::<$plugin_type>(
                    handle as *mut Lv2Instance<$plugin_type>,
                    n_samples,
                );
            }

            unsafe extern "C" fn deactivate(handle: *mut c_void) {
                ::truce_lv2::deactivate::<$plugin_type>(
                    handle as *mut Lv2Instance<$plugin_type>,
                );
            }

            unsafe extern "C" fn cleanup(handle: *mut c_void) {
                ::truce_lv2::cleanup::<$plugin_type>(handle as *mut Lv2Instance<$plugin_type>);
            }

            unsafe extern "C" fn extension_data(uri: *const c_char) -> *const c_void {
                ::truce_lv2::extension_data::<$plugin_type>(uri)
            }

            #[no_mangle]
            pub extern "C" fn lv2_descriptor(index: u32) -> *const LV2Descriptor {
                if index == 0 {
                    get_descriptor() as *const LV2Descriptor
                } else {
                    std::ptr::null()
                }
            }

            // --- UI descriptor ----------------------------------------------
            use ::truce_lv2::Lv2UiDescriptor;

            static UI_URI: OnceLock<std::ffi::CString> = OnceLock::new();
            static UI_DESCRIPTOR: OnceLock<Lv2UiDescriptor> = OnceLock::new();

            fn get_ui_descriptor() -> &'static Lv2UiDescriptor {
                UI_DESCRIPTOR.get_or_init(|| {
                    let info = <$plugin_type as Plugin>::info();
                    let uri_str = ::truce_lv2::ui_uri(&info);
                    let uri = UI_URI.get_or_init(|| std::ffi::CString::new(uri_str).unwrap());
                    ::truce_lv2::ui_descriptor::<$plugin_type>(uri)
                })
            }

            #[no_mangle]
            pub extern "C" fn lv2ui_descriptor(index: u32) -> *const Lv2UiDescriptor {
                if index == 0 {
                    get_ui_descriptor() as *const Lv2UiDescriptor
                } else {
                    std::ptr::null()
                }
            }

            /// Called by `cargo truce install --lv2` after copying the plugin
            /// shared library into the bundle directory. Writes `manifest.ttl`
            /// and `plugin.ttl` describing this plugin's ports and parameters.
            ///
            /// Returns 0 on success, nonzero on failure.
            #[no_mangle]
            pub extern "C" fn __truce_lv2_emit_bundle(
                bundle_dir: *const c_char,
                so_filename: *const c_char,
            ) -> i32 {
                if bundle_dir.is_null() || so_filename.is_null() {
                    return 1;
                }
                let dir = unsafe { std::ffi::CStr::from_ptr(bundle_dir) };
                let so = unsafe { std::ffi::CStr::from_ptr(so_filename) };
                let (Ok(dir_str), Ok(so_str)) = (dir.to_str(), so.to_str()) else {
                    return 2;
                };
                let path = std::path::Path::new(dir_str);
                match ::truce_lv2::emit_bundle::<$plugin_type>(path, so_str) {
                    Ok(()) => 0,
                    Err(_) => 3,
                }
            }
        }
    };
}

// Re-export AtomSequence for port-wiring & callers.
pub use atom::AtomSequence;

// Re-export UI types for the export_lv2 macro to use.
pub use ui::{ui_descriptor, Lv2UiDescriptor};

/// Derive the plugin's LV2 UI URI (plugin URI + "#ui").
pub fn ui_uri(info: &PluginInfo) -> String {
    format!("{}#ui", plugin_uri(info))
}
