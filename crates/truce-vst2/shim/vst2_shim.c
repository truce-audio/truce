/**
 * Truce VST2 shim.
 *
 * Implements the AEffect interface, calling back into Rust for all
 * plugin logic. Clean-room implementation - no Steinberg SDK headers.
 */

#include "vst2_types.h"
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stddef.h>

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
    AEffect effect;             /* MUST be first - host casts pointer */
    void* rust_ctx;
    audioMasterCallback master;
    float sample_rate;
    int32_t block_size;
    Vst2MidiEventCompact midi_buf[256];
    uint32_t midi_count;
    int state_loaded;           /* set after effSetChunk or effMainsChanged */
    void* deferred_parent;      /* stashed parent view if editor opened before state loaded */
    /* Scratch for SysEx output: every `EventBody::SysEx` the plugin
     * emits gets framed (0xF0 + inner + 0xF7) into this buffer
     * before the `VstMidiSysExEvent::sysexDump` pointer is handed
     * to the host. Real-world VST2 hosts (Cubase, Reaper) expect
     * framed bytes per the Steinberg vendor extension's `sysexDump`
     * convention; truce's internal `EventBody::SysEx` stores inner
     * bytes only, so this is the wire-adaptation site. Sized to
     * match truce_core::SYSEX_POOL_PREALLOC (128 KiB) + 2 framing
     * bytes per event × up to 256 events. */
    uint8_t sysex_out_scratch[128 * 1024 + 512];
    uint32_t sysex_out_used;
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
                /* Editor requested before state restored - defer gui_open */
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
                /* Fresh instance (no saved state) - allow deferred editor to open */
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
            for (int32_t i = 0; i < events->numEvents; i++) {
                VstEvent* ev = events->events[i];
                if (ev->type == kVstMidiType) {
                    if (inst->midi_count >= 256) continue;
                    VstMidiEvent* midi = (VstMidiEvent*)ev;
                    Vst2MidiEventCompact* out = &inst->midi_buf[inst->midi_count++];
                    out->delta_frames = (uint32_t)midi->deltaFrames;
                    out->status = (uint8_t)midi->midiData[0];
                    out->data1 = (uint8_t)midi->midiData[1];
                    out->data2 = (uint8_t)midi->midiData[2];
                    out->_pad = 0;
                } else if (ev->type == kVstSysExType) {
                    /* Forward to Rust. The byte buffer the host
                     * hands us is only valid for the duration of
                     * this call; the Rust callback copies into
                     * `EventList`'s SysEx pool synchronously.
                     *
                     * Real-world VST2 hosts (Cubase, Reaper) deliver
                     * framed SysEx - leading 0xF0, trailing 0xF7 -
                     * per the Steinberg vendor extension's
                     * `sysexDump` convention. Truce's internal
                     * `EventBody::SysEx` stores inner bytes only,
                     * so strip the framing here. Defensive: also
                     * accept un-framed payloads from non-conforming
                     * hosts. */
                    VstMidiSysExEvent* sx = (VstMidiSysExEvent*)ev;
                    if (g_vst2_callbacks && g_vst2_callbacks->push_sysex_input
                            && sx->sysexDump && sx->dumpBytes > 0) {
                        const uint8_t* p = (const uint8_t*)sx->sysexDump;
                        uint32_t n = (uint32_t)sx->dumpBytes;
                        if (p[0] == 0xF0) { p++; n--; }
                        if (n > 0 && p[n - 1] == 0xF7) { n--; }
                        g_vst2_callbacks->push_sysex_input(inst->rust_ctx,
                            (uint32_t)sx->deltaFrames, p, n);
                    }
                }
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
            g_vst2_callbacks->param_format_current(
                inst->rust_ctx, id, (char*)ptr, 32);
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
            /* Advertise MIDI capabilities only in the directions the
             * plugin uses, from the one `emits_midi`/`accepts_midi_in`
             * pair (category default, overridable via `midi_input` /
             * `midi_output` in truce.toml). A plain audio effect
             * advertises none. */
            int wants_in = g_vst2_descriptor && g_vst2_descriptor->accepts_midi_in;
            int wants_out = g_vst2_descriptor && g_vst2_descriptor->emits_midi;
            if (strcmp(s, "receiveVstMidiEvent") == 0) return wants_in;
            if (strcmp(s, "receiveVstEvents") == 0) return wants_in;
            if (strcmp(s, "receiveVstMidiSysExEvent") == 0) return wants_in;
            if (strcmp(s, "sendVstMidiEvent") == 0) return wants_out;
            if (strcmp(s, "sendVstEvents") == 0) return wants_out;
            if (strcmp(s, "sendVstMidiSysExEvent") == 0) return wants_out;
            if (strcmp(s, "bypass") == 0) {
                /* Advertise bypass support only if the plugin actually
                 * has an IS_BYPASS-flagged param wired into the
                 * descriptor - the effSetBypass handler is a no-op
                 * otherwise. */
                return (g_vst2_descriptor
                        && g_vst2_descriptor->bypass_param_id != 0xFFFFFFFFu)
                       ? 1 : 0;
            }
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
            /* 1 when the blob was accepted (truce envelope, or the
             * plugin's migrate_state translated it), 0 otherwise. */
            VstIntPtr ok = g_vst2_callbacks->state_load(
                inst->rust_ctx, (const uint8_t*)ptr, (uint32_t)value);
            inst->state_loaded = 1;
            /* If the editor was opened before state was restored, open it now */
            if (inst->deferred_parent) {
                g_vst2_callbacks->gui_open(inst->rust_ctx, inst->deferred_parent);
                inst->deferred_parent = NULL;
            }
            return ok;
        }

        /* opcode 77 - VST 2.4 host announces the precision it will
         * process with (`value`: 0 = f32, 1 = f64). Both entry points
         * stay valid regardless, so this is just an ack: accept f32
         * always, f64 only when advertised. */
        case 77 /* effSetProcessPrecision */:
            if (value == 0) return 1;
            return (value == 1 && g_vst2_descriptor
                    && g_vst2_descriptor->supports_f64) ? 1 : 0;

        /* opcode 44 - host announces bypass on/off. Route to the
         * IS_BYPASS-flagged param (if any) so the param value tracks
         * the host's master-bypass UI. `value` is 0 (off) or 1 (on). */
        case 44 /* effSetBypass */: {
            if (!g_vst2_callbacks || !inst->rust_ctx || !g_vst2_descriptor) return 0;
            if (g_vst2_descriptor->bypass_param_id == 0xFFFFFFFFu) return 0;
            g_vst2_callbacks->param_set_normalized(
                inst->rust_ctx,
                g_vst2_descriptor->bypass_param_id,
                value ? 1.0 : 0.0);
            return 1;
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

/* Fill `out` with the host's current transport state.
 *
 * Wraps `audioMasterGetTime`. Returns with `out->valid = 0` if the host
 * refuses (rare in practice - most hosts always return time info). Safe
 * to call from the audio thread: the host callback is blocking but
 * expected to complete in tens of nanoseconds. */
void truce_vst2_host_get_time(AEffect* e, Vst2TransportSnapshot* out) {
    memset(out, 0, sizeof(*out));
    TruceVst2* inst = (TruceVst2*)e;
    if (!inst->master) return;

    int32_t mask = kVstNanosValid | kVstPpqPosValid | kVstTempoValid |
                   kVstBarsValid | kVstCyclePosValid | kVstTimeSigValid;
    VstIntPtr ret = inst->master(e, audioMasterGetTime, 0, (VstIntPtr)mask, NULL, 0.0f);
    if (!ret) return;

    const VstTimeInfo* ti = (const VstTimeInfo*)(uintptr_t)ret;
    out->valid = 1;
    out->playing = (ti->flags & kVstTransportPlaying) ? 1 : 0;
    out->recording = (ti->flags & kVstTransportRecording) ? 1 : 0;
    out->loop_active = (ti->flags & kVstTransportCycleActive) ? 1 : 0;
    out->position_samples = ti->sample_pos;
    if (ti->flags & kVstTempoValid) out->tempo = ti->tempo;
    if (ti->flags & kVstPpqPosValid) out->position_beats = ti->ppq_pos;
    if (ti->flags & kVstBarsValid) out->bar_start_beats = ti->bar_start_pos;
    if (ti->flags & kVstTimeSigValid) {
        out->time_sig_num = ti->time_sig_num;
        out->time_sig_den = ti->time_sig_den;
    }
    if (ti->flags & kVstCyclePosValid) {
        out->loop_start_beats = ti->cycle_start_pos;
        out->loop_end_beats = ti->cycle_end_pos;
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

    /* VST2 parameters are normalized [0,1]. Hand the raw normalized
     * value through to Rust; the Params layer routes through
     * `ParamRange::denormalize` so non-linear tapers (Logarithmic,
     * Enum, Discrete) round-trip correctly. */
    uint32_t id = g_vst2_params[index].id;
    g_vst2_callbacks->param_set_normalized(inst->rust_ctx, id, (double)value);
}

static float getParameter(AEffect* e, int32_t index) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!g_vst2_callbacks || !inst->rust_ctx) return 0.0f;
    if ((uint32_t)index >= g_vst2_num_params) return 0.0f;

    uint32_t id = g_vst2_params[index].id;
    return (float)g_vst2_callbacks->param_get_normalized(inst->rust_ctx, id);
}

/* ---------------------------------------------------------------------------
 * Process (in-place replacing)
 * --------------------------------------------------------------------------- */

/* Width-agnostic body shared by processReplacing (f32) and
 * processDoubleReplacing (f64): the two differ only in the in-place
 * memcpy width and which Rust callback runs. */
static void processAnyReplacing(AEffect* e, void** inputs, void** outputs,
                                int32_t sampleFrames, int use64) {
    TruceVst2* inst = (TruceVst2*)e;
    if (!g_vst2_callbacks || !inst->rust_ctx) return;

    uint32_t numIn = g_vst2_descriptor->num_inputs;
    uint32_t numOut = g_vst2_descriptor->num_outputs;
    size_t sampleBytes = use64 ? sizeof(double) : sizeof(float);

    /* For effects: copy input to output before processing (in-place). */
    if (numIn > 0) {
        uint32_t ch = numIn < numOut ? numIn : numOut;
        for (uint32_t c = 0; c < ch; c++) {
            if (inputs[c] != outputs[c])
                memcpy(outputs[c], inputs[c], (size_t)sampleFrames * sampleBytes);
        }
    }

    if (use64)
        g_vst2_callbacks->process_f64(
            inst->rust_ctx,
            (const double**)inputs, (double**)outputs,
            numIn, numOut,
            (uint32_t)sampleFrames,
            inst->midi_buf, inst->midi_count);
    else
        g_vst2_callbacks->process(
            inst->rust_ctx,
            (const float**)inputs, (float**)outputs,
            numIn, numOut,
            (uint32_t)sampleFrames,
            inst->midi_buf, inst->midi_count);

    inst->midi_count = 0;

    /* Drain plugin → host MIDI. The Rust side has already filtered
     * the queue down to events that fit in 3-byte MIDI 1.0 packets;
     * we rebuild a `VstEvents` block in stack-local storage and call
     * `audioMasterProcessEvents`. Cap matches the input direction so
     * we can't get a runaway event count past the host's expected
     * per-block budget. */
    if (inst->master) {
        uint32_t midi_count = g_vst2_callbacks->output_event_count(inst->rust_ctx);
        uint32_t sx_count = g_vst2_callbacks->output_sysex_count
            ? g_vst2_callbacks->output_sysex_count(inst->rust_ctx)
            : 0;
        if (midi_count > 256) midi_count = 256;
        if (sx_count > 256) sx_count = 256;
        uint32_t total = midi_count + sx_count;
        if (total > 0) {
            VstMidiEvent midis[256];
            VstMidiSysExEvent sxs[256];
            /* `VstEvents` declares `events[2]` for alignment; for N
             * events, lay out `numEvents`, `reserved`, then a
             * trailing `VstEvent*[N]`. Use `offsetof(VstEvents,
             * events)` rather than summing field sizes: on LP64 the
             * compiler pads `numEvents` out to `reserved`'s 8-byte
             * alignment, so the pointer array starts at offset 16,
             * not the 12 a naive `sizeof(int32_t)+sizeof(VstIntPtr)`
             * would give. The storage buffer is sized for
             * `midi_count + sx_count` so pointer arithmetic stays
             * in-bounds. */
            char vstEvents_storage[offsetof(VstEvents, events)
                                   + 512 * sizeof(VstEvent*)];
            VstEvents* vstEvents = (VstEvents*)vstEvents_storage;
            vstEvents->numEvents = (int32_t)total;
            vstEvents->reserved = 0;
            VstEvent** events_array = (VstEvent**)((char*)vstEvents
                                                   + offsetof(VstEvents, events));
            for (uint32_t i = 0; i < midi_count; i++) {
                Vst2MidiEventCompact pkt = {0};
                g_vst2_callbacks->output_event_at(inst->rust_ctx, i, &pkt);
                VstMidiEvent* m = &midis[i];
                memset(m, 0, sizeof(*m));
                m->type = kVstMidiType;
                m->byteSize = (int32_t)sizeof(VstMidiEvent);
                m->deltaFrames = (int32_t)pkt.delta_frames;
                m->midiData[0] = (char)pkt.status;
                m->midiData[1] = (char)pkt.data1;
                m->midiData[2] = (char)pkt.data2;
                m->midiData[3] = 0;
                events_array[i] = (VstEvent*)m;
            }
            inst->sysex_out_used = 0;
            for (uint32_t i = 0; i < sx_count; i++) {
                uint32_t delta = 0;
                const uint8_t* bytes = NULL;
                uint32_t len = 0;
                g_vst2_callbacks->output_sysex_at(inst->rust_ctx, i,
                                                  &delta, &bytes, &len);
                /* Frame the inner bytes (0xF0 + inner + 0xF7) into
                 * the per-block scratch. Real-world VST2 hosts
                 * expect framed SysEx per the Steinberg vendor
                 * extension; truce's pool stores inner bytes only.
                 * Skip the event if the scratch is exhausted -
                 * truncating SysEx is never the right answer. */
                uint32_t framed_len = len + 2;
                if (inst->sysex_out_used + framed_len > sizeof(inst->sysex_out_scratch)) {
                    continue;
                }
                uint8_t* dst = inst->sysex_out_scratch + inst->sysex_out_used;
                dst[0] = 0xF0;
                if (bytes && len > 0) memcpy(dst + 1, bytes, len);
                dst[1 + len] = 0xF7;
                inst->sysex_out_used += framed_len;

                VstMidiSysExEvent* sx = &sxs[i];
                memset(sx, 0, sizeof(*sx));
                sx->type = kVstSysExType;
                sx->byteSize = (int32_t)sizeof(VstMidiSysExEvent);
                sx->deltaFrames = (int32_t)delta;
                sx->dumpBytes = (int32_t)framed_len;
                /* Cast through `uintptr_t` strips `const`; the SDK
                 * declares the field non-const because legacy hosts
                 * could edit in place, but our scratch is logically
                 * read-only from the host's perspective. */
                sx->sysexDump = (char*)(uintptr_t)dst;
                events_array[midi_count + i] = (VstEvent*)sx;
            }
            inst->master(e, audioMasterProcessEvents, 0, 0, vstEvents, 0.0f);
        }
    }
}

static void processReplacing(AEffect* e, float** inputs, float** outputs,
                             int32_t sampleFrames) {
    processAnyReplacing(e, (void**)inputs, (void**)outputs, sampleFrames, 0);
}

static void processDoubleReplacing(AEffect* e, double** inputs, double** outputs,
                                   int32_t sampleFrames) {
    processAnyReplacing(e, (void**)inputs, (void**)outputs, sampleFrames, 1);
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

    /* `effFlagsIsSynth` is the VST2 signal "host should route MIDI to
     * me." Both instruments (`aumu`) and MIDI processors (`aumi`,
     * arpeggiators / chord generators) need it - without it, hosts
     * default to audio-only routing and the MIDI input never arrives.
     * VST2 has no separate "MIDI effect" category, so setting the
     * synth bit is the documented workaround. */
    int is_synth = (g_vst2_descriptor->component_type[0] == 'a' &&
                    g_vst2_descriptor->component_type[1] == 'u' &&
                    g_vst2_descriptor->component_type[2] == 'm' &&
                    (g_vst2_descriptor->component_type[3] == 'u' ||
                     g_vst2_descriptor->component_type[3] == 'i'));

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
    if (g_vst2_descriptor->supports_f64)
        inst->effect.flags |= effFlagsCanDoubleReplacing;
    if (inst->rust_ctx && g_vst2_callbacks->gui_has_editor(inst->rust_ctx))
        inst->effect.flags |= effFlagsHasEditor;
    inst->effect.uniqueID = FOURCC(g_vst2_descriptor->component_subtype);
    inst->effect.version = (int32_t)g_vst2_descriptor->version;
    inst->effect.initialDelay = (inst->rust_ctx && g_vst2_callbacks->get_latency)
        ? (int32_t)g_vst2_callbacks->get_latency(inst->rust_ctx) : 0;
    inst->effect.processReplacing = processReplacing;
    inst->effect.processDoubleReplacing = g_vst2_descriptor->supports_f64
        ? processDoubleReplacing : NULL;
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
