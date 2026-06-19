//! LV2 format wrapper for the truce framework.
//!
//! Exports a `PluginExport` implementation as an LV2 plugin via the
//! [`export_lv2!`] macro. LV2's C ABI is small and stable, so we
//! hand-roll the bindings rather than pulling in a large `lv2-sys` crate.
//!
//! Port layout (default):
//!   - `0..num_in` - audio input (one port per channel)
//!   - `num_in..num_in+num_out` - audio output (one port per channel)
//!   - next N - control input (one port per parameter, float)
//!   - `atom_in_port` - single `AtomPort` for MIDI input (if plugin accepts MIDI)
//!
//! MIDI, State, and UI support live in sibling modules.

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
}

mod atom;
mod state;
mod types;
mod ui;
mod urid;

pub use types::*;

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;

use truce_core::buffer::RawBufferScratch;
use truce_core::cast::len_u32;
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginInfo;
use truce_core::plugin::PluginRuntime;
use truce_core::state::shared_plugin_state_hash;
use truce_core::wrapper::run_audio_block;
use truce_params::{ParamInfo, Params};

use crate::atom::AtomSequenceReader;
use crate::urid::{Urid, UridMap};

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
    /// Whether the input atom port should additionally advertise
    /// `midi:MidiEvent` support. The port itself always exists - hosts
    /// deliver `time:Position` through it regardless of whether the
    /// plugin consumes MIDI.
    pub accepts_midi_in: bool,
    pub has_midi_out: bool,
}

impl PortLayout {
    #[must_use]
    pub fn audio_in_start(&self) -> u32 {
        0
    }
    #[must_use]
    pub fn audio_out_start(&self) -> u32 {
        self.num_audio_in
    }
    #[must_use]
    pub fn control_start(&self) -> u32 {
        self.num_audio_in + self.num_audio_out
    }
    #[must_use]
    pub fn meter_start(&self) -> u32 {
        self.control_start() + self.num_params
    }
    /// Index of the DSP input atom port. Always present: carries
    /// `time:Position` (transport) for every plugin type and
    /// additionally `midi:MidiEvent` for instruments / note effects.
    #[must_use]
    pub fn atom_in_port(&self) -> u32 {
        self.meter_start() + self.num_meters
    }
    #[must_use]
    pub fn midi_out_port(&self) -> Option<u32> {
        if self.has_midi_out {
            Some(self.atom_in_port() + 1)
        } else {
            None
        }
    }
    /// Index of the DSP→UI notification atom port. Always present: the
    /// DSP writes host transport (and any future plugin-defined notify
    /// messages) here, and the UI listens via `ui:portNotification`.
    #[must_use]
    pub fn notify_out_port(&self) -> u32 {
        self.atom_in_port() + 1 + u32::from(self.has_midi_out)
    }
    #[must_use]
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
    /// Output control ports - one per `#[meter]` slot. We write the
    /// latest meter reading here at the end of each `run()` so the host
    /// forwards it to the UI via `port_event`.
    meter_ports: Vec<*mut f32>,
    /// Parameter/meter IDs for the meter slots, in port order.
    meter_ids: Vec<u32>,
    atom_in_port: *const AtomSequence,
    midi_out_port: *mut AtomSequence,
    notify_out_port: *mut AtomSequence,

    /// Last observed value on each control port; used to emit
    /// `ParamChange` events only when the host actually moved a knob.
    /// `None` means "never read" - the first poll after instantiation
    /// always emits, then subsequent polls only emit on diff.
    last_control: Vec<Option<f32>>,

    event_list: EventList,
    output_events: EventList,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    sub_event_scratch: EventList,
    /// Cached `Arc<P::Params>` handed to the chunker as its
    /// `&dyn Params` handle for `set_plain` calls. Pulled once at
    /// instantiate.
    params_arc: std::sync::Arc<P::Params>,
    /// `min_subblock_samples` from `truce.toml`'s `[automation]`
    /// table. Cached from `PluginInfo` at instantiate.
    min_subblock_samples: u32,

    urid_map: UridMap,
    /// Per-parameter URID → param-id mapping for the LV2 1.18 patch
    /// API. The host delivers parameter updates as `patch:Set` Objects
    /// whose `patch:property` is the parameter's interned URI; we look
    /// it up here to recover the truce `ParamInfo::id`. Built once at
    /// `instantiate()` by interning `<plugin_uri>#p_<id>` for every
    /// parameter - same string the TTL emits for the corresponding
    /// `lv2:Parameter` block (see `truce-build/src/lv2.rs`). A 0 URID
    /// (host didn't expose URID:map) leaves the table empty and the
    /// `patch:Set` path stays inert; the legacy control-port path
    /// still works.
    param_urid_to_id: Vec<(Urid, u32)>,

    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// here so the slice / per-channel-copy storage survives across
    /// `run()` invocations without re-allocating on the audio thread.
    /// LV2 hosts may connect an input and an output port to the same
    /// buffer (in-place processing); the scratch handles the
    /// alias-then-copy fallback internally.
    ///
    /// Parameterised by `P::Sample` so plugins that picked `f64`
    /// (via `prelude64`) get widening scratch transparently: the
    /// host wire is always `f32`, and the scratch widens on input
    /// then narrows on output around `plugin.process()`. Same-precision
    /// (`f32`) plugins stay zero-copy.
    scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,

    /// Shared transport slot - audio thread writes each block. LV2 UIs
    /// are out-of-process so the UI side still reads `None`; this slot
    /// exists so an in-process consumer (tests / DSP-side code) can
    /// observe host transport.
    transport_slot: Arc<truce_core::TransportSlot>,
}

// Raw pointers only - we never share an instance between threads. LV2 hosts
// drive a single instance from one thread at a time (audio thread for
// run(), main thread for everything else).
unsafe impl<P: PluginExport> Send for Lv2Instance<P> {}

// ---------------------------------------------------------------------------
// LV2 lifecycle callbacks
// ---------------------------------------------------------------------------

/// Build a `PortLayout` from a plugin instance's declared bus layout + params.
///
/// Caller passes in `&P` so the layout extraction reuses the existing
/// instance rather than constructing a fresh one. The TTL writer paths
/// build their own plugin and the LV2 `instantiate` callback already
/// owns one - both call this directly to skip a second `P::create()`.
///
/// # Panics
///
/// Panics if `P::bus_layouts()` is empty - same plugin-author
/// contract as [`truce_core::wrapper::first_bus_layout`]; zero-bus
/// plugins must return `vec![BusLayout::new()]` explicitly.
pub fn derive_port_layout<P: PluginExport>(plugin: &P) -> PortLayout {
    let layouts = P::bus_layouts();
    let default_layout = layouts
        .first()
        .expect("Plugin must declare at least one bus layout");
    let params = plugin.params();
    let param_count = len_u32(params.param_infos().len());
    let meter_count = len_u32(params.meter_ids().len());
    let info = P::info();
    PortLayout {
        num_audio_in: default_layout.total_input_channels(),
        num_audio_out: default_layout.total_output_channels(),
        num_params: param_count,
        num_meters: meter_count,
        accepts_midi_in: info.accepts_midi_in,
        has_midi_out: info.emits_midi,
    }
}

/// # Safety
/// Called by the LV2 host during plugin instantiation. `features` must be
/// a null-terminated array of `LV2_Feature` pointers (or null if none).
#[must_use]
pub unsafe fn instantiate<P: PluginExport>(
    sample_rate: f64,
    _bundle_path: *const c_char,
    features: *const *const LV2Feature,
) -> *mut Lv2Instance<P> {
    unsafe {
        let plugin = P::create();
        let layout = derive_port_layout::<P>(&plugin);
        let info = P::info();
        let param_infos = plugin.params().param_infos();
        let params_arc = plugin.params_arc();
        let min_subblock_samples = info.automation.min_subblock_samples;

        let control_port_count = layout.num_params as usize;
        let audio_in_count = layout.num_audio_in as usize;
        let audio_out_count = layout.num_audio_out as usize;
        let meter_ids = plugin.params().meter_ids();
        let meter_count = meter_ids.len();

        let urid_map = UridMap::from_features(features);

        // Build the per-param URID lookup the patch:Set decoder uses.
        // String must match the `<plugin_uri>#p_<id>` URI the TTL emits
        // for each `lv2:Parameter` block (see truce-build/src/lv2.rs).
        // Skipped when the host doesn't expose URID:map - the patch
        // path then stays inert and only the legacy control-port path
        // contributes parameter updates.
        let plugin_uri = truce_build::lv2::plugin_uri(info.url, info.bundle_id);
        let mut param_urid_to_id: Vec<(Urid, u32)> = Vec::with_capacity(param_infos.len());
        if urid_map.has_map() {
            for pi in &param_infos {
                let uri = format!("{plugin_uri}#p_{}", pi.id);
                let urid = urid_map.intern(&uri);
                if urid != 0 {
                    param_urid_to_id.push((urid, pi.id));
                }
            }
        }

        let instance = Box::new(Lv2Instance::<P> {
            plugin,
            sample_rate,
            max_block_size: 0,
            plugin_id_hash: shared_plugin_state_hash(&info),
            param_infos,
            layout,

            audio_inputs: vec![ptr::null(); audio_in_count],
            audio_outputs: vec![ptr::null_mut(); audio_out_count],
            control_ports: vec![ptr::null(); control_port_count],
            meter_ports: vec![ptr::null_mut(); meter_count],
            meter_ids,
            atom_in_port: ptr::null(),
            midi_out_port: ptr::null_mut(),
            notify_out_port: ptr::null_mut(),

            last_control: vec![None; control_port_count],

            event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
            output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
            sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
            params_arc,
            min_subblock_samples,

            urid_map,
            param_urid_to_id,

            scratch: RawBufferScratch::default(),

            transport_slot: truce_core::TransportSlot::new(),
        });
        Box::into_raw(instance)
    }
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer previously returned
/// from `instantiate::<P>()`.
pub unsafe fn connect_port<P: PluginExport>(
    handle: *mut Lv2Instance<P>,
    port: u32,
    data: *mut c_void,
) {
    unsafe {
        let inst = &mut *handle;
        // Snapshot the port-range boundaries up-front (cheap copies of
        // u32 start indices) so we can dispatch on `port` without
        // holding a borrow of `inst.layout` while writing back to a
        // sibling `inst.<port_array>` field. The alternative
        // (`layout.clone()` per call) would allocate on every
        // connect.
        let audio_in_start = inst.layout.audio_in_start();
        let audio_out_start = inst.layout.audio_out_start();
        let control_start = inst.layout.control_start();
        let meter_start = inst.layout.meter_start();
        let num_meters = inst.layout.num_meters;
        let atom_in_port = inst.layout.atom_in_port();
        let midi_out_port = inst.layout.midi_out_port();
        let notify_out_port = inst.layout.notify_out_port();

        if port < audio_out_start {
            inst.audio_inputs[(port - audio_in_start) as usize] = data as *const f32;
        } else if port < control_start {
            inst.audio_outputs[(port - audio_out_start) as usize] = data.cast::<f32>();
        } else if port < meter_start {
            inst.control_ports[(port - control_start) as usize] = data as *const f32;
        } else if port < meter_start + num_meters {
            inst.meter_ports[(port - meter_start) as usize] = data.cast::<f32>();
        } else if port == atom_in_port {
            inst.atom_in_port = data as *const AtomSequence;
        } else if Some(port) == midi_out_port {
            inst.midi_out_port = data.cast::<AtomSequence>();
        } else if port == notify_out_port {
            inst.notify_out_port = data.cast::<AtomSequence>();
        }
    }
}

/// LV2 has no `instantiate`-time max-block-length contract: the
/// `bufsz:maxBlockLength` option is delivered through `lv2:options`,
/// which few hosts implement. We pre-allocate scratch large enough to
/// cover practical session sizes (Pro Tools tops out at 8192 H/W
/// frames; jack/Carla and ardour have been observed up to ~16k).
/// Anything beyond that falls into the realloc edge case in `run()`.
const LV2_MAX_PREALLOC_BLOCK: usize = 16384;

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer.
pub unsafe fn activate<P: PluginExport>(handle: *mut Lv2Instance<P>) {
    unsafe {
        let inst = &mut *handle;
        inst.max_block_size = LV2_MAX_PREALLOC_BLOCK;
        inst.scratch.ensure_capacity(
            inst.audio_inputs.len(),
            inst.audio_outputs.len(),
            LV2_MAX_PREALLOC_BLOCK,
        );
        inst.plugin.reset(inst.sample_rate, LV2_MAX_PREALLOC_BLOCK);
        inst.plugin.params().set_sample_rate(inst.sample_rate);
        inst.plugin.params().snap_smoothers();
    }
}

/// # Safety
/// `handle` must be a valid `Lv2Instance<P>` pointer with port connections
/// established by prior calls to `connect_port()`. Audio and control port
/// memory must be valid for `n_samples`.
#[allow(clippy::too_many_lines)]
pub unsafe fn run<P: PluginExport>(handle: *mut Lv2Instance<P>, n_samples: u32) {
    let n = n_samples as usize;
    let ok = run_audio_block::<P>("LV2", || unsafe {
        let inst = &mut *handle;
        if n == 0 {
            return;
        }
        if n > inst.max_block_size {
            // Host exceeded our pre-allocated ceiling. Calling
            // `plugin.reset(sr, n)` would wipe filter delay lines /
            // oscillator phase mid-stream - plugins assume `reset()`
            // happens at quiescent points only. So we grow the input
            // scratch in place (a one-time realloc per increase) and
            // continue. The audio thread paying for `realloc` here is
            // a known cost of LV2's missing block-size contract.
            debug_assert!(
                false,
                "LV2 host delivered block of {n} samples, exceeding pre-allocated \
                 {LV2_MAX_PREALLOC_BLOCK} - input scratch will realloc on the audio thread",
            );
            inst.scratch
                .ensure_capacity(inst.audio_inputs.len(), inst.audio_outputs.len(), n);
            inst.max_block_size = n;
        }

        inst.event_list.clear();
        inst.output_events.clear();

        // Emit ParamChange events for any control port that moved since last
        // run. The event carries the PLAIN value - format wrappers agree on
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
            let changed = inst.last_control[i].is_none_or(|prev| (v - prev).abs() > f32::EPSILON);
            if changed {
                inst.last_control[i] = Some(v);
                let pid = inst.param_infos[i].id;
                let plain = f64::from(v);
                // `set_plain` is deferred to the chunker's apply pass
                // so smoothers see `set_target` at the event's sample.
                // LV2 control-port reads land at sample 0 of the block
                // so the chunker applies them on entry to the first
                // sub-block, equivalent to the prior eager behaviour.
                inst.event_list.push(Event {
                    sample_offset: 0,
                    body: EventBody::ParamChange {
                        id: pid,
                        value: plain,
                    },
                });
            }
        }

        // Decode MIDI + time:Position + patch:Set from the input atom
        // sequence port. The port is always declared so every plugin
        // type (effects included) can receive host transport and
        // sample-accurate parameter automation; MIDI events are only
        // parsed when the plugin's category opts in.
        let mut transport = TransportInfo::default();
        if !inst.atom_in_port.is_null() {
            let reader = AtomSequenceReader::new(inst.atom_in_port, &inst.urid_map);

            // LV2 1.18+ host-→-plugin parameter automation. Each
            // `patch:Set` Object's `patch:property` identifies the
            // target parameter (looked up against the per-instance
            // URID → param-id table built at instantiate); the
            // event's `time_frames` becomes the within-block
            // `sample_offset`. The chunker downstream splits the
            // audio block at each emitted ParamChange.
            //
            // Coexists with the legacy control-port path below: if a
            // host writes both (e.g. mirrors automation onto the
            // control port at sample 0), the smoother sees two
            // set_target calls for the same value - harmless.
            if !inst.param_urid_to_id.is_empty() {
                reader.for_each_patch_set(|sample_offset, property, value| {
                    if let Some(&(_, pid)) =
                        inst.param_urid_to_id.iter().find(|(u, _)| *u == property)
                    {
                        inst.event_list.push(Event {
                            sample_offset,
                            body: EventBody::ParamChange { id: pid, value },
                        });
                    }
                });
            }

            if inst.layout.accepts_midi_in {
                reader.for_each_midi(|sample_offset, bytes| {
                    // SysEx is delivered as a single MIDI atom whose
                    // payload starts with `0xF0` and ends with `0xF7`.
                    // The framework's `EventBody::SysEx` carries only
                    // the inner bytes - strip the framing here so
                    // plug-in code never sees the start/end markers.
                    // A pool-full push gets dropped silently; truncating
                    // a `SysEx` makes it corrupt by definition, so the
                    // event simply doesn't reach the plug-in.
                    if let Some(0xF0) = bytes.first().copied() {
                        let end = if bytes.last().copied() == Some(0xF7) {
                            bytes.len() - 1
                        } else {
                            bytes.len()
                        };
                        let inner = &bytes[1..end];
                        let _ = inst.event_list.push_sysex(sample_offset, inner);
                        return;
                    }
                    if let Some(event) = atom::midi_bytes_to_event(sample_offset, bytes) {
                        inst.event_list.push(event);
                    }
                });
            }
            reader.apply_time_position(&mut transport);
        }

        // Build AudioBuffer from port pointers via the shared
        // `RawBufferScratch::build` helper. The helper owns the
        // raw-pointer-to-slice conversion plus the alias-detection
        // copy-into-scratch fallback (LV2 hosts may connect an input
        // and an output port to the same buffer for in-place
        // processing). Plugins that want pass-through must do
        // `output.copy_from_slice(input)` themselves - `build` does
        // not auto-copy because that would clobber the previous-block
        // tail delay / reverb feedback paths read from the output.
        //
        // Reborrow `inst` through a raw pointer for the scratch +
        // event-list arms so each can hold an independent `&mut`
        // through the call. SAFETY: single-threaded LV2 instance
        // (`run` is called on one thread at a time per host
        // contract), so the simultaneous `&mut`s never alias an
        // overlapping field - `scratch`, `output_events`, and the
        // immutable reads of `audio_inputs` / `audio_outputs` /
        // `event_list` / `sample_rate` are disjoint.
        {
            let inst_ptr: *mut Lv2Instance<P> = inst;
            let s = &mut *inst_ptr;
            let in_ptrs = s.audio_inputs.as_ptr();
            let out_ptrs = s.audio_outputs.as_mut_ptr();
            let num_in = u32::try_from(s.audio_inputs.len()).unwrap_or(u32::MAX);
            let num_out = u32::try_from(s.audio_outputs.len()).unwrap_or(u32::MAX);
            let mut audio = s.scratch.build(
                in_ptrs,
                out_ptrs,
                num_in,
                num_out,
                n_samples,
                P::supports_in_place(),
            );
            inst.transport_slot.write(&transport);
            let mut transport_snap = transport;
            let chunk_args = ChunkedProcess {
                events: &inst.event_list,
                sub_event_scratch: &mut inst.sub_event_scratch,
                transport: &mut transport_snap,
                sample_rate: inst.sample_rate,
                output_events: &mut inst.output_events,
                params_fn: None,
                meters_fn: None,
                param_infos: &inst.param_infos,
                min_subblock_samples: inst.min_subblock_samples,
            };
            let _ = process_chunked(
                &mut inst.plugin,
                inst.params_arc.as_ref() as &dyn Params,
                &mut audio,
                chunk_args,
            );
            // End the `audio` borrow before reaching back into `scratch`.
            let _ = audio;
            // Narrow rendered output back to host f32 pointers when
            // the plugin's `Sample = f64`. No-op for f32 plugins.
            s.scratch.finish_widening_f32(out_ptrs, num_out, n_samples);
        }

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
            atom::write_midi_out_sequence(inst.midi_out_port, &inst.output_events, &inst.urid_map);
        }

        // Forward transport to the UI as a time:Position atom on the
        // notify-out port. Hosts deliver this to the UI's port_event each
        // block; the UI decodes it and updates its shared `TransportSlot`.
        if !inst.notify_out_port.is_null() {
            atom::write_time_position_sequence(inst.notify_out_port, &transport, &inst.urid_map);
        }
    });
    if !ok {
        // Panic in plugin.process() - zero output port buffers so
        // the host doesn't keep playing whatever stale samples were
        // there when DSP died.
        unsafe {
            let inst = &mut *handle;
            for &ptr in &inst.audio_outputs {
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, n);
                }
            }
        }
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
    unsafe {
        if !handle.is_null() {
            drop(Box::from_raw(handle));
        }
    }
}

/// # Safety
/// `uri` must be a valid null-terminated C string or null.
#[must_use]
pub unsafe fn extension_data<P: PluginExport>(uri: *const c_char) -> *const c_void {
    unsafe {
        if uri.is_null() {
            return ptr::null();
        }
        let Ok(uri) = CStr::from_ptr(uri).to_str() else {
            return ptr::null();
        };
        if uri == state::LV2_STATE__INTERFACE_URI {
            return ptr::from_ref(state::state_interface::<P>()).cast::<c_void>();
        }
        ptr::null()
    }
}

// ---------------------------------------------------------------------------
// Plugin URI
// ---------------------------------------------------------------------------

/// Derive the plugin's LV2 URI from its `PluginInfo`. Thin wrapper
/// around [`truce_build::lv2::plugin_uri`] - the single source of
/// truth shared with the manifest writer in `truce-derive::lv2_emit`.
/// Both paths MUST produce the same string, or hosts will discover
/// the plugin under one URI then fail to look up the saved project's
/// stored URI.
#[must_use]
pub fn plugin_uri(info: &PluginInfo) -> String {
    truce_build::lv2::plugin_uri(info.url, info.bundle_id)
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
    #[allow(clippy::too_many_arguments)]
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
        let uri = CString::new(plugin_uri(info)).unwrap_or_default();
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

            use ::truce_lv2::__macro_deps::truce_core::plugin::PluginRuntime;
            use ::truce_lv2::{DescriptorHolder, LV2Descriptor, LV2Feature, Lv2Instance};

            static DESCRIPTOR: OnceLock<DescriptorHolder> = OnceLock::new();

            fn get_descriptor() -> &'static LV2Descriptor {
                let holder = DESCRIPTOR.get_or_init(|| {
                    let info = <$plugin_type as PluginRuntime>::info();
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
                ::truce_lv2::deactivate::<$plugin_type>(handle as *mut Lv2Instance<$plugin_type>);
            }

            unsafe extern "C" fn cleanup(handle: *mut c_void) {
                ::truce_lv2::cleanup::<$plugin_type>(handle as *mut Lv2Instance<$plugin_type>);
            }

            unsafe extern "C" fn extension_data(uri: *const c_char) -> *const c_void {
                ::truce_lv2::extension_data::<$plugin_type>(uri)
            }

            #[unsafe(no_mangle)]
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
                    let info = <$plugin_type as PluginRuntime>::info();
                    let uri_str = ::truce_lv2::ui_uri(&info);
                    let uri =
                        UI_URI.get_or_init(|| std::ffi::CString::new(uri_str).unwrap_or_default());
                    ::truce_lv2::ui_descriptor::<$plugin_type>(uri)
                })
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn lv2ui_descriptor(index: u32) -> *const Lv2UiDescriptor {
                if index == 0 {
                    get_ui_descriptor() as *const Lv2UiDescriptor
                } else {
                    std::ptr::null()
                }
            }
        }
    };
}

// Re-export AtomSequence for port-wiring & callers.
pub use atom::AtomSequence;

// Re-export UI types for the export_lv2 macro to use.
pub use ui::{Lv2UiDescriptor, ui_descriptor};

/// Derive the plugin's LV2 UI URI (plugin URI + "#ui"). Thin wrapper
/// around [`truce_build::lv2::ui_uri`] - same single-source-of-truth
/// posture as [`plugin_uri`].
#[must_use]
pub fn ui_uri(info: &PluginInfo) -> String {
    truce_build::lv2::ui_uri(info.url, info.bundle_id)
}

#[cfg(test)]
mod uri_consistency_tests {
    //! Pins the LV2 URI agreement: the manifest writer
    //! (`truce-derive::lv2_emit`) and this crate's runtime
    //! `plugin_uri` MUST produce the same string for the same
    //! `(vendor_url, bundle_id)`. Both now delegate to
    //! `truce_build::lv2::plugin_uri`, so this test guarantees the
    //! manifest-vs-runtime contract by checking the runtime call
    //! against the same `truce_build` function the manifest writer
    //! uses - any drift on either side breaks this test.
    use super::{plugin_uri, ui_uri};
    use truce_core::info::{PluginCategory, PluginInfo};

    fn info_with(url: &'static str, bundle_id: &'static str) -> PluginInfo {
        PluginInfo {
            name: "Test",
            vendor: "Vendor",
            url,
            version: "0.0.0",
            category: PluginCategory::Effect,
            accepts_midi_in: false,
            emits_midi: false,
            bundle_id,
            vst3_id: "",
            clap_id: "",
            fourcc: *b"Test",
            au_type: *b"aufx",
            au_manufacturer: *b"Vend",
            aax_id: None,
            aax_category: None,
            vst3_subcategory: None,
            vst3_name: None,
            clap_name: None,
            vst2_name: None,
            au_name: None,
            au3_name: None,
            aax_name: None,
            lv2_name: None,
            preset_user_dir: None,
            mute_preview_output: false,
            automation: truce_core::info::AutomationConfig::DEFAULT,
        }
    }

    #[test]
    fn runtime_uri_matches_manifest_uri_with_vendor_url() {
        let info = info_with("https://example.com", "my-gain");
        assert_eq!(
            plugin_uri(&info),
            truce_build::lv2::plugin_uri("https://example.com", "my-gain"),
        );
    }

    #[test]
    fn runtime_uri_matches_manifest_uri_with_trailing_slash() {
        let info = info_with("https://example.com/", "my-gain");
        assert_eq!(
            plugin_uri(&info),
            truce_build::lv2::plugin_uri("https://example.com/", "my-gain"),
        );
    }

    #[test]
    fn runtime_uri_matches_manifest_uri_empty_url() {
        let info = info_with("", "my-gain");
        assert_eq!(
            plugin_uri(&info),
            truce_build::lv2::plugin_uri("", "my-gain"),
        );
    }

    #[test]
    fn runtime_ui_uri_matches_manifest_ui_uri() {
        let info = info_with("https://example.com", "my-gain");
        assert_eq!(
            ui_uri(&info),
            truce_build::lv2::ui_uri("https://example.com", "my-gain"),
        );
    }
}
