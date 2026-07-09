/**
 * truce AAX bridge - C ABI between the AAX template and the Rust plugin.
 *
 * The AAX template (C++) dlopen()s the Rust cdylib and resolves these
 * symbols to delegate all plugin logic to the Rust side.
 */

#ifndef TRUCE_AAX_BRIDGE_H
#define TRUCE_AAX_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Bumped any time the bridge ABI shape changes. The bridge loader
 * compares this against the cdylib's `truce_aax_abi_version()` and
 * refuses to load a mismatched pair - protects against a manual
 * cdylib swap against a stale C++ template (which would otherwise
 * read fields at the wrong offset).
 *
 * Version history:
 *   1 → 2: initial range_type field on TruceAaxParamInfo (log/discrete).
 *   2 → 3: SysEx I/O - push_sysex_input + output_sysex_count +
 *           output_sysex_at exports.
 *   3 → 4: legacy-state migration - legacy_chunk_ids on the
 *           descriptor + load_state_foreign export.
 *   4 → 5: offline-bounce awareness - set_render_mode export
 *           (host EnteringOfflineMode / ExitingOfflineMode).
 *   5 → 6: latency reporting - latency export (template pushes it
 *           via AAX_IController::SetSignalLatency).
 *   6 → 7: multiple bus layouts - layout_in_channels / layout_out_channels
 *           / num_layouts on the descriptor (one AAX stem-format config
 *           per bus_layouts() entry). */
#define TRUCE_AAX_ABI_VERSION 7u

/* Capacity of TruceAaxDescriptor::legacy_chunk_ids. */
#define TRUCE_AAX_MAX_LEGACY_CHUNKS 8u

/* Wire values for TruceAaxParamInfo::range_type. The shim picks the
 * matching AAX_ITaperDelegate per param so AAX's normalize/denormalize
 * mirrors what truce-params does on the Rust side - a mismatched taper
 * (e.g. AAX-linear over a log-ranged knob) round-trips editor writes
 * back through RenderAudio as a different plain value, which the GUI
 * sees as the knob fighting the user mid-drag. */
#define TRUCE_AAX_RANGE_LINEAR   0u
#define TRUCE_AAX_RANGE_LOG      1u
#define TRUCE_AAX_RANGE_DISCRETE 2u

/* Plugin descriptor - read once at load time. */
typedef struct {
    const char* name;           /* Display name */
    const char* vendor;         /* Vendor name */
    uint32_t version;           /* Version as integer */
    uint32_t num_inputs;        /* 0 for instruments */
    uint32_t num_outputs;       /* Typically 2 (stereo) */
    uint32_t num_params;        /* Parameter count */
    int32_t manufacturer_id;    /* FourCC */
    int32_t product_id;         /* FourCC */
    int32_t plugin_id;          /* FourCC (unique per stem format) */
    int wants_input_midi;       /* 1 if the plugin accepts MIDI input -
                                 * gates the LocalInput MIDI node and the
                                 * per-render MIDI event collection. */
    int emits_midi;             /* 1 if the plugin emits MIDI to the host
                                 * - gates the LocalOutput MIDI node. */
    uint32_t category;          /* AAX_ePlugInCategory bitmask */
    int has_editor;             /* 1 if plugin provides custom GUI */
    uint32_t bypass_param_id;   /* IS_BYPASS-flagged param ID, or
                                 * UINT32_MAX for no bypass param. The
                                 * AAX C++ template registers this as
                                 * the master bypass via
                                 * cDefaultMasterBypassID. */
    /* Chunk fourccs a pre-truce build stored its state under
     * (`aax_chunk_ids` in truce.toml's [plugin.legacy_state]). The
     * template declares them alongside truce's own chunk so Pro Tools
     * hands old sessions' chunks to SetChunk, which routes them to
     * truce_aax_load_state_foreign / the plugin's migrate_state. */
    uint32_t num_legacy_chunk_ids;
    uint32_t legacy_chunk_ids[8]; /* TRUCE_AAX_MAX_LEGACY_CHUNKS */
    /* Main-bus (in, out) channel counts of each bus_layouts() entry. The
     * describe template maps each to an AAX stem format and registers one
     * component config per layout, so Pro Tools offers every declared I/O
     * width. num_layouts == 0 keeps the legacy mono/stereo-only describe
     * (single-layout plugins); arrays are num_layouts long. */
    const int16_t* layout_in_channels;
    const int16_t* layout_out_channels;
    uint32_t num_layouts;
} TruceAaxDescriptor;

/* Parameter info. */
typedef struct {
    uint32_t id;
    const char* name;
    double min;
    double max;
    double default_value;
    uint32_t step_count;
    const char* unit;           /* "dB", "Hz", "%", "" etc. */
    uint8_t range_type;         /* One of TRUCE_AAX_RANGE_*. */
    uint8_t _pad[7];            /* Match Rust-side trailing pad. */
} TruceAaxParamInfo;

/* MIDI event. */
typedef struct {
    uint32_t delta_frames;
    uint8_t status;
    uint8_t data1;
    uint8_t data2;
    uint8_t _pad;
} TruceAaxMidiEvent;

/* Transport snapshot filled by the AAX template each render from
 * AAX_ITransport. Fields default to 0 / false when Pro Tools does not
 * report them. */
typedef struct {
    int32_t valid;              /* 1 if AAX_ITransport returned anything */
    int32_t playing;
    int32_t recording;
    int32_t loop_active;
    int32_t time_sig_num;       /* 0 = not reported */
    int32_t time_sig_den;
    double  tempo;              /* 0 = not reported */
    double  position_samples;
    double  position_beats;
    double  bar_start_beats;
    double  loop_start_beats;
    double  loop_end_beats;
} TruceAaxTransportSnapshot;

/* GUI editor info returned by editor_create. */
typedef struct {
    int has_editor;
    uint32_t width;
    uint32_t height;
} TruceAaxEditorInfo;

/* Callback vtable for GUI → host parameter gestures. */
typedef struct {
    void* aax_ctx;
    void (*touch_param)(void* aax_ctx, uint32_t param_id);
    void (*set_param)(void* aax_ctx, uint32_t param_id, double normalized);
    void (*release_param)(void* aax_ctx, uint32_t param_id);
    int  (*request_resize)(void* aax_ctx, uint32_t w, uint32_t h);
} TruceAaxGuiCallbacks;

/* --- Functions exported by the Rust cdylib --- */

/* Bridge ABI version. Must equal `TRUCE_AAX_ABI_VERSION` above. */
uint32_t truce_aax_abi_version(void);

/* Plugin descriptor. Returned pointer is valid for the library lifetime. */
void truce_aax_get_descriptor(TruceAaxDescriptor* out);

/* Parameter info for each parameter (0-indexed). */
void truce_aax_get_param_info(uint32_t index, TruceAaxParamInfo* out);

/* Instance lifecycle. */
void* truce_aax_create(void);
void  truce_aax_destroy(void* ctx);
void  truce_aax_reset(void* ctx, double sample_rate, uint32_t max_frames);

/* Render-mode signal, as a ProcessMode discriminant (0 realtime, 1
 * buffered, 2 offline). The template calls this from its
 * NotificationReceived override when the host posts
 * AAX_eNotificationEvent_EnteringOfflineMode / ExitingOfflineMode
 * (offline bounce). The Rust side stashes it in an atomic that reset
 * and every process block read. */
void  truce_aax_set_render_mode(void* ctx, uint32_t mode);

/* Current plugin latency in samples, for host delay compensation. The
 * template polls this from its TimerWakeup idle callback and pushes
 * changes to the host via AAX_IController::SetSignalLatency. */
uint32_t truce_aax_latency(void* ctx);

/* Audio processing. `transport` may be NULL when the template did not
 * manage to query AAX_ITransport for this block. */
void truce_aax_process(void* ctx,
    const float** inputs, float** outputs,
    uint32_t num_input_channels, uint32_t num_output_channels,
    uint32_t num_frames,
    const TruceAaxMidiEvent* midi_events, uint32_t num_midi_events,
    const TruceAaxTransportSnapshot* transport);

/* Drain plugin-emitted MIDI events from the most recent process() call.
 * Call _count first; iterate _at(0..count) to read each packet. The
 * C++ template forwards each to AAX_IMIDINode::PostMIDIPacket on the
 * LocalOutput node it registered in its hand-built component
 * descriptor. Only encodable events (NoteOn/Off, CC, channel/poly
 * pressure, pitch bend, program change) are surfaced - see
 * `try_encode_aax_midi` in truce-aax/src/lib.rs for the predicate. */
uint32_t truce_aax_output_event_count(void* ctx);
void     truce_aax_output_event_at(void* ctx, uint32_t index,
                                    TruceAaxMidiEvent* out);

/* SysEx input - the C++ template walks the host's AAX_CMidiStream
 * looking for `0xF0` start bytes and accumulates across consecutive
 * AAX_CMidiPackets until it hits `0xF7`. Once a complete message
 * is reassembled, it calls this once with the inner bytes (no
 * `0xF0`/`0xF7` framing). Pointer is valid for the duration of
 * the call; Rust copies into its `EventList` SysEx pool.
 *
 * Per the AAX SDK (`AAX.h:605-608`):
 *   "SysEx messages greater than 4 bytes in length can be
 *    transmitted via a series of concurrent AAX_CMidiPacket
 *    objects in mBuffer." */
void     truce_aax_push_sysex_input(void* ctx, uint32_t delta_frames,
                                     const uint8_t* bytes, uint32_t len);

/* SysEx output - Rust reports the number of SysEx-shaped events the
 * plug-in queued during process(), and provides each event's inner
 * bytes. The C++ template fragments each event into a sequence of
 * ≤4-byte AAX_CMidiPackets framed with `0xF0` ... `0xF7` and posts
 * them via `PostMIDIPacket` on the LocalOutput node. Pointer is
 * valid until the next process() clears the pool. */
uint32_t truce_aax_output_sysex_count(void* ctx);
void     truce_aax_output_sysex_at(void* ctx, uint32_t index,
                                    uint32_t* out_delta_frames,
                                    const uint8_t** out_bytes,
                                    uint32_t* out_len);

/* Parameters (plain values, not normalized). */
double truce_aax_get_param(void* ctx, uint32_t id);
void   truce_aax_set_param(void* ctx, uint32_t id, double value);
void   truce_aax_format_param(void* ctx, uint32_t id, double value,
                               char* out, uint32_t out_len);

/* State serialization. */
uint32_t truce_aax_save_state(void* ctx, uint8_t** out_data);
void     truce_aax_load_state(void* ctx, const uint8_t* data, uint32_t len);
void     truce_aax_free_state(uint8_t* data, uint32_t len);
/* Bytes found under a legacy chunk id (see the descriptor's
 * legacy_chunk_ids): offered to the plugin's migrate_state hook.
 * Returns 1 when the plugin translated and accepted them. */
int32_t  truce_aax_load_state_foreign(void* ctx, uint32_t chunk_id,
                                      const uint8_t* data, uint32_t len);

/* GUI editor. */
void truce_aax_editor_create(void* ctx, TruceAaxEditorInfo* out);
void truce_aax_editor_open(void* ctx, void* parent_view, int platform,
                            const TruceAaxGuiCallbacks* callbacks);
void truce_aax_editor_close(void* ctx);
void truce_aax_editor_idle(void* ctx);
int  truce_aax_editor_get_size(void* ctx, uint32_t* w, uint32_t* h);

#ifdef __cplusplus
}
#endif

#endif /* TRUCE_AAX_BRIDGE_H */
