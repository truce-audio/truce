/**
 * Truce VST2 shim.
 *
 * Implements the AEffect interface, calling back into Rust for all
 * plugin logic. Clean-room implementation — no Steinberg SDK headers.
 */

#include "vst2_types.h"
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#ifdef _MSC_VER
#define VST2_EXPORT __declspec(dllexport)
#else
#define VST2_EXPORT __attribute__((visibility("default")))
#endif

/* Globals populated by truce_vst2_register() from Rust. */
const Vst2PluginDescriptor* g_vst2_descriptor = NULL;
const Vst2Callbacks* g_vst2_callbacks = NULL;
const Vst2ParamDescriptor* g_vst2_params = NULL;
uint32_t g_vst2_num_params = 0;

static int g_vst2_registered = 0;

void truce_vst2_register(
    const Vst2PluginDescriptor* descriptor,
    const Vst2Callbacks* callbacks,
    const Vst2ParamDescriptor* params,
    uint32_t num_params
) {
    if (g_vst2_registered) return;
    g_vst2_descriptor = descriptor;
    g_vst2_callbacks = callbacks;
    g_vst2_params = params;
    g_vst2_num_params = num_params;
    g_vst2_registered = 1;
}

/* ---------------------------------------------------------------------------
 * Per-instance state
 * --------------------------------------------------------------------------- */

typedef struct {
    AEffect effect;             /* MUST be first — host casts pointer */
    void* rust_ctx;
    audioMasterCallback master;
    float sample_rate;
    int32_t block_size;
    Vst2MidiEventCompact midi_buf[256];
    uint32_t midi_count;
    int state_loaded;           /* set after effSetChunk or effMainsChanged */
    void* deferred_parent;      /* stashed parent view if editor opened before state loaded */
} TruceVst2;

/* ---------------------------------------------------------------------------
 * Dispatcher
 * --------------------------------------------------------------------------- */

static VstIntPtr dispatcher(AEffect* e, int32_t opcode, int32_t index,
                            VstIntPtr value, void* ptr, float opt) {
    TruceVst2* inst = (TruceVst2*)e;
    (void)index;

    switch (opcode) {
        case effOpen:
            return 0;

        case effClose:
            if (inst->rust_ctx && g_vst2_callbacks)
                g_vst2_callbacks->destroy(inst->rust_ctx);
            free(inst);
            return 0;

        case effEditIdle:
            return 0;

        case effEditGetRect: {
            if (!g_vst2_callbacks || !inst->rust_ctx || !ptr) return 0;
            if (!g_vst2_callbacks->gui_has_editor(inst->rust_ctx)) return 0;
            uint32_t w = 0, h = 0;
            g_vst2_callbacks->gui_get_size(inst->rust_ctx, &w, &h);
            /* VST2 ERect: top, left, bottom, right (int16) */
            static int16_t rect[4];
            rect[0] = 0;          /* top */
            rect[1] = 0;          /* left */
            rect[2] = (int16_t)h; /* bottom */
            rect[3] = (int16_t)w; /* right */
            *(int16_t**)ptr = rect;
            return 1;
        }

        case effEditOpen: {
            if (!g_vst2_callbacks || !inst->rust_ctx || !ptr) return 0;
            if (!g_vst2_callbacks->gui_has_editor(inst->rust_ctx)) return 0;
            if (!inst->state_loaded) {
                /* Editor requested before state restored — defer gui_open */
                inst->deferred_parent = ptr;
            } else {
                g_vst2_callbacks->gui_open(inst->rust_ctx, ptr);
            }
            return 1;
        }

        case effEditClose:
            if (!g_vst2_callbacks || !inst->rust_ctx) return 0;
            inst->deferred_parent = NULL;
            g_vst2_callbacks->gui_close(inst->rust_ctx);
            return 0;

        case effSetSampleRate:
            inst->sample_rate = opt;
            return 0;

        case effSetBlockSize:
            inst->block_size = (int32_t)value;
            return 0;

        case effMainsChanged:
            if (value && g_vst2_callbacks && inst->rust_ctx) {
                /* Fresh instance (no saved state) — allow deferred editor to open */
                if (!inst->state_loaded) {
                    inst->state_loaded = 1;
                    if (inst->deferred_parent) {
                        g_vst2_callbacks->gui_open(inst->rust_ctx, inst->deferred_parent);
                        inst->deferred_parent = NULL;
                    }
                }
                g_vst2_callbacks->reset(inst->rust_ctx,
                    (double)inst->sample_rate, (uint32_t)inst->block_size);
            }
            inst->midi_count = 0;
            return 0;

        case effProcessEvents: {
            if (!ptr) return 0;
            VstEvents* events = (VstEvents*)ptr;
            for (int32_t i = 0; i < events->numEvents && inst->midi_count < 256; i++) {
                VstEvent* ev = events->events[i];
                if (ev->type != kVstMidiType) continue;
                VstMidiEvent* midi = (VstMidiEvent*)ev;
                Vst2MidiEventCompact* out = &inst->midi_buf[inst->midi_count++];
                out->delta_frames = (uint32_t)midi->deltaFrames;
                out->status = (uint8_t)midi->midiData[0];
                out->data1 = (uint8_t)midi->midiData[1];
                out->data2 = (uint8_t)midi->midiData[2];
                out->_pad = 0;
            }
            return 1;
        }

        case effGetParamName: {
            if (!ptr || (uint32_t)index >= g_vst2_num_params) return 0;
            strncpy((char*)ptr, g_vst2_params[index].name, 32);
            ((char*)ptr)[31] = 0;
            return 0;
        }

        case effGetParamLabel: {
            if (!ptr || (uint32_t)index >= g_vst2_num_params) return 0;
            strncpy((char*)ptr, g_vst2_params[index].unit, 16);
            ((char*)ptr)[15] = 0;
            return 0;
        }

        case effGetParamDisplay: {
            if (!ptr || !g_vst2_callbacks || !inst->rust_ctx) return 0;
            if ((uint32_t)index >= g_vst2_num_params) return 0;
            uint32_t id = g_vst2_params[index].id;
            double val = g_vst2_callbacks->param_get_value(inst->rust_ctx, id);
            g_vst2_callbacks->param_format_value(
                inst->rust_ctx, id, val, (char*)ptr, 32);
            return 0;
        }

        case effGetProductString:
            if (!ptr || !g_vst2_descriptor) return 0;
            strncpy((char*)ptr, g_vst2_descriptor->name, 64);
            ((char*)ptr)[63] = 0;
            return 1;

        case effGetVendorString:
            if (!ptr || !g_vst2_descriptor) return 0;
            strncpy((char*)ptr, g_vst2_descriptor->vendor, 64);
            ((char*)ptr)[63] = 0;
            return 1;

        case effGetVendorVersion:
            return g_vst2_descriptor ? (VstIntPtr)g_vst2_descriptor->version : 1;

        case effGetEffectName:
            if (!ptr || !g_vst2_descriptor) return 0;
            strncpy((char*)ptr, g_vst2_descriptor->name, 64);
            ((char*)ptr)[63] = 0;
            return 1;

        case effCanDo: {
            if (!ptr) return 0;
            const char* s = (const char*)ptr;
            if (strcmp(s, "receiveVstMidiEvent") == 0) return 1;
            if (strcmp(s, "receiveVstEvents") == 0) return 1;
            if (strcmp(s, "sendVstMidiEvent") == 0) return 0;
            return 0;
        }

        case effGetTailSize:
            return (inst->rust_ctx && g_vst2_callbacks && g_vst2_callbacks->get_tail)
                ? g_vst2_callbacks->get_tail(inst->rust_ctx) : 0;

        case effCanBeAutomated:
            return 1;

        case effGetChunk: {
            if (!g_vst2_callbacks || !inst->rust_ctx || !ptr) return 0;
            uint8_t* data = NULL;
            uint32_t len = 0;
            g_vst2_callbacks->state_save(inst->rust_ctx, &data, &len);
            if (data && len > 0) {
                *(void**)ptr = data;
                return (VstIntPtr)len;
            }
            return 0;
        }

        case effSetChunk: {
            if (!g_vst2_callbacks || !inst->rust_ctx || !ptr) return 0;
            g_vst2_callbacks->state_load(inst->rust_ctx, (const uint8_t*)ptr, (uint32_t)value);
            inst->state_loaded = 1;
            /* If the editor was opened before state was restored, open it now */
            if (inst->deferred_parent) {
                g_vst2_callbacks->gui_open(inst->rust_ctx, inst->deferred_parent);
                inst->deferred_parent = NULL;
            }
            return 0;
        }

        default:
            return 0;
    }
}

/* ---------------------------------------------------------------------------
 * Host notification (GUI → host)
 * --------------------------------------------------------------------------- */

/* Notify the host that the user started editing a parameter (mouse-down). */
void truce_vst2_host_begin_edit(AEffect* e, uint32_t param_id) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!inst->master) return;
    for (uint32_t i = 0; i < g_vst2_num_params; i++) {
        if (g_vst2_params[i].id == param_id) {
            inst->master(e, audioMasterBeginEdit, (int32_t)i, 0, NULL, 0.0f);
            return;
        }
    }
}

/* Notify the host that a parameter value changed during a drag gesture. */
void truce_vst2_host_automate(AEffect* e, uint32_t param_id, float normalized) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!inst->master) return;
    for (uint32_t i = 0; i < g_vst2_num_params; i++) {
        if (g_vst2_params[i].id == param_id) {
            inst->master(e, audioMasterAutomate, (int32_t)i, 0, NULL, normalized);
            return;
        }
    }
}

/* Notify the host that the user finished editing a parameter (mouse-up). */
void truce_vst2_host_end_edit(AEffect* e, uint32_t param_id) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!inst->master) return;
    for (uint32_t i = 0; i < g_vst2_num_params; i++) {
        if (g_vst2_params[i].id == param_id) {
            inst->master(e, audioMasterEndEdit, (int32_t)i, 0, NULL, 0.0f);
            return;
        }
    }
}

/* ---------------------------------------------------------------------------
 * Parameters
 * --------------------------------------------------------------------------- */

static void setParameter(AEffect* e, int32_t index, float value) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!g_vst2_callbacks || !inst->rust_ctx) return;
    if ((uint32_t)index >= g_vst2_num_params) return;

    /* VST2 parameters are normalized [0,1]. Convert to plain value. */
    const Vst2ParamDescriptor* desc = &g_vst2_params[index];
    double plain = desc->min + (double)value * (desc->max - desc->min);
    g_vst2_callbacks->param_set_value(inst->rust_ctx, desc->id, plain);
}

static float getParameter(AEffect* e, int32_t index) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!g_vst2_callbacks || !inst->rust_ctx) return 0.0f;
    if ((uint32_t)index >= g_vst2_num_params) return 0.0f;

    /* Convert plain value to normalized [0,1]. */
    const Vst2ParamDescriptor* desc = &g_vst2_params[index];
    double plain = g_vst2_callbacks->param_get_value(inst->rust_ctx, desc->id);
    double range = desc->max - desc->min;
    if (range <= 0.0) return 0.0f;
    return (float)((plain - desc->min) / range);
}

/* ---------------------------------------------------------------------------
 * Process (in-place replacing)
 * --------------------------------------------------------------------------- */

static void processReplacing(AEffect* e, float** inputs, float** outputs,
                             int32_t sampleFrames) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!g_vst2_callbacks || !inst->rust_ctx) return;

    uint32_t numIn = g_vst2_descriptor->num_inputs;
    uint32_t numOut = g_vst2_descriptor->num_outputs;

    /* For effects: copy input to output before processing (in-place). */
    if (numIn > 0) {
        uint32_t ch = numIn < numOut ? numIn : numOut;
        for (uint32_t c = 0; c < ch; c++) {
            if (inputs[c] != outputs[c])
                memcpy(outputs[c], inputs[c], (size_t)sampleFrames * sizeof(float));
        }
    }

    g_vst2_callbacks->process(
        inst->rust_ctx,
        (const float**)inputs, outputs,
        numIn, numOut,
        (uint32_t)sampleFrames,
        inst->midi_buf, inst->midi_count);

    inst->midi_count = 0;
}

/* ---------------------------------------------------------------------------
 * Entry point
 * --------------------------------------------------------------------------- */

VST2_EXPORT
AEffect* VSTPluginMain(audioMasterCallback audioMaster);

VST2_EXPORT
AEffect* VSTPluginMain(audioMasterCallback audioMaster) {
    /* Check host VST version */
    if (audioMaster) {
        VstIntPtr hostVer = audioMaster(NULL, audioMasterVersion, 0, 0, NULL, 0.0f);
        if (hostVer == 0) return NULL; /* Host doesn't support VST2.4 */
    }

    if (!g_vst2_descriptor || !g_vst2_callbacks) return NULL;

    TruceVst2* inst = (TruceVst2*)calloc(1, sizeof(TruceVst2));
    if (!inst) return NULL;

    int is_synth = (g_vst2_descriptor->component_type[0] == 'a' &&
                    g_vst2_descriptor->component_type[1] == 'u' &&
                    g_vst2_descriptor->component_type[2] == 'm' &&
                    g_vst2_descriptor->component_type[3] == 'u');

    inst->effect.magic = kVstMagic;
    inst->effect.dispatcher = dispatcher;
    inst->effect.process = NULL;
    inst->effect.setParameter = setParameter;
    inst->effect.getParameter = getParameter;
    inst->effect.numPrograms = 1;
    inst->effect.numParams = (int32_t)g_vst2_num_params;
    inst->effect.numInputs = (int32_t)g_vst2_descriptor->num_inputs;
    inst->effect.numOutputs = (int32_t)g_vst2_descriptor->num_outputs;
    inst->rust_ctx = g_vst2_callbacks->create();
    /* Tell Rust about the AEffect pointer (for host callbacks). */
    if (inst->rust_ctx && g_vst2_callbacks->set_effect_ptr)
        g_vst2_callbacks->set_effect_ptr(inst->rust_ctx, &inst->effect);

    inst->effect.flags = effFlagsCanReplacing | effFlagsProgramChunks;
    if (is_synth) inst->effect.flags |= effFlagsIsSynth;
    if (inst->rust_ctx && g_vst2_callbacks->gui_has_editor(inst->rust_ctx))
        inst->effect.flags |= effFlagsHasEditor;
    inst->effect.uniqueID = FOURCC(g_vst2_descriptor->component_subtype);
    inst->effect.version = (int32_t)g_vst2_descriptor->version;
    inst->effect.initialDelay = (inst->rust_ctx && g_vst2_callbacks->get_latency)
        ? (int32_t)g_vst2_callbacks->get_latency(inst->rust_ctx) : 0;
    inst->effect.processReplacing = processReplacing;
    inst->effect.processDoubleReplacing = NULL;
    inst->effect.object = inst;

    inst->master = audioMaster;
    inst->sample_rate = 44100.0f;
    inst->block_size = 1024;

    return &inst->effect;
}

/* Also export as 'main' for some hosts */
VST2_EXPORT
AEffect* main_macho(audioMasterCallback audioMaster) {
    return VSTPluginMain(audioMaster);
}
