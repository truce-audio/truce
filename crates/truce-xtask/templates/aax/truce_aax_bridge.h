/**
 * truce AAX bridge — C ABI between the AAX template and the Rust plugin.
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

/* Plugin descriptor — read once at load time. */
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
    int is_instrument;          /* 1 for instruments, 0 for effects */
    uint32_t category;          /* AAX_ePlugInCategory bitmask */
    int has_editor;             /* 1 if plugin provides custom GUI */
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

/* Plugin descriptor. Returned pointer is valid for the library lifetime. */
void truce_aax_get_descriptor(TruceAaxDescriptor* out);

/* Parameter info for each parameter (0-indexed). */
void truce_aax_get_param_info(uint32_t index, TruceAaxParamInfo* out);

/* Instance lifecycle. */
void* truce_aax_create(void);
void  truce_aax_destroy(void* ctx);
void  truce_aax_reset(void* ctx, double sample_rate, uint32_t max_frames);

/* Audio processing. `transport` may be NULL when the template did not
 * manage to query AAX_ITransport for this block. */
void truce_aax_process(void* ctx,
    const float** inputs, float** outputs,
    uint32_t num_input_channels, uint32_t num_output_channels,
    uint32_t num_frames,
    const TruceAaxMidiEvent* midi_events, uint32_t num_midi_events,
    const TruceAaxTransportSnapshot* transport);

/* Parameters (plain values, not normalized). */
double truce_aax_get_param(void* ctx, uint32_t id);
void   truce_aax_set_param(void* ctx, uint32_t id, double value);
void   truce_aax_format_param(void* ctx, uint32_t id, double value,
                               char* out, uint32_t out_len);

/* State serialization. */
uint32_t truce_aax_save_state(void* ctx, uint8_t** out_data);
void     truce_aax_load_state(void* ctx, const uint8_t* data, uint32_t len);
void     truce_aax_free_state(uint8_t* data, uint32_t len);

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
