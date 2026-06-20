#ifndef AU_SHIM_TYPES_H
#define AU_SHIM_TYPES_H

#include <stdint.h>

// Shared C types for the Rust ↔ ObjC/C boundary.
// Used by both au_shim.m (AU v3) and au_v2_shim.c (AU v2).

// SysEx byte-pool capacity, mirrored from `truce_core::SYSEX_POOL_PREALLOC`.
// Defined here so the Swift template (which can't import Rust consts)
// can size its per-render output scratch buffer without re-declaring
// the magic number. Keep in sync with the Rust constant.
#define TRUCE_SYSEX_POOL_PREALLOC (128 * 1024)

typedef struct {
    uint8_t component_type[4];
    uint8_t component_subtype[4];
    uint8_t component_manufacturer[4];
    const char *name;
    const char *vendor;
    uint32_t version;
    uint32_t num_inputs;
    uint32_t num_outputs;
    /* Param ID flagged as IS_BYPASS, or UINT32_MAX for "no bypass param".
     * The shim routes kAudioUnitProperty_BypassEffect get/set through
     * this ID so the host's master-bypass UI tracks the param value. */
    uint32_t bypass_param_id;
    /* 1 if the plugin emits MIDI to the host. The v2 shim gates the
     * MIDIOutputCallback property and the v3 shim gates
     * `MIDIOutputNames` on this, so a pure audio effect doesn't show
     * a phantom "MIDI Out" port in the host UI. */
    int32_t has_midi_output;
    /* 1 if the plugin accepts MIDI input. The v2 shim gates its
     * `MusicDeviceMIDIEvent` handler lookup on this - decoupled from
     * the `aumu` component type so an `aumf` MusicEffect (audio effect
     * that opts into MIDI input) is also handed events. */
    int32_t accepts_midi_in;
} AuPluginDescriptor;

typedef struct {
    uint32_t id;
    const char *name;
    double min;
    double max;
    double default_value;
    uint32_t step_count;
    const char *unit;
    const char *group;
    /* Default host MIDI-learn binding, surfaced through
     * kAudioUnitProperty_AllParameterMIDIMappings. midi_status is the
     * MIDI status high-nibble (0xB0 control change, 0xE0 pitch bend,
     * 0xD0 channel pressure, 0xC0 program change) or 0 when the param
     * declares no binding. midi_data1 is the CC number (0 for non-CC
     * sources). midi_channel is the wire channel 0..=15, or -1 for any
     * channel. */
    uint8_t midi_status;
    uint8_t midi_data1;
    int16_t midi_channel;
} AuParamDescriptor;

typedef struct {
    uint32_t sample_offset;
    uint8_t status;
    uint8_t data1;
    uint8_t data2;
    uint8_t _pad;
} AuMidiEvent;

/* Universal MIDI Packet container - carries MIDI 2.0 channel-voice
 * messages (64-bit UMPs) and forward-compat slots for SysEx-8 / data
 * (128-bit UMPs). Used only by the AU v3 path; AU v2 hosts deliver
 * legacy 3-byte MIDI 1.0 via `AuMidiEvent` and pass `NULL`/`0` for the
 * MIDI 2.0 array. The MSB of `words[0]` carries the UMP message type
 * nibble (0x2_ = MIDI 1.0 CV, 0x4_ = MIDI 2.0 CV, …) - see the M2-104
 * spec, §2.1.4. */
typedef struct {
    uint32_t sample_offset;
    uint32_t words[4];
} AuMidi2Event;

/* Host-side parameter automation event. The AU v3 shim decodes
 * AURenderEvent.parameter / .parameterRamp entries into this shape
 * (one row per host event), with `sample_offset` relative to the
 * start of the current render block. The Rust process callback
 * converts each row into an `EventBody::ParamChange` with the same
 * offset so the per-sample chunker splits the audio block at each
 * automation point. AU v2 has no per-sample parameter automation
 * in its API (`AudioUnitSetParameter` carries no sample-offset), so
 * the v2 shim passes `NULL`/`0` for this array. */
typedef struct {
    uint32_t sample_offset;
    uint32_t param_id;
    float value;
} AuParamEvent;

/* Transport snapshot filled by the AU v2 / v3 shim from
 * HostCallbackInfo (v2) or AUAudioUnit.musicalContextBlock +
 * transportStateBlock (v3). Fields default to 0 / false when the host
 * does not report them. */
typedef struct {
    int32_t valid;          /* 1 if any host callback responded */
    int32_t playing;
    int32_t recording;
    int32_t loop_active;
    int32_t time_sig_num;   /* 0 = host did not report */
    int32_t time_sig_den;   /* 0 = host did not report */
    double tempo;           /* 0 = host did not report */
    double position_samples;
    double position_beats;  /* 0 = host did not report */
    double bar_start_beats;
    double loop_start_beats;
    double loop_end_beats;
} AuTransportSnapshot;

typedef struct {
    void *(*create)(void);
    void (*destroy)(void *ctx);
    void (*reset)(void *ctx, double sample_rate, uint32_t max_frames);
    /* `transport` may be NULL when the host did not provide any transport
     * info for this block. */
    void (*process)(void *ctx,
                    const float **inputs, float **outputs,
                    uint32_t num_input_channels, uint32_t num_output_channels,
                    uint32_t num_frames,
                    const AuMidiEvent *events, uint32_t num_events,
                    const AuMidi2Event *events2, uint32_t num_events2,
                    const AuParamEvent *param_events, uint32_t num_param_events,
                    const AuTransportSnapshot *transport);
    uint32_t (*param_count)(void *ctx);
    /* Per-param descriptors are read from `g_param_descriptors`
     * rather than via callback (set once at registration). */
    double (*param_get_value)(void *ctx, uint32_t id);
    void (*param_set_value)(void *ctx, uint32_t id, double value);
    uint32_t (*param_format_value)(void *ctx, uint32_t id, double value,
                                    char *out, uint32_t out_len);
    void (*state_save)(void *ctx, uint8_t **out_data, uint32_t *out_len);
    void (*state_load)(void *ctx, const uint8_t *data, uint32_t len);
    void (*state_free)(uint8_t *data, uint32_t len);
    /* Plugin → host MIDI output. The Rust side filters its event
     * queue to events that fit in 3-byte MIDI 1.0 packets so the
     * shim can iterate `0..count` without checking for skipped
     * slots. Mirrors the input direction's `AuMidiEvent` shape. */
    uint32_t (*output_event_count)(void *ctx);
    void (*output_event_at)(void *ctx, uint32_t index, AuMidiEvent *out);
    /* SysEx output. The Rust side iterates over EventBody::SysEx
     * variants in its output_events queue; the shim drains via
     * `output_sysex_count` + `output_sysex_at`, fragments each
     * payload into UMP SysEx-8 packets, and emits through the
     * AU v3 host's `midiOutputEventListBlock` (or - on older AU v2
     * hosts that don't expose that block - the legacy
     * `midiOutputCallback` with framed 0xF0/0xF7 bytestream).
     *
     * `out_bytes` points into the plugin's EventList SysEx pool;
     * valid until the next process() call clears the pool, which
     * is after the shim has copied / emitted. */
    uint32_t (*output_sysex_count)(void *ctx);
    void (*output_sysex_at)(void *ctx, uint32_t index,
                            uint32_t *out_delta_frames,
                            const uint8_t **out_bytes,
                            uint32_t *out_len);
    /* GUI */
    int32_t (*gui_has_editor)(void *ctx);
    void (*gui_get_size)(void *ctx, uint32_t *w, uint32_t *h);
    void (*gui_open)(void *ctx, void *parent);
    void (*gui_close)(void *ctx);
    /* Whether the editor advertised `can_resize() == true`. The AU
     * v3 Swift shim consults this in `viewDidLayoutSubviews` to
     * decide whether to propagate the host's bounds change to the
     * editor via `gui_set_size`. AU v2 hosts don't have a
     * matching mechanism today; that wiring is tracked
     * separately. */
    int32_t (*gui_can_resize)(void *ctx);
    /* Host-driven set_size. The Swift v3 shim calls this when the
     * host's container view changes our bounds (drag-resize). w/h
     * are in logical points. */
    void (*gui_set_size)(void *ctx, uint32_t w, uint32_t h);
    /* Factory presets, backing kAudioUnitProperty_FactoryPresets.
     * Sourced from the .trucepreset files `cargo truce install`
     * bundles into the component's Contents/Resources/Presets/.
     * `count == 0` means none shipped; the shim then reports the
     * property as invalid (matching AUs without factory presets).
     * `factory_preset_name` returns a UTF-8 string owned by the
     * Rust side, valid for the process lifetime.
     *
     * Fields are append-only past this point: the v3 appex shim
     * can be compiled against a newer header than the plugin
     * binary's struct, so earlier offsets must never shift. */
    uint32_t (*factory_preset_count)(void *ctx);
    const char *(*factory_preset_name)(void *ctx, uint32_t index);
    /* Load the index-th factory preset into the plugin - the same
     * apply path as state_load. Returns 1 on success. */
    int32_t (*factory_preset_load)(void *ctx, uint32_t index);
} AuCallbacks;

// Globals shared between v2 and v3 shims.
// Populated by truce_au_register() (called from the constructor).
extern const AuPluginDescriptor *g_descriptor;
extern const AuCallbacks *g_callbacks;
extern const AuParamDescriptor *g_param_descriptors;
extern uint32_t g_num_params;

// Rust-side init function - generated by export_au! macro.
extern void truce_au_init(void);

// FourCharCode helper (big-endian UInt32 from byte array).
#define FOURCC(b) (((uint32_t)(b)[0] << 24) | ((uint32_t)(b)[1] << 16) | \
                   ((uint32_t)(b)[2] << 8)  | (uint32_t)(b)[3])

#endif // AU_SHIM_TYPES_H
